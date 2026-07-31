use digblock_core::chunk::{WORLD_VBLOCK, WORLD_VCHUNK};
use digblock_core::worldgen::{build_chunk, terrain_height, DIRT_DEPTH, SEA_LEVEL, STONE_SURFACE};
use digblock_core::{BlockType, ChunkData, ChunkPos, CHUNK_SIZE};

/// Column rules re-implemented independently of worldgen's internals:
/// stone below DIRT_DEPTH of topsoil, sand topsoil for ocean floors and
/// beaches, bare stone above STONE_SURFACE, grass-over-dirt otherwise,
/// water fills to SEA_LEVEL, air above. Validates fill_chunk's cell
/// indexing and per-chunk assembly across the whole vertical stack.
#[test]
fn column_rules() {
	let seed = 1234.5678;
	for &(cx, cz) in &[(0, 0), (-3, 7), (100, -100), (40, 40)] {
		let chunks: Vec<ChunkData> = (0..WORLD_VCHUNK)
			.map(|cy| build_chunk(ChunkPos::new(cx, cy, cz), seed))
			.collect();
		let block_at = |x: usize, wy: i32, z: usize| -> BlockType {
			chunks[(wy / 16) as usize].get(x, (wy % 16) as usize, z)
		};
		for x in 0..CHUNK_SIZE {
			for z in 0..CHUNK_SIZE {
				let h = terrain_height(x as i32 + cx * 16, z as i32 + cz * 16, seed);
				for wy in 0..=WORLD_VBLOCK {
					let b = block_at(x, wy, z);
					let expect = if wy > h {
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
					assert_eq!(b, expect, "({x},{wy},{z}) h={h}");
				}
			}
		}
	}
}

/// Spawn always lands above the beach line, for a spread of seeds.
#[test]
fn spawn_finds_land() {
	for seed in [0.0, 1.0, 1234.5678, 4242.0, 9999.9] {
		let s = digblock_core::player::spawn(seed);
		let h = terrain_height(s.x.floor() as i32, s.z.floor() as i32, seed);
		assert!(h > SEA_LEVEL + 1, "seed {seed}: spawn over land (h={h})");
		assert!((s.y - (h as f32 + 2.5)).abs() < 0.001, "eye above surface");
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
