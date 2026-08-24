//! Terrain generation. Two low-frequency control fields shape the world into
//! coherent regions — a single fBm heightmap would scatter random peaks and
//! pits everywhere instead:
//!
//! - **continentalness** picks the base elevation through a spline and gates
//!   mountains to well inland;
//! - **roughness** gates where mountain relief appears at all;
//! - a low-frequency **ridge** field draws the mountain forms (its absolute
//!   value creases into valleys at zero crossings and rounds off over maxima);
//! - a mid-frequency **detail** fBm supplies the local relief.
//!
//! Every function here is a pure function of `(x, z, seed)` — column content
//! never depends on generation order. The tuning surface lives in
//! [`TerrainParams`]; the public entry points use `TerrainParams::DEFAULT`.

mod caves;
mod classify;
mod lakes;
pub mod nodes;
pub mod ore;
mod rivers;
mod terrain;

pub use classify::{
	ColumnShape, DIRT_DEPTH, SNOW_LINE, STONE_SURFACE, SurfaceKind, SurfaceProps, TEMP_ALT_SHIFT,
	TreeDensity, TreeSpecies,
};
pub use terrain::{MAX_TERRAIN, SEA_LEVEL, TerrainParams};

use std::sync::Arc;

use crate::block::BlockType;
use crate::chunk::{AIR_CHUNK, CHUNK_SIZE, ChunkData, ChunkPos, WORLD_VCHUNK};
use crate::decoration::{decorate_chunk, structures_near};
use classify::column_block;
use terrain::fields;

/// Every terrain generator that has ever shipped, in shipping order, and NONE is ever removed. A
/// world records one when it is created and keeps it for as long as it exists, so a terrain change
/// adds a row here instead of moving the one existing worlds are standing on — which is the whole
/// of why a terrain change no longer ends them (COMPAT-DESIGN.md §5.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u16)]
pub enum TerrainVersion {
	#[default]
	V1 = 1,
}

impl TerrainVersion {
	/// What a world created now is stamped with.
	pub const CURRENT: Self = Self::V1;
	/// Every generator this build carries, for the pin that each one owes a golden.
	pub const ALL: [Self; 1] = [Self::V1];

	pub const fn to_wire(self) -> u16 {
		self as u16
	}

	/// `None` for a generator this build does not carry — the only thing a worldgen mismatch can
	/// mean now, and it means the CLIENT is too old rather than the world being unreachable. Zero
	/// decodes as the first generator rather than failing: it is what a journal header written
	/// before the slot existed already holds, and the first generator is what wrote it.
	pub fn from_wire(v: u16) -> Option<Self> {
		match v {
			0 | 1 => Some(Self::V1),
			_ => None,
		}
	}
}

/// A world's identity: the number AND the generator that reads it. A bare `f64` was always half an
/// answer — one number under two generators is two different worlds — so the pair travels together
/// wherever terrain is asked, and is RECORDED at creation rather than assumed to be current.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Seed {
	pub value: f64,
	pub version: TerrainVersion,
}

impl Seed {
	pub const fn new(value: f64, version: TerrainVersion) -> Self {
		Self { value, version }
	}

	/// A world being created now. Every other `Seed` comes from a recorded version — a journal
	/// header, a save, or the server's `Welcome` — so this is the only place CURRENT is assumed.
	pub const fn current(value: f64) -> Self {
		Self::new(value, TerrainVersion::CURRENT)
	}
}

// ---- the seam -------------------------------------------------------
//
// The three producers that ARE a generator. A new `TerrainVersion` row makes each match
// non-exhaustive, so the compiler names exactly what a new generator has to supply and nothing
// downstream of `ColumnShape` is asked to care. Version-specific code never learns its own number:
// the arm picks the module, the module takes a plain `f64`.

fn fields_of(p: &TerrainParams, x: i32, z: i32, seed: Seed) -> terrain::Fields {
	match seed.version {
		TerrainVersion::V1 => fields(p, x, z, seed.value),
	}
}

fn classify_of(
	f: &terrain::Fields,
	n: [terrain::Fields; 4],
	x: i32,
	z: i32,
	seed: Seed,
) -> ColumnShape {
	match seed.version {
		TerrainVersion::V1 => classify::classify(f, n, x, z, seed.value),
	}
}

fn cave_of(x: i32, z: i32, seed: Seed) -> caves::CaveColumn {
	match seed.version {
		TerrainVersion::V1 => caves::CaveColumn::new(x, z, seed.value),
	}
}

/// Whether the cave field has removed this cell. Decoration asks it directly (a trunk may not
/// root over a void), which is why it dispatches rather than being re-exported.
pub fn carved(x: i32, y: i32, z: i32, seed: Seed) -> bool {
	match seed.version {
		TerrainVersion::V1 => caves::carved(x, y, z, seed.value),
	}
}

/// The per-column terrain function: control fields → base + modulated
/// detail → height and surface kind.
pub fn column_shape(x: i32, z: i32, seed: Seed) -> ColumnShape {
	let p = &TerrainParams::DEFAULT;
	classify_of(
		&fields_of(p, x, z, seed),
		[
			fields_of(p, x - 1, z, seed),
			fields_of(p, x + 1, z, seed),
			fields_of(p, x, z - 1, seed),
			fields_of(p, x, z + 1, seed),
		],
		x,
		z,
		seed,
	)
}

pub fn terrain_height(x: i32, z: i32, seed: Seed) -> i32 {
	column_shape(x, z, seed).height
}

/// The inclusive y span where pristine soil can live in this column: the surface cell plus
/// `DIRT_DEPTH` of subsoil below it. Every pristine non-stone soil cell — cave-exposed ones
/// included, since the carve removes cells rather than moving them — lies inside;
/// `subsoil_span_bounds_every_pristine_dirt_cell` walks built columns to pin that, so a deeper
/// soil layer fails a test instead of silently escaping callers that scan this span.
pub fn subsoil_span(x: i32, z: i32, seed: Seed) -> (i32, i32) {
	let h = terrain_height(x, z, seed);
	(h - DIRT_DEPTH + 1, h)
}

/// [`column_shape`], but rejecting on altitude from the centre field alone — one evaluation
/// instead of five. Classification only ever LOWERS the discretized height, so a centre height at
/// or below `min_height` proves the column's is too. Survivors pay nothing extra: the centre field
/// is evaluated once either way. `Some` is bit-identical to `column_shape`.
pub fn column_shape_above(x: i32, z: i32, seed: Seed, min_height: i32) -> Option<ColumnShape> {
	let p = &TerrainParams::DEFAULT;
	let f = fields_of(p, x, z, seed);
	if (f.h.floor() as i32) <= min_height {
		return None;
	}
	Some(classify_of(
		&f,
		[
			fields_of(p, x - 1, z, seed),
			fields_of(p, x + 1, z, seed),
			fields_of(p, x, z - 1, seed),
			fields_of(p, x, z + 1, seed),
		],
		x,
		z,
		seed,
	))
}

/// Column shapes over the inclusive rectangle `[x0, x1] × [z0, z1]`,
/// row-major. Shares one padded field grid over the whole block the way
/// [`column_shapes`] does for a chunk — past a couple of columns per side
/// that beats calling `column_shape` per column, and it is bit-identical
/// to doing so.
pub fn column_shapes_in(x0: i32, x1: i32, z0: i32, z1: i32, seed: Seed) -> Vec<ColumnShape> {
	let p = &TerrainParams::DEFAULT;
	let (w, d) = ((x1 - x0 + 1) as usize, (z1 - z0 + 1) as usize);
	let stride = w + 2;
	let mut grid = Vec::with_capacity(stride * (d + 2));
	for gz in 0..d + 2 {
		for gx in 0..stride {
			grid.push(fields_of(p, x0 + gx as i32 - 1, z0 + gz as i32 - 1, seed));
		}
	}
	let mut out = Vec::with_capacity(w * d);
	for z in 0..d {
		for x in 0..w {
			let g = (z + 1) * stride + x + 1;
			out.push(classify_of(
				&grid[g],
				[grid[g - 1], grid[g + 1], grid[g - stride], grid[g + stride]],
				x0 + x as i32,
				z0 + z as i32,
				seed,
			));
		}
	}
	out
}

type ShapeMap = [[ColumnShape; CHUNK_SIZE]; CHUNK_SIZE];

/// Column shapes for a chunk, indexed `[z][x]`. Evaluates the fields once
/// per column of a one-column-padded grid so interior columns classify
/// against their neighbors' heights without re-deriving them — several
/// times cheaper than asking `column_shape` per column, and bit-identical
/// to it (same f64 inputs to the same functions).
fn column_shapes(cx: i32, cz: i32, seed: Seed) -> ShapeMap {
	let p = &TerrainParams::DEFAULT;
	const PAD: usize = CHUNK_SIZE + 2;
	let (px, pz) = (cx * CHUNK_SIZE as i32, cz * CHUNK_SIZE as i32);
	let mut grid = [[terrain::Fields::default(); PAD]; PAD];
	for (gz, row) in grid.iter_mut().enumerate() {
		for (gx, f) in row.iter_mut().enumerate() {
			*f = fields_of(p, gx as i32 - 1 + px, gz as i32 - 1 + pz, seed);
		}
	}
	let mut shapes = [[ColumnShape {
		height: 0,
		surface: SurfaceKind::Plains,
		water: SEA_LEVEL,
		fall: None,
	}; CHUNK_SIZE]; CHUNK_SIZE];
	for (z, row) in shapes.iter_mut().enumerate() {
		for (x, s) in row.iter_mut().enumerate() {
			let (gx, gz) = (x + 1, z + 1);
			*s = classify_of(
				&grid[gz][gx],
				[
					grid[gz][gx - 1],
					grid[gz][gx + 1],
					grid[gz - 1][gx],
					grid[gz + 1][gx],
				],
				x as i32 + px,
				z as i32 + pz,
				seed,
			);
		}
	}
	shapes
}

/// Terrain fill in storage order (x contiguous), no carving. Kept apart
/// from the carve so the fill's writes stay sequential while the carve
/// walks columns.
fn fill_terrain(data: &mut ChunkData, py: i32, shapes: &ShapeMap) {
	for y in 0..CHUNK_SIZE {
		let wy = y as i32 + py;
		for (z, srow) in shapes.iter().enumerate() {
			let row = (y * CHUNK_SIZE + z) * CHUNK_SIZE;
			let out = &mut data.blocks[row..row + CHUNK_SIZE];
			for (b, &shape) in out.iter_mut().zip(srow) {
				*b = column_block(wy, shape) as u16;
			}
		}
	}
}

/// Carve one column's share of one chunk from a cave walk the caller
/// keeps across the whole vertical stack — the walk's lattice cache spans
/// many more cells than one chunk is tall.
fn carve_column(
	data: &mut ChunkData,
	py: i32,
	x: usize,
	z: usize,
	shape: ColumnShape,
	cave: &mut caves::CaveColumn,
) {
	// The carveable band of this column inside this chunk: below the
	// surface, above bedrock, and dry — standing water at a cave mouth
	// would leave a wall.
	let lo = caves::CAVE_FLOOR.max(py);
	let hi = shape.height.min(py + CHUNK_SIZE as i32 - 1);
	if lo > hi {
		return;
	}
	let mut carved = cave.carve_run(lo, hi);
	while carved != 0 {
		let wy = lo + carved.trailing_zeros() as i32;
		carved &= carved - 1;
		// Carveability is block data (blocks! rows), so a new surface or
		// subsoil block opens cave mouths without an edit here. Surface
		// blocks (grass, snow) are carveable so a mouth opens through them
		// instead of leaving a floater.
		if column_block(wy, shape).props().carveable {
			let y = (wy - py) as usize;
			data.blocks[(y * CHUNK_SIZE + z) * CHUNK_SIZE + x] = BlockType::Air as u16;
		}
	}
}

/// Build one chunk: fill → carve → decorate.
pub fn build_chunk(pos: ChunkPos, seed: Seed) -> ChunkData {
	let n = CHUNK_SIZE as i32;
	let shapes = column_shapes(pos.x, pos.z, seed);
	let mut data = ChunkData::default();
	fill_terrain(&mut data, pos.y * n, &shapes);
	for (z, srow) in shapes.iter().enumerate() {
		for (x, &shape) in srow.iter().enumerate() {
			if shape.height <= shape.water {
				continue;
			}
			let mut cave = cave_of(x as i32 + pos.x * n, z as i32 + pos.z * n, seed);
			carve_column(&mut data, pos.y * n, x, z, shape, &mut cave);
		}
	}
	decorate_chunk(&mut data, pos, &structures_near(pos.x * n, pos.z * n, seed));
	data
}

/// Generate the full vertical chunk stack of a column in one call,
/// computing the chunk's shape map once instead of once per chunk and
/// carving each column in a single walk across the whole stack. Chunks
/// entirely above the terrain and the sea resolve to the shared AIR_CHUNK
/// sentinel — no fill, no allocation, recognized by pointer downstream.
pub fn build_column(cx: i32, cz: i32, seed: Seed) -> Vec<Arc<ChunkData>> {
	let n = CHUNK_SIZE as i32;
	let shapes = column_shapes(cx, cz, seed);
	// A column's solid+liquid content reaches to height, its local water
	// surface (river water can sit above carved terrain), or the top of
	// its falling-water sheet.
	let max_h = shapes
		.iter()
		.flatten()
		.map(|s| {
			s.height
				.max(s.water)
				.max(s.fall.map_or(i32::MIN, |(_, t)| t))
		})
		.max()
		.expect("non-empty shape map");
	let structures = structures_near(cx * n, cz * n, seed);
	// Sky starts above both the terrain and anything decoration can reach.
	// Both bounds grow with cy, so the solid chunks are a prefix.
	let solid_top = structures.iter().map(|s| s.top()).fold(max_h, i32::max);
	let solid = (0..WORLD_VCHUNK)
		.take_while(|&cy| cy * n <= solid_top || cy * n <= SEA_LEVEL)
		.count();
	let mut stack = vec![ChunkData::default(); solid];
	for (cy, data) in stack.iter_mut().enumerate() {
		fill_terrain(data, cy as i32 * n, &shapes);
	}
	for (z, srow) in shapes.iter().enumerate() {
		for (x, &shape) in srow.iter().enumerate() {
			if shape.height <= shape.water {
				continue;
			}
			let mut cave = cave_of(x as i32 + cx * n, z as i32 + cz * n, seed);
			for (cy, data) in stack.iter_mut().enumerate() {
				carve_column(data, cy as i32 * n, x, z, shape, &mut cave);
			}
		}
	}
	let mut out: Vec<Arc<ChunkData>> = stack
		.into_iter()
		.enumerate()
		.map(|(cy, mut data)| {
			decorate_chunk(&mut data, ChunkPos::new(cx, cy as i32, cz), &structures);
			Arc::new(data)
		})
		.collect();
	out.resize(WORLD_VCHUNK as usize, AIR_CHUNK.clone());
	out
}

#[cfg(test)]
mod tests;
