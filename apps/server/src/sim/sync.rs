//! Residency, late journal sync, and snapshot broadcast.

use super::{column_of, in_radius, send_frame, OutMsg, OutTx, Sim, DGRAM_STALL_SECS};
use crate::journal::Journal;
use digblock_net::codec;
use digblock_net::messages::{BlockEdit, Datagram, NetPlayerState, ServerMsg, Snapshot};
use digblock_net::protocol::{MAX_SNAPSHOT_PLAYERS, TICK_RATE};
use tokio::sync::mpsc::error::TrySendError;

/// Columns beyond `draw_dist + slack` fall out of a client's synced set,
/// so re-entry re-syncs (clients evict remote chunks too).
const SYNC_SLACK: i32 = 2;

impl Sim {
	pub(super) fn update_residency(&mut self) {
		for client in self.clients.values_mut() {
			let center = column_of(client.player.pos);
			if center != client.center {
				client.center = center;
				client.needs_sync_scan = true;
				self.residency_dirty = true;
			}
		}
		if self.residency_dirty {
			let centers: Vec<(i32, i32)> = self.clients.values().map(|c| c.center).collect();
			self.store.set_residency(&centers, self.sim_radius);
			self.residency_dirty = false;
		}
		self.store.drain(&self.journal.index);

		// Late journal sync + liveness flips. The full edited-column scan
		// is EVENT-DRIVEN: only when this client's center moved or it just
		// went live (invariant: `synced_columns` ⊇ its in-radius edited
		// columns — first edits to fresh columns enroll nearby clients at
		// broadcast time, so a stationary client never needs a rescan).
		let journal = &self.journal;
		let store = &self.store;
		for client in self.clients.values_mut() {
			if !client.live && store.column_ready(client.center.0, client.center.1) {
				client.live = true;
				client.needs_sync_scan = true;
				log::debug!("player {} live", client.id);
			}
			if !client.needs_sync_scan {
				continue;
			}
			client.needs_sync_scan = false;
			// Columns only leave the slack radius when the center moves,
			// so the prune rides the same trigger.
			client
				.synced_columns
				.retain(|&col| in_radius(col, client.center, client.draw_dist + SYNC_SLACK));
			for (cx, cz) in journal.index.edited_columns() {
				if client.dead.get() {
					break;
				}
				if in_radius((cx, cz), client.center, client.draw_dist)
					&& !client.synced_columns.contains(&(cx, cz))
				{
					if !send_journal_column_to(journal, &client.out, cx, cz) {
						client.dead.set(true);
						break;
					}
					client.synced_columns.insert((cx, cz));
				}
			}
		}
	}

	pub(super) fn send_journal_column(&self, out: &OutTx, cx: i32, cz: i32) -> bool {
		send_journal_column_to(&self.journal, out, cx, cz)
	}

	pub(super) fn send_snapshots(&mut self) {
		// Shared roster built once; per client only nearby players are
		// selected, so snapshots can never outgrow one datagram (the cliff
		// that used to silently kill all snapshots at ~30 players).
		let roster: Vec<(NetPlayerState, (i32, i32))> = self
			.clients
			.values()
			.filter(|c| c.live)
			.map(|c| {
				let s = c.player.state();
				(
					NetPlayerState {
						id: c.id,
						pos: s.pos.to_array(),
						vel: s.vel.to_array(),
						look: c.look.to_array(),
					},
					c.center,
				)
			})
			.collect();
		let mut nearby: Vec<(i32, usize)> = Vec::with_capacity(roster.len());
		for client in self.clients.values() {
			if !client.live {
				continue;
			}
			// Self first (the reconciliation anchor), then nearest remotes
			// within the client's own draw distance.
			nearby.clear();
			for (i, (p, col)) in roster.iter().enumerate() {
				if p.id != client.id && in_radius(*col, client.center, client.draw_dist) {
					let (dx, dz) = (col.0 - client.center.0, col.1 - client.center.1);
					nearby.push((dx * dx + dz * dz, i));
				}
			}
			nearby.sort_unstable();
			let me = roster.iter().find(|(p, _)| p.id == client.id);
			let players: Vec<NetPlayerState> = me
				.map(|(p, _)| *p)
				.into_iter()
				.chain(
					nearby
						.iter()
						.take(MAX_SNAPSHOT_PLAYERS - 1)
						.map(|&(_, i)| roster[i].0),
				)
				.collect();
			let selected = players.len();
			let snap = Snapshot {
				server_tick: self.tick,
				your_last_input_tick: client.last_input_tick,
				players,
			};
			match codec::encode_datagram(&Datagram::Snapshot(snap)) {
				Ok(bytes) => match client.out.try_send(OutMsg::Datagram(bytes)) {
					Ok(()) => client.dgram_full_since.set(None),
					Err(TrySendError::Full(_)) => {
						// Lossy by contract: dropping snapshots is fine for
						// a stall, but a queue full for seconds is a peer
						// that stopped reading.
						match client.dgram_full_since.get() {
							None => client.dgram_full_since.set(Some(self.tick)),
							Some(t0)
								if self.tick.wrapping_sub(t0) > DGRAM_STALL_SECS * TICK_RATE =>
							{
								log::info!(
									"player {}: datagram queue stalled {DGRAM_STALL_SECS}s — dropping",
									client.id
								);
								client.dead.set(true);
							}
							Some(_) => {}
						}
					}
					Err(TrySendError::Closed(_)) => client.dead.set(true),
				},
				Err(e) => {
					// Losing snapshots is a game-breaking condition — never
					// swallow it. Rate-limited to once a second.
					let due = self
						.last_oversize_log
						.is_none_or(|at| self.tick.wrapping_sub(at) >= TICK_RATE);
					if due {
						log::warn!(
							"snapshot encode failed for player {} ({selected} players selected): {e}",
							client.id,
						);
						self.last_oversize_log = Some(self.tick);
					}
				}
			}
		}
	}
}

fn send_journal_column_to(journal: &Journal, out: &OutTx, cx: i32, cz: i32) -> bool {
	let Some(cells) = journal.index.column(cx, cz) else {
		return true;
	};
	let cells = cells
		.iter()
		.map(|(&(x, y, z), &block)| BlockEdit { x, y, z, block })
		.collect();
	send_frame(out, &ServerMsg::JournalColumn { cx, cz, cells })
}

#[cfg(test)]
mod tests {
	use super::super::harness::*;
	use super::super::{Sim, SimEvent};
	use digblock_core::raycast::{raycast, REACH};
	use digblock_net::messages::{ClientMsg, EditKind, InputTick, ServerMsg};
	use digblock_net::protocol::MAX_SNAPSHOT_PLAYERS;
	use glam::Vec3;

	fn dig_under(sim: &mut super::super::Sim, conn_id: u64, seq: u32) -> (i32, i32, i32) {
		let pos = sim.test_player(conn_id).unwrap().pos;
		// At an angle: the ground under the player's feet stays intact so
		// it can still walk (a hole underfoot would trap it in the pit).
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_block_at(x, y, z))
			.expect("ground under player");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id,
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
		hit.cell
	}

	fn walk(tick: u32, dz: f32) -> InputTick {
		InputTick {
			tick,
			move_axis: [0.0, dz],
			jump: false,
			look: [0.0, 0.0, 1.0],
			claimed_pos: [0.0, -500.0, 0.0], // never adopted: server physics
			claimed_vel: [0.0; 3],
		}
	}

	/// Trigger B: a column's first edit enrolls nearby clients at
	/// broadcast time — the EditApplied carries the data, no
	/// JournalColumn resend, no per-tick journal scan.
	#[test]
	fn fresh_column_edit_reaches_stationary_peer_via_broadcast_only() {
		let mut sim = flat_sim("fresh");
		let mut a = connect(&mut sim, 1);
		let _b_keepalive = connect(&mut sim, 2);
		wait_live(&mut sim, 1);
		wait_live(&mut sim, 2);
		a.drain();
		a.msgs.clear();

		let cell = dig_under(&mut sim, 2, 1);
		sim.tick_once(vec![]);
		a.drain();
		assert!(
			a.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { edit, .. }
					if (edit.x, edit.y, edit.z) == cell
			)),
			"stationary peer got the broadcast: {:?}",
			a.msgs
		);
		assert!(
			!a.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::JournalColumn { .. })),
			"no redundant JournalColumn for a fresh column"
		);
	}

	/// Trigger A: leaving prunes the synced set; re-entering re-syncs the
	/// edited column exactly once via JournalColumn.
	#[test]
	fn evict_and_reenter_resyncs_column_exactly_once() {
		// Residency radius 3: columns generate well ahead of a 4-block/s
		// walk even when parallel tests starve the worker threads (radius
		// 1 can lag and drop the player through ungenerated terrain).
		let dir = temp_dir("reenter");
		let mut sim = Sim::new_with_source(SEED, &dir, 3, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut a = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let cell = dig_under(&mut sim, 1, 1);
		let col = (cell.0.div_euclid(16), cell.2.div_euclid(16));

		// Walk out (+z) until well past draw_dist 4 + SYNC_SLACK 2, then
		// back until near the edited column again — condition-driven, so
		// scheduler hiccups can't underschoot the distances. Drain as we
		// go: an undrained queue would (correctly) trip the stall-kill.
		let col_dist = |sim: &Sim| {
			let p = sim.test_player(1).expect("client alive").pos;
			((p.z.floor() as i32).div_euclid(16) - col.1).abs()
		};
		let mut t = 100;
		while col_dist(&sim) < 10 {
			sim.tick_once(vec![batch(vec![walk(t, 1.0)], 1)]);
			a.drain();
			t += 1;
			assert!(t < 20_000, "never walked out");
		}
		while col_dist(&sim) > 1 {
			sim.tick_once(vec![batch(vec![walk(t, -1.0)], 1)]);
			a.drain();
			t += 1;
			assert!(t < 40_000, "never walked back");
		}
		for _ in 0..30 {
			sim.tick_once(vec![]);
		}
		a.drain();
		let resyncs = a
			.msgs
			.iter()
			.filter(|m| matches!(m, ServerMsg::JournalColumn { cx, cz, .. } if (*cx, *cz) == col))
			.count();
		assert_eq!(resyncs, 1, "column resynced exactly once on re-entry");
	}

	/// The regression test for the old cliff: every snapshot used to carry
	/// every player and silently exceeded MAX_DATAGRAM at ~30 — now each
	/// client gets self + nearest remotes, always within one datagram.
	#[test]
	fn thirty_clients_all_receive_bounded_snapshots() {
		let mut sim = flat_sim("thirty");
		let mut clients: Vec<_> = (1..=30u64).map(|id| (id, connect(&mut sim, id))).collect();
		for &(id, _) in &clients {
			wait_live(&mut sim, id);
		}
		// A few snapshot cycles.
		for _ in 0..9 {
			sim.tick_once(vec![]);
		}
		for (id, c) in &mut clients {
			c.drain();
			let snap = c
				.snaps
				.last()
				.unwrap_or_else(|| panic!("client {id} got no snapshots"));
			assert!(
				snap.players.len() <= MAX_SNAPSHOT_PLAYERS,
				"client {id}: {} players in snapshot",
				snap.players.len()
			);
			// Self is always present, first (the reconciliation anchor).
			let my_id = (*id - 1) as u8;
			assert_eq!(snap.players.first().map(|p| p.id), Some(my_id));
		}
	}

	#[test]
	fn far_player_excluded_from_snapshots_until_near_again() {
		let mut sim = flat_sim("interest");
		let mut a = connect(&mut sim, 1);
		let mut b = connect(&mut sim, 2);
		wait_live(&mut sim, 1);
		wait_live(&mut sim, 2);

		// Both at spawn: A sees B.
		for _ in 0..6 {
			sim.tick_once(vec![]);
		}
		a.drain();
		assert!(
			a.snaps.last().unwrap().players.iter().any(|p| p.id == 1),
			"near remote present"
		);

		// B walks away far beyond A's draw distance (4 columns + slack).
		// Server-authoritative movement: forward inputs, never-adopted
		// claims, so the sim's own physics carries B.
		let mut t = 100;
		for _ in 0..1500 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 2)]);
			t += 1;
		}
		a.drain();
		a.snaps.clear();
		b.drain();
		b.snaps.clear();
		for _ in 0..6 {
			sim.tick_once(vec![]);
		}
		a.drain();
		b.drain();
		let a_last = a.snaps.last().expect("A still snapshotted");
		assert!(
			!a_last.players.iter().any(|p| p.id == 1),
			"far remote filtered: {:?}",
			a_last.players
		);
		let b_last = b.snaps.last().expect("B still snapshotted");
		assert_eq!(
			b_last.players.first().map(|p| p.id),
			Some(1),
			"B keeps its own state"
		);
	}
}
