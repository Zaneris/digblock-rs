
use super::*;
use crate::chunk::make_cell;
use crate::worldgen::SurfaceKind;

const SEED: f64 = 42.0;

fn maxed() -> Levels {
	Levels {
		mining: crate::skills::MAX_LEVEL,
		alchemy: crate::skills::MAX_LEVEL,
	}
}

/// Find one manifest cell of each surface kind by walking the world —
/// walked rather than named, so a retune of a rate or a band moves the
/// tests with it.
fn find_heartwood(seed: f64) -> (i32, i32, i32) {
	for cz in -64..64 {
		for cx in -64..64 {
			for x in cx * 8..cx * 8 + 8 {
				for z in cz * 8..cz * 8 + 8 {
					let Some(t) = crate::decoration::trunk_at(x, z, seed) else {
						continue;
					};
					for y in t.y0..=t.y1 {
						if heartwood_at(x, y, z, seed) {
							return (x, y, z);
						}
					}
				}
			}
		}
	}
	panic!("no heartwood in the sampled world");
}

fn find_springstone(seed: f64) -> (i32, i32, i32) {
	for x in (-2048..2048).step_by(3) {
		for z in (-2048..2048).step_by(3) {
			let s = crate::worldgen::column_shape(x, z, seed);
			if !matches!(s.surface, SurfaceKind::Lake | SurfaceKind::River) || s.water <= s.height {
				continue;
			}
			if springstone_at(x, s.height, z, seed) {
				return (x, s.height, z);
			}
		}
	}
	panic!("no springstone in the sampled world");
}

/// Purity: same inputs, same answer — the property every side of the
/// wire depends on.
#[test]
fn surface_placement_is_pure() {
	let hw = find_heartwood(SEED);
	let sp = find_springstone(SEED);
	for _ in 0..3 {
		assert!(heartwood_at(hw.0, hw.1, hw.2, SEED));
		assert!(springstone_at(sp.0, sp.1, sp.2, SEED));
	}
	// A different seed is a different world: the exact cells found in
	// one seed are astronomically unlikely to all recur in another.
	let moved = heartwood_at(hw.0, hw.1, hw.2, 7.5) && springstone_at(sp.0, sp.1, sp.2, 7.5);
	assert!(!moved, "the seed reaches the surface nodes");
}

/// The anti-dupe, for every node kind: a node is spent by the write
/// that took it, and cannot be regrown by putting the host back.
#[test]
fn no_node_manifests_in_an_edited_cell() {
	let cases = [
		(BlockType::Wood, find_heartwood(SEED), BlockType::Heartwood),
		(
			BlockType::Sand,
			find_springstone(SEED),
			BlockType::Springstone,
		),
	];
	for (host, at, node) in cases {
		let cell = host as u16;
		assert_eq!(
			revealed(cell, at, SEED, maxed(), false),
			Some(node),
			"pristine {node:?}"
		);
		// Somebody put the host back where the node was.
		assert_eq!(revealed(cell, at, SEED, maxed(), true), None, "{node:?}");
		// The spent face itself never re-reads as a node.
		let spent = spent_block(node).expect("a kind") as u16;
		assert_eq!(revealed(spent, at, SEED, maxed(), false), None);
		// Nor does anything else standing in that coordinate.
		for b in [BlockType::Air, BlockType::Rubble, node] {
			assert_eq!(revealed(b as u16, at, SEED, maxed(), false), None, "{b:?}");
		}
		// A shaped cell in the same coordinate is not the host either.
		assert_eq!(
			revealed(make_cell(BlockType::StoneSlab, 1), at, SEED, maxed(), false),
			None
		);
	}
}

/// You draw what you can see: below its Alchemy reveal a surface node
/// is its host to that player, and the reveal is the ALCHEMY level —
/// Mining opens nothing on the surface.
#[test]
fn surface_nodes_reveal_on_alchemy_alone() {
	let cases = [
		(BlockType::Wood, find_heartwood(SEED), BlockType::Heartwood),
		(
			BlockType::Sand,
			find_springstone(SEED),
			BlockType::Springstone,
		),
	];
	for (host, at, node) in cases {
		let need = crate::unlock::alchemy_reveal(node).expect("a surface node");
		let cell = host as u16;
		let at_lvl = |mining, alchemy| revealed(cell, at, SEED, Levels { mining, alchemy }, false);
		if need > 1 {
			assert_eq!(at_lvl(crate::skills::MAX_LEVEL, need - 1), None, "{node:?}");
		}
		assert_eq!(at_lvl(1, need), Some(node), "{node:?}");
		assert_eq!(
			crate::unlock::reveal(node),
			None,
			"{node:?} must not sit in the Mining reveal table too"
		);
	}
}

/// The dig-mode contract, per kind: break below the reveal, refuse
/// above it bare-handed, draw with the siphon — and the draw is timed
/// as the node, not the host.
#[test]
fn a_revealed_node_needs_the_siphon() {
	let (host, at, node) = (BlockType::Wood, find_heartwood(SEED), BlockType::Heartwood);
	let cell = host as u16;
	let fresh = Levels {
		mining: 1,
		alchemy: 1,
	};
	// Heartwood reveals at the first level (the economy starts here),
	// so "below the reveal" is exercised by the OTHER kinds' tests;
	// here even a fresh player sees it.
	let need = crate::unlock::alchemy_reveal(node).expect("a surface node");
	assert_eq!(need, 1, "heartwood is the day-one node");
	assert_eq!(
		dig_mode(cell, at, SEED, fresh, false, BlockType::Air),
		DigMode::NeedsSiphon
	);
	assert_eq!(
		dig_mode(cell, at, SEED, fresh, false, BlockType::EssenceSiphon),
		DigMode::Extract(node)
	);
	assert_eq!(
		dig_mode(cell, at, SEED, fresh, true, BlockType::EssenceSiphon),
		DigMode::Break,
		"an edited cell is just wood"
	);
	assert_eq!(dig_block(DigMode::Extract(node), host), node);
	assert_eq!(dig_block(DigMode::Break, host), host);
}

/// Every node has a spent face, nothing else does, and the roster
/// matches the block layer's predicate — the pin that keeps
/// `is_slottable` honest about what can never be an item.
#[test]
fn every_node_has_a_spent_face_and_nothing_else_does() {
	let mut spent = Vec::new();
	for &node in &NODES {
		let face = spent_block(node).unwrap_or_else(|| panic!("{node:?} taps into nothing"));
		assert_ne!(face, node, "{node:?} must not tap out into itself");
		assert!(
			face.dig_drop() == Some(face),
			"{face:?} has to drop ITSELF — it is the only thing a node \
				 ever puts in a player's hands"
		);
		assert_eq!(face.props().light, 0, "{face:?} still casts");
		assert!(!face.props().emissive, "{face:?} still glows");
		assert!(face.is_slottable(), "{face:?} cannot be picked up");
		assert!(!face.is_node_block(), "{face:?} reads as a live node");
		spent.push(face);
	}
	let mut ids: Vec<u16> = spent.iter().map(|&b| b as u16).collect();
	ids.sort_unstable();
	ids.dedup();
	assert_eq!(ids.len(), spent.len(), "two kinds share a face: {spent:?}");
	for &b in &BlockType::ALL {
		assert_eq!(
			spent_block(b).is_some(),
			NODES.contains(&b),
			"{b:?} disagrees with the node roster"
		);
		assert_eq!(
			b.is_node_block(),
			NODES.contains(&b),
			"{b:?}: node_flecks and the roster disagree"
		);
		if NODES.contains(&b) {
			assert!(!b.is_slottable(), "{b:?} can still be held");
			assert!(!b.is_tool(), "{b:?} must be excluded as a node, not a tool");
			// Exactly one reveal table claims each node.
			assert_ne!(
				crate::unlock::reveal(b).is_some(),
				crate::unlock::alchemy_reveal(b).is_some(),
				"{b:?} must answer to exactly one reveal skill"
			);
		}
	}
}

/// A landmark never rolls for its heart — the rare giant is always
/// worth the detour — while ordinary trees roll, and most decline.
///
/// The heart is the same SIZE either way, and that half is asserted
/// too: a landmark's four trunk columns each carry [`HEART_CELLS`] and
/// nothing above them. It used to be the whole trunk, which paid more
/// Alchemy XP off one tree than the first dozen levels cost.
#[test]
fn landmarks_always_carry_a_heart_and_most_trees_do_not() {
	let mut landmark_checked = false;
	'l: for cz in -24..24 {
		for cx in -24..24 {
			for x in cx * 64..cx * 64 + 64 {
				for z in cz * 64..cz * 64 + 64 {
					let Some(t) = crate::decoration::trunk_at(x, z, SEED) else {
						continue;
					};
					if !t.landmark {
						continue;
					}
					for y in t.y0..=t.y1 {
						let heart = (t.y0 + 1..=t.y0 + HEART_CELLS).contains(&y);
						assert_eq!(
							heartwood_at(x, y, z, SEED),
							heart,
							"landmark trunk at {x},{y},{z}"
						);
					}
					landmark_checked = true;
					break 'l;
				}
			}
		}
	}
	assert!(landmark_checked, "sample contains a landmark");
	// Ordinary trees: some carry a heart, most do not.
	let (mut with, mut without) = (0u32, 0u32);
	for cz in -48..48 {
		for cx in -48..48 {
			for x in cx * 8..cx * 8 + 8 {
				for z in cz * 8..cz * 8 + 8 {
					let Some(t) = crate::decoration::trunk_at(x, z, SEED) else {
						continue;
					};
					if t.landmark {
						continue;
					}
					if heartwood_at(x, t.y0 + 1, z, SEED) {
						with += 1;
					} else {
						without += 1;
					}
				}
			}
		}
	}
	assert!(with > 0, "some trees carry a heart");
	assert!(without > with, "most trees are plain wood");
}

/// A spring stands only in the bed of standing fresh water, and only
/// in the bed cell itself.
#[test]
fn springs_stand_only_in_fresh_beds() {
	let (x, y, z) = find_springstone(SEED);
	let s = crate::worldgen::column_shape(x, z, SEED);
	assert!(matches!(s.surface, SurfaceKind::Lake | SurfaceKind::River));
	assert!(s.water > s.height, "under standing water");
	assert!(!springstone_at(x, y - 1, z, SEED), "not below the bed");
	assert!(!springstone_at(x, y + 1, z, SEED), "not in the water");
	// And never in the ocean: sweep for ocean columns and assert the
	// rate never fires there.
	let mut ocean = 0u32;
	for ox in (-2048..2048).step_by(7) {
		for oz in (-2048..2048).step_by(7) {
			let s = crate::worldgen::column_shape(ox, oz, SEED);
			if s.surface == SurfaceKind::Ocean {
				ocean += 1;
				assert!(!springstone_at(ox, s.height, oz, SEED), "at {ox},{oz}");
			}
		}
	}
	assert!(ocean > 0, "sample contains ocean");
}
