//! Terrain generation — heightmap from 2 octaves of simplex noise plus a
//! flat sea level; no caves, ores, trees, or biomes (the Java original had
//! none). Ported from the Java WorldBuilder; numerics have since been
//! simplified (plain f64 throughout), so terrain for a given seed differs
//! from the Java.

use crate::block::BlockType;
use crate::chunk::{ChunkData, ChunkPos, CHUNK_SIZE, WORLD_VBLOCK, WORLD_VCHUNK};
use crate::simplex::noise2;

const TERRAIN_INTENSITY: f64 = 0.005;
const TERRAIN_INTENSITY2: f64 = 0.015;
pub const WATER_HEIGHT: i32 = 14;

type Heightmap = [[i32; CHUNK_SIZE]; CHUNK_SIZE];

pub fn terrain_height(x: i32, z: i32, seed: f64) -> i32 {
	let octave = |intensity: f64| -> f64 {
		let n = noise2(
			seed + f64::from(x) * intensity,
			seed + f64::from(z) * intensity,
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

/// Heightmap for a chunk column, indexed `[z][x]`.
fn column_heights(cx: i32, cz: i32, seed: f64) -> Heightmap {
	let (px, pz) = (cx * CHUNK_SIZE as i32, cz * CHUNK_SIZE as i32);
	let mut heights = [[0i32; CHUNK_SIZE]; CHUNK_SIZE];
	for (z, row) in heights.iter_mut().enumerate() {
		for (x, h) in row.iter_mut().enumerate() {
			*h = terrain_height(x as i32 + px, z as i32 + pz, seed);
		}
	}
	heights
}

/// Column rules exactly as the Java: dirt below the surface, dirt at the
/// surface if underwater, grass at the surface otherwise, water up to
/// WATER_HEIGHT, air above. Writes in storage order (x contiguous).
fn fill_chunk(data: &mut ChunkData, py: i32, heights: &Heightmap) {
	for y in 0..CHUNK_SIZE {
		let wy = y as i32 + py;
		for (z, hrow) in heights.iter().enumerate() {
			let row = (y * CHUNK_SIZE + z) * CHUNK_SIZE;
			let out = &mut data.blocks[row..row + CHUNK_SIZE];
			for (b, &h) in out.iter_mut().zip(hrow) {
				let block = if wy < h || (wy == h && wy < WATER_HEIGHT) {
					BlockType::Dirt
				} else if wy == h {
					BlockType::Grass
				} else if wy <= WATER_HEIGHT {
					BlockType::Water
				} else {
					BlockType::Air
				};
				*b = block as u8;
			}
		}
	}
}

/// Fill one chunk's blocks.
pub fn build_chunk(pos: ChunkPos, seed: f64) -> ChunkData {
	let mut data = ChunkData::default();
	fill_chunk(
		&mut data,
		pos.y * CHUNK_SIZE as i32,
		&column_heights(pos.x, pos.z, seed),
	);
	data
}

/// Generate the full vertical chunk stack of a column in one call,
/// computing the 16×16 heightmap once instead of once per chunk.
pub fn build_column(cx: i32, cz: i32, seed: f64) -> [ChunkData; WORLD_VCHUNK as usize] {
	let heights = column_heights(cx, cz, seed);
	std::array::from_fn(|cy| {
		let mut data = ChunkData::default();
		fill_chunk(&mut data, cy as i32 * CHUNK_SIZE as i32, &heights);
		data
	})
}
