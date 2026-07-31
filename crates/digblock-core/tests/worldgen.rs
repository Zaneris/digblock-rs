use std::collections::HashMap;

use digblock_core::chunk::{WORLD_VBLOCK, WORLD_VCHUNK};
use digblock_core::decoration::{trees_near, STRUCTURE_APRON};
use digblock_core::worldgen::{
	build_chunk, carved, terrain_height, DIRT_DEPTH, SEA_LEVEL, STONE_SURFACE,
};
use digblock_core::{BlockType, ChunkData, ChunkPos, CHUNK_SIZE};

/// The terrain-only expectation (no trees), re-implemented independently.
fn expect_terrain(wx: i32, wy: i32, wz: i32, seed: f64) -> BlockType {
	let h = terrain_height(wx, wz, seed);
	let base = if wy > h {
		if wy <= SEA_LEVEL {
			BlockType::Water
		} else {
			BlockType::Air
		}
	} else if wy <= h - DIRT_DEPTH {
		BlockType::Stone
	} else if h <= SEA_LEVEL + 1 {
		BlockType::Sand
	} else if h >= STONE_SURFACE {
		BlockType::Stone
	} else if wy == h {
		BlockType::Grass
	} else {
		BlockType::Dirt
	};
	let carveable = matches!(base, BlockType::Stone | BlockType::Dirt | BlockType::Grass);
	if carveable && h > SEA_LEVEL && wy >= 1 && carved(wx, wy, wz, seed) {
		BlockType::Air
	} else {
		base
	}
}

/// Column rules re-implemented independently of worldgen's internals:
/// stone below DIRT_DEPTH of topsoil, sand topsoil for ocean floors and
/// beaches, bare stone above STONE_SURFACE, grass-over-dirt otherwise,
/// water fills to SEA_LEVEL, air above; the cave field carves terrain
/// cells of land columns (never y=0, never water or sand). Validates
/// fill_chunk's cell indexing and per-chunk assembly across the stack.
#[test]
fn column_rules() {
	let seed = 1234.5678;
	let mut caves_seen = false;
	let mut trees_seen = false;
	for &(cx, cz) in &[(0, 0), (-3, 7), (100, -100), (40, 40)] {
		let chunks: Vec<ChunkData> = (0..WORLD_VCHUNK)
			.map(|cy| build_chunk(ChunkPos::new(cx, cy, cz), seed))
			.collect();
		let block_at = |x: usize, wy: i32, z: usize| -> BlockType {
			chunks[(wy / 16) as usize].get(x, (wy % 16) as usize, z)
		};
		// Mirror of the decoration pass: trees write into terrain-Air
		// cells, in candidate order, first writer wins.
		let mut tree_cells: HashMap<(i32, i32, i32), BlockType> = HashMap::new();
		for tree in trees_near(cx * 16, cz * 16, seed) {
			tree.for_each_block(|x, y, z, b| {
				if expect_terrain(x, y, z, seed) == BlockType::Air {
					tree_cells.entry((x, y, z)).or_insert(b);
				}
			});
		}
		for x in 0..CHUNK_SIZE {
			for z in 0..CHUNK_SIZE {
				let (wx, wz) = (x as i32 + cx * 16, z as i32 + cz * 16);
				let h = terrain_height(wx, wz, seed);
				for wy in 0..=WORLD_VBLOCK {
					let b = block_at(x, wy, z);
					let terrain = expect_terrain(wx, wy, wz, seed);
					if terrain == BlockType::Air && wy <= h {
						caves_seen = true;
					}
					let expect = match tree_cells.get(&(wx, wy, wz)) {
						Some(&t) if terrain == BlockType::Air => {
							trees_seen = true;
							t
						}
						_ => terrain,
					};
					assert_eq!(b, expect, "({x},{wy},{z}) h={h}");
				}
			}
		}
	}
	assert!(caves_seen, "the sampled chunks contain some carved cells");
	assert!(trees_seen, "the sampled chunks contain some tree blocks");
}

/// A tree whose canopy crosses a chunk border must appear identically in
/// every chunk it spills into — the core correctness property of pure
/// apron evaluation.
#[test]
fn trees_span_chunk_borders_seamlessly() {
	let seed = 1234.5678;
	let mut cache: HashMap<ChunkPos, ChunkData> = HashMap::new();
	for ccz in -6..6 {
		for ccx in -6..6 {
			for tree in trees_near(ccx * 16, ccz * 16, seed) {
				let (lx, lz) = (tree.x.rem_euclid(16), tree.z.rem_euclid(16));
				let crosses = !(2..=13).contains(&lx) || !(2..=13).contains(&lz);
				if !crosses {
					continue;
				}
				let mut chunks_touched = std::collections::HashSet::new();
				tree.for_each_block(|x, y, z, b| {
					let pos = ChunkPos::new(x.div_euclid(16), y.div_euclid(16), z.div_euclid(16));
					chunks_touched.insert((pos.x, pos.z));
					let chunk = cache.entry(pos).or_insert_with(|| build_chunk(pos, seed));
					let got = chunk.get(
						x.rem_euclid(16) as usize,
						y.rem_euclid(16) as usize,
						z.rem_euclid(16) as usize,
					);
					// The cell holds this tree's block unless terrain or an
					// earlier tree already occupied it — never bare air.
					assert!(
						got == b || got != BlockType::Air,
						"({x},{y},{z}) empty in {pos:?}"
					);
				});
				assert!(
					chunks_touched.len() > 1,
					"tree at ({},{}) spans chunks",
					tree.x,
					tree.z
				);
				return; // one verified border tree is the property
			}
		}
	}
	panic!("no border-crossing tree found in the sample area");
}

/// Trees never grow below the apron contract: every block a tree emits
/// stays within STRUCTURE_APRON of its origin column.
#[test]
fn trees_respect_the_apron() {
	let seed = 42.0;
	for tree in trees_near(0, 0, seed) {
		tree.for_each_block(|x, _y, z, _b| {
			assert!((x - tree.x).abs() <= STRUCTURE_APRON);
			assert!((z - tree.z).abs() <= STRUCTURE_APRON);
		});
	}
}

/// Structural cave invariants, independent of the carve function: the
/// bedrock layer survives, and ocean stacks are exactly rule-shaped (no
/// underwater carving anywhere).
#[test]
fn caves_respect_floor_and_oceans() {
	let seed = 1234.5678;
	for cx in -8..8 {
		for cz in -8..8 {
			let bottom = build_chunk(ChunkPos::new(cx, 0, cz), seed);
			for x in 0..CHUNK_SIZE {
				for z in 0..CHUNK_SIZE {
					assert_ne!(bottom.get(x, 0, z), BlockType::Air, "y=0 never carved");
				}
			}
		}
	}
	// Find an ocean column and check nothing below its surface is air.
	let mut checked = false;
	'outer: for x in -512..512 {
		for z in -64..64 {
			let h = terrain_height(x, z, seed);
			if h >= SEA_LEVEL {
				continue;
			}
			let (cx, cz) = (x.div_euclid(16), z.div_euclid(16));
			let (lx, lz) = (x.rem_euclid(16) as usize, z.rem_euclid(16) as usize);
			for wy in 0..=h {
				let chunk = build_chunk(ChunkPos::new(cx, wy.div_euclid(16), cz), seed);
				let b = chunk.get(lx, wy.rem_euclid(16) as usize, lz);
				assert_ne!(b, BlockType::Air, "ocean floor intact at ({x},{wy},{z})");
			}
			checked = true;
			break 'outer;
		}
	}
	assert!(checked, "found an ocean column to check");
}

/// Spawn always lands above the beach line, for a spread of seeds.
#[test]
fn spawn_finds_land() {
	for seed in [0.0, 1.0, 1234.5678, 4242.0, 9999.9] {
		let s = digblock_core::player::spawn(seed);
		let (x, z) = (s.x.floor() as i32, s.z.floor() as i32);
		let h = terrain_height(x, z, seed);
		assert!(h > SEA_LEVEL + 1, "seed {seed}: spawn over land (h={h})");
		assert!((s.y - (h as f32 + 2.5)).abs() < 0.001, "eye above surface");
		assert!(!carved(x, h, z, seed), "spawn surface is not a cave mouth");
	}
}

/// ASCII region map — run with `--nocapture` to eyeball terrain shape:
/// `cargo test -p digblock-core heightmap_dump -- --nocapture`
/// The window spans ~4km so continental-scale features are visible.
#[test]
fn heightmap_dump() {
	let seed = 0.0;
	let glyphs = b" .:-=+*#%@";
	let mut out = String::new();
	let step = 32usize;
	for z in (-1024..1024).rev().step_by(step) {
		for x in (-2048..2048).step_by(step) {
			let h = terrain_height(x, z, seed);
			let g = glyphs[(h as usize * (glyphs.len() - 1)) / WORLD_VBLOCK as usize] as char;
			out.push(if h <= SEA_LEVEL { '~' } else { g });
		}
		out.push('\n');
	}
	println!("{out}");
	// Sanity: the window contains both ocean and land at seed 0.
	assert!(out.contains('~'));
	assert!(out.chars().any(|c| c != '~' && c != '\n'));
}
