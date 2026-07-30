//! Terrain generation — a line-for-line port of the Java WorldBuilder.
//! Heightmap from 2 octaves of simplex noise plus a flat sea level; no
//! caves, ores, trees, or biomes (the original had none).

use crate::block::BlockType;
use crate::chunk::{ChunkData, ChunkPos, CHUNK_SIZE, WORLD_VBLOCK, WORLD_VCHUNK};
use crate::simplex::noise2;

// Java constants (WorldBuilder.java). The intensities are f32 on purpose:
// the Java computed `seed + (x * 0.005f)` with an f32 multiply widened to
// f64, and bit-parity at large |x| depends on reproducing that.
const TERRAIN_INTENSITY: f32 = 0.005;
const TERRAIN_INTENSITY2: f32 = 0.015;
pub const WATER_HEIGHT: i32 = 14;

pub fn terrain_height(x: i32, z: i32, seed: f64) -> i32 {
	let octave = |intensity: f32| -> f64 {
		let n = noise2(
			seed + f64::from(x as f32 * intensity),
			seed + f64::from(z as f32 * intensity),
		);
		(1.0 + n) * f64::from(WORLD_VCHUNK) * CHUNK_SIZE as f64 / 4.0
	};
	let noise = octave(TERRAIN_INTENSITY) + octave(TERRAIN_INTENSITY2);
	if noise < 1.0 {
		return 1;
	}
	if noise >= f64::from(WORLD_VBLOCK) {
		return WORLD_VBLOCK;
	}
	noise.floor() as i32
}

/// Fill a chunk's blocks. Column rules exactly as the Java:
/// dirt below the surface, dirt at the surface if underwater, grass at the
/// surface otherwise, water up to WATER_HEIGHT, air above.
pub fn build_chunk(pos: ChunkPos, seed: f64) -> ChunkData {
	let mut data = ChunkData::default();
	let (px, py, pz) = (
		pos.x * CHUNK_SIZE as i32,
		pos.y * CHUNK_SIZE as i32,
		pos.z * CHUNK_SIZE as i32,
	);
	for x in 0..CHUNK_SIZE {
		for z in 0..CHUNK_SIZE {
			let h = terrain_height(x as i32 + px, z as i32 + pz, seed);
			for y in 0..CHUNK_SIZE {
				let wy = y as i32 + py;
				let block = if wy < h || (wy == h && wy < WATER_HEIGHT) {
					BlockType::Dirt
				} else if wy == h {
					BlockType::Grass
				} else if wy <= WATER_HEIGHT {
					BlockType::Water
				} else {
					BlockType::Air
				};
				data.set(x, y, z, block);
			}
		}
	}
	data
}
