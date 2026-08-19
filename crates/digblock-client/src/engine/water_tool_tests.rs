use super::*;

/// A generated world with the camera parked in a corridor: a water
/// source two blocks ahead, then a gap, then a stone wall. Aiming at
/// the source and at the wall is the same look — which ray is cast
/// decides which one the crosshair finds.
fn corridor() -> (Engine, (i32, i32, i32), (i32, i32, i32)) {
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
	// Open sky well above spawn: nothing here depends on terrain, so a
	// worldgen retune can't quietly move the corridor into rock.
	let p = engine.player.pos;
	let (x, y, z) = (
		p.x.floor() as i32,
		p.y.floor() as i32 + 20,
		p.z.floor() as i32,
	);
	let (source, wall) = ((x, y, z), (x, y, z + 2));
	for c in [source, wall, (x, y, z + 1)] {
		assert_eq!(
			engine.pipeline.block_at(c.0, c.1, c.2),
			Some(BlockType::Air),
			"the corridor must start empty at {c:?}"
		);
	}
	engine
		.pipeline
		.set_block(source.0, source.1, source.2, BlockType::Water)
		.expect("chunk ready");
	engine
		.pipeline
		.set_block(wall.0, wall.1, wall.2, BlockType::Stone)
		.expect("chunk ready");
	engine.camera.pos = Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 - 2.5);
	engine.camera.yaw_deg = 0.0; // +z
	engine.camera.pitch_deg = 0.0;
	// The rig owns its bucket like any player now — crafted rather
	// than assumed — and reconciling is what equips a first vessel.
	engine.inventory.add(BlockType::Bucket, 1);
	digblock_core::inv_grid::reconcile_layout(
		&engine.inventory,
		&mut engine.hotbar,
		&mut engine.inv_grid,
	);
	engine
		.hotbar
		.select_digit(digblock_core::hotbar::HOTBAR_SLOTS as u8);
	assert!(engine.water_tool_selected());
	(engine, source, wall)
}

/// The charge decides which ray is cast, and so what the crosshair
/// is even pointing at: an empty bucket sees the source it can scoop,
/// a charged one sees straight through it to the block it can pour
/// against. Both outline; neither ever draws the dig crack.
#[test]
fn the_charge_switches_the_ray_and_the_outline() {
	let (mut engine, source, wall) = corridor();

	engine.update_target();
	assert!(engine.scoop_ready());
	assert_eq!(engine.target.map(|h| h.cell), Some(source));
	assert!(
		engine.diggable(&engine.target.unwrap()),
		"a scoopable source"
	);
	assert_eq!(
		engine.dig_progress().map(|t| (t.cell, t.progress)),
		Some((source, None)),
		"outline, no cracks"
	);

	engine.inventory.add(BlockType::Water, 1);
	engine.update_target();
	assert!(!engine.scoop_ready());
	assert_eq!(
		engine.target.map(|h| h.cell),
		Some(wall),
		"a charged bucket picks with the ordinary ray"
	);
	assert!(
		!engine.diggable(&engine.target.unwrap()),
		"and left-click has nothing to do"
	);
	assert_eq!(
		engine.dig_progress().map(|t| (t.cell, t.progress)),
		Some((wall, None)),
		"the wall still outlines — that is where the water goes"
	);
}

/// Right-click pours, and only when there is something to pour. The
/// charge is spent by the ordinary stock debit, which is what makes
/// the pour roll back with the edit if a server refuses it.
#[test]
fn right_click_pours_the_charge_and_only_the_charge() {
	let (mut engine, _source, wall) = corridor();
	let target = (wall.0, wall.1, wall.2 - 1);

	// Empty: the click is consumed and nothing happens.
	engine.update_target();
	engine.input.click_place = true;
	engine.apply_place();
	assert_eq!(
		engine.pipeline.block_at(target.0, target.1, target.2),
		Some(BlockType::Air),
		"an empty bucket poured"
	);

	engine.inventory.add(BlockType::Water, 1);
	engine.update_target();
	engine.input.click_place = true;
	engine.apply_place();
	assert_eq!(
		engine.pipeline.block_at(target.0, target.1, target.2),
		Some(BlockType::Water),
		"the charged bucket poured against the wall"
	);
	assert!(!engine.inventory.bucket_full(), "and emptied itself");
}
