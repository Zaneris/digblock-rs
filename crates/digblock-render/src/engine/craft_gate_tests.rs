
use super::*;
use digblock_core::skills::{Skill, total_xp_for};
use digblock_core::unlock::Kind;

/// A torch — craft-only, Alchemy 8 — two blocks ahead.
fn torch_ahead() -> (Engine, (i32, i32, i32)) {
	block_ahead(BlockType::Torch)
}

/// A generated world with one block floating in open air two blocks
/// ahead of the camera. Open sky well above spawn, so a worldgen
/// retune can't move terrain into the shot.
pub(super) fn block_ahead(block: BlockType) -> (Engine, (i32, i32, i32)) {
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
	assert_eq!(
		engine.pipeline.block_at(cell.0, cell.1, cell.2),
		Some(BlockType::Air),
		"the shot must start empty"
	);
	engine
		.pipeline
		.set_block(cell.0, cell.1, cell.2, block)
		.expect("chunk ready");
	// Eye at the cell's mid-height, dead on the post's axis.
	engine.camera.pos = Vec3::new(
		cell.0 as f32 + 0.5,
		cell.1 as f32 + 0.5,
		cell.2 as f32 - 2.5,
	);
	engine.camera.yaw_deg = 0.0; // +z
	engine.camera.pitch_deg = 0.0;
	(engine, cell)
}

/// The craft gate, client half: a torch is somebody's crafted item, so
/// a player who cannot make one cannot pick one up either — the hold
/// never opens, the outline never draws, and holding the button says
/// why exactly once. The gate is the recipe's own level, not a level
/// near it.
#[test]
fn a_craft_only_block_refuses_the_dig_until_its_recipe_opens() {
	let (mut engine, cell) = torch_ahead();
	engine.update_target();
	assert_eq!(
		engine.target.map(|h| h.cell),
		Some(cell),
		"aimed at the torch"
	);
	assert!(
		!engine.diggable(&engine.target.unwrap()),
		"Alchemy 1 cannot make a torch, so it cannot take one"
	);
	assert_eq!(
		engine.dig_progress().map(|t| t.cell),
		None,
		"and it does not even outline"
	);

	// Holding the button explains itself, once per aim.
	engine.dig_held = true;
	engine.step_dig();
	engine.step_dig();
	assert!(engine.dig.is_none(), "no hold opened");
	let spoken = |e: &Engine| {
		e.feed
			.iter()
			.filter(|(t, ..)| t.as_str() == Kind::Recipe.message())
			.count()
	};
	assert_eq!(spoken(&engine), 1, "latched: {:?}", engine.feed);

	// Aiming away and back is what clears the latch in play.
	engine.dig_held = false;
	engine.refused_cell = None;

	// Read out of the unlock table, never written down: every
	// threshold in the game lives in `unlock`, and this pair had to be
	// hand-edited the last time the torch moved.
	let need = digblock_core::unlock::pickup(BlockType::Torch);
	engine
		.skills
		.set_total(Skill::Alchemy, total_xp_for(need - 1));
	engine.update_target();
	assert!(
		!engine.diggable(&engine.target.unwrap()),
		"one level short of the recipe is still short"
	);

	engine.skills.set_total(Skill::Alchemy, total_xp_for(need));
	engine.update_target();
	assert!(
		engine.diggable(&engine.target.unwrap()),
		"the Alchemy level that makes torches takes them too"
	);
	assert_eq!(
		engine.dig_progress().map(|t| t.cell),
		Some(cell),
		"and the outline is back"
	);
	engine.dig_held = true;
	engine.step_dig();
	assert_eq!(
		engine.dig.as_ref().map(|d| d.cell),
		Some(cell),
		"the hold opens"
	);
}
