//! Residency, late journal sync, and snapshot broadcast.

use super::spawn::kindled_msg_at;
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

/// Frames of owed join sync handed to one client per tick — journal
/// columns and standing kindled lights together, since both are backlog
/// and one budget is what bounds the pair. At the tick rate this drains
/// even a wide join in under a second while costing a fraction of a
/// millisecond per tick.
const JOIN_SYNC_PER_TICK: usize = 16;
/// Outbound slots kept free for live traffic while a sync drains, so a
/// joining client's backlog can never starve edits, chat or teleports —
/// nor trip the queue-refused drop that `send_frame` reports.
const JOIN_SYNC_RESERVE: usize = 64;

/// Outbound slots a WRAPPED snapshot may not take, for the same reason [`JOIN_SYNC_RESERVE`]
/// exists and with more force. A datagram leaves as fast as the pump hands it to the socket and
/// can never durably occupy a slot; wrapped, it leaves at ONE stream's flow control and a slow
/// peer parks it there — putting a LOSSY class where it can fill the queue whose refusal kills
/// the client. Half the queue is left to the traffic that dies without a slot.
const WRAPPED_SNAPSHOT_RESERVE: usize = OUT_CAP / 2;

/// The background re-sync: one already-synced column handed back to one settled client at a time,
/// nearest first, for as long as they stay put. The push sync has no acknowledgement, so nothing
/// can NOTICE a client whose copy went astray; replay is last-write-wins, so a repeat costs only
/// bytes. Settling keeps the cost off a MOVING client, already handed the columns it enters. It
/// is the SECOND line — `Pipeline`'s overlay re-applies the journal whenever a column regrows.
const RESYNC_SETTLE_TICKS: u32 = TICK_RATE * 10;
/// Ticks between two columns of one rotation. See [`RESYNC_SETTLE_TICKS`].
const RESYNC_INTERVAL_TICKS: u32 = TICK_RATE;

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
			if center != client.residency.center {
				client.residency.center = center;
				client.residency.needs_sync_scan = true;
				self.residency_dirty = true;
			}
		}
		if self.residency_dirty {
			let centers: Vec<(i32, i32)> =
				self.clients.values().map(|c| c.residency.center).collect();
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
			if !client.live
				&& store.column_ready(client.residency.center.0, client.residency.center.1)
			{
				client.live = true;
				client.residency.needs_sync_scan = true;
				log::debug!("player {} live", client.id);
			}
			if !client.residency.needs_sync_scan {
				continue;
			}
			client.residency.needs_sync_scan = false;
			// This client is not settled: restart the background rotation's
			// wait, and drop what was left of it — the next one will be
			// built around wherever they stop.
			client.residency.resync_countdown = RESYNC_SETTLE_TICKS;
			client.residency.resync_queue.clear();
			// Columns only leave the slack radius when the center moves,
			// so the prune rides the same trigger.
			client.residency.synced_columns.retain(|&col| {
				in_radius(
					col,
					client.residency.center,
					client.residency.draw_dist + SYNC_SLACK,
				)
			});
			// Region-indexed, not a walk of every edited column in the
			// world: this fires per client every time their center moves —
			// once per column crossed — and `edited_columns` grows for the
			// life of the world while everything around it stays O(active).
			// The result is a superset (whole regions), so the radius test
			// below still does the real filtering.
			for col in journal
				.index
				.columns_near(client.residency.center, client.residency.draw_dist)
			{
				if in_radius(col, client.residency.center, client.residency.draw_dist)
					&& !client.residency.synced_columns.contains(&col)
					&& !client.residency.sync_queue_set.contains(&col)
				{
					client.residency.sync_queue.push_back(col);
					client.residency.sync_queue_set.insert(col);
				}
			}
			// A client mid-arrival is waiting on the columns nearest its
			// new centre, so put those at the head. The join path sorts
			// its own queue as it builds it; this covers the teleport
			// path, where the scan appends to whatever was already owed.
			// `AreaReady` below then reads the front and nothing else.
			if client.arrival_hold {
				let center = client.residency.center;
				let mut cols: Vec<(i32, i32)> = client.residency.sync_queue.drain(..).collect();
				cols.sort_by_key(|&col| super::radius_key(col, center));
				client.residency.sync_queue = cols.into();
			}
		}
		self.pump_join_sync();
		self.pump_resync();
	}

	/// Hand each settled client one already-synced column back, on the
	/// rotation [`RESYNC_SETTLE_TICKS`] describes — the push sync's retry
	/// loop. See that constant for what it is for.
	fn pump_resync(&mut self) {
		let journal = &self.journal;
		for client in self.clients.values_mut() {
			if client.dead.get() || !client.live {
				continue;
			}
			// Never while anything is genuinely OWED. That queue carries
			// the same columns, fresher, and an arriving client is waiting
			// on its front — spending the budget on a repeat would delay
			// the world the player is standing in.
			if client.arrival_hold || !client.residency.sync_queue.is_empty() {
				continue;
			}
			if client.residency.resync_countdown > 0 {
				client.residency.resync_countdown -= 1;
				continue;
			}
			// Charged before the work, not after it: an unedited disc
			// finds nothing to send, and re-deciding that every tick is
			// the per-tick journal scan the steady state exists without.
			client.residency.resync_countdown = RESYNC_INTERVAL_TICKS;
			// The same head-room live traffic gets from the join pump: a
			// backstop must never be the thing that fills the queue an
			// edit dies on.
			if client.out.capacity() <= JOIN_SYNC_RESERVE {
				continue;
			}
			if client.residency.resync_queue.is_empty() {
				// One rotation's worth, nearest first — so the ground the
				// player is standing on heals in seconds and the rim takes
				// as long as it takes. Region-indexed like the sync scan,
				// and paid once per rotation rather than per column.
				let center = client.residency.center;
				let mut cols: Vec<(i32, i32)> = journal
					.index
					.columns_near(center, client.residency.draw_dist)
					.filter(|&col| in_radius(col, center, client.residency.draw_dist))
					.collect();
				cols.sort_by_key(|&col| super::radius_key(col, center));
				client.residency.resync_queue = cols.into();
			}
			// A rotation outlives the odd sub-column shuffle that does not
			// count as moving, so skip past anything now out of range
			// rather than spending the interval on an invisible column.
			while let Some(col) = client.residency.resync_queue.pop_front() {
				if !in_radius(col, client.residency.center, client.residency.draw_dist) {
					continue;
				}
				if send_journal_column_to(journal, &client.out, col.0, col.1) {
					client.residency.synced_columns.insert(col);
				} else {
					client.dead.set(true);
				}
				break;
			}
		}
	}

	/// Hand each client a few queued journal columns — then a few of the standing lights it is
	/// owed — per tick, never more than its outbound queue can take.
	///
	/// Encoding every in-range column in one tick is thousands of frames inside one tick's budget
	/// and overruns `OUT_CAP`; reserving capacity keeps sync to slots live traffic will not need.
	fn pump_join_sync(&mut self) {
		let journal = &self.journal;
		let tick = self.tick;
		for client in self.clients.values_mut() {
			if client.dead.get() {
				continue;
			}
			let mut room = client
				.out
				.capacity()
				.saturating_sub(JOIN_SYNC_RESERVE)
				.min(JOIN_SYNC_PER_TICK);
			while room > 0 {
				let Some(col) = client.residency.sync_queue.pop_front() else {
					break;
				};
				room -= 1;
				client.residency.sync_queue_set.remove(&col);
				// It may have left range while it sat in the queue.
				if !in_radius(col, client.residency.center, client.residency.draw_dist) {
					continue;
				}
				if !send_journal_column_to(journal, &client.out, col.0, col.1) {
					client.dead.set(true);
					break;
				}
				client.residency.synced_columns.insert(col);
			}
			// The lights, behind the columns: terrain is what the arrival
			// hold waits on, and a light that expired in the queue is
			// simply dropped — its own clock has already answered.
			while room > 0 && !client.dead.get() {
				let Some(light) = client.residency.kindled_queue.pop_front() else {
					break;
				};
				room -= 1;
				if light.until <= tick {
					continue;
				}
				if !send_frame(&client.out, &kindled_msg_at(tick, &light)) {
					client.dead.set(true);
					break;
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
				&& !client.residency.needs_sync_scan
				&& client
					.residency
					.sync_queue
					.front()
					.is_none_or(|&col| !in_radius(col, client.residency.center, AREA_SYNC_RADIUS))
			{
				client.arrival_hold = false;
				if !send_frame(&client.out, &ServerMsg::AreaReady) {
					client.dead.set(true);
				}
			}
		}
	}

	pub(super) fn send_snapshots(&mut self) {
		// Shared roster built once; per client only nearby players are selected, so a snapshot can
		// never outgrow one datagram. Every buffer is reused across ticks — allocating per client
		// per snapshot churns megabytes a second on the sim thread at a full server — and the
		// selection is a `select_nth_unstable`, since only the nearest matter. The roster carries
		// each player BOTH ways: quantized once for remotes, full precision for its own owner.
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
					fx: {
						use digblock_net::messages::{RemoteFx, pack_fx};
						match c.wisp_until {
							Some(t) if self.tick < t => {
								let (level, whitened) = digblock_core::unlock::wisp_light(
									c.skills.level(digblock_core::skills::Skill::Arcana),
								);
								pack_fx(RemoteFx::Wisp { level, whitened })
							}
							_ => pack_fx(RemoteFx::None),
						}
					},
				},
				SelfState {
					pos: s.pos.to_array(),
					vel: s.vel.to_array(),
				},
				c.residency.center,
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
				if !in_radius(*col, client.residency.center, client.residency.draw_dist) {
					continue;
				}
				let (dx, dz) = (
					col.0 as i64 - client.residency.center.0 as i64,
					col.1 as i64 - client.residency.center.1 as i64,
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
				// A client with no datagram channel gets the same payload wrapped in a
				// reliable frame, and keeps every bit of the handling below it, drop-on-full
				// included: `send_frame` kills a client whose queue refuses a frame, which is
				// right for an edit and catastrophic for a snapshot — the one message class
				// always allowed to stall.
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

/// Hand one snapshot to the client's outbound queue, or `None` when it may not have a slot at all.
///
/// A datagram may always try: the pump hands those to the socket without awaiting, so one cannot
/// sit in the queue. A WRAPPED one is held to what [`WRAPPED_SNAPSHOT_RESERVE`] leaves it, and
/// above that is skipped rather than refused — a refusal is what the stall reaper counts.
fn queue_snapshot(
	client: &super::Client,
	bytes: Vec<u8>,
) -> Option<Result<(), TrySendError<OutMsg>>> {
	let out = snapshot_out(client, bytes);
	let wrapped = matches!(out, OutMsg::Frame(_));
	(!wrapped || client.out.capacity() > WRAPPED_SNAPSHOT_RESERVE).then(|| client.out.try_send(out))
}

/// How one snapshot travels to one client: as a datagram, or — for a client that announced
/// `NoDatagrams` — as the same bytes wrapped in a reliable frame.
///
/// An encode failure falls back to the datagram that client cannot receive, deliberately: the
/// wrap cannot fail for any reason a retry would fix, and the alternative drops a client for it.
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
mod tests;
