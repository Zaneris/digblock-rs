use digblock_core::worldgen::{build_chunk, terrain_height, WATER_HEIGHT};
use digblock_core::{BlockType, ChunkData, ChunkPos, CHUNK_SIZE};

/// Column rules: dirt below surface, grass at surface above water,
/// dirt at surface underwater, water fills to WATER_HEIGHT, air above.
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
					let expect = if wy < h || (wy == h && wy < WATER_HEIGHT) {
						BlockType::Dirt
					} else if wy == h {
						BlockType::Grass
					} else if wy <= WATER_HEIGHT {
						BlockType::Water
					} else {
						BlockType::Air
					};
					assert_eq!(b, expect, "({x},{wy},{z}) h={h}");
				}
			}
		}
	}
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
