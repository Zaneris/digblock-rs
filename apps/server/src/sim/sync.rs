//! Residency, late journal sync, and snapshot broadcast.

use super::{column_of, in_radius, send_frame, OutMsg, OutTx, Sim, DGRAM_STALL_SECS};
use crate::journal::Journal;
use digblock_net::codec;
use digblock_net::messages::{
	vel_to_wire, yaw_to_wire, BlockEdit, Datagram, RemoteState, SelfState, ServerMsg, Snapshot,
};
use digblock_net::protocol::{MAX_SNAPSHOT_PLAYERS, TICK_RATE};
use tokio::sync::mpsc::error::TrySendError;

/// Columns beyond `draw_dist + slack` fall out of a client's synced set,
/// so re-entry re-syncs (clients evict remote chunks too).
const SYNC_SLACK: i32 = 2;

/// Neighbor journal-water rescans processed per tick (each is O(that
/// column's journaled cells)) — an initial radius-6 load can't hitch the
/// sim thread.
const FACE_SCANS_PER_TICK: usize = 4;
/// Backstop on the rescan queue under eviction churn.
const FACE_SCAN_QUEUE_CAP: usize = 4096;

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
		let ready = self.store.drain(&self.journal.index);
		for rc in &ready {
			// A region evicted mid-flow comes back with its journaled
			// water re-overlaid but its work queue long gone: reschedule
			// every restored water cell so the relaxation resumes (frozen
			// mid-air falls drain, half-spread pools finish).
			for &(x, y, z) in &rc.overlaid_water {
				self.fluid.schedule_around(x, y, z);
			}
			// And queue budgeted rescans of resident neighbors' journaled
			// water: a front that stalled at this column's boundary while
			// it was unloaded lives on the OTHER side of the face.
			for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
				if self.face_scans.len() < FACE_SCAN_QUEUE_CAP {
					self.face_scans
						.push_back(((rc.cx, rc.cz), (rc.cx + dx, rc.cz + dz)));
				}
			}
		}
		for _ in 0..FACE_SCANS_PER_TICK {
			let Some((col, neighbor)) = self.face_scans.pop_front() else {
				break;
			};
			// Either side may have been evicted since this was queued.
			if !self.store.column_ready(col.0, col.1)
				|| !self.store.column_ready(neighbor.0, neighbor.1)
			{
				continue;
			}
			if let Some(cells) = self.journal.index.column(neighbor.0, neighbor.1) {
				for (&(x, y, z), &block) in cells {
					if digblock_core::block::BlockType::from_id(block).is_water() {
						self.fluid.schedule_around(x, y, z);
					}
				}
			}
		}

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
		//
		// Every buffer here is reused across ticks. This runs 20×/s and
		// used to cost 9 allocations per client per snapshot — 46 k
		// allocations and 15 MB/s of churn at 255 players, on the sim
		// thread. The selection is also a `select_nth_unstable` rather than
		// a full sort: only the nearest `MAX_SNAPSHOT_PLAYERS - 1` matter,
		// and sorting the whole candidate list per client made the pass
		// O(n² log n) — ~10 M comparisons/s at 255 players to choose 23.
		// The roster carries each player BOTH ways: as the remote everyone
		// else sees (quantized once here, not per recipient) and as the
		// full-precision self state its owner needs back.
		let mut roster = std::mem::take(&mut self.snap_roster);
		roster.clear();
		roster.extend(self.clients.values().filter(|c| c.live).map(|c| {
			let s = c.player.state();
			(
				RemoteState {
					id: c.id,
					pos: s.pos.to_array(),
					vel: [
						vel_to_wire(s.vel.x),
						vel_to_wire(s.vel.y),
						vel_to_wire(s.vel.z),
					],
					yaw: yaw_to_wire(c.look.to_array()),
					held: c.held_wire(),
				},
				SelfState {
					pos: s.pos.to_array(),
					vel: s.vel.to_array(),
				},
				c.center,
			)
		}));
		let mut nearby = std::mem::take(&mut self.snap_nearby);
		let mut players = std::mem::take(&mut self.snap_players);
		let mut dgram = std::mem::take(&mut self.snap_buf);
		for client in self.clients.values() {
			if !client.live {
				continue;
			}
			// Self is its own field now (the reconciliation anchor, at full
			// precision); the rest are nearest-first within draw distance.
			nearby.clear();
			let mut me = None;
			for (i, (p, own, col)) in roster.iter().enumerate() {
				if p.id == client.id {
					me = Some(*own);
				} else if in_radius(*col, client.center, client.draw_dist) {
					let (dx, dz) = (
						col.0 as i64 - client.center.0 as i64,
						col.1 as i64 - client.center.1 as i64,
					);
					nearby.push((dx * dx + dz * dz, i));
				}
			}
			let take = MAX_SNAPSHOT_PLAYERS - 1;
			if nearby.len() > take {
				// Partition only — the client does not care what order the
				// remotes arrive in, just that they are the nearest ones.
				nearby.select_nth_unstable(take);
				nearby.truncate(take);
			}
			players.clear();
			players.extend(nearby.iter().map(|&(_, i)| roster[i].0));
			let selected = players.len() + usize::from(me.is_some());
			// Serializing needs the owned `Datagram`; unwrap it afterwards to
			// take the Vec back, exactly as the client's `recycle_batch` does.
			let dg = Datagram::Snapshot(Snapshot {
				server_tick: self.tick,
				your_last_input_tick: client.last_input_tick,
				me,
				others: std::mem::take(&mut players),
			});
			let encoded = codec::encode_datagram_into(&dg, &mut dgram).map(<[u8]>::to_vec);
			if let Datagram::Snapshot(s) = dg {
				players = s.others;
			}
			match encoded {
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
		self.snap_roster = roster;
		self.snap_nearby = nearby;
		self.snap_players = players;
		self.snap_buf = dgram;
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
	use super::super::Sim;
	use digblock_net::messages::{EditKind, InputTick, ServerMsg};
	use digblock_net::protocol::MAX_SNAPSHOT_PLAYERS;
	use glam::Vec3;

	fn dig_under(sim: &mut super::super::Sim, conn_id: u64, seq: u32) -> (i32, i32, i32) {
		// At an angle: the ground under the player's feet stays intact so
		// it can still walk (a hole underfoot would trap it in the pit).
		// Full timed flow: the flat world is stone (90-tick hardness).
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let mut t = 0;
		dig_hold(sim, conn_id, down, 90, seq, EditKind::Dig, &mut t)
	}

	fn walk(tick: u32, dz: f32) -> InputTick {
		InputTick {
			tick,
			move_axis: [0.0, dz],
			jump: false,
			look: [0.0, 0.0, 1.0],
			claimed_pos: [0.0, -500.0, 0.0], // never adopted: server physics
			claimed_vel: [0.0; 3],
			active_slot: 0,
		}
	}

	/// A crash/evict left a WaterFall journaled mid-air with nothing
	/// feeding it. Schedule-on-ready must drain it — before this fix it
	/// stayed frozen in the sky forever.
	#[test]
	fn frozen_midair_water_drains_on_load() {
		let dir = temp_dir("midair");
		{
			let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
			j.append(
				digblock_net::messages::BlockEdit {
					x: 8,
					y: 200,
					z: 8,
					block: digblock_core::block::BlockType::WaterFall as u16,
				},
				0,
			);
			j.sync();
		}
		let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut a = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let mut done = false;
		for _ in 0..600 {
			sim.tick_once(vec![]);
			a.drain();
			if sim.test_block_at(8, 200, 8) == Some(digblock_core::block::BlockType::Air) {
				done = true;
				break;
			}
		}
		assert!(done, "mid-air fall drained instead of freezing");
	}

	/// A journaled source at a residency boundary: its pool can only half
	/// form. When the far column loads (player approaches), the neighbor
	/// face rescan resumes the front into the fresh column.
	#[test]
	fn stalled_front_resumes_when_neighbor_column_loads() {
		let dir = temp_dir("front");
		{
			let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
			// Column (1, 0) edge cell, resting on the flat ground.
			j.append(
				digblock_net::messages::BlockEdit {
					x: 31,
					y: 127,
					z: 2,
					block: digblock_core::block::BlockType::Water as u16,
				},
				0,
			);
			j.sync();
		}
		let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut a = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		// Wait for column (1,0) — the walk's path — to be resident, so a
		// loaded test host can't drop the player through ungenerated
		// terrain mid-walk. Then let the loaded-side pool settle; col
		// (2,0) is beyond sim_radius 1 of the spawn center, so the +x
		// half cannot form yet.
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
		while sim.test_block_at(17, 0, 8).is_none() {
			assert!(
				std::time::Instant::now() < deadline,
				"col (1,0) never generated"
			);
			sim.tick_once(vec![]);
			a.drain();
		}
		for _ in 0..300 {
			sim.tick_once(vec![]);
			a.drain();
		}
		assert!(
			sim.test_block_at(30, 127, 2).is_some_and(|b| b.is_water()),
			"loaded-side pool formed"
		);
		assert_eq!(
			sim.test_block_at(32, 127, 2),
			None,
			"far column unloaded, front stalled"
		);

		// Walk +x until the far column becomes resident, then let fluid run.
		let walk_x = |tick: u32| InputTick {
			tick,
			move_axis: [0.0, 1.0],
			jump: false,
			look: [1.0, 0.0, 0.0],
			claimed_pos: [0.0, -500.0, 0.0],
			claimed_vel: [0.0; 3],
			active_slot: 0,
		};
		let x_ahead_ready = |sim: &Sim| {
			let p = sim.test_player(1).expect("client alive").pos;
			let (cx, cz) = (
				(p.x.floor() as i32).div_euclid(16),
				(p.z.floor() as i32).div_euclid(16),
			);
			sim.test_block_at((cx + 1) * 16 + 8, 0, cz * 16 + 8)
				.is_some()
		};
		let mut t = 100;
		while sim.test_player(1).unwrap().pos.x < 24.0 {
			// Hold with a stop input when the next column isn't ready —
			// coasting (no input) would walk off the generated edge.
			let mv = if x_ahead_ready(&sim) { 1.0 } else { 0.0 };
			let mut input = walk_x(t);
			input.move_axis = [0.0, mv];
			sim.tick_once(vec![batch(vec![input], 1)]);
			a.drain();
			t += 1;
			assert!(t < 60_000, "never walked toward the boundary");
		}
		let mut resumed = false;
		for _ in 0..1200 {
			sim.tick_once(vec![]);
			a.drain();
			if sim.test_block_at(32, 127, 2).is_some_and(|b| b.is_water()) {
				resumed = true;
				break;
			}
		}
		assert!(resumed, "front resumed into the freshly loaded column");
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
		// Only step when the column ahead is generated: on a saturated
		// test host the walk can outrun the workers, drop through
		// ungenerated ground, and entomb the player. The hold must be an
		// explicit stop INPUT — with no input the sim coasts on the last
		// intent and walks off the edge anyway.
		let ahead_ready = |sim: &Sim, dz: i32| {
			let p = sim.test_player(1).expect("client alive").pos;
			let (cx, cz) = (
				(p.x.floor() as i32).div_euclid(16),
				(p.z.floor() as i32).div_euclid(16),
			);
			sim.test_block_at(cx * 16 + 8, 0, (cz + dz) * 16 + 8)
				.is_some()
		};
		let mut t = 100;
		while col_dist(&sim) < 10 {
			let dz = if ahead_ready(&sim, 1) { 1.0 } else { 0.0 };
			sim.tick_once(vec![batch(vec![walk(t, dz)], 1)]);
			a.drain();
			t += 1;
			assert!(t < 60_000, "never walked out");
		}
		while col_dist(&sim) > 1 {
			let dz = if ahead_ready(&sim, -1) { -1.0 } else { 0.0 };
			sim.tick_once(vec![batch(vec![walk(t, dz)], 1)]);
			a.drain();
			t += 1;
			assert!(t < 90_000, "never walked back");
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
				snap.others.len() < MAX_SNAPSHOT_PLAYERS,
				"client {id}: {} remotes in snapshot",
				snap.others.len()
			);
			// Own state always rides its own field (the reconciliation
			// anchor), and never duplicates into the remote list.
			let my_id = (*id - 1) as u8;
			assert!(snap.me.is_some(), "client {id} got no self state");
			assert!(!snap.others.iter().any(|p| p.id == my_id));
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
			a.snaps.last().unwrap().others.iter().any(|p| p.id == 1),
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
			!a_last.others.iter().any(|p| p.id == 1),
			"far remote filtered: {:?}",
			a_last.others
		);
		let b_last = b.snaps.last().expect("B still snapshotted");
		assert!(
			b_last.me.is_some(),
			"B keeps its own state even with nobody nearby"
		);
	}
}
