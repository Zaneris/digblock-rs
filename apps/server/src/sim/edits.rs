//! Edit validation (re-raycast from the claimed origin) and the fluid
//! step, which turns settled flow into ordinary journaled edits.

use super::{cell_of, client_send, in_radius, send_frame, Sim};
use crate::store::StoreWorld;
use digblock_core::block::BlockType;
use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
use digblock_core::player::TICK;
use digblock_core::raycast::{raycast, REACH};
use digblock_net::messages::{BlockEdit, EditKind, RejectReason, ServerMsg};
use digblock_net::protocol::{
	PlayerId, EDIT_BURST, EDIT_POS_TOLERANCE, EDIT_RATE_PER_SEC, SERVER_ID, TICK_RATE,
};
use glam::Vec3;

pub(super) struct PendingEditReq {
	pub(super) conn_id: u64,
	pub(super) seq: u32,
	pub(super) origin: Vec3,
	pub(super) dir: Vec3,
	pub(super) kind: EditKind,
	pub(super) cell: (i32, i32, i32),
	pub(super) normal: (i32, i32, i32),
}

impl Sim {
	pub(super) fn process_edits(&mut self) {
		// Refill token buckets.
		for client in self.clients.values_mut() {
			client.edit_tokens =
				(client.edit_tokens + EDIT_RATE_PER_SEC as f32 / TICK_RATE as f32).min(EDIT_BURST);
		}
		let queue = std::mem::take(&mut self.edit_queue);
		for req in queue {
			self.process_one_edit(req);
		}
	}

	fn process_one_edit(&mut self, req: PendingEditReq) {
		// Copy what we need out of the client entry so the &mut borrow
		// ends before the all-players InsidePlayer scan below.
		let (id, out, live, player_pos, has_token) = {
			let Some(client) = self.clients.get_mut(&req.conn_id) else {
				return;
			};
			let has_token = client.edit_tokens >= 1.0;
			if client.live && has_token {
				client.edit_tokens -= 1.0;
			}
			(
				client.id,
				client.out.clone(),
				client.live,
				client.player.pos,
				has_token,
			)
		};
		// A refused push here just drops the reply; a stalled peer is
		// caught (and killed) by the snapshot/broadcast paths within ticks.
		let reject = |reason: RejectReason| {
			send_frame(
				&out,
				&ServerMsg::EditRejected {
					client_seq: req.seq,
					reason,
				},
			);
		};

		if !live {
			return reject(RejectReason::NotLoaded);
		}
		if !has_token {
			return reject(RejectReason::RateLimited);
		}
		// A journal that can't write means edits would exist in memory
		// only — refuse visibly rather than silently losing the world.
		if !self.journal.healthy() {
			return reject(RejectReason::ServerError);
		}

		let place_block = match req.kind {
			EditKind::Dig => None,
			EditKind::Place { block } => {
				if block == BlockType::Air as u8 || block as usize >= BlockType::COUNT {
					return reject(RejectReason::BadBlock);
				}
				Some(BlockType::from_u8(block))
			}
		};
		if !req.origin.is_finite() || !req.dir.is_finite() {
			return reject(RejectReason::OutOfReach);
		}
		let dir = req.dir.normalize_or_zero();
		if dir == Vec3::ZERO {
			return reject(RejectReason::OutOfReach);
		}
		if req.origin.distance(player_pos) > EDIT_POS_TOLERANCE {
			return reject(RejectReason::OutOfReach);
		}

		// Re-cast the client's ray against the server world: an honest
		// client computed the identical result.
		let store = &self.store;
		let Some(hit) = raycast(req.origin, dir, REACH, &|x, y, z| store.block_at(x, y, z)) else {
			return reject(RejectReason::OutOfReach);
		};
		if (hit.cell.0, hit.cell.1, hit.cell.2) != req.cell {
			return reject(RejectReason::Occluded);
		}

		let (target, block) = match place_block {
			None => (req.cell, BlockType::Air),
			Some(b) => {
				if hit.normal == (0, 0, 0) {
					return reject(RejectReason::Occluded);
				}
				if (hit.normal.0, hit.normal.1, hit.normal.2) != req.normal {
					return reject(RejectReason::Occluded);
				}
				let target = (
					req.cell.0 + hit.normal.0,
					req.cell.1 + hit.normal.1,
					req.cell.2 + hit.normal.2,
				);
				// Mirrors the local-engine rejection (eye/body cell) but
				// for every live player.
				for c in self.clients.values() {
					if !c.live {
						continue;
					}
					let eye = cell_of(c.player.pos);
					let body = cell_of(c.player.pos - Vec3::new(0.0, 1.25, 0.0));
					if target == eye || target == body {
						return reject(RejectReason::InsidePlayer);
					}
				}
				(target, b)
			}
		};

		let Some(prev) = self.store.set_block(target.0, target.1, target.2, block) else {
			return reject(RejectReason::NotLoaded);
		};
		let edit = BlockEdit {
			x: target.0,
			y: target.1,
			z: target.2,
			block: block as u8,
		};
		let (seq, new_column) = self.journal.append(edit, id);
		if new_column {
			self.enroll_new_column(edit);
		}
		self.fluid.schedule_around(target.0, target.1, target.2);
		self.broadcast_edit(seq, id, req.seq, edit, prev as u8);
	}

	/// A column just gained its FIRST edit: enroll every in-radius client
	/// BEFORE the broadcast, so the `EditApplied` itself carries the data
	/// and no JournalColumn resend is ever needed for it. This is what
	/// lets the steady-state residency sync skip journal scans entirely.
	fn enroll_new_column(&mut self, edit: BlockEdit) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		for c in self.clients.values_mut() {
			if in_radius(col, c.center, c.draw_dist) {
				c.synced_columns.insert(col);
			}
		}
	}

	pub(super) fn broadcast_edit(
		&self,
		seq: u64,
		by: PlayerId,
		client_seq: u32,
		edit: BlockEdit,
		prev: u8,
	) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		let msg = ServerMsg::EditApplied {
			seq,
			tick: self.tick,
			by,
			client_seq,
			edit,
			prev,
		};
		for c in self.clients.values() {
			if c.synced_columns.contains(&col) || c.id == by {
				client_send(c, &msg);
			}
		}
	}

	pub(super) fn step_fluid(&mut self) {
		// Unhealthy journal: stop mutating the world durably — fluid
		// writes are journaled edits like any other.
		if !self.journal.healthy() {
			return;
		}
		self.flow_accum = (self.flow_accum + TICK).min(1.0);
		while self.flow_accum >= FLOW_STEP_SECS {
			self.flow_accum -= FLOW_STEP_SECS;
			let mut edits = Vec::new();
			let mut world = StoreWorld {
				store: &mut self.store,
				edits: &mut edits,
			};
			self.fluid.step(&mut world, FLOW_BUDGET);
			for (edit, prev) in edits {
				let (seq, new_column) = self.journal.append(edit, SERVER_ID);
				if new_column {
					self.enroll_new_column(edit);
				}
				self.broadcast_edit(seq, SERVER_ID, 0, edit, prev);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::super::harness::*;
	use super::super::SimEvent;
	use digblock_core::block::BlockType;
	use digblock_core::raycast::{raycast, REACH};
	use digblock_net::messages::{ClientMsg, EditKind, RejectReason, ServerMsg};
	use glam::Vec3;

	#[test]
	fn edits_validate_apply_and_journal() {
		let mut sim = flat_sim("edits");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;

		// The ground block straight down is in reach.
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_block_at(x, y, z))
			.expect("ground under spawn");

		let dig = |seq: u32, origin: Vec3, cell: (i32, i32, i32)| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 100,
				origin: origin.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [cell.0, cell.1, cell.2],
				normal: [0, 0, 0],
			},
		};

		// 1. Wrong cell → Occluded.
		sim.tick_once(vec![dig(1, pos, (hit.cell.0 + 1, hit.cell.1, hit.cell.2))]);
		// 2. Origin 3 blocks off → OutOfReach.
		sim.tick_once(vec![dig(2, pos + Vec3::new(3.0, 0.0, 0.0), hit.cell)]);
		// 3. Honest dig → applied.
		sim.tick_once(vec![dig(3, pos, hit.cell)]);
		c.drain();

		let rejected: Vec<(u32, RejectReason)> = c
			.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::EditRejected { client_seq, reason } => Some((*client_seq, *reason)),
				_ => None,
			})
			.collect();
		assert!(
			rejected.contains(&(1, RejectReason::Occluded)),
			"{rejected:?}"
		);
		assert!(
			rejected.contains(&(2, RejectReason::OutOfReach)),
			"{rejected:?}"
		);
		let applied = c.msgs.iter().find_map(|m| match m {
			ServerMsg::EditApplied {
				client_seq, edit, ..
			} if *client_seq == 3 => Some(*edit),
			_ => None,
		});
		let edit = applied.expect("dig 3 applied");
		assert_eq!((edit.x, edit.y, edit.z), hit.cell);
		assert_eq!(edit.block, BlockType::Air as u8);
		assert_eq!(
			sim.test_block_at(edit.x, edit.y, edit.z),
			Some(BlockType::Air)
		);
		assert!(sim
			.journal
			.index
			.column(edit.x.div_euclid(16), edit.z.div_euclid(16))
			.is_some_and(|col| col.get(&(edit.x, edit.y, edit.z)) == Some(&(BlockType::Air as u8))));
	}

	#[test]
	fn unhealthy_journal_rejects_edits_with_server_error() {
		let mut sim = flat_sim("jhealth");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();

		sim.journal.force_write_failure = true;
		// Each accepted dig hits one failing append; after three the
		// journal flips unhealthy and the next request is refused.
		for seq in 1..=4u32 {
			let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_block_at(x, y, z));
			let Some(hit) = hit else { break };
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 0,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Dig,
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [0, 0, 0],
				},
			}]);
		}
		// The dig target changes as blocks vanish, so also fire a plain
		// guaranteed-processed request once unhealthy.
		assert!(!sim.journal.healthy(), "injected failures flipped health");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 9,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [0, 0, 0],
				normal: [0, 0, 0],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::ServerError,
					..
				}
			)),
			"unhealthy journal surfaces as ServerError: {:?}",
			c.msgs
		);
	}

	#[test]
	fn place_into_player_and_rate_limit() {
		let mut sim = flat_sim("place");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_block_at(x, y, z)).unwrap();

		// Placing on the ground's top face targets our own body cell.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 100,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place {
					block: BlockType::Dirt as u8,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::InsidePlayer
				}
			)),
			"{:?}",
			c.msgs
		);

		// Hammer digs: the token bucket runs dry.
		let digs: Vec<SimEvent> = (10..30)
			.map(|seq| SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 100,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Dig,
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [0, 0, 0],
				},
			})
			.collect();
		sim.tick_once(digs);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::RateLimited,
					..
				}
			)),
			"rate limiter engaged"
		);
	}
}
