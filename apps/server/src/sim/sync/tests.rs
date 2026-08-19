use super::super::harness::*;
use super::super::{Sim, column_of};
use super::{
	DGRAM_STALL_SECS, OUT_CAP, RESYNC_INTERVAL_TICKS, RESYNC_SETTLE_TICKS, WRAPPED_SNAPSHOT_RESERVE,
};
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

/// The retry loop: a client that stays put is handed its own edited
/// columns again, so a column it somehow lacks heals without anyone
/// having to notice — the push sync has no acknowledgement and can
/// notice nothing.
///
/// The walking half is pinned by
/// `evict_and_reenter_resyncs_column_exactly_once` below, which counts
/// JournalColumns across a long walk and would see these as extras.
#[test]
fn a_settled_client_is_handed_its_columns_again() {
	let mut sim = flat_sim("resync");
	let mut a = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let cell = dig_under(&mut sim, 1, 1);
	let col = (cell.0.div_euclid(16), cell.2.div_euclid(16));
	a.drain();
	a.msgs.clear();

	// Nothing at one INTERVAL: the rotation waits out the whole settle
	// first, which is what keeps it off a player crossing a column
	// boundary at walking pace. Measured in intervals rather than as a
	// fraction of the settle, so the dig above can lengthen without
	// eating the margin.
	for _ in 0..RESYNC_INTERVAL_TICKS {
		sim.tick_once(vec![]);
	}
	a.drain();
	assert!(
		!a.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::JournalColumn { .. })),
		"the rotation must not start on a client that just arrived"
	);

	// Stay put through the wait and one interval: the column comes
	// back, unasked.
	for _ in 0..RESYNC_SETTLE_TICKS + RESYNC_INTERVAL_TICKS {
		sim.tick_once(vec![]);
	}
	a.drain();
	assert!(
		a.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::JournalColumn { cx, cz, cells }
					if (*cx, *cz) == col
						&& cells.iter().any(|e| (e.x, e.y, e.z) == cell))),
		"a settled client is handed its edited column again: {:?}",
		a.msgs
	);
}

/// Trigger A: leaving prunes the synced set; re-entering re-syncs the
/// edited column exactly once via JournalColumn.
///
/// The background rotation cannot contribute to that count: the walk
/// crosses a column boundary — which restarts its wait — far more
/// often than `RESYNC_SETTLE_TICKS`, and the settle at the end is a
/// fraction of it. A stall long enough to change that would fail
/// loudly here rather than quietly, which is why the count is exact.
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
	// Server-authoritative movement: forward inputs, never-adopted claims,
	// so the sim's own physics carries B.
	//
	// A DISTANCE goal, never a tick budget. The two clocks are unrelated —
	// this loop advances world time as fast as the host will run it, while
	// the ground ahead of B is laid down by a worker on the wall clock — so
	// a fixed tick count measures the GENERATOR, and on a loaded host B is
	// left standing at the frontier still inside A's radius. Hold with a
	// stop input while the ground ahead is missing, and yield the core while
	// holding. Both clients drain every tick, or an undrained snapshot queue
	// reaches `OUT_CAP` and trips the stall drop.
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
/// reliable stream in BOTH directions — protocol v26.
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

/// The stream fallback must not put a LOSSY message class in a position to
/// fill the queue a lossless one dies on.
///
/// A snapshot travelling as a datagram leaves the outbound queue the moment
/// the pump reaches it; wrapped in a frame it leaves at the speed of one
/// stream's flow control, and a peer reading slowly parks it there. Without a
/// reserve the queue fills with snapshots and the next edit, chat line or
/// teleport is refused — and `client_send_frame` drops a client whose queue
/// refuses a frame, so the session ends over the one message class that was
/// always allowed to be dropped instead.
///
/// The twin of `stalled_datagram_queue_escalates_to_drop`, which pins the
/// other half: a peer that has genuinely stopped reading still goes.
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
