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
//! content never depends on generation order. The tuning surface lives in
//! [`TerrainParams`]; the public entry points use `TerrainParams::DEFAULT`
//! (the future "world type" seam threads a custom one).

mod caves;
mod classify;
mod lakes;
pub mod ore;
mod rivers;
mod terrain;

pub use caves::carved;
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

/// The per-column terrain function: control fields → base + modulated
/// detail → height and surface kind.
pub fn column_shape(x: i32, z: i32, seed: f64) -> ColumnShape {
	let p = &TerrainParams::DEFAULT;
	classify::classify(
		&fields(p, x, z, seed),
		[
			fields(p, x - 1, z, seed),
			fields(p, x + 1, z, seed),
			fields(p, x, z - 1, seed),
			fields(p, x, z + 1, seed),
		],
		x,
		z,
		seed,
	)
}

pub fn terrain_height(x: i32, z: i32, seed: f64) -> i32 {
	column_shape(x, z, seed).height
}

/// [`column_shape`], but rejecting on altitude from the centre field
/// alone — one evaluation instead of five. Classification only ever
/// LOWERS the discretized height (marsh puddles dip a block), so a centre
/// height at or below `min_height` proves the column's is too. Every
/// caller that scans candidate columns rejects the overwhelming majority
/// on altitude, and the survivors pay nothing extra: the centre field is
/// evaluated once either way. `Some` is bit-identical to `column_shape`.
pub fn column_shape_above(x: i32, z: i32, seed: f64, min_height: i32) -> Option<ColumnShape> {
	let p = &TerrainParams::DEFAULT;
	let f = fields(p, x, z, seed);
	if (f.h.floor() as i32) <= min_height {
		return None;
	}
	Some(classify::classify(
		&f,
		[
			fields(p, x - 1, z, seed),
			fields(p, x + 1, z, seed),
			fields(p, x, z - 1, seed),
			fields(p, x, z + 1, seed),
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
pub fn column_shapes_in(x0: i32, x1: i32, z0: i32, z1: i32, seed: f64) -> Vec<ColumnShape> {
	let p = &TerrainParams::DEFAULT;
	let (w, d) = ((x1 - x0 + 1) as usize, (z1 - z0 + 1) as usize);
	let stride = w + 2;
	let mut grid = Vec::with_capacity(stride * (d + 2));
	for gz in 0..d + 2 {
		for gx in 0..stride {
			grid.push(fields(p, x0 + gx as i32 - 1, z0 + gz as i32 - 1, seed));
		}
	}
	let mut out = Vec::with_capacity(w * d);
	for z in 0..d {
		for x in 0..w {
			let g = (z + 1) * stride + x + 1;
			out.push(classify::classify(
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
fn column_shapes(cx: i32, cz: i32, seed: f64) -> ShapeMap {
	let p = &TerrainParams::DEFAULT;
	const PAD: usize = CHUNK_SIZE + 2;
	let (px, pz) = (cx * CHUNK_SIZE as i32, cz * CHUNK_SIZE as i32);
	let mut grid = [[terrain::Fields::default(); PAD]; PAD];
	for (gz, row) in grid.iter_mut().enumerate() {
		for (gx, f) in row.iter_mut().enumerate() {
			*f = fields(p, gx as i32 - 1 + px, gz as i32 - 1 + pz, seed);
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
			*s = classify::classify(
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
pub fn build_chunk(pos: ChunkPos, seed: f64) -> ChunkData {
	let n = CHUNK_SIZE as i32;
	let shapes = column_shapes(pos.x, pos.z, seed);
	let mut data = ChunkData::default();
	fill_terrain(&mut data, pos.y * n, &shapes);
	for (z, srow) in shapes.iter().enumerate() {
		for (x, &shape) in srow.iter().enumerate() {
			if shape.height <= shape.water {
				continue;
			}
			let mut cave = caves::CaveColumn::new(x as i32 + pos.x * n, z as i32 + pos.z * n, seed);
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
pub fn build_column(cx: i32, cz: i32, seed: f64) -> Vec<Arc<ChunkData>> {
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
			let mut cave = caves::CaveColumn::new(x as i32 + cx * n, z as i32 + cz * n, seed);
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
mod tests {
	use super::classify::{column_block, holds_water};
	use super::terrain::{TerrainParams, fields, spline};
	use super::*;
	use crate::chunk::WORLD_VBLOCK;

	const P: &TerrainParams = &TerrainParams::DEFAULT;

	#[test]
	fn spline_is_monotonic_and_anchored() {
		let mut prev = f64::MIN;
		let mut c = -1.2;
		while c <= 1.2 {
			let v = spline(P, c);
			assert!(v >= prev, "monotonic at c={c}");
			prev = v;
			c += 0.01;
		}
		for (c, v) in P.base_spline {
			assert_eq!(spline(P, c), v, "anchor at {c}");
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

	/// The padded-grid path and the per-column path must classify
	/// identically — the invariant that keeps chunk fills and per-column
	/// probes (trees, spawn) agreeing about the world.
	#[test]
	fn classification_paths_agree() {
		let seed = 1234.5678;
		for &(cx, cz) in &[(0, 0), (-3, 7), (40, 40), (-17, -29)] {
			let shapes = column_shapes(cx, cz, seed);
			for (z, row) in shapes.iter().enumerate() {
				for (x, &s) in row.iter().enumerate() {
					let (wx, wz) = (
						x as i32 + cx * CHUNK_SIZE as i32,
						z as i32 + cz * CHUNK_SIZE as i32,
					);
					assert_eq!(s, column_shape(wx, wz, seed), "at ({wx},{wz})");
				}
			}
		}
	}

	/// Rivers exist inland (water level above the sea's), sit in carved
	/// channels (water above bed), and never hold water below sea level.
	#[test]
	fn rivers_exist_inland() {
		let seed = 1234.5678;
		let mut inland = 0u32;
		for x in (-4096..4096).step_by(13) {
			for z in (-4096..4096).step_by(11) {
				let s = column_shape(x, z, seed);
				assert!(s.water >= SEA_LEVEL, "water never below sea at ({x},{z})");
				if s.surface == SurfaceKind::River {
					assert!(s.water > s.height, "river holds water at ({x},{z})");
					if s.water > SEA_LEVEL {
						inland += 1;
					}
				}
			}
		}
		assert!(inland > 0, "sample contains inland river columns");
	}

	/// Wherever a river pool steps down (quantized levels), the lower
	/// column carries a WaterFall span from just above its own pool up to
	/// the highest adjacent pool — the sheet that connects the cascade.
	#[test]
	fn falls_connect_pool_steps() {
		let seed = 1234.5678;
		let mut found = 0u32;
		'scan: for x in (-2048..2048).step_by(4) {
			for z in (-2048..2048).step_by(4) {
				let f = fields(P, x, z, seed);
				if !holds_water(&f) {
					continue;
				}
				let wl = f.water.floor() as i32;
				let top = [(x - 1, z), (x + 1, z), (x, z - 1), (x, z + 1)]
					.iter()
					.map(|&(nx, nz)| fields(P, nx, nz, seed))
					.filter(holds_water)
					.map(|n| n.water.floor() as i32)
					.max()
					.unwrap_or(i32::MIN);
				let s = column_shape(x, z, seed);
				if top > wl {
					assert_eq!(s.fall, Some((wl + 1, top)), "at ({x},{z})");
					// The blocks of the span really are falling water.
					assert_eq!(column_block(wl + 1, s), BlockType::WaterFall);
					assert_eq!(column_block(top, s), BlockType::WaterFall);
					assert_eq!(column_block(wl, s), BlockType::Water);
					found += 1;
					if found >= 5 {
						break 'scan;
					}
				} else {
					assert_eq!(s.fall, None, "no step, no fall at ({x},{z})");
				}
			}
		}
		assert!(found > 0, "sample contains pool steps");
	}

	/// Snow only appears near the (dithered, temperature-shifted) snow
	/// line — cold regions legitimately snow TEMP_ALT_SHIFT lower — and
	/// snow surfaces exist somewhere in a mountain sample.
	#[test]
	fn snow_implies_altitude() {
		let seed = 1234.5678;
		let mut seen = 0u32;
		for x in (-4096..4096).step_by(17) {
			for z in (-4096..4096).step_by(19) {
				let shape = column_shape(x, z, seed);
				if shape.surface == SurfaceKind::Snow {
					seen += 1;
					assert!(
						f64::from(shape.height) >= SNOW_LINE - TEMP_ALT_SHIFT - 7.0,
						"snow at h={} ({x},{z})",
						shape.height
					);
				}
			}
		}
		assert!(seen > 0, "sample contains snow surfaces");
	}

	/// Temperature moves the snow line both ways: somewhere in a wide
	/// sample a cold region snows below the neutral line, and some warm
	/// high column stands above it bare. Snow/mountain belts are hundreds
	/// of blocks wide, so a coarse scan suffices.
	#[test]
	fn climate_varies_snow_line() {
		let seed = 1234.5678;
		let (mut cold_snow, mut warm_bare) = (false, false);
		for x in (-16384..16384).step_by(128) {
			for z in (-16384..16384).step_by(128) {
				let shape = column_shape(x, z, seed);
				let h = f64::from(shape.height);
				cold_snow |= shape.surface == SurfaceKind::Snow && h < SNOW_LINE - 10.0;
				warm_bare |= shape.surface == SurfaceKind::Mountain && h > SNOW_LINE + 10.0;
				if cold_snow && warm_bare {
					return;
				}
			}
		}
		panic!("cold_snow={cold_snow} warm_bare={warm_bare}");
	}

	/// The terrace tread transform holds its contract: continuous,
	/// monotone non-decreasing, and never further than one step (plus the
	/// edge dither) from its input.
	#[test]
	fn terrace_tread_is_continuous_and_bounded() {
		let step = 6.0;
		for &edge in &[0.0, -2.0, 1.3] {
			let mut prev = terrain::tread(step, 100.0, edge);
			let mut h = 100.0;
			while h < 200.0 {
				h += 0.01;
				let t = terrain::tread(step, h, edge);
				assert!(t + 1e-9 >= prev, "monotone at h={h}");
				assert!(t - prev < 0.5, "continuous at h={h}");
				assert!((t - h).abs() <= step + edge.abs(), "bounded at h={h}");
				prev = t;
			}
		}
	}

	/// Lakes exist, hold their water above the terrain, keep bounded
	/// beds, and every water cell is laterally contained: each of the four
	/// neighbor columns covers it with water, solid ground within a bank's
	/// height (the fill rule's own bound), or a connecting fall sheet
	/// (quantized level steps, exactly like river pools).
	#[test]
	fn lakes_exist_and_are_contained() {
		let seed = 1234.5678;
		let mut found = 0u32;
		for x in (-8192..8192).step_by(32) {
			for z in (-8192..8192).step_by(32) {
				let s = column_shape(x, z, seed);
				if s.surface != SurfaceKind::Lake {
					continue;
				}
				found += 1;
				assert!(s.water > s.height, "lake holds water at ({x},{z})");
				assert!(s.water >= SEA_LEVEL, "lake level above sea at ({x},{z})");
				assert!(s.height >= s.water - 7, "bounded bed at ({x},{z})");
				for (nx, nz) in [(x - 1, z), (x + 1, z), (x, z - 1), (x, z + 1)] {
					let n = column_shape(nx, nz, seed);
					for wy in (s.height + 1)..=s.water {
						let covered = wy <= n.water
							|| wy <= n.height + 2 || n
							.fall
							.is_some_and(|(b, t)| (b..=t).contains(&wy));
						assert!(covered, "water wall at ({x},{z}) wy={wy} vs ({nx},{nz})");
					}
				}
				if found >= 200 {
					return;
				}
			}
		}
		assert!(found > 0, "sample contains lakes");
	}

	/// The sandstone stratum sits under sandy floors, and scree patches
	/// break up rock faces.
	#[test]
	fn strata_and_scree_exist() {
		let seed = 1234.5678;
		let (mut sandy, mut scree) = (false, false);
		for x in (-8192..8192).step_by(37) {
			for z in (-8192..8192).step_by(41) {
				let s = column_shape(x, z, seed);
				match s.surface {
					SurfaceKind::Beach | SurfaceKind::Ocean if !sandy => {
						assert_eq!(
							classify::column_block(s.height - DIRT_DEPTH, s),
							BlockType::Sandstone
						);
						assert_eq!(
							classify::column_block(s.height - DIRT_DEPTH - 4, s),
							BlockType::Stone
						);
						sandy = true;
					}
					SurfaceKind::Scree if !scree => {
						assert_eq!(classify::column_block(s.height, s), BlockType::Gravel);
						scree = true;
					}
					_ => {}
				}
				if sandy && scree {
					return;
				}
			}
		}
		panic!("sandy={sandy} scree={scree}");
	}

	/// The coastal band kinds exist. They live in a few-block height band
	/// along shorelines, so a blind dense scan would be enormous: instead
	/// scan coarsely for waterline-adjacent patches and only dense-scan
	/// those until both kinds turn up.
	#[test]
	fn coastal_and_wet_kinds_exist() {
		let seed = 1234.5678;
		let (mut marsh, mut gravel) = (false, false);
		'outer: for x in (-8192..8192).step_by(64) {
			for z in (-8192..8192).step_by(64) {
				let s = column_shape(x, z, seed);
				if !(SEA_LEVEL..SEA_LEVEL + 8).contains(&s.height) {
					continue;
				}
				for dx in (0..64).step_by(4) {
					for dz in (0..64).step_by(4) {
						match column_shape(x + dx, z + dz, seed).surface {
							SurfaceKind::Marsh => marsh = true,
							SurfaceKind::GravelShore => gravel = true,
							_ => {}
						}
						if marsh && gravel {
							break 'outer;
						}
					}
				}
			}
		}
		assert!(marsh && gravel, "marsh={marsh} gravel={gravel}");
	}

	/// The major surface kinds occur in a large sample, with open terrain
	/// (walkable lowland + ocean) dominating mountains, and every climate
	/// biome present.
	#[test]
	fn regions_are_present_and_balanced() {
		let seed = 1234.5678;
		let (mut ocean, mut beach, mut plains, mut mountain) = (0u32, 0u32, 0u32, 0u32);
		let (mut forest, mut savanna, mut taiga) = (0u32, 0u32, 0u32);
		let mut total = 0u32;
		// The sweep spans several erosion and continent wavelengths —
		// enough regions that single-seed variance can't starve a bucket.
		for x in (-8192..8192).step_by(64) {
			for z in (-8192..8192).step_by(64) {
				total += 1;
				match column_shape(x, z, seed).surface {
					// Rivers and lakes are open water for balance purposes.
					SurfaceKind::Ocean | SurfaceKind::River | SurfaceKind::Lake => ocean += 1,
					// The narrow coastal bands bucket with beaches.
					SurfaceKind::Beach | SurfaceKind::Marsh | SurfaceKind::GravelShore => {
						beach += 1
					}
					SurfaceKind::Plains => plains += 1,
					// Snow and scree are high-mountain surface for balance.
					SurfaceKind::Mountain | SurfaceKind::Snow | SurfaceKind::Scree => mountain += 1,
					SurfaceKind::Forest => forest += 1,
					SurfaceKind::Savanna => savanna += 1,
					SurfaceKind::Taiga => taiga += 1,
				}
			}
		}
		let pct = |n: u32| f64::from(n) * 100.0 / f64::from(total);
		// Walkable grassy lowland, whatever its biome dressing.
		let open = plains + forest + savanna + taiga;
		assert!(pct(ocean) > 5.0, "ocean {:.1}%", pct(ocean));
		assert!(beach > 0, "beaches exist");
		assert!(pct(open) > 25.0, "open lowland {:.1}%", pct(open));
		assert!(pct(plains) > 5.0, "plains {:.1}%", pct(plains));
		assert!(forest > 0, "forests exist");
		assert!(savanna > 0, "savannas exist");
		assert!(taiga > 0, "taigas exist");
		assert!(mountain > 0, "mountains exist");
		assert!(
			pct(mountain) < pct(open) + pct(ocean),
			"mountains {:.1}% don't dominate",
			pct(mountain)
		);
	}
}
