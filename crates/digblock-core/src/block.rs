/// Block types. Values are contiguous so they index tables directly
/// (the Java used AIR=-1, WATER=0, DIRT=1, GRASS=2; shifted by +1 here so
/// a chunk is a flat `[u8; 4096]` with AIR=0 as the natural default).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum BlockType {
	#[default]
	Air = 0,
	Water = 1,
	Dirt = 2,
	Grass = 3,
}

impl BlockType {
	#[inline]
	pub fn from_u8(v: u8) -> Self {
		match v {
			1 => BlockType::Water,
			2 => BlockType::Dirt,
			3 => BlockType::Grass,
			_ => BlockType::Air,
		}
	}

	/// Opaque solid — occludes neighbor faces and goes in the solid mesh.
	#[inline]
	pub fn is_solid(self) -> bool {
		matches!(self, BlockType::Dirt | BlockType::Grass)
	}

	/// Texture-array layers for side faces, matching the Java texture
	/// indices: 0=Water 1=Dirt 2=GrassSide 3=GrassTop.
	#[inline]
	pub fn side_texture(self) -> u32 {
		match self {
			BlockType::Air => 0,
			BlockType::Water => 0,
			BlockType::Dirt => 1,
			BlockType::Grass => 2,
		}
	}

	#[inline]
	pub fn top_texture(self) -> u32 {
		match self {
			BlockType::Grass => 3,
			other => other.side_texture(),
		}
	}
}
