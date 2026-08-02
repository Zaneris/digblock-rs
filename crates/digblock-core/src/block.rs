//! Block types and their data-driven properties. Adding a block is: a new
//! enum variant, a `PROPS` row, a texture PNG, and a `LAYERS` entry in
//! digblock-assets — nothing else.
//!
//! Values are contiguous so they index tables directly (the Java used
//! AIR=-1, WATER=0, DIRT=1, GRASS=2; shifted by +1 here so a chunk is a
//! flat `[u8; 4096]` with AIR=0 as the natural default).

use crate::dir::Dir;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum BlockType {
	#[default]
	Air = 0,
	/// A water source: infinite, amount 8. All generated water (oceans,
	/// river pools) is sources — flowing/falling states describe water in
	/// transit under the Minecraft-style flow rules.
	Water = 1,
	Dirt = 2,
	Grass = 3,
	Sand = 4,
	Stone = 5,
	Wood = 6,
	Leaves = 7,
	Snow = 8,
	/// Flowing water, amount 1 (thinnest) … 7. Spread sideways from a
	/// source or a landed fall, losing one amount per block.
	WaterFlow1 = 9,
	WaterFlow2 = 10,
	WaterFlow3 = 11,
	WaterFlow4 = 12,
	WaterFlow5 = 13,
	WaterFlow6 = 14,
	WaterFlow7 = 15,
	/// Water falling through air: renders full-height, lands at amount 8.
	WaterFall = 16,
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
	pub const SNOW: u8 = 9;
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
		props(R::Opaque, true, tex::SNOW, tex::SNOW, tex::SNOW), // Snow
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow1
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow2
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow3
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow4
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow5
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow6
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFlow7
		props(R::Translucent, false, tex::WATER, tex::WATER, tex::WATER), // WaterFall
	]
};

impl BlockType {
	pub const COUNT: usize = 17;
	pub const ALL: [BlockType; Self::COUNT] = [
		BlockType::Air,
		BlockType::Water,
		BlockType::Dirt,
		BlockType::Grass,
		BlockType::Sand,
		BlockType::Stone,
		BlockType::Wood,
		BlockType::Leaves,
		BlockType::Snow,
		BlockType::WaterFlow1,
		BlockType::WaterFlow2,
		BlockType::WaterFlow3,
		BlockType::WaterFlow4,
		BlockType::WaterFlow5,
		BlockType::WaterFlow6,
		BlockType::WaterFlow7,
		BlockType::WaterFall,
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

	/// How much water this block holds, 1..=8 — None for non-water.
	/// Sources and falling water are full; flowing water carries its
	/// spread level.
	#[inline]
	pub fn water_amount(self) -> Option<u8> {
		match self {
			BlockType::Water | BlockType::WaterFall => Some(8),
			BlockType::WaterFlow1 => Some(1),
			BlockType::WaterFlow2 => Some(2),
			BlockType::WaterFlow3 => Some(3),
			BlockType::WaterFlow4 => Some(4),
			BlockType::WaterFlow5 => Some(5),
			BlockType::WaterFlow6 => Some(6),
			BlockType::WaterFlow7 => Some(7),
			_ => None,
		}
	}

	/// Any member of the water family (source, flowing, falling).
	#[inline]
	pub fn is_water(self) -> bool {
		self.water_amount().is_some()
	}

	#[inline]
	pub fn is_falling(self) -> bool {
		self == BlockType::WaterFall
	}

	/// The flowing-water block for an amount 1..=7.
	#[inline]
	pub fn water_flow(amount: u8) -> BlockType {
		debug_assert!((1..=7).contains(&amount));
		Self::from_u8(BlockType::WaterFlow1 as u8 + amount - 1)
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

	#[test]
	fn water_family_props() {
		for &b in &BlockType::ALL {
			match b.water_amount() {
				Some(a) => {
					assert!((1..=8).contains(&a), "{b:?} amount {a}");
					assert_eq!(b.props().class, RenderClass::Translucent, "{b:?}");
					assert!(!b.props().collides, "{b:?} never collides");
					assert!(b.is_water());
				}
				None => assert!(!b.is_water()),
			}
		}
		assert_eq!(BlockType::Water.water_amount(), Some(8), "source is full");
		assert_eq!(BlockType::WaterFall.water_amount(), Some(8), "fall is full");
		assert!(BlockType::WaterFall.is_falling());
		assert!(!BlockType::Water.is_falling());
		for a in 1..=7u8 {
			assert_eq!(BlockType::water_flow(a).water_amount(), Some(a));
		}
	}
}
