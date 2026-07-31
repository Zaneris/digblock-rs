//! Block types and their data-driven properties. Adding a block is: a new
//! enum variant, a `PROPS` row, a texture PNG, and a `LAYERS` entry in
//! digblock-assets — nothing else.
//!
//! Values are contiguous so they index tables directly (the Java used
//! AIR=-1, WATER=0, DIRT=1, GRASS=2; shifted by +1 here so a chunk is a
//! flat `[u8; 4096]` with AIR=0 as the natural default).

use crate::mesher::Dir;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum BlockType {
	#[default]
	Air = 0,
	Water = 1,
	Dirt = 2,
	Grass = 3,
	Sand = 4,
	Stone = 5,
	Wood = 6,
	Leaves = 7,
}

/// How a block participates in meshing and rendering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderClass {
	/// Emits no faces, occludes nothing (Air).
	None,
	/// Solid mesh; occludes neighbor faces.
	Opaque,
	/// Solid mesh; does not occlude; culls faces against same-type
	/// neighbors (leaves-style).
	Cutout,
	/// Water mesh (alpha-blended, no depth write); emits faces only toward
	/// `None` neighbors; culls same-type; top faces get the surface lip.
	Translucent,
}

pub struct BlockProps {
	pub class: RenderClass,
	/// Blocks the player and stops the picking ray.
	pub collides: bool,
	/// Texture-array layer indices (order fixed by digblock_assets::tex::LAYERS).
	pub tex_side: u8,
	pub tex_top: u8,
	pub tex_bottom: u8,
}

const fn props(class: RenderClass, collides: bool, side: u8, top: u8, bottom: u8) -> BlockProps {
	BlockProps {
		class,
		collides,
		tex_side: side,
		tex_top: top,
		tex_bottom: bottom,
	}
}

/// Texture-array layers, matching digblock_assets::tex::LAYERS order.
mod tex {
	pub const WATER: u8 = 0;
	pub const DIRT: u8 = 1;
	pub const GRASS_SIDE: u8 = 2;
	pub const GRASS_TOP: u8 = 3;
	pub const SAND: u8 = 4;
	pub const STONE: u8 = 5;
	pub const WOOD: u8 = 6;
	pub const WOOD_TOP: u8 = 7;
	pub const LEAVES: u8 = 8;
}

/// Indexed by `BlockType as usize` — the repr(u8) discriminants are
/// contiguous by design.
pub const PROPS: [BlockProps; BlockType::COUNT] = {
	use RenderClass as R;
	[
		props(R::None, false, 0, 0, 0), // Air
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // Water
		props(R::Opaque, true, tex::DIRT, tex::DIRT, tex::DIRT), // Dirt
		props(R::Opaque, true, tex::GRASS_SIDE, tex::GRASS_TOP, tex::DIRT), // Grass
		props(R::Opaque, true, tex::SAND, tex::SAND, tex::SAND), // Sand
		props(R::Opaque, true, tex::STONE, tex::STONE, tex::STONE), // Stone
		props(R::Opaque, true, tex::WOOD, tex::WOOD_TOP, tex::WOOD_TOP), // Wood
		props(R::Cutout, true, tex::LEAVES, tex::LEAVES, tex::LEAVES), // Leaves
	]
};

impl BlockType {
	pub const COUNT: usize = 8;
	pub const ALL: [BlockType; Self::COUNT] = [
		BlockType::Air,
		BlockType::Water,
		BlockType::Dirt,
		BlockType::Grass,
		BlockType::Sand,
		BlockType::Stone,
		BlockType::Wood,
		BlockType::Leaves,
	];

	#[inline]
	pub fn from_u8(v: u8) -> Self {
		if (v as usize) < Self::COUNT {
			Self::ALL[v as usize]
		} else {
			BlockType::Air
		}
	}

	#[inline]
	pub fn props(self) -> &'static BlockProps {
		&PROPS[self as usize]
	}

	/// Texture-array layer for a face in direction `dir`.
	#[inline]
	pub fn texture(self, dir: Dir) -> u32 {
		let p = self.props();
		u32::from(match dir {
			Dir::Top => p.tex_top,
			Dir::Bottom => p.tex_bottom,
			_ => p.tex_side,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn discriminants_index_props() {
		for (i, &b) in BlockType::ALL.iter().enumerate() {
			assert_eq!(b as usize, i, "{b:?} discriminant matches ALL order");
			assert_eq!(BlockType::from_u8(b as u8), b, "from_u8 roundtrip");
		}
		assert_eq!(
			BlockType::from_u8(255),
			BlockType::Air,
			"out of range → Air"
		);
	}

	#[test]
	fn table_sanity() {
		for &b in &BlockType::ALL {
			let p = b.props();
			// Air is the only class-None block, and nothing colliding is None.
			assert_eq!(p.class == RenderClass::None, b == BlockType::Air);
			if p.collides {
				assert_ne!(p.class, RenderClass::None);
			}
		}
	}
}
