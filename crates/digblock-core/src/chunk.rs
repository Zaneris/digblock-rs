use crate::block::BlockType;

pub const CHUNK_SIZE: usize = 16;
pub const CHUNK_VOLUME: usize = CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE;

/// World is 2 chunks (32 blocks) tall, unbounded horizontally — same as the
/// Java WorldBuilder.WORLD_VCHUNK.
pub const WORLD_VCHUNK: i32 = 2;
pub const WORLD_VBLOCK: i32 = WORLD_VCHUNK * CHUNK_SIZE as i32 - 1;

/// Chunk id in chunk units. y ∈ 0..WORLD_VCHUNK.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChunkPos {
	pub x: i32,
	pub y: i32,
	pub z: i32,
}

impl ChunkPos {
	pub const fn new(x: i32, y: i32, z: i32) -> Self {
		Self { x, y, z }
	}

	/// World-space block origin of this chunk.
	pub fn origin(self) -> (f32, f32, f32) {
		(
			(self.x * CHUNK_SIZE as i32) as f32,
			(self.y * CHUNK_SIZE as i32) as f32,
			(self.z * CHUNK_SIZE as i32) as f32,
		)
	}
}

/// Immutable-after-generation block data. Indexed `(y*16 + z)*16 + x`.
pub struct ChunkData {
	pub blocks: [u8; CHUNK_VOLUME],
}

impl Default for ChunkData {
	fn default() -> Self {
		Self {
			blocks: [0; CHUNK_VOLUME],
		}
	}
}

impl ChunkData {
	#[inline]
	pub fn index(x: usize, y: usize, z: usize) -> usize {
		(y * CHUNK_SIZE + z) * CHUNK_SIZE + x
	}

	#[inline]
	pub fn get(&self, x: usize, y: usize, z: usize) -> BlockType {
		BlockType::from_u8(self.blocks[Self::index(x, y, z)])
	}

	#[inline]
	pub fn set(&mut self, x: usize, y: usize, z: usize, b: BlockType) {
		self.blocks[Self::index(x, y, z)] = b as u8;
	}
}
