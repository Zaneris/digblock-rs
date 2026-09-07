//! The surface nodes through the engine's dig prediction. A node stands in a
//! cell that still holds its host, so the gates have to be asked of what the
//! player SEES: asked of the cell, a fresh siphon-holder met the host's Mining
//! wall on the first leystone the siphon had just shown them.

use super::*;
use digblock_core::unlock;
use digblock_core::worldgen::nodes;

/// A surface node in the loaded world with open air on its -z or +z side, and the engine
/// standing in that air looking at it. Searched, not written down: the sites are a property of
/// the placement hash and worldgen, and a coordinate would silently stop being one.
fn a_surface_node_in_view() -> (Engine, (i32, i32, i32), BlockType) {
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
	let (px, py, pz) = (p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
	for dy in -48..=96 {
		for dz in -64..=64 {
			for dx in -64..=64 {
				let at = (px + dx, py + dy, pz + dz);
				let Some(v) = engine.pipeline.cell_at(at.0, at.1, at.2) else {
					continue;
				};
				let host = BlockType::from_id(digblock_core::chunk::cell_id(v));
				let Some(node) = nodes::node_at(host, at, engine.seed) else {
					continue;
				};
				if !unlock::siphon_reveal(node) || engine.pipeline.cell_edited(at.0, at.1, at.2) {
					continue;
				}
				for (side, yaw) in [(-1, 0.0), (1, 180.0)] {
					let air = (at.0, at.1, at.2 + side);
					if engine.pipeline.block_at(air.0, air.1, air.2) != Some(BlockType::Air) {
						continue;
					}
					engine.camera.pos =
						Vec3::new(air.0 as f32 + 0.5, air.1 as f32 + 0.5, air.2 as f32 + 0.5);
					engine.set_look(yaw, 0.0);
					engine.update_target();
					if engine.target.map(|h| h.cell) == Some(at) {
						return (engine, at, node);
					}
				}
			}
		}
	}
	panic!("no surface node with open air beside it near spawn — widen the search");
}

/// The siphon is a surface node's whole gate, so the day it is made every surface source draws
/// at Mining 1 — including one standing in a host with a strength wall of its own. Before the
/// siphon the cell IS its host and its wall stands; after, the node is what is seen, judged and
/// dug, and the only wall left is the one on the spent face a draw leaves behind.
#[test]
fn a_fresh_siphon_holder_draws_a_surface_node_through_its_hosts_wall() {
	let (mut engine, cell, node) = a_surface_node_in_view();
	let host = nodes::host_block(node).expect("a node has a host");
	assert!(
		unlock::mine(host) > 1,
		"{host:?} has no wall, so {node:?} proves nothing here"
	);

	// No siphon yet: the cell is its host in every respect, wall included.
	assert_eq!(
		engine.seen_block(cell),
		Some(host),
		"unrevealed, it is the host"
	);
	assert!(
		!engine.diggable(&engine.target.unwrap()),
		"Mining 1 cannot break {host:?}"
	);

	// The siphon in hand: the node is what is seen, and no strength wall stands between.
	engine.inventory.add(BlockType::EssenceSiphon, 1);
	engine.hotbar.select_digit(9);
	engine
		.hotbar
		.set_active_tool(Some(BlockType::EssenceSiphon));
	engine.update_target();
	assert_eq!(
		engine.seen_block(cell),
		Some(node),
		"revealed, it is the node"
	);
	assert!(
		engine.diggable(&engine.target.unwrap()),
		"a fresh siphon-holder cannot draw {node:?} through {host:?}'s wall"
	);
	engine.input.dig_held = true;
	engine.step_dig();
	assert_eq!(
		engine.dig.as_ref().map(|d| d.cell),
		Some(cell),
		"the draw opens at Mining 1"
	);
	assert!(
		!engine
			.session
			.feed
			.iter()
			.any(|(m, ..)| matches!(m, Msg::DigRefused(_))),
		"and nothing was refused: {:?}",
		engine.session.feed
	);
}
