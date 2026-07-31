//! Terrain generation. Two low-frequency control fields shape the world
//! into coherent regions — a single fBm heightmap would scatter random
//! peaks and pits everywhere instead:
//!
//! - **continentalness** picks the base elevation through a spline (deep
//!   ocean → shelf → coast → plains → highlands) and gates mountains to
//!   well inland;
//! - **roughness** gates where mountain relief appears at all;
//! - a low-frequency **ridge** field draws the mountain forms themselves
//!   (its absolute value creases into valleys at zero crossings and rounds
//!   off over maxima, so ranges rise from their base with walkable slopes);
//! - a mid-frequency **detail** fBm supplies the local relief.
//!
//! Every function here is a pure function of `(x, z, seed)` — column
//! content never depends on generation order.

use std::sync::Arc;

use crate::block::BlockType;
use crate::chunk::{ChunkData, ChunkPos, AIR_CHUNK, CHUNK_SIZE, WORLD_VCHUNK};
use crate::decoration::{decorate_chunk, trees_near};
use crate::simplex::{fbm2, noise3};

// The terrain tuning surface. Frequencies are per-block; amplitudes in
// blocks. Field seeds are domain offsets so the three fields decorrelate.
const CONTINENT_FREQ: f64 = 0.0015;
const ROUGH_FREQ: f64 = 0.004;
const DETAIL_FREQ: f64 = 0.015;
/// Mountain forms live on their own broad field — far lower frequency than
/// the detail fBm, so 90 blocks of amplitude spreads over hundreds of
/// blocks of run instead of spiking.
const RIDGE_FREQ: f64 = 0.004;
const PLAINS_AMP: f64 = 6.0;
/// Extra ridged amplitude in fully mountainous regions.
const MOUNTAIN_AMP: f64 = 90.0;
const ROUGH_SEED_OFF: f64 = 4079.0;
const DETAIL_SEED_OFF: f64 = 8161.0;
const RIDGE_SEED_OFF: f64 = 12281.0;

/// Sea sits at mid-world: oceans below, up to ~112 blocks of relief above.
pub const SEA_LEVEL: i32 = 128;
/// Asymptotic terrain ceiling: heights above CEILING_KNEE are compressed
/// toward this limit but never reach it, so the tallest peaks round off
/// instead of clamping into flat tops.
pub const MAX_TERRAIN: f64 = 240.0;
/// Where the soft ceiling starts to bite.
const CEILING_KNEE: f64 = 200.0;
/// Above this the surface turns to bare rock.
pub const STONE_SURFACE: i32 = 190;
/// Blocks of dirt (or sand) between the surface and the stone below it.
pub const DIRT_DEPTH: i32 = 3;

// Caves: the intersection of two independent 3D-noise zero bands. One
// band alone carves space-filling sheets; requiring both fields near zero
// reduces the void to worm-like tunnel curves (and the carved volume
// roughly quadratically). Land columns only (no ocean-floor caves → no
// water walls at carved openings), never the bedrock layer at y=0.
// Frequency sets tunnel spacing (zero bands sit ~half a wavelength
// apart); threshold/frequency sets bore width in blocks — so lower
// frequency + higher threshold = fewer, larger caves.
const CAVE_FREQ: f64 = 0.02;
const CAVE_THRESHOLD: f64 = 0.15;
const CAVE_SEED_OFF: f64 = 16193.0;
const CAVE_SEED_OFF2: f64 = 27449.0;
const CAVE_FLOOR: i32 = 1;

/// Base elevation by continentalness: piecewise-linear anchors from deep
/// ocean floor to highland base. Monotonic in `c`.
const BASE_SPLINE: [(f64, f64); 5] = [
	(-1.0, 60.0),
	(-0.2, 120.0),
	(0.0, 130.0),
	(0.3, 140.0),
	(1.0, 170.0),
];

fn spline(c: f64) -> f64 {
	let first = BASE_SPLINE[0];
	if c <= first.0 {
		return first.1;
	}
	for w in BASE_SPLINE.windows(2) {
		let (x0, y0) = w[0];
		let (x1, y1) = w[1];
		if c <= x1 {
			return y0 + (y1 - y0) * (c - x0) / (x1 - x0);
		}
	}
	BASE_SPLINE[BASE_SPLINE.len() - 1].1
}

fn smoothstep(t: f64) -> f64 {
	let t = t.clamp(0.0, 1.0);
	t * t * (3.0 - 2.0 * t)
}

/// Mountain gate: 0 at the coast, 1 well inland — amplitude fades to
/// plains level near shores so there are no cliffs at the beach.
fn inland(c: f64) -> f64 {
	smoothstep((c - 0.05) / 0.30)
}

/// Compress heights above CEILING_KNEE asymptotically toward MAX_TERRAIN:
/// tanh keeps the curve tangent to the identity at the knee and never
/// reaches the limit, so no peak ever clamps into a flat top.
fn soft_ceiling(h: f64) -> f64 {
	if h <= CEILING_KNEE {
		h
	} else {
		let span = MAX_TERRAIN - CEILING_KNEE;
		CEILING_KNEE + span * ((h - CEILING_KNEE) / span).tanh()
	}
}

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
	/// Bare rock above STONE_SURFACE.
	Mountain,
}

#[derive(Clone, Copy, Debug)]
pub struct ColumnShape {
	/// World y of the surface block.
	pub height: i32,
	pub surface: SurfaceKind,
	/// World y of the local water surface: SEA_LEVEL everywhere today,
	/// above it inside future river channels. Water fills `height+1..=water`.
	pub water: i32,
}

/// fBm concentrates near zero (octave sums rarely approach ±1); gain +
/// clamp spreads a field over its nominal [-1, 1] so spline anchors and
/// gates see their full input range.
fn gained(v: f64, gain: f64) -> f64 {
	(v * gain).clamp(-1.0, 1.0)
}

/// The continuous per-column terrain fields, before block-grid
/// discretization and surface classification. Split from `classify` so
/// classification can sample neighbor heights (for slope) without
/// recursing: `fields`/`raw_height` never classify, `classify` never
/// evaluates noise beyond its own inputs.
#[derive(Clone, Copy, Default)]
struct Fields {
	/// Surface height (f64, soft-ceilinged, ≥ 1; floor = the block y).
	h: f64,
}

fn fields(x: i32, z: i32, seed: f64) -> Fields {
	let (xf, zf) = (f64::from(x), f64::from(z));
	let c = gained(fbm2(xf, zf, CONTINENT_FREQ, 2, seed), 2.2);
	let r = gained(fbm2(xf, zf, ROUGH_FREQ, 2, seed + ROUGH_SEED_OFF), 2.2);
	let d = fbm2(xf, zf, DETAIL_FREQ, 4, seed + DETAIL_SEED_OFF);
	let m = fbm2(xf, zf, RIDGE_FREQ, 3, seed + RIDGE_SEED_OFF);

	let base = spline(c);
	// 0..1 mountainousness, gated to inland regions.
	let rough = smoothstep(((r + 1.0) * 0.5 - 0.55) / 0.3) * inland(c);
	// Ridged (|m|): ranges crease into valleys at zero crossings and only
	// ever rise above their base. The smoothstep flattens the derivative at
	// both ends, so summits and valley floors round off instead of spiking.
	let ridge = smoothstep(gained(m, 2.2).abs());
	let h = base + gained(d, 2.0) * PLAINS_AMP + ridge * MOUNTAIN_AMP * rough;
	Fields {
		h: soft_ceiling(h).max(1.0),
	}
}

/// The height a neighbor column contributes to slope classification.
fn raw_height(x: i32, z: i32, seed: f64) -> f64 {
	fields(x, z, seed).h
}

/// Discretize fields + neighbor heights into a ColumnShape. Both call
/// paths (per-column and padded chunk grid) feed identical f64 inputs
/// here, so their classifications are bit-identical.
fn classify(f: &Fields, _hxm: f64, _hxp: f64, _hzm: f64, _hzp: f64) -> ColumnShape {
	let height = f.h.floor() as i32;
	let surface = if height < SEA_LEVEL {
		SurfaceKind::Ocean
	} else if height <= SEA_LEVEL + 1 {
		SurfaceKind::Beach
	} else if height >= STONE_SURFACE {
		SurfaceKind::Mountain
	} else {
		SurfaceKind::Plains
	};
	ColumnShape {
		height,
		surface,
		water: SEA_LEVEL,
	}
}

/// The per-column terrain function: control fields → base + modulated
/// detail → height and surface kind.
pub fn column_shape(x: i32, z: i32, seed: f64) -> ColumnShape {
	classify(
		&fields(x, z, seed),
		raw_height(x - 1, z, seed),
		raw_height(x + 1, z, seed),
		raw_height(x, z - 1, seed),
		raw_height(x, z + 1, seed),
	)
}

pub fn terrain_height(x: i32, z: i32, seed: f64) -> i32 {
	column_shape(x, z, seed).height
}

type ShapeMap = [[ColumnShape; CHUNK_SIZE]; CHUNK_SIZE];

/// Column shapes for a chunk, indexed `[z][x]`. Evaluates the fields once
/// per column of a one-column-padded grid (18×18 = 324 evals) so interior
/// columns classify against their neighbors' heights without re-deriving
/// them — 4× cheaper than the 5-evaluation `column_shape` path, and
/// bit-identical to it (same f64 inputs to the same functions).
fn column_shapes(cx: i32, cz: i32, seed: f64) -> ShapeMap {
	const PAD: usize = CHUNK_SIZE + 2;
	let (px, pz) = (cx * CHUNK_SIZE as i32, cz * CHUNK_SIZE as i32);
	let mut grid = [[Fields::default(); PAD]; PAD];
	for (gz, row) in grid.iter_mut().enumerate() {
		for (gx, f) in row.iter_mut().enumerate() {
			*f = fields(gx as i32 - 1 + px, gz as i32 - 1 + pz, seed);
		}
	}
	let mut shapes = [[ColumnShape {
		height: 0,
		surface: SurfaceKind::Plains,
		water: SEA_LEVEL,
	}; CHUNK_SIZE]; CHUNK_SIZE];
	for (z, row) in shapes.iter_mut().enumerate() {
		for (x, s) in row.iter_mut().enumerate() {
			let (gx, gz) = (x + 1, z + 1);
			*s = classify(
				&grid[gz][gx],
				grid[gz][gx - 1].h,
				grid[gz][gx + 1].h,
				grid[gz - 1][gx].h,
				grid[gz + 1][gx].h,
			);
		}
	}
	shapes
}

/// Whether the cave field carves this cell (pure; decoration needs it to
/// judge "surface not carved" without building the chunk). Short-circuits
/// so most cells pay for one noise evaluation, not two.
pub fn carved(x: i32, y: i32, z: i32, seed: f64) -> bool {
	let band = |off: f64| {
		let o = seed + off;
		let n = noise3(
			o + f64::from(x) * CAVE_FREQ,
			o + f64::from(y) * CAVE_FREQ,
			o + f64::from(z) * CAVE_FREQ,
		);
		n.abs() < CAVE_THRESHOLD
	};
	band(CAVE_SEED_OFF) && band(CAVE_SEED_OFF2)
}

/// One cell of the column rules: stone below DIRT_DEPTH of topsoil;
/// topsoil is sand on ocean floors and beaches, bare stone on mountain
/// tops, grass-over-dirt otherwise; water up to the column's water
/// level, air above.
#[inline]
fn column_block(wy: i32, shape: ColumnShape) -> BlockType {
	let h = shape.height;
	if wy > h {
		if wy <= shape.water {
			BlockType::Water
		} else {
			BlockType::Air
		}
	} else if wy <= h - DIRT_DEPTH {
		BlockType::Stone
	} else {
		match shape.surface {
			SurfaceKind::Ocean | SurfaceKind::Beach => BlockType::Sand,
			SurfaceKind::Mountain => BlockType::Stone,
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

/// Column fill + cave carve, in storage order (x contiguous). Carving
/// runs only on terrain cells of land columns, so ocean stacks and sky
/// chunks never pay for the 3D noise.
fn fill_chunk(data: &mut ChunkData, pos: ChunkPos, shapes: &ShapeMap, seed: f64) {
	let (px, py, pz) = (
		pos.x * CHUNK_SIZE as i32,
		pos.y * CHUNK_SIZE as i32,
		pos.z * CHUNK_SIZE as i32,
	);
	for y in 0..CHUNK_SIZE {
		let wy = y as i32 + py;
		for (z, srow) in shapes.iter().enumerate() {
			let row = (y * CHUNK_SIZE + z) * CHUNK_SIZE;
			let out = &mut data.blocks[row..row + CHUNK_SIZE];
			for (x, (b, &shape)) in out.iter_mut().zip(srow).enumerate() {
				let mut block = column_block(wy, shape);
				let carveable =
					matches!(block, BlockType::Stone | BlockType::Dirt | BlockType::Grass);
				if carveable
					&& shape.height > shape.water
					&& wy >= CAVE_FLOOR
					&& carved(x as i32 + px, wy, z as i32 + pz, seed)
				{
					block = BlockType::Air;
				}
				*b = block as u8;
			}
		}
	}
}

/// Build one chunk: fill → carve (inside fill) → decorate.
pub fn build_chunk(pos: ChunkPos, seed: f64) -> ChunkData {
	let n = CHUNK_SIZE as i32;
	let mut data = ChunkData::default();
	fill_chunk(&mut data, pos, &column_shapes(pos.x, pos.z, seed), seed);
	decorate_chunk(&mut data, pos, &trees_near(pos.x * n, pos.z * n, seed));
	data
}

/// Generate the full vertical chunk stack of a column in one call,
/// computing the 16×16 shape map once instead of once per chunk. Chunks
/// entirely above the terrain and the sea resolve to the shared AIR_CHUNK
/// sentinel — no fill, no allocation, recognized by pointer downstream.
pub fn build_column(cx: i32, cz: i32, seed: f64) -> Vec<Arc<ChunkData>> {
	let n = CHUNK_SIZE as i32;
	let shapes = column_shapes(cx, cz, seed);
	// A column's solid+liquid content reaches to height *or* its local
	// water surface (river water can sit above carved terrain).
	let max_h = shapes
		.iter()
		.flatten()
		.map(|s| s.height.max(s.water))
		.max()
		.expect("non-empty shape map");
	let trees = trees_near(cx * n, cz * n, seed);
	// Sky starts above both the terrain and anything decoration can reach.
	let solid_top = trees.iter().map(|t| t.top()).fold(max_h, i32::max);
	(0..WORLD_VCHUNK)
		.map(|cy| {
			let bottom = cy * n;
			if bottom > solid_top && bottom > SEA_LEVEL {
				return AIR_CHUNK.clone();
			}
			let pos = ChunkPos::new(cx, cy, cz);
			let mut data = ChunkData::default();
			fill_chunk(&mut data, pos, &shapes, seed);
			decorate_chunk(&mut data, pos, &trees);
			Arc::new(data)
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::WORLD_VBLOCK;

	#[test]
	fn spline_is_monotonic_and_anchored() {
		let mut prev = f64::MIN;
		let mut c = -1.2;
		while c <= 1.2 {
			let v = spline(c);
			assert!(v >= prev, "monotonic at c={c}");
			prev = v;
			c += 0.01;
		}
		for (c, v) in BASE_SPLINE {
			assert_eq!(spline(c), v, "anchor at {c}");
		}
	}

	#[test]
	fn heights_stay_in_bounds() {
		let seed = 77.7;
		for x in (-2048..2048).step_by(61) {
			for z in (-2048..2048).step_by(67) {
				let h = terrain_height(x, z, seed);
				assert!((1..=MAX_TERRAIN as i32).contains(&h), "h={h} at {x},{z}");
				assert!(h < WORLD_VBLOCK, "terrain never reaches the ceiling");
			}
		}
	}

	/// All four surface kinds occur in a large sample, with open terrain
	/// (plains + ocean) dominating mountains.
	#[test]
	fn regions_are_present_and_balanced() {
		let seed = 1234.5678;
		let (mut ocean, mut beach, mut plains, mut mountain) = (0u32, 0u32, 0u32, 0u32);
		let mut total = 0u32;
		for x in (-4096..4096).step_by(32) {
			for z in (-4096..4096).step_by(32) {
				total += 1;
				match column_shape(x, z, seed).surface {
					SurfaceKind::Ocean => ocean += 1,
					SurfaceKind::Beach => beach += 1,
					SurfaceKind::Plains => plains += 1,
					SurfaceKind::Mountain => mountain += 1,
				}
			}
		}
		let pct = |n: u32| f64::from(n) * 100.0 / f64::from(total);
		assert!(pct(ocean) > 5.0, "ocean {:.1}%", pct(ocean));
		assert!(beach > 0, "beaches exist");
		assert!(pct(plains) > 25.0, "plains {:.1}%", pct(plains));
		assert!(mountain > 0, "mountains exist");
		assert!(
			pct(mountain) < pct(plains) + pct(ocean),
			"mountains {:.1}% don't dominate",
			pct(mountain)
		);
	}
}
