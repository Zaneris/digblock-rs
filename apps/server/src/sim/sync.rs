//! Residency, late journal sync, and snapshot broadcast.

use super::{DGRAM_STALL_SECS, OUT_CAP, OutMsg, OutTx, Sim, column_of, in_radius, send_frame};
use crate::journal::Journal;
use digblock_net::codec;
use digblock_net::messages::{
	BlockEdit, Datagram, RemoteState, SelfState, ServerMsg, Snapshot, vel_to_wire, yaw_to_wire,
};
use digblock_net::protocol::{
	AREA_SYNC_RADIUS, FAR_PLAYER_STRIDE, MAX_SNAPSHOT_PLAYERS, PLAYER_NEAR_COLS, SNAPSHOT_DIVISOR,
	TICK_RATE,
};
use tokio::sync::mpsc::error::TrySendError;

/// Columns beyond `draw_dist + slack` fall out of a client's synced set,
/// so re-entry re-syncs (clients evict remote chunks too).
const SYNC_SLACK: i32 = 2;

/// Journal columns handed to one client per tick. At the tick rate this
/// drains even a wide join in under a second while costing a fraction of a
/// millisecond per tick.
const JOIN_SYNC_PER_TICK: usize = 16;
/// Outbound slots kept free for live traffic while a sync drains, so a
/// joining client's backlog can never starve edits, chat or teleports —
/// nor trip the queue-refused drop that `send_frame` reports.
const JOIN_SYNC_RESERVE: usize = 64;

/// Outbound slots a WRAPPED snapshot may not take, for the same reason
/// [`JOIN_SYNC_RESERVE`] exists and with more force.
///
/// A snapshot travelling as a datagram leaves the queue as fast as the
/// pump can hand it to the socket, so it can never durably occupy a slot.
/// Wrapped in a reliable frame it leaves at the speed of ONE stream's flow
/// control, and a peer that reads slowly parks it there — which puts a
/// lossy message class in a position to fill the queue whose refusal kills
/// the client (`client_send_frame`). The player then loses the session
/// over snapshots that were droppable the whole time, and the edit that
/// happened to arrive during the stall is what does it. Half the queue is
/// left to the traffic that dies without a slot; snapshots take the rest
/// and are simply skipped above it.
const WRAPPED_SNAPSHOT_RESERVE: usize = OUT_CAP / 2;

/// Neighbor journal-water rescans processed per tick (each is O(that
/// column's journaled cells)) — an initial full-radius load can't hitch the
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
			// Region-indexed, not a walk of every edited column in the
			// world: this fires per client every time their center moves —
			// once per column crossed — and `edited_columns` grows for the
			// life of the world while everything around it stays O(active).
			// The result is a superset (whole regions), so the radius test
			// below still does the real filtering.
			for col in journal.index.columns_near(client.center, client.draw_dist) {
				if in_radius(col, client.center, client.draw_dist)
					&& !client.synced_columns.contains(&col)
					&& !client.sync_queue_set.contains(&col)
				{
					client.sync_queue.push_back(col);
					client.sync_queue_set.insert(col);
				}
			}
			// A client mid-arrival is waiting on the columns nearest its
			// new centre, so put those at the head. The join path sorts
			// its own queue as it builds it; this covers the teleport
			// path, where the scan appends to whatever was already owed.
			// `AreaReady` below then reads the front and nothing else.
			if client.arrival_hold {
				let center = client.center;
				let mut cols: Vec<(i32, i32)> = client.sync_queue.drain(..).collect();
				cols.sort_by_key(|&col| super::radius_key(col, center));
				client.sync_queue = cols.into();
			}
		}
		self.pump_join_sync();
	}

	/// Hand each client a few queued journal columns per tick, never more
	/// than its outbound queue can take.
	///
	/// Join sync used to run entirely inside `handle_hello`: every in-range
	/// edited column encoded and pushed in ONE tick. On a mature world that
	/// is thousands of frames and megabytes inside one tick's budget — a
	/// hitch for every other player on the server, on every join — and past
	/// `OUT_CAP` it was a race the joiner lost outright, dropped for
	/// "overflowing the outbound queue" through no fault of its own.
	///
	/// Reserving capacity is what makes the cliff structurally impossible:
	/// sync only ever uses slots live traffic is not going to need.
	fn pump_join_sync(&mut self) {
		let journal = &self.journal;
		for client in self.clients.values_mut() {
			if client.dead.get() {
				continue;
			}
			if !client.sync_queue.is_empty() {
				let room = client
					.out
					.capacity()
					.saturating_sub(JOIN_SYNC_RESERVE)
					.min(JOIN_SYNC_PER_TICK);
				for _ in 0..room {
					let Some(col) = client.sync_queue.pop_front() else {
						break;
					};
					client.sync_queue_set.remove(&col);
					// It may have left range while it sat in the queue.
					if !in_radius(col, client.center, client.draw_dist) {
						continue;
					}
					if !send_journal_column_to(journal, &client.out, col.0, col.1) {
						client.dead.set(true);
						break;
					}
					client.synced_columns.insert(col);
				}
			}
			// Release the arrival hold once nothing within `AREA_SYNC_RADIUS`
			// is still owed. The queue is sorted nearest-first for a client
			// in this state, so its FRONT is the whole test — an O(1) read
			// rather than a scan of a queue that can be thousands long.
			//
			// `live` and `needs_sync_scan` are both load-bearing: a teleport
			// sets the hold a tick before `update_residency` notices the new
			// centre, and releasing on the pre-teleport queue would report
			// the wrong area ready. An empty queue is the ordinary case for
			// an unedited destination and releases immediately, which is why
			// this sits outside the emptiness check above.
			if client.arrival_hold
				&& client.live
				&& !client.needs_sync_scan
				&& client
					.sync_queue
					.front()
					.is_none_or(|&col| !in_radius(col, client.center, AREA_SYNC_RADIUS))
			{
				client.arrival_hold = false;
				if !send_frame(&client.out, &ServerMsg::AreaReady) {
					client.dead.set(true);
				}
			}
		}
	}

	pub(super) fn send_snapshots(&mut self) {
		// Shared roster built once; per client only nearby players are
		// selected, so snapshots can never outgrow one datagram (the cliff
		// that used to silently kill all snapshots on a busy server).
		//
		// Every buffer here is reused across ticks. This runs on every
		// snapshot and used to allocate per client per snapshot — tens of
		// thousands of allocations and megabytes a second of churn at a full
		// server, on the sim thread. The selection is also a
		// `select_nth_unstable` rather than a full sort: only the nearest
		// `MAX_SNAPSHOT_PLAYERS - 1` matter, and sorting the whole candidate
		// list per client made the pass O(n² log n) — millions of
		// comparisons a second at a full server to choose a handful.
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
		// Counts snapshots, not ticks — the tiering phase must advance once
		// per snapshot or a far player would be skipped forever.
		let snap_index = self.tick / SNAPSHOT_DIVISOR;
		let mut nearby = std::mem::take(&mut self.snap_nearby);
		let mut players = std::mem::take(&mut self.snap_players);
		let mut dgram = std::mem::take(&mut self.snap_buf);
		for client in self.clients.values() {
			if !client.live {
				continue;
			}
			// Self is its own field now (the reconciliation anchor, at full
			// precision); the rest are nearest-first within draw distance.
			//
			// Distance TIERS the rate rather than gating membership: a
			// remote inside PLAYER_NEAR_COLS rides every snapshot, one
			// beyond it rides one in FAR_PLAYER_STRIDE. Which snapshot is
			// phased by player id, so far players spread across the cycle
			// instead of all arriving together and then not at all.
			nearby.clear();
			let mut me = None;
			for (i, (p, own, col)) in roster.iter().enumerate() {
				if p.id == client.id {
					me = Some(*own);
					continue;
				}
				if !in_radius(*col, client.center, client.draw_dist) {
					continue;
				}
				let (dx, dz) = (
					col.0 as i64 - client.center.0 as i64,
					col.1 as i64 - client.center.1 as i64,
				);
				let d2 = dx * dx + dz * dz;
				let near = d2 <= (PLAYER_NEAR_COLS as i64).pow(2);
				if !near && !(snap_index + p.id as u32).is_multiple_of(FAR_PLAYER_STRIDE) {
					continue;
				}
				nearby.push((d2, i));
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
				// A client with no datagram channel gets the same payload
				// wrapped in a reliable frame — and keeps every bit of the
				// handling below it, drop-on-full included. That is the
				// load-bearing part: `send_frame` kills a client whose
				// queue refuses a frame, which is right for an edit and
				// catastrophic for a snapshot, since snapshots are the one
				// message class that has always been allowed to stall.
				Ok(bytes) => match queue_snapshot(client, bytes) {
					Some(Ok(())) => client.dgram_full_since.set(None),
					// Above the reserve the snapshot is skipped rather than
					// refused (`None`), so it never counts toward the stall
					// below: a peer whose stream really has stopped draining
					// is reaped by a reliable frame meeting a genuinely full
					// queue, which is the class that cannot survive a drop.
					None => {}
					Some(Err(TrySendError::Full(_))) => {
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
					Some(Err(TrySendError::Closed(_))) => client.dead.set(true),
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

/// Hand one snapshot to the client's outbound queue, or `None` when it may
/// not have a slot at all.
///
/// A datagram may always try: the pump hands those to the socket without
/// ever awaiting, so one cannot sit in the queue. A WRAPPED one is held to
/// what [`WRAPPED_SNAPSHOT_RESERVE`] leaves it, and above that is skipped
/// rather than refused — a refusal is what the stall reaper counts, and
/// this is a policy, not a peer that stopped reading.
fn queue_snapshot(
	client: &super::Client,
	bytes: Vec<u8>,
) -> Option<Result<(), TrySendError<OutMsg>>> {
	let out = snapshot_out(client, bytes);
	let wrapped = matches!(out, OutMsg::Frame(_));
	(!wrapped || client.out.capacity() > WRAPPED_SNAPSHOT_RESERVE).then(|| client.out.try_send(out))
}

/// How one snapshot travels to one client: as a datagram, or — for a
/// client that announced `NoDatagrams` — as the same bytes wrapped in a
/// reliable frame.
///
/// An encode failure here falls back to the datagram, which that client
/// cannot receive; that is deliberate. The wrap is a `Vec<u8>` inside an
/// enum variant and cannot fail for any reason a retry would fix, and the
/// alternative is dropping a client over a condition that does not exist.
fn snapshot_out(client: &super::Client, bytes: Vec<u8>) -> OutMsg {
	if !client.no_datagrams.get() {
		return OutMsg::Datagram(bytes);
	}
	let mut frame = Vec::new();
	match codec::write_frame(&ServerMsg::Unreliable(bytes), &mut frame) {
		Ok(()) => OutMsg::Frame(frame.into()),
		Err(e) => {
			log::error!("player {}: wrapping a snapshot: {e}", client.id);
			OutMsg::Datagram(Vec::new())
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
	use super::super::{Sim, column_of};
	use super::{DGRAM_STALL_SECS, OUT_CAP, WRAPPED_SNAPSHOT_RESERVE};
	use digblock_core::chunk::CHUNK_SIZE;
	use digblock_net::codec;
	use digblock_net::messages::{ClientMsg, Datagram, EditKind, InputBatch, InputTick, ServerMsg};
	use digblock_net::protocol::{MAX_SNAPSHOT_PLAYERS, TICK_RATE};
	use glam::Vec3;

	fn dig_under(sim: &mut super::super::Sim, conn_id: u64, seq: u32) -> (i32, i32, i32) {
		// At an angle: the ground under the player's feet stays intact so
		// it can still walk (a hole underfoot would trap it in the pit).
		// Full timed flow, held for stone's own hardness.
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

	/// Ticks until `AreaReady` lands, panicking rather than looping if it
	/// never does. Returns how many `JournalColumn`s preceded it.
	fn columns_before_area_ready(sim: &mut Sim, c: &mut TestClient) -> usize {
		for _ in 0..600 {
			sim.tick_once(vec![]);
			std::thread::sleep(std::time::Duration::from_millis(1));
			c.drain();
			if let Some(at) = c
				.msgs
				.iter()
				.position(|m| matches!(m, ServerMsg::AreaReady))
			{
				return c.msgs[..at]
					.iter()
					.filter(|m| matches!(m, ServerMsg::JournalColumn { .. }))
					.count();
			}
		}
		panic!("AreaReady never arrived");
	}

	/// An untouched world owes the joiner nothing, so the hold must
	/// release on the first pump. A gate that waits for a message the
	/// server has no reason to send is a gate that always times out.
	#[test]
	fn an_unedited_spawn_releases_the_arrival_hold_immediately() {
		let mut sim = flat_sim("area-clean");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		assert_eq!(
			columns_before_area_ready(&mut sim, &mut c),
			0,
			"nothing was edited; no column should precede the marker"
		);
	}

	/// The marker is the whole point: it must arrive AFTER the journaled
	/// column the player is standing on, or the client un-holds onto the
	/// worldgen the journal was about to overwrite — the artifact this
	/// exists to remove.
	#[test]
	fn the_spawn_column_lands_before_the_arrival_hold_releases() {
		let dir = temp_dir("area-edited");
		// The spawn's own column, journaled before anyone joins — history
		// to sync rather than an edit made after the join.
		let (sx, sz) = (FLAT_SPAWN.x.floor() as i32, FLAT_SPAWN.z.floor() as i32);
		let (cx, cz) = (sx.div_euclid(16), sz.div_euclid(16));
		{
			let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
			j.append(
				digblock_net::messages::BlockEdit {
					x: sx,
					y: FLAT_SPAWN.y.floor() as i32 - 2,
					z: sz,
					block: digblock_core::block::BlockType::Air as u16,
				},
				0,
			);
			j.sync();
		}
		let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let before = columns_before_area_ready(&mut sim, &mut c);
		assert!(before >= 1, "the edited column should have been sent first");
		let spawn_col_sent = c.msgs.iter().any(
			|m| matches!(m, ServerMsg::JournalColumn { cx: mx, cz: mz, .. } if (*mx, *mz) == (cx, cz)),
		);
		assert!(spawn_col_sent, "spawn column {cx},{cz} never arrived");
	}

	/// A teleport is an arrival too: the destination's journal is no more
	/// synced than a join's, so the hold re-arms and a SECOND marker has
	/// to follow. Without it a `/tp` drops the player onto worldgen with
	/// nothing telling the client to wait.
	#[test]
	fn a_teleport_rearms_the_arrival_hold() {
		let mut sim = flat_sim("area-tp");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		columns_before_area_ready(&mut sim, &mut c);
		c.msgs.clear();
		// The real path, not `test_teleport` (which stages a position
		// without going through `teleport_client` and so would prove
		// nothing): return-to-spawn is the one every client can ask for.
		sim.tick_once(vec![super::super::SimEvent::Msg {
			conn_id: 1,
			msg: digblock_net::messages::ClientMsg::ReturnToSpawn,
		}]);
		c.drain();
		assert!(
			c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::Teleported { .. })),
			"the teleport itself should have landed"
		);
		columns_before_area_ready(&mut sim, &mut c);
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
		// (2,0) is beyond this sim's residency radius from the spawn
		// center, so the +x half cannot form yet.
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
		// A generous residency radius: columns generate well ahead of a
		// walking player even when parallel tests starve the worker threads
		// (a tight one can lag and drop the player through ungenerated
		// terrain).
		let dir = temp_dir("reenter");
		let mut sim = Sim::new_with_source(SEED, &dir, 3, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut a = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let cell = dig_under(&mut sim, 1, 1);
		let col = (cell.0.div_euclid(16), cell.2.div_euclid(16));

		// Walk out (+z) until well past the client's draw_dist + SYNC_SLACK,
		// then back until near the edited column again — condition-driven, so
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
	/// every player and silently exceeded MAX_DATAGRAM once a crowd
	/// gathered — now each client gets self + nearest remotes, always
	/// within one datagram.
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

	/// Distance tiers the RATE, it does not gate membership. A player past
	/// `PLAYER_NEAR_COLS` but still inside draw distance keeps arriving —
	/// out there they are only a few pixels tall, which is small but very
	/// much visible, so dropping them outright would delete people you can
	/// see. They simply arrive in one snapshot out of `FAR_PLAYER_STRIDE`.
	#[test]
	fn a_distant_player_is_sent_slower_not_dropped() {
		use digblock_net::protocol::{FAR_PLAYER_STRIDE, PLAYER_NEAR_COLS};
		let mut sim = flat_sim("tier");
		let mut a = connect(&mut sim, 1);
		let _b = connect(&mut sim, 2);
		let _c = connect(&mut sim, 3);
		for id in [1, 2, 3] {
			wait_live(&mut sim, id);
		}
		// The harness's join draw_dist sits inside PLAYER_NEAR_COLS — give A
		// a real client's radius so "far but visible" can exist at all.
		sim.tick_once(vec![super::super::SimEvent::Msg {
			conn_id: 1,
			msg: digblock_net::messages::ClientMsg::SetDrawDist { draw_dist: 20 },
		}]);
		// B stays on top of A; C is moved well past the near radius but
		// comfortably inside A's draw distance.
		let near = sim.test_player(1).unwrap().pos;
		sim.test_teleport(
			3,
			near + Vec3::new((PLAYER_NEAR_COLS as f32 + 2.0) * 16.0, 0.0, 0.0),
		);
		for _ in 0..3 {
			sim.tick_once(vec![]);
		}
		a.drain();
		a.snaps.clear();

		// A couple of full stride cycles' worth of snapshots.
		let want = (FAR_PLAYER_STRIDE * 4) as usize;
		while a.snaps.len() < want {
			sim.tick_once(vec![]);
			a.drain();
		}
		let seen = |id: u8| {
			a.snaps
				.iter()
				.filter(|s| s.others.iter().any(|p| p.id == id))
				.count()
		};
		let (b_seen, c_seen) = (seen(1), seen(2));
		assert_eq!(
			b_seen,
			a.snaps.len(),
			"the near player rides every snapshot"
		);
		assert!(c_seen > 0, "the far player is still sent, just less often");
		assert!(
			c_seen < b_seen,
			"far {c_seen} vs near {b_seen} of {} — the tiering did nothing",
			a.snaps.len()
		);
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

		// B walks away, out past A's draw distance rather than onto its edge.
		// Server-authoritative movement: forward inputs, never-adopted
		// claims, so the sim's own physics carries B.
		//
		// A DISTANCE goal, never a tick budget. The two clocks here are
		// unrelated — this loop advances world time as fast as the host
		// will run it, while the ground ahead of B is laid down by a
		// worker on the wall clock — so a fixed tick count measures the
		// GENERATOR: on a loaded host B was left standing at the frontier,
		// stopped dead on a column boundary still well inside A's radius,
		// and the test failed for a reason it is not about. Hold with a
		// stop input while the ground ahead is missing, exactly as the
		// fluid walk above does, and yield the core while holding so the
		// worker can lay the next column. Both clients drain every tick:
		// the walk is now as long as it needs to be, and an undrained
		// snapshot queue reaches `OUT_CAP` and trips the stall drop.
		let ahead_ready = |sim: &Sim| {
			let (cx, cz) = column_of(sim.test_player(2).expect("B still connected").pos);
			let mid = CHUNK_SIZE as i32 / 2;
			sim.test_block_at(
				cx * CHUNK_SIZE as i32 + mid,
				0,
				(cz + 1) * CHUNK_SIZE as i32 + mid,
			)
			.is_some()
		};
		let goal = ((JOIN_DRAW_DIST + 2) * CHUNK_SIZE as i32) as f32;
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
		let mut t = 100;
		while sim.test_player(2).expect("B still connected").pos.z < goal {
			assert!(
				std::time::Instant::now() < deadline,
				"B never walked clear of A's radius"
			);
			let go = ahead_ready(&sim);
			if !go {
				std::thread::sleep(std::time::Duration::from_millis(1));
			}
			sim.tick_once(vec![batch(vec![input(t, go, no_claim())], 2)]);
			t += 1;
			a.drain();
			b.drain();
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

	/// A client whose transport has no datagram channel (iOS Safari ships
	/// `WebTransport.datagrams` with no writable side) is served over the
	/// reliable stream in BOTH directions — protocol v25.
	///
	/// Both halves matter and they are separate mechanisms: the server has
	/// to stop aiming snapshots at a channel that is not there, and it has
	/// to accept inputs that arrive wrapped. Either one alone leaves a
	/// player who can see the world but not move, or move but never see
	/// anyone.
	#[test]
	fn a_client_without_datagrams_is_served_over_the_reliable_stream() {
		let mut sim = flat_sim("no-datagrams");
		let mut c = connect(&mut sim, 1);
		sim.tick_once(vec![super::super::SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::NoDatagrams,
		}]);
		wait_live(&mut sim, 1);
		for _ in 0..6 {
			sim.tick_once(vec![]);
		}
		c.drain();
		assert!(!c.snaps.is_empty(), "snapshots still arrive");
		assert_eq!(
			c.dgram_snaps, 0,
			"nothing was sent down the channel this client does not have"
		);

		// And its inputs, wrapped the way the client wraps them, drive the
		// same physics an input datagram drives.
		let start = sim.test_player(1).expect("connected").pos;
		for t in 100..160 {
			let dg = Datagram::Input(InputBatch {
				inputs: vec![input(t, true, no_claim())],
			});
			let payload = codec::encode_datagram(&dg).expect("encodes");
			sim.tick_once(vec![super::super::SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Unreliable(payload),
			}]);
		}
		let moved = sim.test_player(1).expect("connected").pos;
		assert!(
			(moved - start).length() > 1.0,
			"a wrapped input batch moves the player ({start} -> {moved})"
		);
	}

	/// The stream fallback must not put a LOSSY message class in a
	/// position to fill the queue a lossless one dies on.
	///
	/// A snapshot travelling as a datagram leaves the outbound queue the
	/// moment the pump reaches it, so it cannot durably hold a slot;
	/// wrapped in a frame it leaves at the speed of one stream's flow
	/// control, and a peer reading slowly parks it there. Without a
	/// reserve the queue fills with snapshots and the next edit, chat
	/// line or teleport is refused — and `client_send_frame` drops a
	/// client whose queue refuses a frame, so the session ends over the
	/// one message class that was always allowed to be dropped instead.
	///
	/// The twin of `stalled_datagram_queue_escalates_to_drop`, which pins
	/// the other half: a peer that has genuinely stopped reading still
	/// goes. Here the peer is merely behind, and stays.
	#[test]
	fn wrapped_snapshots_leave_the_queue_reliable_frames_die_on() {
		let mut sim = flat_sim("no-datagrams-reserve");
		let mut c = connect(&mut sim, 1);
		sim.tick_once(vec![super::super::SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::NoDatagrams,
		}]);
		wait_live(&mut sim, 1);
		c.snaps.clear();
		// Never drained, and run well past where snapshots alone would
		// have filled the queue and escalated — the same measure the
		// datagram half of the rule is tested at.
		let enough = 3 * OUT_CAP as u32 + (DGRAM_STALL_SECS + 2) * TICK_RATE;
		for _ in 0..enough {
			sim.tick_once(vec![]);
		}
		assert_eq!(
			sim.player_count(),
			1,
			"a client merely behind on snapshots keeps its session"
		);
		// Everything the run queued is still sitting in the channel, so
		// what arrives now IS what the snapshots were allowed to hold.
		c.drain();
		assert!(!c.snaps.is_empty(), "snapshots were being sent at all");
		assert!(
			c.snaps.len() <= OUT_CAP - WRAPPED_SNAPSHOT_RESERVE,
			"{} snapshots parked in the queue — past the share a lossy class may hold",
			c.snaps.len()
		);
	}
}
