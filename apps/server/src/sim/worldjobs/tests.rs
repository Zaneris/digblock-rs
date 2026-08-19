use super::super::harness::*;
use super::*;
use digblock_core::block::BlockType;
use digblock_core::worldjob::{Candidates, CellSource, Schedule, Scope, WorldJob};

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
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: f64, _: u64) -> Option<u16> {
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
	// One above the ground: the lock is the standing capsule over the
	// spawn column, so the floor itself is outside it.
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

	let lock = sim.spawn_lock().expect("spawn is locked");
	let (mut refused, mut swept) = (0, 0);
	for &(x, _, z) in &sown {
		if !(0..16).contains(&x) || !(0..16).contains(&z) {
			continue;
		}
		if digblock_core::protect::immutable(x, y, z, Some(&lock)) {
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

/// The frontier feeds itself: run 1's writes are journaled edits, so run
/// 2's candidates include their neighbors — grass advances exactly one
/// ring per run with no rule-side bookkeeping. The noon registration
/// only: its ray is vertical, and the flat harness world is one real
/// worldgen knows nothing about, so an angled ray leaving residency
/// would be judged against terrain that is not there.
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
