use super::super::harness::*;
use super::*;
use digblock_core::block::BlockType;
use digblock_core::sun::DAY_SECS;
use digblock_core::worldjob::{CellSource, Schedule, Scope, WorldJob};

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

/// The archetype from the design note: dirt turns to grass, twice a day,
/// near whoever is online.
struct GrassSpread {
	radius_chunks: i32,
	y: i32,
}

impl WorldJob for GrassSpread {
	fn name(&self) -> &'static str {
		"grass-spread"
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
	sim.register_world_job(Box::new(GrassSpread {
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

/// A sweep is a batch, not a tick system: it must never spend a whole
/// tick's worth of the world in one go.
#[test]
fn a_sweep_is_sliced_across_ticks() {
	let mut sim = flat_sim("wj-sliced");
	let y = FLAT_GROUND_TOP as i32;
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sow_dirt(&mut sim, y, 1);
	sim.register_world_job(Box::new(GrassSpread {
		radius_chunks: 2,
		y,
	}));
	c.drain();

	let mut ticks_sweeping = 0;
	for _ in 0..20_000 {
		sim.tick_once(Vec::new());
		if sim.test_world_job_running() {
			ticks_sweeping += 1;
		} else if ticks_sweeping > 0 {
			break;
		}
	}
	assert!(
		ticks_sweeping > 1,
		"a 5x5-chunk sweep finished in one tick — it is not sliced"
	);
}

/// Nothing runs where nobody is: the world is infinite, so a sweep is
/// only ever defined relative to players. (That the slot stays OWED
/// rather than being consumed is pinned in `worldjob`'s own tests.)
#[test]
fn nothing_is_swept_with_the_server_empty() {
	let mut sim = flat_sim("wj-empty");
	sim.register_world_job(Box::new(GrassSpread {
		radius_chunks: 0,
		y: FLAT_GROUND_TOP as i32,
	}));
	for _ in 0..600 {
		sim.tick_once(Vec::new());
		assert!(!sim.test_world_job_running(), "swept with nobody online");
	}
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
	sim.register_world_job(Box::new(GrassSpread {
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

/// One slot, one sweep — however long the server stays up.
#[test]
fn a_slot_is_swept_once_however_long_the_server_runs() {
	let mut sim = flat_sim("wj-once");
	let y = FLAT_GROUND_TOP as i32;
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sow_dirt(&mut sim, y, 0);
	sim.register_world_job(Box::new(GrassSpread {
		radius_chunks: 0,
		y,
	}));
	c.drain();

	assert!(
		!run_sweep(&mut sim, &mut c, 8).is_empty(),
		"first slot swept nothing"
	);
	// Put the dirt back and run out the rest of the half-day.
	sow_dirt(&mut sim, y, 0);
	c.drain();
	let ticks = (f64::from(DAY_SECS) * 0.4 * f64::from(TICK_RATE)) as u32;
	for _ in 0..ticks {
		sim.tick_once(Vec::new());
	}
	c.drain();
	assert!(
		!c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::EditApplied { .. })),
		"the same slot swept twice"
	);
}
