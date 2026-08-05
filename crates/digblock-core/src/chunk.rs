use crate::block::BlockType;

pub const CHUNK_SIZE: usize = 16;
pub const CHUNK_VOLUME: usize = CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE;

/// World height in chunks (16 = 256 blocks), unbounded horizontally. The
/// single knob for world height: columns above the terrain surface resolve
/// to the shared AIR_CHUNK sentinel, so raising this costs almost nothing.
pub const WORLD_VCHUNK: i32 = 16;
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

/// Block data, shared with mesh jobs via `Arc`; edits go through
/// copy-on-write (`Arc::make_mut`), so a job's snapshot stays consistent.
/// Indexed `(y*16 + z)*16 + x`.
/// Bits of a cell that hold the block id (1024 ids). Bits 12..=15 are a
/// reserved per-cell STATE nibble (future stairs facing×half etc.); bits
/// 10..11 are spare. The whole u16 cell travels the wire and the journal,
/// so the id/state split can be re-drawn without another format break.
pub const ID_BITS: u32 = 10;
pub const ID_MASK: u16 = (1 << ID_BITS) - 1;
pub const STATE_SHIFT: u32 = 12;
pub const STATE_MASK: u16 = 0xF000;

/// The block id half of a cell.
#[inline]
pub fn cell_id(cell: u16) -> u16 {
	cell & ID_MASK
}

/// The state nibble of a cell (0 for every stateless block).
#[inline]
pub fn cell_state(cell: u16) -> u8 {
	((cell & STATE_MASK) >> STATE_SHIFT) as u8
}

/// Compose a cell from a block type and a state nibble.
#[inline]
pub fn make_cell(b: BlockType, state: u8) -> u16 {
	b as u16 | (((state as u16) << STATE_SHIFT) & STATE_MASK)
}

#[derive(Clone)]
pub struct ChunkData {
	/// One cell per block: id in the low ID_BITS, state nibble on top.
	pub blocks: [u16; CHUNK_VOLUME],
}

impl Default for ChunkData {
	fn default() -> Self {
		Self {
			blocks: [0; CHUNK_VOLUME],
		}
	}
}

/// The shared all-air chunk. Worldgen returns clones of this Arc for every
/// chunk above the terrain, so thousands of sky chunks share one 8KB body
/// and are recognized by pointer to skip meshing and border-plane copies.
/// `Arc::make_mut` in `set_block` copy-on-writes it, so editing an air
/// chunk transparently turns it into a normal one.
pub static AIR_CHUNK: std::sync::LazyLock<std::sync::Arc<ChunkData>> =
	std::sync::LazyLock::new(|| std::sync::Arc::new(ChunkData::default()));

#[inline]
pub fn is_air_chunk(d: &std::sync::Arc<ChunkData>) -> bool {
	std::sync::Arc::ptr_eq(d, &AIR_CHUNK)
}

/// `split_world` result: (chunk coordinate, in-chunk cell).
pub type SplitWorld = ((i32, i32, i32), (usize, usize, usize));

/// Split a world coordinate into its chunk coordinate `(cx, cy, cz)` and
/// in-chunk cell. `None` = outside the vertical world. THE one place the
/// div/rem-euclid split lives — the client `Pipeline`, the server
/// `ChunkStore`, and every future world consumer share it, so their
/// indexing can never drift apart.
#[inline]
pub fn split_world(x: i32, y: i32, z: i32) -> Option<SplitWorld> {
	if !(0..=WORLD_VBLOCK).contains(&y) {
		return None;
	}
	let s = CHUNK_SIZE as i32;
	Some((
		(x.div_euclid(s), y.div_euclid(s), z.div_euclid(s)),
		(
			x.rem_euclid(s) as usize,
			y.rem_euclid(s) as usize,
			z.rem_euclid(s) as usize,
		),
	))
}

/// Shared world-read contract (`None` = unloaded or outside the vertical
/// world — never solid). Implemented by the client `Pipeline` and the
/// server `ChunkStore`; a divergence between the two would be a silent
/// desync, so consumers that can take either should take this.
pub trait BlockWorld {
	fn block_at(&self, x: i32, y: i32, z: i32) -> Option<crate::block::BlockType>;

	/// Full cell (id + state nibble). The default maps `block_at` for
	/// impls that can't produce state; real world stores override it.
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.block_at(x, y, z).map(|b| b as u16)
	}
}

impl ChunkData {
	#[inline]
	pub fn index(x: usize, y: usize, z: usize) -> usize {
		(y * CHUNK_SIZE + z) * CHUNK_SIZE + x
	}

	#[inline]
	pub fn get(&self, x: usize, y: usize, z: usize) -> BlockType {
		BlockType::from_id(self.blocks[Self::index(x, y, z)] & ID_MASK)
	}

	/// Set the block TYPE, clearing any state bits.
	#[inline]
	pub fn set(&mut self, x: usize, y: usize, z: usize, b: BlockType) {
		self.blocks[Self::index(x, y, z)] = b as u16;
	}

	/// Raw cell access (id + state bits) for consumers that preserve or
	/// interpret state. Nothing writes state bits yet; the accessors exist
	/// so the convention has one home from day one.
	#[inline]
	pub fn get_cell(&self, x: usize, y: usize, z: usize) -> u16 {
		self.blocks[Self::index(x, y, z)]
	}

	#[inline]
	pub fn set_cell(&mut self, x: usize, y: usize, z: usize, cell: u16) {
		self.blocks[Self::index(x, y, z)] = cell;
	}
}
