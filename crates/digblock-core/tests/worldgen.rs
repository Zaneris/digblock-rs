use std::collections::HashMap;

use digblock_core::chunk::{WORLD_VBLOCK, WORLD_VCHUNK};
use digblock_core::decoration::{trees_near, STRUCTURE_APRON};
use digblock_core::worldgen::{
	build_chunk, carved, column_shape, terrain_height, SurfaceKind, DIRT_DEPTH, SEA_LEVEL,
};
use digblock_core::{BlockType, ChunkData, ChunkPos, CHUNK_SIZE};

/// The terrain-only expectation (no trees), re-implemented from the
/// column rules over the public per-column shape. Validates fill_chunk's
/// cell indexing and per-chunk assembly, not the shape function itself.
fn expect_terrain(wx: i32, wy: i32, wz: i32, seed: f64) -> BlockType {
	let shape = column_shape(wx, wz, seed);
	let h = shape.height;
	let base = if wy > h {
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
	};
	let carveable = matches!(
		base,
		BlockType::Stone | BlockType::Dirt | BlockType::Grass | BlockType::Snow
	);
	if carveable && h > shape.water && wy >= 1 && carved(wx, wy, wz, seed) {
		BlockType::Air
	} else {
		base
	}
}

/// Column rules re-implemented independently of worldgen's internals:
/// stone below DIRT_DEPTH of topsoil, sand topsoil for ocean floors and
/// beaches, bare stone above STONE_SURFACE, grass-over-dirt otherwise,
/// water fills to SEA_LEVEL, air above; the cave field carves terrain
/// cells of land columns (never y=0, never water or sand). Validates
/// fill_chunk's cell indexing and per-chunk assembly across the stack.
#[test]
fn column_rules() {
	let seed = 1234.5678;
	let mut caves_seen = false;
	let mut trees_seen = false;
	// Fixed samples cover interesting positions; the scan adds a chunk
	// with a tree rooted inside it, so the tree-overlay path stays
	// exercised no matter how terrain retunes move the forests.
	let mut samples = vec![(0, 0), (-3, 7), (100, -100), (40, 40)];
	'scan: for cx in -24..24 {
		for cz in -24..24 {
			let (bx, bz) = (cx * 16, cz * 16);
			if trees_near(bx, bz, seed)
				.iter()
				.any(|t| (bx..bx + 16).contains(&t.x) && (bz..bz + 16).contains(&t.z))
			{
				samples.push((cx, cz));
				break 'scan;
			}
		}
	}
	for &(cx, cz) in &samples {
		let chunks: Vec<ChunkData> = (0..WORLD_VCHUNK)
			.map(|cy| build_chunk(ChunkPos::new(cx, cy, cz), seed))
			.collect();
		let block_at = |x: usize, wy: i32, z: usize| -> BlockType {
			chunks[(wy / 16) as usize].get(x, (wy % 16) as usize, z)
		};
		// Mirror of the decoration pass: trees write into terrain-Air
		// cells, in candidate order, first writer wins.
		let mut tree_cells: HashMap<(i32, i32, i32), BlockType> = HashMap::new();
		for tree in trees_near(cx * 16, cz * 16, seed) {
			tree.for_each_block(|x, y, z, b| {
				if expect_terrain(x, y, z, seed) == BlockType::Air {
					tree_cells.entry((x, y, z)).or_insert(b);
				}
			});
		}
		for x in 0..CHUNK_SIZE {
			for z in 0..CHUNK_SIZE {
				let (wx, wz) = (x as i32 + cx * 16, z as i32 + cz * 16);
				let h = terrain_height(wx, wz, seed);
				for wy in 0..=WORLD_VBLOCK {
					let b = block_at(x, wy, z);
					let terrain = expect_terrain(wx, wy, wz, seed);
					if terrain == BlockType::Air && wy <= h {
						caves_seen = true;
					}
					let expect = match tree_cells.get(&(wx, wy, wz)) {
						Some(&t) if terrain == BlockType::Air => {
							trees_seen = true;
							t
						}
						_ => terrain,
					};
					assert_eq!(b, expect, "({x},{wy},{z}) h={h}");
				}
			}
		}
	}
	assert!(caves_seen, "the sampled chunks contain some carved cells");
	assert!(trees_seen, "the sampled chunks contain some tree blocks");
}

/// A tree whose canopy crosses a chunk border must appear identically in
/// every chunk it spills into — the core correctness property of pure
/// apron evaluation.
#[test]
fn trees_span_chunk_borders_seamlessly() {
	let seed = 1234.5678;
	let mut cache: HashMap<ChunkPos, ChunkData> = HashMap::new();
	// Wide scan: regions are province-sized, so a small patch near the
	// origin can be all ocean or all mountain at a given seed.
	for ccz in -24..24 {
		for ccx in -24..24 {
			for tree in trees_near(ccx * 16, ccz * 16, seed) {
				let (lx, lz) = (tree.x.rem_euclid(16), tree.z.rem_euclid(16));
				let crosses = !(2..=13).contains(&lx) || !(2..=13).contains(&lz);
				if !crosses {
					continue;
				}
				let mut chunks_touched = std::collections::HashSet::new();
				tree.for_each_block(|x, y, z, b| {
					let pos = ChunkPos::new(x.div_euclid(16), y.div_euclid(16), z.div_euclid(16));
					chunks_touched.insert((pos.x, pos.z));
					let chunk = cache.entry(pos).or_insert_with(|| build_chunk(pos, seed));
					let got = chunk.get(
						x.rem_euclid(16) as usize,
						y.rem_euclid(16) as usize,
						z.rem_euclid(16) as usize,
					);
					// The cell holds this tree's block unless terrain or an
					// earlier tree already occupied it — never bare air.
					assert!(
						got == b || got != BlockType::Air,
						"({x},{y},{z}) empty in {pos:?}"
					);
				});
				assert!(
					chunks_touched.len() > 1,
					"tree at ({},{}) spans chunks",
					tree.x,
					tree.z
				);
				return; // one verified border tree is the property
			}
		}
	}
	panic!("no border-crossing tree found in the sample area");
}

/// Trees never grow below the apron contract: every block a tree emits
/// stays within STRUCTURE_APRON of its origin column.
#[test]
fn trees_respect_the_apron() {
	let seed = 42.0;
	for tree in trees_near(0, 0, seed) {
		tree.for_each_block(|x, _y, z, _b| {
			assert!((x - tree.x).abs() <= STRUCTURE_APRON);
			assert!((z - tree.z).abs() <= STRUCTURE_APRON);
		});
	}
}

/// Structural cave invariants, independent of the carve function: the
/// bedrock layer survives, and ocean stacks are exactly rule-shaped (no
/// underwater carving anywhere).
#[test]
fn caves_respect_floor_and_oceans() {
	let seed = 1234.5678;
	for cx in -8..8 {
		for cz in -8..8 {
			let bottom = build_chunk(ChunkPos::new(cx, 0, cz), seed);
			for x in 0..CHUNK_SIZE {
				for z in 0..CHUNK_SIZE {
					assert_ne!(bottom.get(x, 0, z), BlockType::Air, "y=0 never carved");
				}
			}
		}
	}
	// Find an ocean column and check nothing below its surface is air.
	let mut checked = false;
	'outer: for x in -512..512 {
		for z in -64..64 {
			let h = terrain_height(x, z, seed);
			if h >= SEA_LEVEL {
				continue;
			}
			let (cx, cz) = (x.div_euclid(16), z.div_euclid(16));
			let (lx, lz) = (x.rem_euclid(16) as usize, z.rem_euclid(16) as usize);
			for wy in 0..=h {
				let chunk = build_chunk(ChunkPos::new(cx, wy.div_euclid(16), cz), seed);
				let b = chunk.get(lx, wy.rem_euclid(16) as usize, lz);
				assert_ne!(b, BlockType::Air, "ocean floor intact at ({x},{wy},{z})");
			}
			checked = true;
			break 'outer;
		}
	}
	assert!(checked, "found an ocean column to check");
	// Same for a river column: the carve gate skips columns whose water
	// sits above their bed, so nothing below the bed is air either.
	let mut checked_river = false;
	'river: for x in (-2048..2048).step_by(3) {
		for z in (-2048..2048).step_by(5) {
			let s = column_shape(x, z, seed);
			if s.water <= s.height || s.height < SEA_LEVEL {
				continue;
			}
			let (cx, cz) = (x.div_euclid(16), z.div_euclid(16));
			let (lx, lz) = (x.rem_euclid(16) as usize, z.rem_euclid(16) as usize);
			for wy in 0..=s.height {
				let chunk = build_chunk(ChunkPos::new(cx, wy.div_euclid(16), cz), seed);
				let b = chunk.get(lx, wy.rem_euclid(16) as usize, lz);
				assert_ne!(b, BlockType::Air, "river bed intact at ({x},{wy},{z})");
			}
			checked_river = true;
			break 'river;
		}
	}
	assert!(checked_river, "found a river column to check");
}

/// Every water cell in and around a river rests on water or solid —
/// river water never floats over air.
#[test]
fn river_water_never_floats() {
	let seed = 1234.5678;
	let mut found = None;
	'outer: for x in (-2048..2048).step_by(5) {
		for z in (-2048..2048).step_by(7) {
			let s = column_shape(x, z, seed);
			// An inland river column (above the sea's own water plane).
			if s.water > s.height && s.water > SEA_LEVEL {
				found = Some((x, z));
				break 'outer;
			}
		}
	}
	let (rx, rz) = found.expect("sample area contains an inland river");
	for dcz in -1..=1 {
		for dcx in -1..=1 {
			let (cx, cz) = (rx.div_euclid(16) + dcx, rz.div_euclid(16) + dcz);
			let chunks: Vec<ChunkData> = (0..WORLD_VCHUNK)
				.map(|cy| build_chunk(ChunkPos::new(cx, cy, cz), seed))
				.collect();
			let block_at = |x: usize, wy: i32, z: usize| -> BlockType {
				chunks[(wy / 16) as usize].get(x, (wy % 16) as usize, z)
			};
			for x in 0..CHUNK_SIZE {
				for z in 0..CHUNK_SIZE {
					for wy in 1..WORLD_VBLOCK {
						if block_at(x, wy, z) == BlockType::Water {
							let below = block_at(x, wy - 1, z);
							assert_ne!(
								below,
								BlockType::Air,
								"water floats at ({x},{wy},{z}) in chunk ({cx},{cz})"
							);
						}
					}
				}
			}
		}
	}
}

/// Spawn always lands above the beach line, for a spread of seeds.
#[test]
fn spawn_finds_land() {
	for seed in [0.0, 1.0, 1234.5678, 4242.0, 9999.9] {
		let s = digblock_core::player::spawn(seed);
		let (x, z) = (s.x.floor() as i32, s.z.floor() as i32);
		let shape = column_shape(x, z, seed);
		let h = shape.height;
		assert!(h > SEA_LEVEL + 1, "seed {seed}: spawn over land (h={h})");
		assert!(shape.water <= h, "seed {seed}: spawn not in a river");
		assert!((s.y - (h as f32 + 2.5)).abs() < 0.001, "eye above surface");
		assert!(!carved(x, h, z, seed), "spawn surface is not a cave mouth");
	}
}

/// ASCII cave cross-section — run with `--nocapture` to eyeball cave size
/// and spacing: `cargo test -p digblock-core cave_slice_dump -- --nocapture`
/// Prints an x/y slice ('.' = carved void, '#' = solid) plus the carved
/// fraction of below-surface land cells over a larger sample.
#[test]
fn cave_slice_dump() {
	let seed = 0.0;
	let z = 0;
	let mut out = String::new();
	for wy in (40..=190).rev().step_by(2) {
		for x in -128..128 {
			let h = terrain_height(x, z, seed);
			out.push(if wy > h {
				' '
			} else if h > SEA_LEVEL && wy >= 1 && carved(x, wy, z, seed) {
				'.'
			} else {
				'#'
			});
		}
		out.push('\n');
	}
	println!("{out}");
	let (mut solid, mut void) = (0u64, 0u64);
	for x in (-2048..2048).step_by(7) {
		for z in (-2048..2048).step_by(11) {
			let h = terrain_height(x, z, seed);
			if h <= SEA_LEVEL {
				continue;
			}
			for wy in (1..h).step_by(3) {
				if carved(x, wy, z, seed) {
					void += 1;
				} else {
					solid += 1;
				}
			}
		}
	}
	let pct = void as f64 * 100.0 / (void + solid) as f64;
	println!("carved fraction of land underground: {pct:.2}%");
	assert!(void > 0, "caves exist");
	assert!(pct < 15.0, "carving stays a small fraction ({pct:.2}%)");
}

/// ASCII region map — run with `--nocapture` to eyeball terrain shape:
/// `cargo test -p digblock-core heightmap_dump -- --nocapture`
/// The window spans ~4km so continental-scale features are visible.
#[test]
fn heightmap_dump() {
	let seed = 0.0;
	let glyphs = b" .:-=+*#%@";
	let mut out = String::new();
	let step = 32usize;
	let mut hmax = 0;
	for z in (-1024..1024).rev().step_by(step) {
		for x in (-2048..2048).step_by(step) {
			let s = column_shape(x, z, seed);
			let h = s.height;
			hmax = hmax.max(h);
			let g = glyphs[(h as usize * (glyphs.len() - 1)) / WORLD_VBLOCK as usize] as char;
			out.push(if h <= SEA_LEVEL {
				'~'
			} else if s.water > h {
				'≈' // river
			} else {
				g
			});
		}
		out.push('\n');
	}
	println!("{out}\npeak in window: {hmax}");
	// Sanity: the window contains both ocean and land at seed 0.
	assert!(out.contains('~'));
	assert!(out.chars().any(|c| c != '~' && c != '\n'));
}

/// Pins worldgen output to the bit. If this fails, a given seed now
/// produces different terrain: update the hash AND bump
/// `digblock_core::SIM_VERSION` — terrain is never sent over the wire
/// (join sync is seed + edits), so client and server worldgen must agree
/// exactly or every multiplayer world silently diverges.
#[test]
fn worldgen_golden_hash_pins_sim_version() {
	// FNV-1a over the raw block bytes of a spread of columns (spawn area,
	// ocean, mountains — chosen to touch every generation path).
	let mut hash: u64 = 0xcbf29ce484222325;
	for &(cx, cz) in &[(0, 0), (1, -1), (7, 3), (-12, 5), (40, -33)] {
		for chunk in digblock_core::worldgen::build_column(cx, cz, 42.0) {
			for &b in chunk.blocks.iter() {
				hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
			}
		}
	}
	assert_eq!(
		hash, 3873827593272335437,
		"worldgen output changed — update this hash AND bump digblock_core::SIM_VERSION"
	);
}
