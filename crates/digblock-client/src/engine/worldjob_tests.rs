use super::*;
use digblock_core::worldjob::{Candidates, CellSource, Schedule, Scope, WorldJob};

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
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: f64, _: u64) -> Option<u16> {
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
		fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: f64, _: u64) -> Option<u16> {
			Some(BlockType::Grass as u16)
		}
	}
	engine.register_world_job(Box::new(Wide));
	engine.step_world_jobs();
	assert!(engine.world_jobs.sweeping().is_some(), "nothing to abandon");
	engine.reset_world_transients(engine.player.pos, true);
	assert!(engine.world_jobs.sweeping().is_none());
}
