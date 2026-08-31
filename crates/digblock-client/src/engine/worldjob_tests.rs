use super::*;
use digblock_core::worldgen::Seed;
use digblock_core::worldjob::{Candidates, CellSource, Run, Schedule, Scope, WorldJob};

/// Dirt to grass, once a day, in the player's own chunk — the same rule
/// the server tests drive, so the two arms are compared on one job.
struct GrassSpread(i32);

impl WorldJob for GrassSpread {
	fn name(&self) -> &'static str {
		"grass-spread"
	}
	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 1,
			offset: 0.0,
		}
	}
	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Dirt],
			radius_chunks: 0,
			y: (self.0, self.0),
			candidates: Candidates::Band,
		}
	}
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: Seed, _: Run) -> Option<u16> {
		Some(BlockType::Grass as u16)
	}
}

/// A local world with its terrain settled and one Dirt cell placed in
/// the player's own chunk.
fn local_world() -> (Engine, (i32, i32, i32)) {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = engine.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
	let p = engine.player.pos;
	let cell = (
		p.x.floor() as i32,
		p.y.floor() as i32 + 20,
		p.z.floor() as i32,
	);
	engine
		.pipeline
		.set_block(cell.0, cell.1, cell.2, BlockType::Dirt);
	(engine, cell)
}

/// Step until `cell` reads as `want`, or fail. The production roster
/// registered by `Engine::new` sweeps first, so a test job's turn comes
/// after those drain — one call was enough only while the roster was
/// empty.
fn step_until(engine: &mut Engine, cell: (i32, i32, i32), want: BlockType) {
	for _ in 0..20_000 {
		engine.step_world_jobs();
		if engine.pipeline.cell_at(cell.0, cell.1, cell.2) == Some(want as u16) {
			return;
		}
	}
	panic!("{cell:?} never became {want:?}");
}

/// Offline parity: a local world runs the same core scheduler the server
/// does, so a rule cannot mean two different things.
#[test]
fn a_local_world_sweeps_on_the_same_clock() {
	let (mut engine, cell) = local_world();
	engine.register_world_job(Box::new(GrassSpread(cell.1)));
	step_until(&mut engine, cell, BlockType::Grass);
}

/// The scheduler reads the unwrapped world clock: a slot due past the
/// shader clock's wrap must still come due. Feeding `time_secs` here
/// starves every job after its first session run.
#[test]
fn the_world_clock_outlives_the_shader_clock() {
	let (mut engine, cell) = local_world();
	engine.register_world_job(Box::new(GrassSpread(cell.1)));
	step_until(&mut engine, cell, BlockType::Grass);

	engine
		.pipeline
		.set_block(cell.0, cell.1, cell.2, BlockType::Dirt);
	// A full game day is far past the shader wrap; the next slot is owed.
	engine.world_secs += f64::from(digblock_core::sun::DAY_SECS);
	step_until(&mut engine, cell, BlockType::Grass);
}

/// The production rule, end to end through the real pipeline: a placed
/// grass seed spreads onto placed dirt beside it. This is also the guard
/// against `PipelineWorld` silently inheriting `edited_cells`'s empty
/// default — with no overlay enumeration the frontier would offer
/// nothing and the pocket would stay dirt forever.
#[test]
fn offline_grass_spreads_from_a_placed_seed() {
	let (mut engine, cell) = local_world();
	engine
		.pipeline
		.set_block(cell.0 + 1, cell.1, cell.2, BlockType::Grass);
	// Twenty blocks above the spawn surface, the noon registration's
	// vertical ray cannot be shaded; the angled runs may or may not be,
	// so the drain waits for whichever daylight run says yes.
	step_until(&mut engine, cell, BlockType::Grass);
}

/// A sweep names columns of the world it started in. Leaving abandons it
/// rather than resuming into terrain that is no longer there.
#[test]
fn leaving_a_world_abandons_the_sweep_in_flight() {
	let (mut engine, _) = local_world();
	// Wide enough that one slice cannot finish it.
	struct Wide;
	impl WorldJob for Wide {
		fn name(&self) -> &'static str {
			"wide"
		}
		fn schedule(&self) -> Schedule {
			Schedule {
				per_day: 1,
				offset: 0.0,
			}
		}
		fn scope(&self) -> Scope {
			Scope {
				blocks: &[BlockType::Dirt],
				radius_chunks: 3,
				y: (0, 200),
				candidates: Candidates::Band,
			}
		}
		fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: Seed, _: Run) -> Option<u16> {
			Some(BlockType::Grass as u16)
		}
	}
	engine.register_world_job(Box::new(Wide));
	engine.step_world_jobs();
	assert!(engine.world_jobs.sweeping().is_some(), "nothing to abandon");
	engine.reset_world_transients(engine.player.pos, true);
	assert!(engine.world_jobs.sweeping().is_none());
}

/// The offline arm of the place consequence: capping staged turf with
/// stone turns it to dirt through the real `apply_place` path — with no
/// server in the loop, the engine's own arm is the only thing that can.
#[test]
fn an_offline_place_smothers_the_grass_beneath() {
	let (mut engine, cell) = local_world();
	engine
		.pipeline
		.set_block(cell.0, cell.1, cell.2, BlockType::Grass);
	engine.inventory.add(BlockType::Stone, 1);
	digblock_core::inv_grid::reconcile_layout(
		&engine.inventory,
		&mut engine.hotbar,
		&mut engine.inv_grid,
	);
	engine.hotbar.select_digit(1);
	// Straight down onto the turf's top face, so the place lands above it.
	engine.camera.pos = glam::Vec3::new(
		cell.0 as f32 + 0.5,
		cell.1 as f32 + 3.5,
		cell.2 as f32 + 0.5,
	);
	engine.set_look(engine.look.yaw_deg, -89.0);
	engine.update_target();
	assert_eq!(
		engine.target.map(|h| (h.cell, h.normal)),
		Some((cell, (0, 1, 0))),
		"the aim must land on top of the turf"
	);
	engine.input.click_place = true;
	engine.apply_place();
	assert_eq!(
		engine.pipeline.block_at(cell.0, cell.1 + 1, cell.2),
		Some(BlockType::Stone)
	);
	assert_eq!(
		engine.pipeline.block_at(cell.0, cell.1, cell.2),
		Some(BlockType::Dirt),
		"the cover smothers"
	);
}

/// The head-count a local world feeds the scheduler reaches the rule. A solo world is deliberately
/// ONE — `Run::players` is a peak, and the offline arm has exactly one player to peak at — but
/// nothing said so: the constant could be swapped, or the `observe` call dropped entirely, and the
/// sweep would still run. This is the client host's half of the feed; core pins the scheduler's.
#[test]
fn a_local_world_sweeps_with_one_player() {
	use std::sync::atomic::{AtomicU32, Ordering};
	static SEEN: AtomicU32 = AtomicU32::new(u32::MAX);
	struct CountsHeads(i32);
	impl WorldJob for CountsHeads {
		fn name(&self) -> &'static str {
			"counts-heads"
		}
		fn schedule(&self) -> Schedule {
			Schedule {
				per_day: 1,
				offset: 0.0,
			}
		}
		fn scope(&self) -> Scope {
			Scope {
				blocks: &[BlockType::Dirt],
				radius_chunks: 0,
				y: (self.0, self.0),
				candidates: Candidates::Band,
			}
		}
		fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: Seed, run: Run) -> Option<u16> {
			SEEN.store(run.players, Ordering::Relaxed);
			Some(BlockType::Grass as u16)
		}
	}
	let (mut engine, cell) = local_world();
	engine.register_world_job(Box::new(CountsHeads(cell.1)));
	step_until(&mut engine, cell, BlockType::Grass);
	assert_eq!(
		SEEN.load(Ordering::Relaxed),
		1,
		"a solo world must sweep at a head-count of one"
	);
}

/// The scheduler's clock is the WORLD's age, not the session's. `worldjob::regrow` salts its roll
/// with the run's slot, so a clock that restarted at zero on every entry would replay the same
/// slot and draw the same answer: a spent node that lost its roll would lose it again for ever.
/// Entering a save winds the clock to the age its own stamps describe — both already persisted,
/// so nothing new is stored.
#[test]
fn entering_a_save_resumes_the_worlds_age() {
	let (mut engine, _) = local_world();
	let day = f64::from(digblock_core::sun::DAY_SECS);
	let age_ms = (day * 3.5 * 1000.0) as u64;
	let created = 1_700_000_000_000u64;
	let save = digblock_persist::WorldSave {
		meta: digblock_persist::WorldMeta {
			id: "w".into(),
			name: "w".into(),
			seed: engine.seed,
			created_ms: created,
			last_played_ms: created + age_ms,
			compatible: true,
		},
		rev: digblock_persist::SAVE_REV,
		edits: Vec::new(),
		inventory: Vec::new(),
		hotbar: Vec::new(),
		grid: Vec::new(),
		skills: Vec::new(),
		player: None,
	};
	engine.enter_local_world(save);
	// The age its stamps describe, to the second — and past the first
	// day boundary, which is the whole point: a slot nobody has run.
	assert_eq!(engine.world_secs, age_ms as f64 / 1000.0);
	assert!(
		engine.world_secs > day,
		"a save older than a day must not enter on day zero"
	);
}
