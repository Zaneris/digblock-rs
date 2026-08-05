//! Edit validation (re-raycast from the claimed origin) and the fluid
//! step, which turns settled flow into ordinary journaled edits.

use super::{client_send, in_radius, send_frame, Sim};
use crate::store::StoreWorld;
use digblock_core::block::BlockType;
use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
use digblock_core::player::TICK;
use digblock_core::raycast::{raycast, raycast_fluid, REACH};
use digblock_net::messages::{BlockEdit, EditKind, RejectReason, ServerMsg};
use digblock_net::protocol::{
	dig_grace, PlayerId, CHAT_BURST, CHAT_RATE_PER_SEC, DIG_OFF_TARGET_RESET, EDIT_BURST,
	EDIT_POS_TOLERANCE, EDIT_RATE_PER_SEC, SERVER_ID, TICK_RATE,
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

/// One queued client action, processed in WIRE ORDER within the tick.
/// DigStart/DigCancel must ride the same queue as edit requests: back-to-
/// back digs put "complete block A" and "start block B" in the same tick
/// drain, and applying the start out of order would clobber the hold that
/// A's completion is about to be validated against (an honest dig would
/// bounce with DigIncomplete).
pub(super) enum ClientAction {
	Edit(PendingEditReq),
	DigStart {
		conn_id: u64,
		cell: (i32, i32, i32),
	},
	DigCancel {
		conn_id: u64,
	},
	/// Alchemy rides the same queue: a craft-then-place in one tick must
	/// debit before the place's stock gate runs (wire order).
	Alchemy {
		conn_id: u64,
		op: digblock_net::messages::AlchemyOp,
	},
}

/// Server-side hold-to-dig record. The block is captured at DigStart from
/// the server's own world — the client never claims it, so a mid-hold
/// content swap (say leaves → stone) resets rather than inheriting the
/// cheaper block's timer.
pub(super) struct DigState {
	pub(super) cell: (i32, i32, i32),
	pub(super) block: BlockType,
	/// Ticks the server's own aim ray has confirmed on this cell. Only
	/// these count toward completion — wall ticks with a missed aim are
	/// a pause, not progress and not a reset.
	pub(super) progress: u32,
	/// Consecutive ticks the server's own aim ray missed the cell.
	pub(super) off_target: u32,
}

impl Sim {
	/// Aim-tracking for in-progress holds: every tick, re-raycast each
	/// digging client from the server's OWN simulated position and look.
	/// An on-target tick advances `progress`; a missed tick merely
	/// doesn't (the server's view of the aim lags the client's by jitter
	/// buffering and reconciliation offsets, so transient misses are
	/// expected from honest clients). Only a sustained miss — longer
	/// than [`DIG_OFF_TARGET_RESET`] consecutive ticks — or the cell's
	/// content changing abandons the hold.
	///
	/// This is what makes the completion check unforgeable: the only
	/// client-supplied input is the initial cell, and every counted tick
	/// was verified against the server's own position and look. Do NOT
	/// "optimize" this away — it is ≤254 short DDA walks per tick, and
	/// it is the anti-cheat.
	pub(super) fn verify_digs(&mut self) {
		let store = &self.store;
		for client in self.clients.values_mut() {
			let Some(state) = &mut client.digging else {
				continue;
			};
			// Content swap: reset immediately (never pause-and-resume) —
			// a collaborator can't cycle a cell under a banked timer.
			if store.block_at(state.cell.0, state.cell.1, state.cell.2) != Some(state.block) {
				client.digging = None;
				continue;
			}
			// Water sources are picked with the fluid-aware ray; solid
			// digs with the plain one — same filters the client used.
			let cast = if state.block.is_fluid_source() {
				raycast_fluid
			} else {
				raycast
			};
			// Reach slack: this ray starts at the server's eye, which may
			// trail the client's by up to the claimed-origin tolerance — a
			// target the client legitimately picked at max reach would
			// otherwise miss every verify tick and never bank progress.
			// The completing edit still enforces exact REACH from its
			// (tolerance-checked) claimed origin.
			let hit = cast(
				client.player.pos,
				client.look,
				REACH + EDIT_POS_TOLERANCE,
				&|x, y, z| store.cell_at(x, y, z),
			);
			let on_target = hit.is_some_and(|h| h.cell == state.cell && h.block == state.block);
			if on_target {
				state.progress += 1;
				state.off_target = 0;
			} else {
				state.off_target += 1;
				if state.off_target > DIG_OFF_TARGET_RESET {
					client.digging = None;
				}
			}
		}
	}

	pub(super) fn process_edits(&mut self) {
		// Refill token buckets.
		for client in self.clients.values_mut() {
			client.edit_tokens =
				(client.edit_tokens + EDIT_RATE_PER_SEC as f32 / TICK_RATE as f32).min(EDIT_BURST);
			client.chat_tokens =
				(client.chat_tokens + CHAT_RATE_PER_SEC as f32 / TICK_RATE as f32).min(CHAT_BURST);
		}
		let queue = std::mem::take(&mut self.action_queue);
		for action in queue {
			match action {
				ClientAction::Edit(req) if req.kind == EditKind::Interact => {
					self.process_interact(req)
				}
				ClientAction::Edit(req) => self.process_one_edit(req),
				ClientAction::DigStart { conn_id, cell } => self.handle_dig_start(conn_id, cell),
				ClientAction::DigCancel { conn_id } => {
					if let Some(client) = self.clients.get_mut(&conn_id) {
						client.digging = None;
					}
				}
				ClientAction::Alchemy { conn_id, op } => self.process_alchemy(conn_id, op),
			}
		}
	}

	fn process_one_edit(&mut self, req: PendingEditReq) {
		// Copy what we need out of the client entry so the &mut borrow
		// ends before the all-players InsidePlayer scan below.
		let (id, out, live, player_pos, has_token, dig_state) = {
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
				client
					.digging
					.as_ref()
					.map(|d| (d.cell, d.block, d.progress)),
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

		let place_cell = match req.kind {
			EditKind::Dig | EditKind::Collect => None,
			EditKind::Interact => unreachable!("routed to process_interact"),
			EditKind::Place { block } => {
				// Strict: unknown ids, spare bits, and states the id's
				// shape doesn't define are refused (`state_valid` is the
				// one gate). Door requests must be the LOWER+CLOSED cell —
				// the server derives the upper half itself.
				if block == BlockType::Air as u16 || !digblock_core::shapes::state_valid(block) {
					return reject(RejectReason::BadBlock);
				}
				let b = BlockType::from_id(digblock_core::chunk::cell_id(block));
				if b.is_door() {
					let (_, open, upper) =
						digblock_core::shapes::door_parts(digblock_core::chunk::cell_state(block));
					if open || upper {
						return reject(RejectReason::BadBlock);
					}
				}
				Some(block)
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

		// Re-cast the client's ray against the server world with the
		// kind's filter: an honest client computed the identical result.
		let store = &self.store;
		let cell_at = |x: i32, y: i32, z: i32| store.cell_at(x, y, z);
		let hit = match req.kind {
			EditKind::Collect => raycast_fluid(req.origin, dir, REACH, &cell_at),
			_ => raycast(req.origin, dir, REACH, &cell_at),
		};
		let Some(hit) = hit else {
			return reject(RejectReason::OutOfReach);
		};
		if (hit.cell.0, hit.cell.1, hit.cell.2) != req.cell {
			return reject(RejectReason::Occluded);
		}
		// Collect must stop IN a fluid source (the fluid ray also stops
		// in solids — scooping stone is not a thing).
		if req.kind == EditKind::Collect && !hit.block.is_fluid_source() {
			return reject(RejectReason::Occluded);
		}

		// Timed-dig gate for dig/collect: a hold recorded by DigStart on
		// this exact cell with this exact block, with enough aim-verified
		// ticks banked (`verify_digs` counted them the whole interval).
		// The hardness-scaled grace absorbs honest shortfall: the hold's
		// first verify lands a tick after DigStart, and the server's look
		// lags the client's by the input jitter buffer.
		if place_cell.is_none() {
			let satisfied = dig_state.is_some_and(|(cell, block, progress)| {
				cell == req.cell
					&& block == hit.block
					&& hit
						.block
						.dig_hardness(req.cell.1)
						.is_some_and(|h| progress + dig_grace(h as u32) >= h as u32)
			});
			if !satisfied {
				return reject(RejectReason::DigIncomplete);
			}
		}

		// The full (cell, value) list this action writes: one for a plain
		// dig/place, two for a door (place derives the upper half; digging
		// either half clears both).
		let air = BlockType::Air as u16;
		let mut writes: Vec<((i32, i32, i32), u16)> = Vec::with_capacity(2);
		let place_b = place_cell.map(|v| BlockType::from_id(digblock_core::chunk::cell_id(v)));
		match place_cell {
			None => {
				writes.push((req.cell, air));
				if hit.block.is_door() {
					let partner = digblock_core::shapes::door_partner(
						req.cell,
						digblock_core::chunk::cell_state(hit.cell_value),
					);
					if self
						.store
						.cell_at(partner.0, partner.1, partner.2)
						.is_some_and(|c| {
							BlockType::from_id(digblock_core::chunk::cell_id(c)).is_door()
						}) {
						writes.push((partner, air));
					}
				}
			}
			Some(v) => {
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
				writes.push((target, v));
				if place_b.is_some_and(|b| b.is_door()) {
					// The derived upper half must be loaded and free.
					let upper = (target.0, target.1 + 1, target.2);
					let free = self
						.store
						.cell_at(upper.0, upper.1, upper.2)
						.is_some_and(|c| {
							!BlockType::from_id(digblock_core::chunk::cell_id(c))
								.props()
								.collides
						});
					if !free {
						return reject(RejectReason::Occluded);
					}
					let upper_cell = v | (0b1000 << digblock_core::chunk::STATE_SHIFT);
					writes.push((upper, upper_cell));
				}
				// Mirrors the local-engine rejection via the shared
				// shape-aware contract, but for every live player.
				for c in self.clients.values() {
					if !c.live {
						continue;
					}
					for &(cell, val) in &writes {
						if digblock_core::player::placement_blocked(c.player.pos, cell, val) {
							return reject(RejectReason::InsidePlayer);
						}
					}
				}
			}
		};

		// Stock gate: the LAST check before mutating the world, so a
		// refused take never needs unwinding of anything else. The
		// InsidePlayer scan's shared borrow has ended — re-fetch mutably.
		// One debit covers a door's two cells.
		let client = self
			.clients
			.get_mut(&req.conn_id)
			.expect("checked live above");
		if let Some(b) = place_b {
			if !client.inventory.try_take(b, 1) {
				let inv = client.inventory.to_wire();
				reject(RejectReason::NoStock);
				// Drift-repair backstop: the client predicted stock it
				// doesn't have — hand it the authoritative base.
				send_frame(&out, &ServerMsg::InventoryFull { counts: inv });
				return;
			}
		}

		let (first_cell, first_val) = writes[0];
		let Some(prev_cell) =
			self.store
				.set_cell(first_cell.0, first_cell.1, first_cell.2, first_val)
		else {
			// Refund the debit — the world never changed.
			if let Some(b) = place_b {
				if let Some(client) = self.clients.get_mut(&req.conn_id) {
					client.inventory.add(b, 1);
				}
			}
			return reject(RejectReason::NotLoaded);
		};
		let prev = BlockType::from_id(digblock_core::chunk::cell_id(prev_cell));

		// Dig/collect credit comes from the journaled truth (the actual
		// replaced cell), not the raycast hit. One credit per door pair.
		let client = self
			.clients
			.get_mut(&req.conn_id)
			.expect("checked live above");
		if place_cell.is_none() {
			if let Some(drop) = prev.dig_drop() {
				client.inventory.add(drop, 1);
			}
			client.digging = None;
		}
		if let Some(hash) = client.identity {
			self.inv_store.put(hash, &client.inventory);
		}

		// Journal + broadcast the first write under the client's seq, the
		// companion (a door's other half) under seq 0 — the in-order
		// stream confirms the pair before the companion applies
		// idempotently over the client's identical prediction.
		self.apply_write(first_cell, first_val, prev_cell, id, req.seq, false);
		for &(cell, val) in &writes[1..] {
			if let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) {
				self.apply_write(cell, val, prev, id, 0, false);
			}
		}
	}

	/// Journal, fluid-schedule, and broadcast one landed world write (the
	/// store mutation already happened). `already_set` is a lie-guard for
	/// the doc only — callers pass the actual prev they observed.
	fn apply_write(
		&mut self,
		cell: (i32, i32, i32),
		val: u16,
		prev_cell: u16,
		by: PlayerId,
		client_seq: u32,
		_already_set: bool,
	) {
		let edit = BlockEdit {
			x: cell.0,
			y: cell.1,
			z: cell.2,
			block: val,
		};
		let (seq, new_column) = self.journal.append(edit, by);
		if new_column {
			self.enroll_new_column(edit);
		}
		self.fluid.schedule_around(cell.0, cell.1, cell.2);
		self.broadcast_edit(seq, by, client_seq, edit, prev_cell);
	}

	/// A use/interact click: re-raycast with the interact-aware ray and
	/// toggle the hit door pair. Same preamble gates as edits; `normal`
	/// is not required to match (a swung open leaf has no meaningful
	/// entry face).
	fn process_interact(&mut self, req: PendingEditReq) {
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
		if !self.journal.healthy() {
			return reject(RejectReason::ServerError);
		}
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
		let store = &self.store;
		let hit = digblock_core::raycast::raycast_interact(req.origin, dir, REACH, &|x, y, z| {
			store.cell_at(x, y, z)
		});
		let Some(hit) = hit else {
			return reject(RejectReason::OutOfReach);
		};
		if (hit.cell.0, hit.cell.1, hit.cell.2) != req.cell {
			return reject(RejectReason::Occluded);
		}
		if !hit.block.is_door() {
			return reject(RejectReason::Occluded);
		}
		use digblock_core::chunk::{cell_id, cell_state};
		use digblock_core::shapes::{door_partner, door_toggled};
		let cur = hit.cell_value;
		let partner = door_partner(hit.cell, cell_state(cur));
		let mut writes = vec![(hit.cell, cur, door_toggled(cur))];
		if let Some(pc) = self.store.cell_at(partner.0, partner.1, partner.2) {
			if BlockType::from_id(cell_id(pc)).is_door() {
				writes.push((partner, pc, door_toggled(pc)));
			}
		}
		// Closing must not trap any live player in the leaf.
		let closing = cell_state(cur) & 0b0100 != 0;
		if closing {
			for c in self.clients.values() {
				if !c.live {
					continue;
				}
				for &(cell, _, val) in &writes {
					if digblock_core::player::placement_blocked(c.player.pos, cell, val) {
						return reject(RejectReason::InsidePlayer);
					}
				}
			}
		}
		let mut first = true;
		for &(cell, _, val) in &writes {
			if let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) {
				let seq = if first { req.seq } else { 0 };
				self.apply_write(cell, val, prev, id, seq, false);
			}
			first = false;
		}
	}

	/// Alchemy convert/craft: validate against the core tables, apply the
	/// shared `apply_alchemy` rule, persist, and answer — success or
	/// failure — with the authoritative `InventoryFull` rebase.
	fn process_alchemy(&mut self, conn_id: u64, op: digblock_net::messages::AlchemyOp) {
		use digblock_core::inventory::{apply_alchemy, AlchemyOp as CoreOp};
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		// Shares the edit token bucket; over-budget ops drop silently
		// (the client locally gates affordability, so honest clients
		// never burst here).
		if client.edit_tokens < 1.0 {
			return;
		}
		client.edit_tokens -= 1.0;
		let core_op = match op {
			digblock_net::messages::AlchemyOp::Convert { block, count } => {
				BlockType::try_from_id(block).map(|b| CoreOp::Convert { block: b, count })
			}
			digblock_net::messages::AlchemyOp::Craft { item, count } => {
				BlockType::try_from_id(item).map(|b| CoreOp::Craft { item: b, count })
			}
		};
		if let Some(core_op) = core_op {
			let _ = apply_alchemy(&mut client.inventory, core_op);
		}
		if let Some(hash) = client.identity {
			self.inv_store.put(hash, &client.inventory);
		}
		let counts = client.inventory.to_wire();
		send_frame(&client.out, &ServerMsg::InventoryFull { counts });
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
		prev: u16,
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
	use super::super::{Sim, SimEvent};
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
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
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
		// 3. Honest dig with the full timed hold (stone: 90 ticks) → applied.
		let mut t = 0;
		let dug = dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
		assert_eq!(dug, hit.cell);
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
		assert_eq!(edit.block, BlockType::Air as u16);
		assert_eq!(
			sim.test_block_at(edit.x, edit.y, edit.z),
			Some(BlockType::Air)
		);
		assert!(sim
			.journal
			.index
			.column(edit.x.div_euclid(16), edit.z.div_euclid(16))
			.is_some_and(
				|col| col.get(&(edit.x, edit.y, edit.z)) == Some(&(BlockType::Air as u16))
			));
	}

	/// The world floor (y = BEDROCK_Y) is indestructible: DigStart
	/// records no hold there, so even a full-length honest hold ends in
	/// DigIncomplete and the block survives. World: a single stone layer
	/// at y = 0 with the player standing on it, so the floor is the ray
	/// target — no digging down through 126 blocks first.
	#[test]
	fn bedrock_floor_refuses_digs() {
		use digblock_core::block::BEDROCK_Y;
		use digblock_core::chunk::{ChunkData, AIR_CHUNK, WORLD_VCHUNK};
		use std::sync::Arc;

		let source: crate::store::ColumnSource = Arc::new(|_cx, _cz| {
			let mut floor = ChunkData::default();
			for z in 0..16 {
				for x in 0..16 {
					floor.set(x, 0, z, BlockType::Stone);
				}
			}
			let floor = Arc::new(floor);
			(0..WORLD_VCHUNK as usize)
				.map(|cy| {
					if cy == 0 {
						floor.clone()
					} else {
						AIR_CHUNK.clone()
					}
				})
				.collect()
		});
		let spawn = Vec3::new(8.5, BEDROCK_Y as f32 + 2.5, 8.5);
		let dir = temp_dir("bedrock");
		let mut sim = super::super::Sim::new_with_source(SEED, &dir, 1, 2, source, spawn).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let mut t = 0;
		let cell = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		assert_eq!(cell.1, BEDROCK_Y, "the ray targeted the floor layer");
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete,
				}
			)),
			"{:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Stone),
			"floor block survived"
		);
	}

	/// The reserved state nibble travels the wire but nothing consumes
	/// it yet: placements carrying state bits are BadBlock, not silently
	/// stripped — the rejection is the contract until states ship.
	#[test]
	fn place_with_state_bits_is_bad_block() {
		let mut sim = flat_sim("statebits");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place {
					block: (BlockType::Dirt as u16) | (1 << digblock_core::chunk::STATE_SHIFT),
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
					reason: RejectReason::BadBlock,
					..
				}
			)),
			"{:?}",
			c.msgs
		);
	}

	#[test]
	fn unhealthy_journal_rejects_edits_with_server_error() {
		let mut sim = flat_sim("jhealth");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();

		sim.journal.force_write_failure = true;
		// Each accepted timed dig hits one failing append; after three the
		// journal flips unhealthy and the next request is refused.
		let mut t = 0;
		for seq in 1..=3u32 {
			dig_hold(&mut sim, 1, down, 90, seq, EditKind::Dig, &mut t);
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

	/// An instant dig (no DigStart) and an under-held dig both fail the
	/// timing gate; the honest hold at the grace boundary is accepted.
	#[test]
	fn dig_timing_gate() {
		let mut sim = flat_sim("digtime");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		// No DigStart at all.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
			},
		}]);
		// Held for only 10 ticks (stone needs 90 − 16 grace).
		let mut t = 0;
		dig_hold(&mut sim, 1, down, 10, 2, EditKind::Dig, &mut t);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigCancel,
		}]);
		// Honest hold: accepted (dig_hold holds the full hardness).
		let dug = dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
		c.drain();

		for early in [1u32, 2] {
			assert!(
				c.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::EditRejected {
						client_seq,
						reason: RejectReason::DigIncomplete
					} if *client_seq == early
				)),
				"seq {early} should be DigIncomplete: {:?}",
				c.msgs
			);
		}
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq: 3, edit, .. }
					if (edit.x, edit.y, edit.z) == dug
			)),
			"{:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Stone),
			1,
			"dig credited the drop"
		);
	}

	/// Looking away for a SUSTAINED stretch (past DIG_OFF_TARGET_RESET)
	/// abandons the server's record entirely.
	#[test]
	fn look_away_mid_hold_resets() {
		let mut sim = flat_sim("lookaway");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		// 30 on target, 45 at the horizon (> the 30-tick abandon), then
		// back on target well past the would-be completion time: the
		// hold is gone, the banked 30 ticks with it.
		for _ in 0..30 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		for _ in 0..45 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, Vec3::Z)], 1)]);
		}
		assert!(
			sim.clients.get(&1).unwrap().digging.is_none(),
			"sustained miss abandoned the hold"
		);
		for _ in 0..90 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete
				}
			)),
			"look-away reset the hold: {:?}",
			c.msgs
		);
	}

	/// A brief aim miss (over the old 8-tick reset, under the 30-tick
	/// abandon) PAUSES the hold: the missed ticks don't count toward
	/// completion, but the banked progress survives and the dig finishes
	/// once enough verified ticks accumulate.
	#[test]
	fn aim_miss_pauses_hold_instead_of_resetting() {
		let mut sim = flat_sim("digpause");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		// 40 verified ticks, then 15 off target: the wall clock says 55
		// but only ~40 count — completing now is premature (this also
		// pins that misses are a pause, not elapsed time).
		for _ in 0..40 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		for _ in 0..15 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, Vec3::Z)], 1)]);
		}
		let dig = |seq: u32| SimEvent::Msg {
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
		};
		sim.tick_once(vec![dig(1)]);
		// Back on target: the banked 40 survive the miss stretch (which
		// would have reset the hold under consecutive-miss-8 semantics),
		// so ~50 more verified ticks complete the stone (needs ≥ 74).
		for _ in 0..50 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.tick_once(vec![dig(2)]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete
				}
			)),
			"paused ticks don't count toward completion: {:?}",
			c.msgs
		);
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq: 2, edit, .. }
					if (edit.x, edit.y, edit.z) == hit.cell
			)),
			"hold survived the brief miss and completed: {:?}",
			c.msgs
		);
	}

	/// Regression: back-to-back digs put "complete block A" and "start
	/// block B" in the SAME tick drain. Wire order must hold — the new
	/// DigStart must not clobber the hold A's completion validates
	/// against (it did, and honest rapid digging bounced blocks back).
	#[test]
	fn same_tick_complete_and_restart_keeps_wire_order() {
		let mut sim = flat_sim("digrace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		for _ in 0..95 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		// One tick carries both the completing EditRequest for A and the
		// DigStart for the block beneath it (what a client does 16 ms
		// after finishing A).
		let below = (hit.cell.0, hit.cell.1 - 1, hit.cell.2);
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq: 1,
					tick: 0,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Dig,
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [0, 0, 0],
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [below.0, below.1, below.2],
				},
			},
		]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq: 1, edit, .. }
					if (edit.x, edit.y, edit.z) == hit.cell
			)),
			"completion processed before the same-tick restart: {:?}",
			c.msgs
		);
		let digging = sim
			.clients
			.get(&1)
			.unwrap()
			.digging
			.as_ref()
			.map(|d| (d.cell, d.block));
		assert_eq!(
			digging,
			Some((below, BlockType::Stone)),
			"the new hold took over after the completion"
		);
	}

	/// The cell's content changing mid-hold resets the record — a
	/// collaborator can't swap a cheap block in under a running timer.
	#[test]
	fn block_swap_mid_hold_resets() {
		let mut sim = flat_sim("swap");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		for _ in 0..30 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		// The target vanishes out from under the hold (authoritative
		// change by "someone else"), then reappears.
		sim.store
			.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Air);
		for _ in 0..12 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.store
			.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Stone);
		for _ in 0..60 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete
				}
			)),
			"content swap reset the hold: {:?}",
			c.msgs
		);
	}

	/// Dig → place round trip: the credit funds exactly one placement;
	/// the second placement is NoStock and carries the repair snapshot.
	#[test]
	fn place_consumes_stock_then_no_stock() {
		let mut sim = flat_sim("stock");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let mut t = 0;
		dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		assert_eq!(sim.test_inventory_count(1, BlockType::Stone), 1);

		// Place onto ground a few blocks out — outside our own capsule.
		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let place = |sim: &mut super::super::Sim, seq: u32| {
			let hit =
				raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground out");
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 0,
					origin: pos.to_array(),
					dir: out.to_array(),
					kind: EditKind::Place {
						block: BlockType::Stone as u16,
					},
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				},
			}]);
		};
		place(&mut sim, 2);
		assert_eq!(sim.test_inventory_count(1, BlockType::Stone), 0);
		place(&mut sim, 3);
		c.drain();
		assert!(
			c.msgs.iter().any(
				|m| matches!(m, ServerMsg::EditApplied { client_seq: 2, edit, .. } if edit.block == BlockType::Stone as u16)
			),
			"funded placement applied: {:?}",
			c.msgs
		);
		let no_stock_at = c
			.msgs
			.iter()
			.position(|m| {
				matches!(
					m,
					ServerMsg::EditRejected {
						client_seq: 3,
						reason: RejectReason::NoStock
					}
				)
			})
			.expect("unfunded placement refused");
		assert!(
			matches!(
				c.msgs.get(no_stock_at + 1),
				Some(ServerMsg::InventoryFull { .. })
			),
			"NoStock is followed by the repair snapshot: {:?}",
			c.msgs
		);
	}

	/// Water tool round trip: place a source (funded), collect it back
	/// with the fluid ray + hold, and a Collect aimed at a flow cell
	/// can't name the flow (the ray passes it) — Occluded.
	#[test]
	fn collect_water_end_to_end() {
		let mut sim = flat_sim("collect");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.clients
			.get_mut(&1)
			.unwrap()
			.inventory
			.add(BlockType::Water, 1);

		// Place the source on the ground a few blocks out.
		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let ground = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place {
					block: BlockType::Water as u16,
				},
				cell: [ground.cell.0, ground.cell.1, ground.cell.2],
				normal: [
					ground.normal.0 as i8,
					ground.normal.1 as i8,
					ground.normal.2 as i8,
				],
			},
		}]);
		assert_eq!(sim.test_inventory_count(1, BlockType::Water), 0);
		let source = (
			ground.cell.0 + ground.normal.0,
			ground.cell.1 + ground.normal.1,
			ground.cell.2 + ground.normal.2,
		);
		assert_eq!(
			sim.test_block_at(source.0, source.1, source.2),
			Some(BlockType::Water)
		);
		// Let the fluid spread so flow cells exist around the source.
		for _ in 0..40 {
			sim.tick_once(vec![]);
		}

		// Collect naming a flow cell: the fluid ray never stops there.
		let flow_cell = (source.0 + 1, source.1, source.2);
		assert!(sim
			.test_block_at(flow_cell.0, flow_cell.1, flow_cell.2)
			.is_some_and(|b| b.is_water() && !b.is_fluid_source()));
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 2,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Collect,
				cell: [flow_cell.0, flow_cell.1, flow_cell.2],
				normal: [0, 0, 0],
			},
		}]);

		// Collect the source with the real hold (water: 20 ticks).
		let toward = (Vec3::new(
			source.0 as f32 + 0.5,
			source.1 as f32 + 0.5,
			source.2 as f32 + 0.5,
		) - pos)
			.normalize();
		let mut t = 100;
		let collected = dig_hold(&mut sim, 1, toward, 20, 3, EditKind::Collect, &mut t);
		assert_eq!(collected, source);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 2,
					reason: RejectReason::Occluded
				}
			)),
			"flow cell is not collectible: {:?}",
			c.msgs
		);
		assert!(
			c.msgs.iter().any(
				|m| matches!(m, ServerMsg::EditApplied { client_seq: 3, edit, .. } if edit.block == BlockType::Air as u16)
			),
			"source collected: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Water),
			1,
			"water credited back"
		);
		// The scooped pool drains (lone source's spread, no regen pair).
		for _ in 0..600 {
			sim.tick_once(vec![]);
		}
		assert!(sim
			.test_block_at(flow_cell.0, flow_cell.1, flow_cell.2)
			.is_some_and(|b| !b.is_water()));
	}

	/// Two placed sources one apart: the middle regenerates into a source
	/// server-side and broadcasts as an ordinary SERVER_ID edit.
	#[test]
	fn regen_broadcasts_as_server_edit() {
		let mut sim = flat_sim("regen");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let y = 127; // first air layer over the flat stone ground
		assert!(sim.store.set_block(6, y, 8, BlockType::Water).is_some());
		assert!(sim.store.set_block(8, y, 8, BlockType::Water).is_some());
		sim.fluid.schedule_around(6, y, 8);
		sim.fluid.schedule_around(8, y, 8);
		let mut regenerated = false;
		for _ in 0..300 {
			sim.tick_once(vec![]);
			if sim.test_block_at(7, y, 8) == Some(BlockType::Water) {
				regenerated = true;
				break;
			}
		}
		assert!(regenerated, "middle cell became a source");
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { by, edit, .. }
					if *by == digblock_net::protocol::SERVER_ID
						&& (edit.x, edit.y, edit.z) == (7, y, 8)
						&& edit.block == BlockType::Water as u16
			)),
			"regen broadcast as a SERVER_ID edit: {:?}",
			c.msgs
		);
	}

	/// Same-tick DigStarts resolve in wire order — the LAST start wins,
	/// exactly like the client's retarget stream. The old first-wins
	/// per-tick cap silently dropped the restart half of a cancel +
	/// restart pair bunched into one tick drain, leaving an honest hold
	/// with no server record (guaranteed DigIncomplete on completion).
	#[test]
	fn same_tick_dig_starts_resolve_in_wire_order() {
		let mut sim = flat_sim("digflood");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		let other = (hit.cell.0, hit.cell.1 - 1, hit.cell.2);
		let start = |cell: (i32, i32, i32)| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [cell.0, cell.1, cell.2],
			},
		};
		let cancel = || SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigCancel,
		};
		// A retarget flick bunched into one server tick: start A, then
		// cancel + restart on B — B's hold must stand.
		sim.tick_once(vec![start(hit.cell), cancel(), start(other)]);
		let digging = |sim: &super::super::Sim| {
			sim.clients
				.get(&1)
				.unwrap()
				.digging
				.as_ref()
				.map(|d| d.cell)
		};
		assert_eq!(digging(&sim), Some(other), "last start wins the tick");
		// And the next tick's start replaces it as usual.
		sim.tick_once(vec![start(hit.cell)]);
		assert_eq!(digging(&sim), Some(hit.cell));
	}

	#[test]
	fn place_into_player_and_rate_limit() {
		let mut sim = flat_sim("place");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		// Placing on the ground's top face targets our own body cell.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 100,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place {
					block: BlockType::Dirt as u16,
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

	// ---- sub-block shapes (v8) ------------------------------------------

	use digblock_core::chunk::{cell_state, make_cell, STATE_SHIFT};
	use digblock_core::shapes::{door_state, stair_state, Facing};

	/// Aim at the ground and place `cell_value`; returns (support cell,
	/// target cell). The caller grants stock first.
	fn place_cell(sim: &mut Sim, c: &mut TestClient, seq: u32, cell_value: u16) -> (i32, i32, i32) {
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place { block: cell_value },
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			},
		}]);
		c.drain();
		(
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		)
	}

	#[test]
	fn place_stair_with_facing_applies_full_cell() {
		let mut sim = flat_sim("stairplace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::StoneStairs, 1);
		let want = make_cell(BlockType::StoneStairs, stair_state(Facing::NegZ));
		let target = place_cell(&mut sim, &mut c, 1, want);
		assert_eq!(
			sim.test_cell_at(target.0, target.1, target.2),
			Some(want),
			"state nibble survives to the store"
		);
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { edit, .. } if edit.block == want
			)),
			"broadcast carries the full cell: {:?}",
			c.msgs
		);
	}

	#[test]
	fn stateful_place_gates() {
		let mut sim = flat_sim("stategates");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodStairs, 5);
		sim.test_grant(1, BlockType::WoodDoor, 5);
		let bad = [
			// Stateless block with state bits: still BadBlock.
			(BlockType::Dirt as u16) | (1 << STATE_SHIFT),
			// Stair with the non-facing state bits set.
			make_cell(BlockType::WoodStairs, 0b0100),
			// Door requests must be lower+closed — server derives upper.
			make_cell(BlockType::WoodDoor, door_state(Facing::PosX, true, false)),
			make_cell(BlockType::WoodDoor, door_state(Facing::PosX, false, true)),
			// Spare bits 10-11.
			(BlockType::WoodStairs as u16) | (1 << 10),
		];
		for (i, cell) in bad.into_iter().enumerate() {
			place_cell(&mut sim, &mut c, 10 + i as u32, cell);
			assert!(
				c.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::EditRejected {
						client_seq,
						reason: RejectReason::BadBlock,
					} if *client_seq == 10 + i as u32
				)),
				"cell {cell:#06x} refused: {:?}",
				c.msgs
			);
		}
	}

	#[test]
	fn door_place_writes_both_halves_one_debit() {
		let mut sim = flat_sim("doorplace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodDoor, 1);
		let lower_cell = make_cell(BlockType::WoodDoor, door_state(Facing::PosZ, false, false));
		let target = place_cell(&mut sim, &mut c, 1, lower_cell);
		let upper = (target.0, target.1 + 1, target.2);
		assert_eq!(
			sim.test_cell_at(target.0, target.1, target.2),
			Some(lower_cell)
		);
		assert_eq!(
			sim.test_cell_at(upper.0, upper.1, upper.2),
			Some(make_cell(
				BlockType::WoodDoor,
				door_state(Facing::PosZ, false, true)
			)),
			"server derived the upper half"
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::WoodDoor),
			0,
			"one debit"
		);
		// Two broadcasts: the lower carries our seq, the upper seq 0.
		let applied: Vec<_> = c
			.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::EditApplied {
					client_seq, edit, ..
				} => Some((*client_seq, (edit.x, edit.y, edit.z))),
				_ => None,
			})
			.collect();
		assert!(applied.contains(&(1, target)), "{applied:?}");
		assert!(applied.contains(&(0, upper)), "{applied:?}");
	}

	#[test]
	fn door_toggle_flips_both_halves_and_dig_clears_both() {
		let mut sim = flat_sim("doortoggle");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodDoor, 1);
		let lower_cell = make_cell(BlockType::WoodDoor, door_state(Facing::PosZ, false, false));
		let target = place_cell(&mut sim, &mut c, 1, lower_cell);
		let upper = (target.0, target.1 + 1, target.2);

		// Toggle open: aim at the lower half.
		let pos = sim.test_player(1).unwrap().pos;
		let dir = (Vec3::new(
			target.0 as f32 + 0.5,
			target.1 as f32 + 0.5,
			target.2 as f32 + 0.05,
		) - pos)
			.normalize();
		let hit = digblock_core::raycast::raycast_interact(pos, dir, REACH, &|x, y, z| {
			sim.test_cell_at(x, y, z)
		})
		.expect("door under the interact ray");
		assert_eq!(hit.cell, target);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 2,
				tick: 0,
				origin: pos.to_array(),
				dir: dir.to_array(),
				kind: EditKind::Interact,
				cell: [target.0, target.1, target.2],
				normal: [0, 0, 0],
			},
		}]);
		c.drain();
		let lower_now = sim.test_cell_at(target.0, target.1, target.2).unwrap();
		let upper_now = sim.test_cell_at(upper.0, upper.1, upper.2).unwrap();
		assert!(cell_state(lower_now) & 0b0100 != 0, "lower open");
		assert!(cell_state(upper_now) & 0b0100 != 0, "upper open");

		// An OPEN door is transparent to the dig ray, so close it first —
		// clicking the SWUNG leaf, which now hugs a different cell face —
		// then hold-dig the lower half: both cells clear, one credit.
		let leaf_box = digblock_core::shapes::interaction_boxes(lower_now)[0];
		let (leaf_lo, leaf_hi) = leaf_box.aabb(target);
		let dir2 = ((leaf_lo + leaf_hi) * 0.5 - pos).normalize();
		let hit2 = digblock_core::raycast::raycast_interact(pos, dir2, REACH, &|x, y, z| {
			sim.test_cell_at(x, y, z)
		})
		.expect("open leaf under the interact ray");
		assert_eq!(hit2.cell, target, "swung leaf still clickable");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 3,
				tick: 0,
				origin: pos.to_array(),
				dir: dir2.to_array(),
				kind: EditKind::Interact,
				cell: [target.0, target.1, target.2],
				normal: [0, 0, 0],
			},
		}]);
		c.drain();
		assert!(
			cell_state(sim.test_cell_at(target.0, target.1, target.2).unwrap()) & 0b0100 == 0,
			"closed again"
		);
		let mut t = 100;
		let hold = BlockType::WoodDoor.hardness_ticks().unwrap() as u32 + 6;
		let dug = dig_hold(&mut sim, 1, dir, hold, 4, EditKind::Dig, &mut t);
		assert_eq!(dug, target);
		c.drain();
		assert_eq!(sim.test_cell_at(target.0, target.1, target.2), Some(0));
		assert_eq!(
			sim.test_cell_at(upper.0, upper.1, upper.2),
			Some(0),
			"partner cleared too"
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::WoodDoor),
			1,
			"one credit for the pair"
		);
	}

	#[test]
	fn alchemy_convert_craft_and_same_tick_place() {
		use digblock_net::messages::AlchemyOp;
		let mut sim = flat_sim("alchemy");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::Stone, 1);

		// Convert + craft + place, all queued in ONE tick: wire order
		// must debit before the place's stock gate runs.
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		let stair = make_cell(BlockType::StoneStairs, stair_state(Facing::PosX));
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Alchemy {
					op: AlchemyOp::Convert {
						block: BlockType::Stone as u16,
						count: 1,
					},
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Alchemy {
					op: AlchemyOp::Craft {
						item: BlockType::StoneStairs as u16,
						count: 1,
					},
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq: 1,
					tick: 0,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Place { block: stair },
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				},
			},
		]);
		c.drain();
		let target = (
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		);
		assert_eq!(
			sim.test_cell_at(target.0, target.1, target.2),
			Some(stair),
			"craft-then-place landed in one tick: {:?}",
			c.msgs
		);
		// 27 sub-stone minted, 18 spent.
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts }
					if counts.contains(&(0x8001, 9))
			)),
			"sub-stone remainder in the rebase: {:?}",
			c.msgs
		);

		// Invalid ops still answer with the authoritative rebase.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Alchemy {
				op: AlchemyOp::Craft {
					item: BlockType::StoneStairs as u16,
					count: 999,
				},
			},
		}]);
		let before = sim.test_inventory_count(1, BlockType::StoneStairs);
		c.drain();
		assert_eq!(sim.test_inventory_count(1, BlockType::StoneStairs), before);
		assert!(
			c.msgs
				.iter()
				.rev()
				.any(|m| matches!(m, ServerMsg::InventoryFull { .. })),
			"failed op still rebases"
		);
	}
}
