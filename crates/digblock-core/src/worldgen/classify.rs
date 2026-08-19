//! Surface classification: fields → SurfaceKind + the discretized
//! ColumnShape, and the per-cell column rules. Classification itself
//! stays code (it PRODUCES the kind); what each kind means for blocks
//! lives in the consumers.

use super::terrain::{Fields, SEA_LEVEL};
use crate::block::BlockType;
use crate::simplex::noise2;

/// Above this the surface turns to bare rock.
pub const STONE_SURFACE: i32 = 190;
/// Above this, rock surfaces are capped with snow.
pub const SNOW_LINE: f64 = 208.0;
/// Blocks of dirt (or sand) between the surface and the stone below it.
pub const DIRT_DEPTH: i32 = 3;
/// Surface-classification dither: a short-wavelength noise nudges the
/// beach band, the stone line, and the snow line so altitude zones meet
/// in ragged natural edges instead of world-wide contours.
const SURF_DITHER_FREQ: f64 = 0.11;
const SURF_DITHER_OFF: f64 = 70999.0;
/// Rise (blocks per block) beyond which a face is bare rock at any
/// altitude — grass doesn't hold on anything steeper.
const STEEP_SLOPE: f64 = 1.4;
/// Blocks the stone and snow lines move per unit of regional temperature
/// (in lockstep, preserving the rock band between them): cold regions
/// snow far lower, warm ones never (their line exceeds MAX_TERRAIN).
pub const TEMP_ALT_SHIFT: f64 = 40.0;
/// Temperature above which lowland turns to savanna.
const WARM_T: f64 = 0.35;
/// Temperature below which lush lowland turns to taiga.
const COLD_T: f64 = -0.35;
/// Vegetation above which temperate lowland is forest.
const FOREST_VEG: f64 = 0.15;
/// Vegetation above which cold lowland is taiga (conifers take less).
const TAIGA_VEG: f64 = 0.05;
/// Rise/block within the waterline band beyond which the shore is gravel.
const GRAVEL_SLOPE: f64 = 0.7;
/// Marshes need near-flat land…
const MARSH_SLOPE: f64 = 0.25;
/// …in lush regions.
const MARSH_VEG: f64 = 0.1;
/// Marsh puddle roll: a short-wavelength noise dips scattered columns
/// for standing pools.
const PUDDLE_FREQ: f64 = 0.31;
const PUDDLE_OFF: f64 = 71501.0;
/// Scree patches: a broad noise turns rock surfaces below the snow
/// line into gravel where it exceeds the threshold.
const SCREE_FREQ: f64 = 0.035;
const SCREE_OFF: f64 = 108631.0;
const SCREE_THRESH: f64 = 0.45;

/// What grows (or doesn't) on a column's surface — the seam where the
/// biome system attaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceKind {
	/// Below sea level: sand floor under water.
	Ocean,
	/// At the water line: sand above water.
	Beach,
	/// Grass over dirt, scattered trees.
	Plains,
	/// Bare rock above the (temperature-shifted) stone line — or any
	/// steep face.
	Mountain,
	/// Snow-capped rock above the (dithered, temperature-shifted) snow
	/// line.
	Snow,
	/// Inside a river channel: sand bed under river water.
	River,
	/// Lush temperate lowland: grass with dense broadleaf woods.
	Forest,
	/// Cold lush lowland: grass under conifers.
	Taiga,
	/// Hot lowland: dry grass, sparse flat-topped trees.
	Savanna,
	/// Flat lush land barely above the waterline: mud, puddles, no trees.
	Marsh,
	/// The waterline band where the land behind climbs steeply: gravel.
	GravelShore,
	/// Inside a closed inland basin: sand bed under lake water.
	Lake,
	/// Gravel patches breaking up rock faces below the snow line.
	Scree,
}

impl SurfaceKind {
	/// Declaration order — the `SURFACE_PROPS` index. Tests iterate this
	/// so a new kind cannot silently go unasserted.
	pub const ALL: [SurfaceKind; 13] = [
		SurfaceKind::Ocean,
		SurfaceKind::Beach,
		SurfaceKind::Plains,
		SurfaceKind::Mountain,
		SurfaceKind::Snow,
		SurfaceKind::River,
		SurfaceKind::Forest,
		SurfaceKind::Taiga,
		SurfaceKind::Savanna,
		SurfaceKind::Marsh,
		SurfaceKind::GravelShore,
		SurfaceKind::Lake,
		SurfaceKind::Scree,
	];
}

/// How many tree candidates a surface hosts, as decoration table data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeDensity {
	None,
	/// Scattered singles (plains, savanna).
	Sparse,
	/// Open woodland (taiga).
	Normal,
	/// Closed grove (forest).
	Dense,
}

/// Which silhouette family decoration grows on a surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeSpecies {
	/// Round/tall/squat leafy trees.
	Broadleaf,
	/// Conical conifers with dark foliage.
	Pine,
	/// Umbrella acacias.
	FlatTop,
}

/// What a SurfaceKind MEANS, as data: the block palette, decoration
/// rules, and spawn acceptance, in one row per kind. A new biome is a
/// classification arm plus a row here — not edits across worldgen,
/// decoration, and spawn.
pub struct SurfaceProps {
	/// Block at the surface cell (`wy == height`).
	pub top: BlockType,
	/// Blocks between the surface and the stone below it.
	pub subsoil: BlockType,
	/// Tree-candidate density class for this surface.
	pub tree_density: TreeDensity,
	/// Species grown when the density is not None.
	pub tree_species: TreeSpecies,
	/// The spawn walk accepts this surface (land altitude still applies).
	pub spawnable: bool,
	/// Stratum block between the subsoil and deep stone…
	pub band: BlockType,
	/// …this many blocks of it (0 = straight to stone).
	pub band_depth: i32,
	/// Boulders may decorate this surface.
	pub boulders: bool,
}

const fn sprops(
	top: BlockType,
	subsoil: BlockType,
	tree_density: TreeDensity,
	tree_species: TreeSpecies,
	spawnable: bool,
) -> SurfaceProps {
	SurfaceProps {
		top,
		subsoil,
		tree_density,
		tree_species,
		spawnable,
		band: BlockType::Stone,
		band_depth: 0,
		boulders: false,
	}
}

/// Sandy floors sit on a sandstone stratum before deep stone.
const fn banded(mut p: SurfaceProps) -> SurfaceProps {
	p.band = BlockType::Sandstone;
	p.band_depth = 4;
	p
}

/// Rock surfaces that host boulder clumps.
const fn bouldery(mut p: SurfaceProps) -> SurfaceProps {
	p.boulders = true;
	p
}

/// Indexed by `SurfaceKind as usize` (declaration order).
pub(super) const SURFACE_PROPS: [SurfaceProps; 13] = [
	// Ocean
	banded(sprops(
		BlockType::Sand,
		BlockType::Sand,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		false,
	)),
	// Beach
	banded(sprops(
		BlockType::Sand,
		BlockType::Sand,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		true,
	)),
	// Plains: scattered trees — forests now carry the woods.
	sprops(
		BlockType::Grass,
		BlockType::Dirt,
		TreeDensity::Sparse,
		TreeSpecies::Broadleaf,
		true,
	),
	// Mountain
	bouldery(sprops(
		BlockType::Stone,
		BlockType::Stone,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		true,
	)),
	// Snow
	sprops(
		BlockType::Snow,
		BlockType::Stone,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		true,
	),
	// River
	banded(sprops(
		BlockType::Sand,
		BlockType::Sand,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		false,
	)),
	// Forest
	sprops(
		BlockType::Grass,
		BlockType::Dirt,
		TreeDensity::Dense,
		TreeSpecies::Broadleaf,
		true,
	),
	// Taiga
	sprops(
		BlockType::Grass,
		BlockType::Dirt,
		TreeDensity::Normal,
		TreeSpecies::Pine,
		true,
	),
	// Savanna
	sprops(
		BlockType::DryGrass,
		BlockType::Dirt,
		TreeDensity::Sparse,
		TreeSpecies::FlatTop,
		true,
	),
	// Marsh: soggy ground, puddles — keep spawns off it.
	sprops(
		BlockType::Mud,
		BlockType::Mud,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		false,
	),
	// GravelShore
	sprops(
		BlockType::Gravel,
		BlockType::Gravel,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		true,
	),
	// Lake
	banded(sprops(
		BlockType::Sand,
		BlockType::Sand,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		false,
	)),
	// Scree
	bouldery(sprops(
		BlockType::Gravel,
		BlockType::Gravel,
		TreeDensity::None,
		TreeSpecies::Broadleaf,
		true,
	)),
];

impl SurfaceKind {
	#[inline]
	pub fn props(self) -> &'static SurfaceProps {
		&SURFACE_PROPS[self as usize]
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnShape {
	/// World y of the surface block.
	pub height: i32,
	pub surface: SurfaceKind,
	/// World y of the local water surface: SEA_LEVEL in the ocean, above
	/// it inside river channels. Water fills `height+1..=water`.
	pub water: i32,
	/// Falling-water span `(bottom, top)` inclusive, filled with
	/// `WaterFall` above this column's pool where a neighboring pool sits
	/// higher — the sheet that visually connects stepped river pools.
	pub fall: Option<(i32, i32)>,
}

/// Whether a column holds standing water (its water surface sits above
/// its terrain surface).
#[inline]
pub(super) fn holds_water(f: &Fields) -> bool {
	f.water > f.h
}

/// Discretize fields + neighbor fields into a ColumnShape. Both call
/// paths (per-column and padded chunk grid) feed identical f64 inputs
/// here, so their classifications are bit-identical.
///
/// Order matters: ocean, then the (dithered) beach band, then steepness
/// — so cliffs are bare rock at any altitude, and snowfields break into
/// rock faces wherever the terrain stands too steep to hold snow.
/// `nbrs` is the neighbor fields at `[x-1, x+1, z-1, z+1]`.
pub(super) fn classify(f: &Fields, nbrs: [Fields; 4], x: i32, z: i32, seed: f64) -> ColumnShape {
	let [nxm, nxp, nzm, nzp] = &nbrs;
	let height = f.h.floor() as i32;
	let o = seed + SURF_DITHER_OFF;
	let dith = noise2(
		o + f64::from(x) * SURF_DITHER_FREQ,
		o + f64::from(z) * SURF_DITHER_FREQ,
	);
	// Central-difference rise per block, the max of both axes — from the
	// pre-river surfaces, so channels don't read as cliffs.
	let slope = (nxp.h_pre - nxm.h_pre)
		.abs()
		.max((nzp.h_pre - nzm.h_pre).abs())
		* 0.5;
	// Dithered altitude for the stone/snow lines (pre-river surface),
	// both shifted in lockstep by regional temperature so the rock band
	// between them survives at every latitude-equivalent.
	let hd = f.h_pre + dith * 6.0;
	let shift = f.temp * TEMP_ALT_SHIFT;
	let water = f.water.floor() as i32;
	// Rock faces below the snow line break into gravel scree patches.
	let rocky = |below_snow: bool| {
		if below_snow {
			let o2 = seed + SCREE_OFF;
			let gv = noise2(
				o2 + f64::from(x) * SCREE_FREQ,
				o2 + f64::from(z) * SCREE_FREQ,
			);
			if gv > SCREE_THRESH {
				return SurfaceKind::Scree;
			}
		}
		SurfaceKind::Mountain
	};
	let surface = if height < SEA_LEVEL {
		SurfaceKind::Ocean
	} else if water > height {
		if f.lake {
			SurfaceKind::Lake
		} else {
			SurfaceKind::River
		}
	} else if f64::from(height) <= f64::from(SEA_LEVEL) + 1.0 + (dith + 1.0) * 1.2 {
		// The waterline band: gravel where the land behind it climbs
		// steeply, sand elsewhere — dithered so the two interleave.
		if slope > GRAVEL_SLOPE + 0.2 * dith {
			SurfaceKind::GravelShore
		} else {
			SurfaceKind::Beach
		}
	} else if f64::from(height) <= f64::from(SEA_LEVEL) + 3.0 + (dith + 1.0) * 1.5
		&& slope < MARSH_SLOPE
		&& f.veg > MARSH_VEG
	{
		SurfaceKind::Marsh // flat, lush, barely above the waterline
	} else if slope > STEEP_SLOPE + 0.3 * dith {
		rocky(hd < SNOW_LINE + shift) // cliffs bare at any altitude
	} else if hd >= SNOW_LINE + shift {
		SurfaceKind::Snow
	} else if hd >= f64::from(STONE_SURFACE) + shift {
		rocky(true)
	} else if f.temp > WARM_T + 0.1 * dith {
		SurfaceKind::Savanna
	} else if f.temp < COLD_T + 0.1 * dith {
		if f.veg > TAIGA_VEG {
			SurfaceKind::Taiga
		} else {
			SurfaceKind::Plains
		}
	} else if f.veg > FOREST_VEG + 0.1 * dith {
		SurfaceKind::Forest
	} else {
		SurfaceKind::Plains
	};
	// Marsh puddles: scattered columns dip one block and fill with water
	// flush to the marsh grade. The carve gate (height > water) goes
	// false under pools, and `fall` below reads raw Fields, so pools
	// never cascade.
	let (height, water) = if surface == SurfaceKind::Marsh {
		let po = seed + PUDDLE_OFF;
		if noise2(
			po + f64::from(x) * PUDDLE_FREQ,
			po + f64::from(z) * PUDDLE_FREQ,
		) > 0.55
		{
			(height - 1, height)
		} else {
			(height, water)
		}
	} else {
		(height, water)
	};
	// Falling water above this column's pool wherever a neighboring pool
	// sits higher — quantized river levels descend in steps, and the fall
	// sheet is what connects them. Only in-channel columns fall (dry
	// banks whose terrain happens to sit below a neighbor pool don't).
	let fall = if holds_water(f) {
		nbrs.iter()
			.filter(|n| holds_water(n))
			.map(|n| n.water.floor() as i32)
			.max()
			.filter(|&top| top > water)
			.map(|top| (water + 1, top))
	} else {
		None
	};
	ColumnShape {
		height,
		surface,
		water,
		fall,
	}
}

/// One cell of the column rules: the surface block, DIRT_DEPTH of
/// subsoil, the kind's stratum band (if any), then stone; water up to
/// the column's water level, air above.
#[inline]
pub(super) fn column_block(wy: i32, shape: ColumnShape) -> BlockType {
	let h = shape.height;
	if wy > h {
		if wy <= shape.water {
			BlockType::Water
		} else if shape.fall.is_some_and(|(b, t)| (b..=t).contains(&wy)) {
			BlockType::WaterFall
		} else {
			BlockType::Air
		}
	} else {
		let p = shape.surface.props();
		if wy == h {
			p.top
		} else if wy > h - DIRT_DEPTH {
			p.subsoil
		} else if wy > h - DIRT_DEPTH - p.band_depth {
			p.band
		} else {
			BlockType::Stone
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Pins every kind's table row. Iterating `ALL` with an exhaustive
	/// match makes a new kind a compile error here instead of a silently
	/// untested row.
	#[test]
	fn surface_props_are_pinned() {
		use SurfaceKind::*;
		use TreeDensity as D;
		use TreeSpecies as S;
		assert_eq!(SurfaceKind::ALL.len(), SURFACE_PROPS.len());
		for (i, k) in SurfaceKind::ALL.into_iter().enumerate() {
			assert_eq!(k as usize, i, "ALL follows declaration order");
			let p = k.props();
			let (top, subsoil, density, species, spawnable) = match k {
				Ocean => (
					BlockType::Sand,
					BlockType::Sand,
					D::None,
					S::Broadleaf,
					false,
				),
				Beach => (
					BlockType::Sand,
					BlockType::Sand,
					D::None,
					S::Broadleaf,
					true,
				),
				Plains => (
					BlockType::Grass,
					BlockType::Dirt,
					D::Sparse,
					S::Broadleaf,
					true,
				),
				Mountain => (
					BlockType::Stone,
					BlockType::Stone,
					D::None,
					S::Broadleaf,
					true,
				),
				Snow => (
					BlockType::Snow,
					BlockType::Stone,
					D::None,
					S::Broadleaf,
					true,
				),
				River => (
					BlockType::Sand,
					BlockType::Sand,
					D::None,
					S::Broadleaf,
					false,
				),
				Forest => (
					BlockType::Grass,
					BlockType::Dirt,
					D::Dense,
					S::Broadleaf,
					true,
				),
				Taiga => (BlockType::Grass, BlockType::Dirt, D::Normal, S::Pine, true),
				Savanna => (
					BlockType::DryGrass,
					BlockType::Dirt,
					D::Sparse,
					S::FlatTop,
					true,
				),
				Marsh => (BlockType::Mud, BlockType::Mud, D::None, S::Broadleaf, false),
				GravelShore => (
					BlockType::Gravel,
					BlockType::Gravel,
					D::None,
					S::Broadleaf,
					true,
				),
				Lake => (
					BlockType::Sand,
					BlockType::Sand,
					D::None,
					S::Broadleaf,
					false,
				),
				Scree => (
					BlockType::Gravel,
					BlockType::Gravel,
					D::None,
					S::Broadleaf,
					true,
				),
			};
			assert_eq!((p.top, p.subsoil), (top, subsoil), "{k:?} palette");
			assert_eq!(p.tree_density, density, "{k:?} density");
			assert_eq!(p.tree_species, species, "{k:?} species");
			assert_eq!(p.spawnable, spawnable, "{k:?} spawn");
			let (band, band_depth) = match k {
				Ocean | Beach | River | Lake => (BlockType::Sandstone, 4),
				_ => (BlockType::Stone, 0),
			};
			assert_eq!((p.band, p.band_depth), (band, band_depth), "{k:?} band");
			assert_eq!(p.boulders, matches!(k, Mountain | Scree), "{k:?} boulders");
		}
	}
}
