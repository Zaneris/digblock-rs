use super::super::harness::*;
use super::*;
use digblock_core::block::BlockType;
use digblock_core::worldgen::Seed;
use digblock_core::worldjob::{Candidates, CellSource, Run, Schedule, Scope, WorldJob};

/// Caps the flat world's Stone floor with Dirt, so a job has a legal
/// target — Stone is a node host and a sweep may never touch it.
fn sow_dirt(sim: &mut Sim, y: i32, span: i32) -> Vec<(i32, i32, i32)> {
	let mut cells = Vec::new();
	for z in -span..=span {
		for x in -span..=span {
			sim.test_set_cell(x, y, z, BlockType::Dirt as u16);
			cells.push((x, y, z));
		}
	}
	cells
}

/// A band-mode stand-in: dirt turns to grass unconditionally. The real
/// rule is `worldjob::grass`, exercised by the frontier test below.
struct Sprout {
	radius_chunks: i32,
	y: i32,
}

impl WorldJob for Sprout {
	fn name(&self) -> &'static str {
		"sprout"
	}
	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 2,
			offset: 0.0,
		}
	}
	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Dirt],
			radius_chunks: self.radius_chunks,
			y: (self.y, self.y),
			candidates: Candidates::Band,
		}
	}
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: Seed, _: Run) -> Option<u16> {
		Some(BlockType::Grass as u16)
	}
}

/// Tick long enough for one sweep of this size to finish, then hand back
/// the edits the client received. A small sweep fits in a single slice,
/// so completion is not something a caller can watch for.
fn run_sweep(
	sim: &mut Sim,
	c: &mut TestClient,
	ticks: u32,
) -> Vec<digblock_net::messages::BlockEdit> {
	for _ in 0..ticks {
		sim.tick_once(Vec::new());
	}
	assert!(
		!sim.test_world_job_running(),
		"the sweep is still running after {ticks} ticks"
	);
	c.drain();
	let edits = c
		.msgs
		.iter()
		.filter_map(|m| match m {
			ServerMsg::EditApplied { edit, .. } => Some(*edit),
			_ => None,
		})
		.collect();
	c.msgs.clear();
	edits
}

/// The whole path, end to end: the clock comes due, the sweep finds the
/// cells, and what it writes reaches a connected client as an ordinary
/// edit — no new wire message anywhere.
#[test]
fn a_sweep_reaches_a_client_as_an_ordinary_edit() {
	let mut sim = flat_sim("wj-e2e");
	let y = FLAT_GROUND_TOP as i32;
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let sown = sow_dirt(&mut sim, y, 2);
	c.drain();
	sim.register_world_job(Box::new(Sprout {
		radius_chunks: 0,
		y,
	}));

	let edits = run_sweep(&mut sim, &mut c, 8);
	let mut swept = 0;
	for &(x, _, z) in &sown {
		// `sow_dirt` reaches past the one chunk this job sweeps.
		if !(0..16).contains(&x) || !(0..16).contains(&z) {
			continue;
		}
		assert_eq!(
			sim.cell_at(x, y, z),
			Some(BlockType::Grass as u16),
			"({x},{y},{z}) was not swept"
		);
		assert!(
			edits
				.iter()
				.any(|e| (e.x, e.y, e.z) == (x, y, z) && e.block == BlockType::Grass as u16),
			"({x},{y},{z}) never reached the client"
		);
		swept += 1;
	}
	assert!(swept > 0, "the sown band missed the swept chunk entirely");
}

/// The spawn lock refuses the sweep's write, the same way it refuses
/// water flowing in from outside.
#[test]
fn a_locked_spawn_refuses_a_sweep() {
	let mut sim = flat_sim("wj-lock");
	// One above the ground, inside the lock sphere near the pad and
	// outside it farther away — the sweep band straddles the boundary.
	let y = FLAT_GROUND_TOP as i32 + 1;
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let sown = sow_dirt(&mut sim, y, 8);
	sim.bench_lock_spawn_at(FLAT_SPAWN);
	sim.register_world_job(Box::new(Sprout {
		radius_chunks: 0,
		y,
	}));
	c.drain();
	run_sweep(&mut sim, &mut c, 8);

	let locks = sim.protected_regions();
	assert!(!locks.is_empty(), "spawn is locked");
	let (mut refused, mut swept) = (0, 0);
	for &(x, _, z) in &sown {
		if !(0..16).contains(&x) || !(0..16).contains(&z) {
			continue;
		}
		if digblock_core::protect::immutable(x, y, z, &locks) {
			assert_eq!(
				sim.cell_at(x, y, z),
				Some(BlockType::Dirt as u16),
				"({x},{y},{z}) is protected and was swept anyway"
			);
			refused += 1;
		} else {
			assert_eq!(sim.cell_at(x, y, z), Some(BlockType::Grass as u16));
			swept += 1;
		}
	}
	assert!(refused > 0, "the lock covered none of the sown band");
	assert!(swept > 0, "the lock covered ALL of it — nothing was proven");
}

/// The frontier feeds itself: run 1's writes are journaled edits, so run 2's candidates include
/// their neighbors — grass advances exactly one ring per run with no rule-side bookkeeping. The
/// noon registration only: its ray is vertical, and the flat harness world is one real worldgen
/// knows nothing about, so an angled ray leaving residency would be judged against terrain that
/// is not there.
#[test]
fn the_grass_frontier_sustains_itself() {
	let mut sim = flat_sim("wj-frontier");
	let y = FLAT_GROUND_TOP as i32;
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sow_dirt(&mut sim, y, 8);
	// The seed goes through the real edit path — journaled, so the
	// frontier can see it. `test_set_cell`'s store-only writes cannot be.
	// Mid-chunk: `sow_dirt` reaches past residency and those cells are
	// silently dropped, so the pad under audit is the resident 0..=8
	// quarter and both rings must fit inside it.
	let seed_cell = (4, y, 4);
	let grass = BlockType::Grass as u16;
	let prev = sim
		.store
		.set_cell(seed_cell.0, seed_cell.1, seed_cell.2, grass)
		.expect("seed cell resident");
	sim.apply_write(seed_cell, grass, prev, SERVER_ID, 0, true);
	for job in digblock_core::worldjob::grass::jobs() {
		if job.name().contains("noon") {
			sim.register_world_job(job);
		}
	}
	c.drain();

	// The pad is graded by ring: in-plane Chebyshev distance from the
	// seed, one ring per completed run.
	let check = |sim: &Sim, runs: i32| {
		let (mut green, mut brown) = (0, 0);
		for z in 0i32..=8 {
			for x in 0i32..=8 {
				let d = (x - seed_cell.0).abs().max((z - seed_cell.2).abs());
				let want = if d <= runs {
					BlockType::Grass
				} else {
					BlockType::Dirt
				};
				assert_eq!(
					sim.cell_at(x, y, z),
					Some(want as u16),
					"({x},{z}) is {d} rings out after {runs} runs"
				);
				if d <= runs { green += 1 } else { brown += 1 }
			}
		}
		assert!(green > 0 && brown > 0, "the pad proves nothing");
	};

	run_sweep(&mut sim, &mut c, 120);
	check(&sim, 1);

	// Later the next slot is owed; ring 2 is reachable only through
	// ring 1's journaled writes. Two days, not one: slot indices clamp
	// at zero, so the boot catch-up consumed slot 0 BEFORE its nominal
	// window, and one day past boot the index is still 0.
	sim.tod_offset_secs += 2.0 * f64::from(digblock_core::sun::DAY_SECS);
	run_sweep(&mut sim, &mut c, 120);
	check(&sim, 2);
}

/// The sweep clock survives a restart. A rolled job salts its draw with the slot index, so a
/// clock that began again at zero would re-offer one slot's draw on every boot — the same finds
/// handed back forever, and the sites that lost that roll losing it forever. Two sims over ONE
/// data directory: the second opens with its clock strictly ahead of where the first one stood.
#[test]
fn the_sweep_clock_does_not_restart_with_the_process() {
	let dir = temp_dir("wj-epoch");
	let open = |dir: &std::path::Path| {
		Sim::new_with_source(SEED, dir, 1, 2, flat_source(), FLAT_SPAWN).expect("open the world")
	};
	let before = open(&dir).sweep_secs();
	// Enough for any clock to move, far under the sweeps' own day.
	std::thread::sleep(std::time::Duration::from_millis(20));
	let after = open(&dir).sweep_secs();
	assert!(
		after > before,
		"a reopened world's sweep clock went back: {before} then {after}"
	);
}

/// Records the head-count it was offered and writes nothing — the only
/// way to ask what the sim handed the scheduler.
struct Spy {
	seen: std::sync::Arc<std::sync::atomic::AtomicU32>,
	y: i32,
}

impl WorldJob for Spy {
	fn name(&self) -> &'static str {
		"spy"
	}
	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 2,
			offset: 0.0,
		}
	}
	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Dirt],
			radius_chunks: 0,
			y: (self.y, self.y),
			candidates: Candidates::Band,
		}
	}
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: Seed, run: Run) -> Option<u16> {
		self.seen
			.store(run.players, std::sync::atomic::Ordering::Relaxed);
		None
	}
}

/// A sweep is handed the server's OWN roster, at its busiest over the
/// game day rather than the count standing there when it starts. The
/// regrow odds scale by that number and nothing else, so a constant here
/// would be a solo world regrowing at a full server's rate — or a full
/// server never regrowing — with every other test still green.
#[test]
fn a_sweep_reads_the_days_peak_head_count() {
	let mut sim = flat_sim("wj-presence");
	let y = FLAT_GROUND_TOP as i32;
	let mut c = connect(&mut sim, 1);
	let crowd = [2, 3];
	// Held, not dropped: a `TestClient` going out of scope closes its
	// channel, and the next broadcast reaps the client behind it.
	let _others: Vec<TestClient> = crowd.iter().map(|&n| connect(&mut sim, n)).collect();
	wait_live(&mut sim, 1);
	for &conn in &crowd {
		wait_live(&mut sim, conn);
	}
	sow_dirt(&mut sim, y, 2);
	let seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
	sim.register_world_job(Box::new(Spy {
		seen: seen.clone(),
		y,
	}));
	// They leave before the sweep: what it reads is the PEAK the day
	// held, not the one player still standing here.
	for &conn in &crowd {
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: conn }]);
	}
	c.drain();
	run_sweep(&mut sim, &mut c, 8);
	assert_eq!(
		seen.load(std::sync::atomic::Ordering::Relaxed) as usize,
		crowd.len() + 1,
		"the sweep read a head-count the roster never held"
	);
}

/// The one sweep whose write is CURRENCY, end to end on a real server: a
/// spent face standing where worldgen put a node comes back as the live
/// node and reaches a connected client as an ordinary edit. Registration
/// included — delete the roster's regrow entry and only this fails.
#[test]
fn a_spent_node_regrows_and_reaches_a_client() {
	use digblock_core::worldgen::nodes::{node_at, spent_block};
	use digblock_core::worldjob::regrow::chance_percent;

	let mut sim = flat_sim("wj-regrow");
	// The odds climb with the day's peak and then stop — derived, so a
	// retune of either keeps this test at the best odds the rule offers.
	let crowd = (1..=64)
		.find(|&n| chance_percent(n) == chance_percent(n + 1))
		.expect("the odds stop climbing somewhere");
	let mut c = connect(&mut sim, 1);
	// Held and drained below: a `TestClient` dropped here would close its
	// channel and be reaped off the roster the peak is read from.
	let mut others: Vec<TestClient> = (2..=u64::from(crowd))
		.map(|conn| connect(&mut sim, conn))
		.collect();
	for conn in 1..=u64::from(crowd) {
		wait_live(&mut sim, conn);
	}
	// Real node sites in the resident column — searched, because a
	// deposit is a position hash and a written-down coordinate stops
	// being one the moment anybody retunes the density.
	let sites: Vec<((i32, i32, i32), BlockType)> = (1..FLAT_GROUND_TOP as i32)
		.flat_map(|y| (0..16).flat_map(move |z| (0..16).map(move |x| (x, y, z))))
		.filter_map(|at| node_at(BlockType::Stone, at, SEED).map(|kind| (at, kind)))
		.collect();
	assert!(
		sites.len() > 4,
		"the search found {} sites — widen it",
		sites.len()
	);
	// Planted through the real edit path: a Frontier sweep's candidates
	// are the journal's edit record, which `test_set_cell` never reaches.
	for &(at, kind) in &sites {
		let face = spent_block(kind).expect("a node has a spent face") as u16;
		let prev = sim
			.store
			.set_cell(at.0, at.1, at.2, face)
			.expect("a resident site");
		sim.apply_write(at, face, prev, SERVER_ID, 0, true);
	}
	for job in digblock_core::worldjob::production() {
		sim.register_world_job(job);
	}
	c.drain();
	// A night at a time: the draw is a hash of the site and the slot, so
	// a night that returns nothing is a night rather than a failure.
	// Bounded, because "eventually" is not a test.
	let mut back = 0;
	for _ in 0..5 {
		sim.tod_offset_secs += f64::from(digblock_core::sun::DAY_SECS);
		let edits = run_sweep(&mut sim, &mut c, 240);
		for o in &mut others {
			o.drain();
			o.msgs.clear();
		}
		for &(at, kind) in &sites {
			if sim.cell_at(at.0, at.1, at.2) != Some(kind as u16) {
				continue;
			}
			// Every one of them came back TONIGHT — the loop stops on the
			// first night that returns anything — so every one of them is
			// owed an edit in this night's traffic.
			assert!(
				edits
					.iter()
					.any(|e| (e.x, e.y, e.z) == at && e.block == kind as u16),
				"{at:?} came back without reaching the client"
			);
			back += 1;
		}
		if back > 0 {
			break;
		}
	}
	assert!(
		back > 0,
		"not one of {} spent faces came back in five nights",
		sites.len()
	);
}
