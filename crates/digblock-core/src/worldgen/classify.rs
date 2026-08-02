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
/// Surface-classification dither: a ~9-block-wavelength noise nudges the
/// beach band, the stone line, and the snow line (±6 blocks) so altitude
/// zones meet in ragged natural edges instead of world-wide contours.
const SURF_DITHER_FREQ: f64 = 0.11;
const SURF_DITHER_OFF: f64 = 70999.0;
/// Rise (blocks per block) beyond which a face is bare rock at any
/// altitude — grass doesn't hold on ~54°+ slopes.
const STEEP_SLOPE: f64 = 1.4;

/// What grows (or doesn't) on a column's surface — the seam where a
/// future biome system attaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceKind {
	/// Below sea level: sand floor under water.
	Ocean,
	/// At the water line: sand above water.
	Beach,
	/// Grass over dirt.
	Plains,
	/// Bare rock above STONE_SURFACE — or any steep face.
	Mountain,
	/// Snow-capped rock above the (dithered) snow line.
	Snow,
	/// Inside a river channel: sand bed under river water.
	River,
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
	// Dithered altitude for the stone/snow lines (pre-river surface).
	let hd = f.h_pre + dith * 6.0;
	let water = f.water.floor() as i32;
	let surface = if height < SEA_LEVEL {
		SurfaceKind::Ocean
	} else if water > height {
		SurfaceKind::River
	} else if f64::from(height) <= f64::from(SEA_LEVEL) + 1.0 + (dith + 1.0) * 1.2 {
		SurfaceKind::Beach
	} else if slope > STEEP_SLOPE + 0.3 * dith {
		SurfaceKind::Mountain
	} else if hd >= SNOW_LINE {
		SurfaceKind::Snow
	} else if hd >= f64::from(STONE_SURFACE) {
		SurfaceKind::Mountain
	} else {
		SurfaceKind::Plains
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

/// One cell of the column rules: stone below DIRT_DEPTH of topsoil;
/// topsoil is sand on ocean floors and beaches, bare stone on mountain
/// tops, grass-over-dirt otherwise; water up to the column's water
/// level, air above.
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
	} else if wy <= h - DIRT_DEPTH {
		BlockType::Stone
	} else {
		match shape.surface {
			SurfaceKind::Ocean | SurfaceKind::Beach | SurfaceKind::River => BlockType::Sand,
			SurfaceKind::Mountain => BlockType::Stone,
			SurfaceKind::Snow => {
				if wy == h {
					BlockType::Snow
				} else {
					BlockType::Stone
				}
			}
			SurfaceKind::Plains => {
				if wy == h {
					BlockType::Grass
				} else {
					BlockType::Dirt
				}
			}
		}
	}
}
