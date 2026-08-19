use super::*;
use digblock_core::worldjob::{CellSource, Schedule, Scope, WorldJob};

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

/// Offline parity: a local world runs the same core scheduler the server
/// does, so a rule cannot mean two different things.
#[test]
fn a_local_world_sweeps_on_the_same_clock() {
	let (mut engine, cell) = local_world();
	engine.register_world_job(Box::new(GrassSpread(cell.1)));
	engine.step_world_jobs();
	assert_eq!(
		engine.pipeline.cell_at(cell.0, cell.1, cell.2),
		Some(BlockType::Grass as u16),
		"the offline arm did not sweep"
	);
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
