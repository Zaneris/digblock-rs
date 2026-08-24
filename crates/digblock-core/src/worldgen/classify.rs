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
	/// Bare rock above the (temperature-shifted) stone line — or any steep face.
	Mountain,
	/// Snow-capped rock above the (dithered, temperature-shifted) snow line.
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
	/// The y span over which some neighbour holds water — its raw pool, or
	/// the puddle its roll dips it into — inclusive and empty as `(MAX,
	/// MIN)`: this column's cells inside it are that water's wall and the
	/// carve leaves them alone. A span, not a ceiling, so a cave BELOW it
	/// still runs; read wide, since a spare wall costs one carve cell.
	pub wall: (i32, i32),
}

/// Whether a column holds standing water (its water surface sits above
/// its terrain surface).
#[inline]
pub(super) fn holds_water(f: &Fields) -> bool {
	f.water > f.h
}

/// The highest surface [`classify`] could return for these fields, from
/// the centre field alone — `contain` raises, and this is the bound it
/// raises within. `column_shape_above` rejects against this rather than
/// the raw height; `containment_stays_inside_the_reject_bound` is what
/// keeps the two honest.
#[inline]
pub(super) fn height_ceiling(f: &Fields) -> i32 {
	(f.h.floor() as i32 + BANK_LIFT).max(f.water.floor() as i32)
}

/// Discretize fields + neighbor fields into a ColumnShape. Both call paths (per-column and
/// padded chunk grid) feed identical f64 inputs here, so their classifications are bit-identical.
/// Order matters: ocean, then the (dithered) beach band, then steepness — so cliffs are bare
/// rock at any altitude, and snowfields break into rock faces wherever the terrain stands too
/// steep to hold snow. `nbrs` is the neighbor fields at `[x-1, x+1, z-1, z+1]`.
pub(super) fn classify(f: &Fields, nbrs: [Fields; 4], x: i32, z: i32, seed: f64) -> ColumnShape {
	let [nxm, nxp, nzm, nzp] = &nbrs;
	let height = f.h.floor() as i32;
	// Central-difference rise per block, the max of both axes — from the
	// pre-river surfaces, so channels don't read as cliffs.
	let slope = (nxp.h_pre - nxm.h_pre)
		.abs()
		.max((nzp.h_pre - nzm.h_pre).abs())
		* 0.5;
	let surface = surface_kind(f, slope, x, z, seed);
	let water = f.water.floor() as i32;
	let wall = nbrs
		.iter()
		.filter(|n| holds_water(n))
		.fold((i32::MAX, i32::MIN), |(lo, hi), n| {
			(
				lo.min(n.h.floor() as i32 + 1),
				hi.max(n.water.floor() as i32),
			)
		});
	let (height, water) = contain(f, &nbrs, wall, height, water);
	// Marsh puddles: scattered columns dip one block and fill with water
	// flush to the marsh grade. Two gates, and both are containment: no
	// neighbour may hold a pool — which is also what keeps `contain`'s
	// reading of a marsh neighbour honest, since a column that could dip
	// is never beside one asking — and every neighbour's grade must reach
	// the level filled. That second gate reads raw heights on purpose: a
	// neighbour dipping from the same grade fills to it, and one standing
	// above it walls the cell, so the undipped height answers both without
	// asking whether the neighbour is marsh at all.
	let (height, water) = if surface == SurfaceKind::Marsh
		&& !nbrs.iter().any(holds_water)
		&& nbrs.iter().all(|n| n.h.floor() as i32 >= height)
		&& puddle_roll(x, z, seed)
	{
		(height - 1, height)
	} else {
		(height, water)
	};
	// A neighbour's puddle is walled AFTER containment, because the bank
	// rule reads `wall` and would lift toward a column that only might dip.
	// A puddle's gate already makes every neighbour's ground reach its
	// level; what the wall does is keep the carve from taking that ground.
	let wall = nbrs
		.iter()
		.zip([(-1, 0), (1, 0), (0, -1), (0, 1)])
		.filter(|(n, (dx, dz))| !holds_water(n) && could_puddle(n, x + dx, z + dz, seed))
		.fold(wall, |(lo, hi), (n, _)| {
			let level = n.h.floor() as i32;
			(lo.min(level), hi.max(level))
		});
	ColumnShape {
		height,
		surface,
		water,
		wall,
	}
}

/// Whether the puddle roll dips this column — one noise, shared so the
/// column and its neighbours cannot roll differently.
fn puddle_roll(x: i32, z: i32, seed: f64) -> bool {
	let po = seed + PUDDLE_OFF;
	noise2(
		po + f64::from(x) * PUDDLE_FREQ,
		po + f64::from(z) * PUDDLE_FREQ,
	) > 0.55
}

/// Whether a column might be a puddle, from its own fields alone — what a
/// NEIGHBOUR can know of it. Slope is the one input a neighbour cannot
/// see, and a column read as flat claims at least the puddle it gets, so
/// every real puddle is walled and a false one spares a carve cell.
fn could_puddle(f: &Fields, x: i32, z: i32, seed: f64) -> bool {
	surface_kind(f, 0.0, x, z, seed) == SurfaceKind::Marsh && puddle_roll(x, z, seed)
}

/// The surface kind of a column from its fields and its rise per block.
fn surface_kind(f: &Fields, slope: f64, x: i32, z: i32, seed: f64) -> SurfaceKind {
	let height = f.h.floor() as i32;
	let o = seed + SURF_DITHER_OFF;
	let dith = noise2(
		o + f64::from(x) * SURF_DITHER_FREQ,
		o + f64::from(z) * SURF_DITHER_FREQ,
	);
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
	if height < SEA_LEVEL {
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
	}
}

/// How far a bank may be lifted to wall the pool beside it. Load-bearing
/// beyond taste: every raise here is bounded by the column's OWN fields —
/// this much above its height, or up to its own water line — which is
/// what lets [`super::column_shape_above`] reject on the centre field
/// alone. Widen it and that rejection needs the neighbours too.
const BANK_LIFT: i32 = 1;

/// Lateral containment: worldgen emits water at the fluid sim's fixpoint,
/// so a pool never stands against air that would pour once an edit
/// scheduled it. Two rules, both decided from this column's own fields
/// and its four neighbours' — one pass, no iteration, because each only
/// ever RAISES ground and a raise cannot expose anything new.
///
/// One exception is deliberate: a weir's lip IS left open, so the settle
/// pass pours a real fall through it instead of worldgen painting a
/// sheet no player could build.
fn contain(
	f: &Fields,
	nbrs: &[Fields; 4],
	wall: (i32, i32),
	height: i32,
	water: i32,
) -> (i32, i32) {
	// What each neighbour covers this column to: its pool if it holds
	// one, else its ground plus whatever lift the bank rule will give it.
	// A neighbour lifts to at most OUR level, so `>= water` is exact.
	let covers = |n: &Fields| {
		if holds_water(n) {
			n.water.floor() as i32
		} else {
			n.h.floor() as i32 + BANK_LIFT
		}
	};
	if !holds_water(f) {
		// Bank: stand up to the highest pool leaning on us, no further.
		return match wall.1 {
			t if t > height => ((height + BANK_LIFT).min(t), water),
			_ => (height, water),
		};
	}
	// Anything we cannot be walled against — a pool below us, or ground
	// that falls away — makes our edge a weir: one block deep on solid
	// ground, which is the one shape that CAN spill. The sim's spread rule
	// reads the FEEDING cell's floor, so a pool deeper than one block
	// never pours sideways at all, and the drop below the lip is
	// irrelevant to the rule — a river ending at a cliff shallows and
	// pours over exactly as one stepping into the pool below does.
	if nbrs.iter().any(|n| covers(n) < water) {
		return (water - 1, water);
	}
	(height, water)
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

	/// The table's rows hang together, asserted as the rules that make them coherent rather than
	/// as a copy of their values: what a kind is made of decides its band, whether it grows trees
	/// decides whether you can stand on it. The values are already pinned to the BYTE by
	/// `a_shipped_generator_never_moves` and re-derived by `tests/worldgen.rs`. The `match`
	/// stays exhaustive on purpose: a new kind is a compile error, not a silently untested row.
	#[test]
	fn surface_props_are_pinned() {
		use SurfaceKind::*;
		use TreeDensity as D;
		use TreeSpecies as S;
		assert_eq!(SurfaceKind::ALL.len(), SURFACE_PROPS.len());
		let mut wooded = 0;
		let mut sandy = 0;
		for (i, k) in SurfaceKind::ALL.into_iter().enumerate() {
			assert_eq!(k as usize, i, "ALL follows declaration order");
			let p = k.props();

			// Every kind says whether it sits under water. Exhaustive:
			// a new kind lands here as a compile error.
			let underwater = match k {
				Ocean | River | Lake => true,
				Beach | Plains | Mountain | Snow | Forest | Taiga | Savanna | Marsh
				| GravelShore | Scree => false,
			};
			if underwater {
				assert!(!p.spawnable, "{k:?} is underwater and spawnable");
				assert_eq!(p.tree_density, D::None, "{k:?} grows trees underwater");
			}

			// A sandy surface is what puts sandstone under it — the band
			// follows the material, not a list of kinds.
			let is_sand = p.top == BlockType::Sand;
			let (band, depth) = if is_sand {
				(BlockType::Sandstone, 4)
			} else {
				(BlockType::Stone, 0)
			};
			assert_eq!((p.band, p.band_depth), (band, depth), "{k:?} band");
			sandy += usize::from(is_sand);

			// Trees need soil to stand in, and somewhere a player could
			// stand too.
			if p.tree_density != D::None {
				wooded += 1;
				assert!(p.spawnable, "{k:?} grows trees you may not spawn among");
				assert_eq!(p.subsoil, BlockType::Dirt, "{k:?} grows trees without soil");
				assert!(
					matches!(p.top, BlockType::Grass | BlockType::DryGrass),
					"{k:?} grows trees out of {:?}",
					p.top
				);
			} else {
				// With nothing growing, the species field is unused, and
				// an unused field must not drift into meaning something.
				assert_eq!(
					p.tree_species,
					S::Broadleaf,
					"{k:?} names a species it never plants"
				);
			}

			assert_eq!(p.boulders, matches!(k, Mountain | Scree), "{k:?} boulders");
		}
		// Neither sweep may pass by examining nothing.
		assert!(wooded >= 3, "only {wooded} kinds grow trees");
		assert!(sandy >= 3, "only {sandy} kinds are sandy");
	}
}
