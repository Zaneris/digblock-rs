use super::*;
use crate::chunk::make_cell;
use crate::worldgen::Seed;
use crate::worldgen::SurfaceKind;

const SEED: Seed = Seed::current(42.0);

fn maxed() -> Levels {
	Levels {
		mining: crate::skills::MAX_LEVEL,
		alchemy: crate::skills::MAX_LEVEL,
	}
}

/// Find one manifest cell of each surface kind by walking the world —
/// walked rather than named, so a retune of a rate or a band moves the
/// tests with it.
fn find_heartwood(seed: Seed) -> (i32, i32, i32) {
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

fn find_springstone(seed: Seed) -> (i32, i32, i32) {
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
	let moved = heartwood_at(hw.0, hw.1, hw.2, Seed::current(7.5))
		&& springstone_at(sp.0, sp.1, sp.2, Seed::current(7.5));
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
		// Nor does anything else standing in that coordinate — except the
		// live face itself, stored: that is the regrown node.
		for b in [BlockType::Air, BlockType::Rubble] {
			assert_eq!(revealed(b as u16, at, SEED, maxed(), false), None, "{b:?}");
		}
		assert_eq!(revealed(node as u16, at, SEED, maxed(), true), Some(node));
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

/// A landmark never rolls for its heart — the rare giant is always worth the detour — while
/// ordinary trees roll, and most decline. The heart is the same SIZE either way, and that half is
/// asserted too: a landmark's four trunk columns each carry [`HEART_CELLS`] and nothing above
/// them, not the whole trunk, which paid more Alchemy XP off one tree than the first dozen levels
/// cost.
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

/// A spring stands under standing water, in the bed SURFACE cell and
/// nowhere else in its column — which is what puts the find under water
/// rather than in the sand beside it. Which waters carry one at all is
/// the shoreline test's question, below.
#[test]
fn a_spring_stands_in_the_bed_cell_and_nowhere_else_in_its_column() {
	let (x, y, z) = find_springstone(SEED);
	let s = crate::worldgen::column_shape(x, z, SEED);
	assert!(s.water > s.height, "under standing water");
	assert_eq!(y, s.height, "the bed surface cell");
	for probe in 0..=s.water + 1 {
		assert_eq!(
			springstone_at(x, probe, z, SEED),
			probe == y,
			"({x},{probe},{z})"
		);
	}
}

/// The ocean carries springs only in its shoreline shallows, and more
/// sparsely than the fresh beds do: sweep the world, sort every bed
/// column by kind, and compare the rates that fall out.
#[test]
fn ocean_springs_keep_to_the_shallows_and_are_rarer() {
	let (mut fresh, mut fresh_hit) = (0u32, 0u32);
	let (mut shore, mut shore_hit) = (0u32, 0u32);
	let mut deep = 0u32;
	for ox in (-2048..2048).step_by(5) {
		for oz in (-2048..2048).step_by(5) {
			let s = crate::worldgen::column_shape(ox, oz, SEED);
			if s.water <= s.height {
				continue;
			}
			let hit = springstone_at(ox, s.height, oz, SEED);
			match s.surface {
				SurfaceKind::Lake | SurfaceKind::River => {
					fresh += 1;
					fresh_hit += u32::from(hit);
				}
				SurfaceKind::Ocean if s.water - s.height <= SHORE_DEPTH => {
					shore += 1;
					shore_hit += u32::from(hit);
				}
				SurfaceKind::Ocean => {
					deep += 1;
					assert!(!hit, "a spring on the open floor at {ox},{oz}");
				}
				_ => assert!(!hit, "a spring under a puddle at {ox},{oz}"),
			}
		}
	}
	assert!(
		fresh > 0 && shore > 0 && deep > 0,
		"sample covers every bed kind"
	);
	assert!(shore_hit > 0, "the shoreline carries springs");
	// Rates, in the tables' own 256ths, each within a few of its constant.
	let per_256 = |hit: u32, n: u32| u64::from(hit) * 256 / u64::from(n);
	assert!(
		per_256(fresh_hit, fresh).abs_diff(SPRING_IN_256) <= 3,
		"{fresh_hit}/{fresh}"
	);
	assert!(
		per_256(shore_hit, shore).abs_diff(SPRING_SHORE_IN_256) <= 2,
		"{shore_hit}/{shore}"
	);
}

/// A live face stored in the cell outright — a regrown node — is found
/// by everyone: ahead of the reveal table and ahead of the edit bit,
/// since the mesh already shows it to every client.
#[test]
fn a_stored_live_face_is_revealed_to_anyone() {
	for &node in &NODES {
		let cell = make_cell(node, 0);
		for edited in [false, true] {
			assert_eq!(
				revealed(cell, (0, 0, 0), SEED, Levels::default(), edited),
				Some(node)
			);
		}
		assert_eq!(
			dig_mode(
				cell,
				(0, 0, 0),
				SEED,
				Levels::default(),
				true,
				BlockType::EssenceSiphon
			),
			DigMode::Extract(node)
		);
		assert_eq!(
			dig_mode(
				cell,
				(0, 0, 0),
				SEED,
				Levels::default(),
				true,
				BlockType::Air
			),
			DigMode::NeedsSiphon
		);
	}
}

/// `node_at` routes on the HOST, and the host is whatever the world really holds. Walked over real
/// [`crate::worldgen::build_column`] output rather than planted cells, because planting the host
/// the test already chose cannot see this: the streams overlap the terrain, so a trunk column's
/// heart span answers "heartwood" where the tree grew LEAVES, and a host-blind query would let a
/// spent face placed there become a node site. Both counts are asserted, so neither passes empty.
#[test]
fn node_at_routes_on_the_host_the_world_really_holds() {
	use crate::chunk::{CHUNK_SIZE, WORLD_VCHUNK, cell_id};
	let n = CHUNK_SIZE as i32;
	let hw = find_heartwood(SEED);
	let (cx, cz) = (hw.0.div_euclid(n), hw.2.div_euclid(n));
	// The three hosts, asked blind — the query this function used to be.
	let hosts = [BlockType::Stone, BlockType::Wood, BlockType::Sand];
	let (mut sited, mut overlapping) = (0, 0);
	let column = crate::worldgen::build_column(cx, cz, SEED);
	assert_eq!(column.len(), WORLD_VCHUNK as usize, "a whole column");
	for (cy, chunk) in column.iter().enumerate() {
		for ly in 0..CHUNK_SIZE {
			for lz in 0..CHUNK_SIZE {
				for lx in 0..CHUNK_SIZE {
					let at = (
						cx * n + lx as i32,
						cy as i32 * n + ly as i32,
						cz * n + lz as i32,
					);
					let block = BlockType::try_from_id(cell_id(chunk.get_cell(lx, ly, lz)))
						.expect("worldgen writes real blocks");
					match node_at(block, at, SEED) {
						// Routed: the kind named must be one this very
						// host carries, or `host_block` and `node_at`
						// have come apart.
						Some(node) => {
							assert_eq!(host_block(node), Some(block), "{at:?} {node:?}");
							sited += 1;
						}
						// Not this host — and if any OTHER host's stream
						// would have answered here, this is a cell a
						// blind query manufactures a site in.
						None => {
							overlapping += u32::from(
								hosts
									.iter()
									.any(|&h| h != block && node_at(h, at, SEED).is_some()),
							);
						}
					}
				}
			}
		}
	}
	assert!(sited > 0, "no node stands in the sampled column at all");
	assert!(
		overlapping > 0,
		"no cell in the sampled column holds a non-host over a live stream"
	);
}
