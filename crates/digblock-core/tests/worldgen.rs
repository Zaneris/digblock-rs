use digblock_core::worldgen::{build_chunk, terrain_height, DIRT_DEPTH, WATER_HEIGHT};
use digblock_core::{BlockType, ChunkData, ChunkPos, CHUNK_SIZE};

/// Column rules: stone below DIRT_DEPTH of topsoil, sand topsoil near or
/// under the water line (beaches), grass-over-dirt otherwise, water fills
/// to WATER_HEIGHT, air above.
#[test]
fn column_rules() {
	let seed = 1234.5678;
	for &(cx, cz) in &[(0, 0), (-3, 7), (100, -100)] {
		let chunks: Vec<ChunkData> = (0..2)
			.map(|cy| build_chunk(ChunkPos::new(cx, cy, cz), seed))
			.collect();
		let block_at = |x: usize, wy: i32, z: usize| -> BlockType {
			chunks[(wy / 16) as usize].get(x, (wy % 16) as usize, z)
		};
		for x in 0..CHUNK_SIZE {
			for z in 0..CHUNK_SIZE {
				let h = terrain_height(x as i32 + cx * 16, z as i32 + cz * 16, seed);
				for wy in 0..32 {
					let b = block_at(x, wy, z);
					let expect = if wy > h {
						if wy <= WATER_HEIGHT {
							BlockType::Water
						} else {
							BlockType::Air
						}
					} else if wy <= h - DIRT_DEPTH {
						BlockType::Stone
					} else if h <= WATER_HEIGHT + 1 {
						BlockType::Sand
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

/// The three column archetypes all occur at this seed: a sea-floor column
/// (sand under water), a beach column (sand above water), and an inland
/// column (grass over dirt over stone).
#[test]
fn layering_archetypes() {
	let seed = 1234.5678;
	let (mut sea, mut beach, mut inland) = (false, false, false);
	for x in -64..64 {
		for z in -64..64 {
			let h = terrain_height(x, z, seed);
			if h < WATER_HEIGHT {
				sea = true;
			} else if h <= WATER_HEIGHT + 1 {
				beach = true;
			} else if h >= DIRT_DEPTH {
				inland = true;
			}
		}
	}
	assert!(
		sea && beach && inland,
		"sea={sea} beach={beach} inland={inland}"
	);
}

/// ASCII heightmap dump — run with `--nocapture` to eyeball terrain shape:
/// `cargo test -p digblock-core heightmap_dump -- --nocapture`
#[test]
fn heightmap_dump() {
	let seed = 0.0;
	let glyphs = b" .:-=+*#%@";
	let mut out = String::new();
	for z in (-32..32).rev() {
		for x in -64..64 {
			let h = terrain_height(x, z, seed);
			let g = glyphs[(h as usize * (glyphs.len() - 1)) / 31] as char;
			out.push(if h <= WATER_HEIGHT { '~' } else { g });
		}
		out.push('\n');
	}
	println!("{out}");
	// Sanity: terrain is not flat and contains some water at seed 0.
	assert!(out.contains('~'));
	assert!(out.chars().any(|c| c != '~' && c != '\n'));
}
