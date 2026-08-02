//! Block types and their data-driven properties. Adding a block is: one
//! `blocks!` row, a texture PNG, and a `LAYERS` entry in digblock-assets
//! — nothing else.
//!
//! Values are contiguous so they index tables directly (the Java used
//! AIR=-1, WATER=0, DIRT=1, GRASS=2; shifted by +1 here so a chunk is a
//! flat array of cells with AIR=0 as the natural default).

use crate::dir::Dir;

/// One table generates the enum, `COUNT`, `ALL`, and `PROPS` together —
/// adding a block is ONE row here (plus a PNG + `LAYERS` entry in
/// digblock-assets); the old hand-synced quadruple cannot skew.
macro_rules! blocks {
	(
		$(#[$fmeta:meta])* $first:ident = $fid:literal => $fprops:expr,
		$($(#[$meta:meta])* $name:ident = $id:literal => $props:expr,)+
	) => {
		#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
		#[repr(u16)]
		pub enum BlockType {
			$(#[$fmeta])*
			#[default]
			$first = $fid,
			$($(#[$meta])* $name = $id,)+
		}

		impl BlockType {
			pub const COUNT: usize = 1 + [$($id as u16),+].len();
			pub const ALL: [BlockType; Self::COUNT] =
				[BlockType::$first, $(BlockType::$name),+];
		}

		/// Indexed by `BlockType as usize` — discriminant contiguity is
		/// pinned by `discriminants_index_props`.
		pub const PROPS: [BlockProps; BlockType::COUNT] = [$fprops, $($props),+];
	};
}

blocks! {
	Air = 0 => props(RenderClass::None, false, 0, 0, 0),
	/// A water source: infinite, amount 8. All generated water (oceans,
	/// river pools) is sources — flowing/falling states describe water in
	/// transit under the Minecraft-style flow rules.
	Water = 1 => water_props(8, true, false),
	Dirt = 2 => props(RenderClass::Opaque, true, tex::DIRT, tex::DIRT, tex::DIRT),
	Grass = 3 => props(RenderClass::Opaque, true, tex::GRASS_SIDE, tex::GRASS_TOP, tex::DIRT),
	Sand = 4 => props(RenderClass::Opaque, true, tex::SAND, tex::SAND, tex::SAND),
	Stone = 5 => props(RenderClass::Opaque, true, tex::STONE, tex::STONE, tex::STONE),
	Wood = 6 => props(RenderClass::Opaque, true, tex::WOOD, tex::WOOD_TOP, tex::WOOD_TOP),
	Leaves = 7 => props(RenderClass::Cutout, true, tex::LEAVES, tex::LEAVES, tex::LEAVES),
	Snow = 8 => props(RenderClass::Opaque, true, tex::SNOW, tex::SNOW, tex::SNOW),
	/// Flowing water, amount 1 (thinnest) … 7. Spread sideways from a
	/// source or a landed fall, losing one amount per block.
	WaterFlow1 = 9 => water_props(1, false, false),
	WaterFlow2 = 10 => water_props(2, false, false),
	WaterFlow3 = 11 => water_props(3, false, false),
	WaterFlow4 = 12 => water_props(4, false, false),
	WaterFlow5 = 13 => water_props(5, false, false),
	WaterFlow6 = 14 => water_props(6, false, false),
	WaterFlow7 = 15 => water_props(7, false, false),
	/// Water falling through air: renders full-height, lands at amount 8.
	WaterFall = 16 => water_props(8, false, true),
}

/// Ceiling on texture-array layers, tied to the packed vertex format's
/// layer field width. Every PROPS row is const-checked against it.
pub const MAX_TEX_LAYERS: usize = 1024;
const _: () = {
	let mut i = 0;
	while i < BlockType::COUNT {
		let p = &PROPS[i];
		assert!(
			(p.tex_side as usize) < MAX_TEX_LAYERS
				&& (p.tex_top as usize) < MAX_TEX_LAYERS
				&& (p.tex_bottom as usize) < MAX_TEX_LAYERS,
			"texture layer exceeds the vertex format's layer field"
		);
		i += 1;
	}
};

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

/// Which vertex stream a block's faces land in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeshBucket {
	/// The single-word solid stream (opaque and cutout geometry).
	Solid,
	/// The two-word water stream (surface lips, flow/splash payload,
	/// alpha blend, no depth write).
	Water,
}

/// Fluid behavior, when a block IS one. Data, not variant identity: a
/// second fluid (lava) is new PROPS rows, not new match arms across the
/// mesher and the fluid sim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FluidProps {
	/// Level 1..=8 this block holds (sources and falls are full).
	pub amount: u8,
	/// Infinite, never recomputed by the fluid sim.
	pub source: bool,
	/// Falling through air; renders full-height, lands at full amount.
	pub falling: bool,
}

pub struct BlockProps {
	/// Occlusion/face-culling rule ONLY — bucket and fluid behavior are
	/// separate fields (a future glass is Translucent but Solid-bucket
	/// and not a fluid).
	pub class: RenderClass,
	/// Blocks the player and stops the picking ray.
	pub collides: bool,
	pub bucket: MeshBucket,
	pub fluid: Option<FluidProps>,
	/// Texture-array layer indices (order fixed by digblock_assets::tex::LAYERS).
	pub tex_side: u8,
	pub tex_top: u8,
	pub tex_bottom: u8,
}

const fn props(class: RenderClass, collides: bool, side: u8, top: u8, bottom: u8) -> BlockProps {
	BlockProps {
		class,
		collides,
		bucket: MeshBucket::Solid,
		fluid: None,
		tex_side: side,
		tex_top: top,
		tex_bottom: bottom,
	}
}

/// A water-family row: water bucket + fluid behavior.
const fn water_props(amount: u8, source: bool, falling: bool) -> BlockProps {
	BlockProps {
		class: RenderClass::Translucent,
		collides: false,
		bucket: MeshBucket::Water,
		fluid: Some(FluidProps {
			amount,
			source,
			falling,
		}),
		tex_side: tex::WATER,
		tex_top: tex::WATER,
		tex_bottom: tex::WATER,
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

impl BlockType {
	/// Forgiving decode for world data: out-of-range ids read as Air.
	/// The id occupies the low [`crate::chunk::ID_BITS`] of a cell; any
	/// state bits must be stripped by the caller (`ChunkData::get` does).
	#[inline]
	pub fn from_id(v: u16) -> Self {
		if (v as usize) < Self::COUNT {
			Self::ALL[v as usize]
		} else {
			BlockType::Air
		}
	}

	/// Strict decode for VALIDATION paths (network input): `None` for
	/// unknown ids or set state bits, instead of silently reading Air.
	#[inline]
	pub fn try_from_id(v: u16) -> Option<Self> {
		((v as usize) < Self::COUNT).then(|| Self::ALL[v as usize])
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
	/// spread level. Pure table lookup: variant identity plays no part.
	#[inline]
	pub fn water_amount(self) -> Option<u8> {
		self.props().fluid.map(|f| f.amount)
	}

	/// Any member of the water family (source, flowing, falling).
	#[inline]
	pub fn is_water(self) -> bool {
		self.props().fluid.is_some()
	}

	#[inline]
	pub fn is_falling(self) -> bool {
		matches!(self.props().fluid, Some(f) if f.falling)
	}

	/// Infinite fluid source, never recomputed by the fluid sim.
	#[inline]
	pub fn is_fluid_source(self) -> bool {
		matches!(self.props().fluid, Some(f) if f.source)
	}

	/// The flowing-water block for an amount 1..=7.
	#[inline]
	pub fn water_flow(amount: u8) -> BlockType {
		debug_assert!((1..=7).contains(&amount));
		Self::from_id(BlockType::WaterFlow1 as u16 + amount as u16 - 1)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn discriminants_index_props() {
		for (i, &b) in BlockType::ALL.iter().enumerate() {
			assert_eq!(b as usize, i, "{b:?} discriminant matches ALL order");
			assert_eq!(BlockType::from_id(b as u16), b, "from_id roundtrip");
			assert_eq!(BlockType::try_from_id(b as u16), Some(b));
		}
		assert_eq!(
			BlockType::from_id(1000),
			BlockType::Air,
			"out of range → Air"
		);
		assert_eq!(BlockType::try_from_id(1000), None, "strict decode refuses");
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

	/// The table is now the source of truth; this pins it to the old
	/// concrete-variant semantics for every block, so the generalization
	/// cannot have changed behavior.
	#[test]
	fn fluid_table_matches_legacy_variant_semantics() {
		for &b in &BlockType::ALL {
			let legacy_amount = match b {
				BlockType::Water | BlockType::WaterFall => Some(8u8),
				BlockType::WaterFlow1 => Some(1),
				BlockType::WaterFlow2 => Some(2),
				BlockType::WaterFlow3 => Some(3),
				BlockType::WaterFlow4 => Some(4),
				BlockType::WaterFlow5 => Some(5),
				BlockType::WaterFlow6 => Some(6),
				BlockType::WaterFlow7 => Some(7),
				_ => None,
			};
			assert_eq!(b.water_amount(), legacy_amount, "{b:?}");
			assert_eq!(b.is_water(), legacy_amount.is_some(), "{b:?}");
			assert_eq!(b.is_falling(), b == BlockType::WaterFall, "{b:?}");
			assert_eq!(b.is_fluid_source(), b == BlockType::Water, "{b:?}");
			// Today Translucent ⇔ Water bucket; the split exists so a
			// future glass can break the equivalence deliberately.
			assert_eq!(
				b.props().bucket == MeshBucket::Water,
				b.props().class == RenderClass::Translucent,
				"{b:?}"
			);
			assert_eq!(b.props().fluid.is_some(), b.is_water(), "{b:?}");
		}
		for a in 1..=7u8 {
			let f = BlockType::water_flow(a)
				.props()
				.fluid
				.expect("flow is fluid");
			assert_eq!((f.amount, f.source, f.falling), (a, false, false));
		}
	}
}
