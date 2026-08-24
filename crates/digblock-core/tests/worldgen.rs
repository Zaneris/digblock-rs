use digblock_core::worldgen::{Seed, TerrainVersion};
use std::collections::HashMap;

use digblock_core::chunk::{WORLD_VBLOCK, WORLD_VCHUNK};
use digblock_core::decoration::{Structure, structures_near};
use digblock_core::worldgen::{
	DIRT_DEPTH, SEA_LEVEL, SurfaceKind, build_chunk, carved, column_shape, terrain_height,
};
use digblock_core::{BlockType, CHUNK_SIZE, ChunkData, ChunkPos};

/// The terrain-only expectation (no trees), re-implemented from the
/// column rules over the public per-column shape. Validates fill_chunk's
/// cell indexing and per-chunk assembly, not the shape function itself.
fn expect_terrain(wx: i32, wy: i32, wz: i32, seed: Seed) -> BlockType {
	let shape = column_shape(wx, wz, seed);
	let h = shape.height;
	let base = if wy > h {
		if wy <= shape.water {
			BlockType::Water
		} else {
			BlockType::Air
		}
	} else {
		// Independent re-statement of the column rules: top block, then
		// DIRT_DEPTH of subsoil, then (for sandy floors) a sandstone
		// stratum, then stone.
		let (top, subsoil) = match shape.surface {
			SurfaceKind::Ocean | SurfaceKind::Beach | SurfaceKind::River | SurfaceKind::Lake => {
				(BlockType::Sand, BlockType::Sand)
			}
			SurfaceKind::Mountain => (BlockType::Stone, BlockType::Stone),
			SurfaceKind::Snow => (BlockType::Snow, BlockType::Stone),
			SurfaceKind::Plains | SurfaceKind::Forest | SurfaceKind::Taiga => {
				(BlockType::Grass, BlockType::Dirt)
			}
			SurfaceKind::Savanna => (BlockType::DryGrass, BlockType::Dirt),
			SurfaceKind::Marsh => (BlockType::Mud, BlockType::Mud),
			SurfaceKind::GravelShore | SurfaceKind::Scree => (BlockType::Gravel, BlockType::Gravel),
		};
		let sandy = matches!(
			shape.surface,
			SurfaceKind::Ocean | SurfaceKind::Beach | SurfaceKind::River | SurfaceKind::Lake
		);
		if wy == h {
			top
		} else if wy > h - DIRT_DEPTH {
			subsoil
		} else if sandy && wy > h - DIRT_DEPTH - 4 {
			BlockType::Sandstone
		} else {
			BlockType::Stone
		}
	};
	let carveable = matches!(
		base,
		BlockType::Stone
			| BlockType::Dirt
			| BlockType::Grass
			| BlockType::Snow
			| BlockType::DryGrass
			| BlockType::Mud
			| BlockType::Gravel
			| BlockType::Sandstone
	);
	// A cell inside the span a neighbour's pool occupies is that pool's
	// wall and survives the carve — otherwise the cave opens the water
	// onto air, which is what containment exists to prevent.
	let walls_water = (shape.wall.0..=shape.wall.1).contains(&wy);
	if carveable && h > shape.water && wy >= 1 && !walls_water && carved(wx, wy, wz, seed) {
		BlockType::Air
	} else {
		base
	}
}

/// Column rules re-implemented independently of worldgen's internals: stone below DIRT_DEPTH of
/// topsoil, sand topsoil for ocean floors and beaches, bare stone above STONE_SURFACE,
/// grass-over-dirt otherwise, water to SEA_LEVEL, air above; the cave field carves terrain cells
/// of land columns, never the bedrock course and never water or sand. Validates fill_chunk's cell
/// indexing and per-chunk assembly across the stack.
#[test]
fn column_rules() {
	let seed = Seed::current(1234.5678);
	let mut caves_seen = false;
	let mut trees_seen = false;
	// Fixed samples cover interesting positions; the scan adds a chunk
	// with a tree rooted inside it, so the tree-overlay path stays
	// exercised no matter how terrain retunes move the forests.
	let mut samples = vec![(0, 0), (-3, 7), (100, -100), (40, 40)];
	'scan: for cx in -24..24 {
		for cz in -24..24 {
			let (bx, bz) = (cx * 16, cz * 16);
			if structures_near(bx, bz, seed).iter().any(|s| {
				matches!(s, Structure::Tree(_)) && {
					let (x, z) = s.origin();
					(bx..bx + 16).contains(&x) && (bz..bz + 16).contains(&z)
				}
			}) {
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
		// Mirror of the decoration pass: structures write into terrain-Air
		// cells — plus the trunk-root exception, where Wood replaces the
		// Grass/Dirt/DryGrass surface cell — in candidate order, first
		// writer wins.
		let mut tree_cells: HashMap<(i32, i32, i32), BlockType> = HashMap::new();
		for st in structures_near(cx * 16, cz * 16, seed) {
			st.for_each_block(|x, y, z, b| {
				let t = expect_terrain(x, y, z, seed);
				let writable = t == BlockType::Air
					|| (st.is_root(x, y, z)
						&& matches!(t, BlockType::Grass | BlockType::Dirt | BlockType::DryGrass));
				if writable {
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
					// Writability was already enforced at insertion, so any
					// mapped cell holds its tree block.
					let expect = match tree_cells.get(&(wx, wy, wz)) {
						Some(&t) => {
							trees_seen = true;
							t
						}
						None => terrain,
					};
					assert_eq!(b, expect, "({x},{wy},{z}) h={h}");
				}
			}
		}
	}
	assert!(caves_seen, "the sampled chunks contain some carved cells");
	assert!(trees_seen, "the sampled chunks contain some tree blocks");
}

/// A structure whose blocks cross a chunk border must appear identically
/// in every chunk it spills into — the core correctness property of pure
/// apron evaluation.
#[test]
fn structures_span_chunk_borders_seamlessly() {
	let seed = Seed::current(1234.5678);
	let mut cache: HashMap<ChunkPos, ChunkData> = HashMap::new();
	// Wide scan: regions are province-sized, so a small patch near the
	// origin can be all ocean or all mountain at a given seed.
	for ccz in -24..24 {
		for ccx in -24..24 {
			for st in structures_near(ccx * 16, ccz * 16, seed) {
				// Actual span, not a radius heuristic — footprints vary by
				// structure kind and jitter.
				let (mut min_x, mut max_x) = (i32::MAX, i32::MIN);
				let (mut min_z, mut max_z) = (i32::MAX, i32::MIN);
				st.for_each_block(|x, _, z, _| {
					(min_x, max_x) = (min_x.min(x), max_x.max(x));
					(min_z, max_z) = (min_z.min(z), max_z.max(z));
				});
				let crosses = min_x.div_euclid(16) != max_x.div_euclid(16)
					|| min_z.div_euclid(16) != max_z.div_euclid(16);
				if !crosses {
					continue;
				}
				let mut cells = Vec::new();
				st.for_each_block(|x, y, z, b| cells.push((x, y, z, b)));
				let mut touched = std::collections::HashSet::new();
				let mut exact: HashMap<(i32, i32), u32> = HashMap::new();
				for &(x, y, z, b) in &cells {
					let pos = ChunkPos::new(x.div_euclid(16), y.div_euclid(16), z.div_euclid(16));
					touched.insert((pos.x, pos.z));
					let chunk = cache.entry(pos).or_insert_with(|| build_chunk(pos, seed));
					let got = chunk.get(
						x.rem_euclid(16) as usize,
						y.rem_euclid(16) as usize,
						z.rem_euclid(16) as usize,
					);
					// A cell may be overridden by terrain or an earlier
					// structure, but a HOLE means the apron dropped it.
					assert!(got != BlockType::Air, "({x},{y},{z}) empty in {pos:?}");
					if got == b {
						*exact.entry((pos.x, pos.z)).or_default() += 1;
					}
				}
				assert!(
					touched.len() > 1,
					"structure at {:?} spans chunks",
					st.origin()
				);
				// "Identically" has to mean the structure's OWN blocks, not
				// merely non-air: every chunk it spills into must hold at
				// least one of them, or a neighbour stamping the WRONG
				// block would pass. A structure fully overridden on one
				// side proves nothing either way — keep scanning.
				if touched
					.iter()
					.all(|c| exact.get(c).copied().unwrap_or(0) > 0)
				{
					return;
				}
			}
		}
	}
	panic!("no border-crossing structure appears identically on every side it spills into");
}

/// Structural cave invariants, independent of the carve function: the
/// bedrock layer survives, and ocean stacks are exactly rule-shaped (no
/// underwater carving anywhere).
#[test]
fn caves_respect_floor_and_oceans() {
	let seed = Seed::current(1234.5678);
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
	let seed = Seed::current(1234.5678);
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

/// Spawn always lands above the beach line, in the open — no decoration
/// block in or near the capsule, no surrounding terrain at eye level —
/// for a spread of seeds.
#[test]
fn spawn_finds_land() {
	for value in [0.0, 1.0, 1234.5678, 4242.0, 9999.9] {
		let seed = Seed::current(value);
		let s = digblock_core::player::spawn(seed);
		let (x, z) = (s.x.floor() as i32, s.z.floor() as i32);
		let shape = column_shape(x, z, seed);
		let h = shape.height;
		assert!(h > SEA_LEVEL + 1, "seed {value}: spawn over land (h={h})");
		assert!(shape.water <= h, "seed {value}: spawn not in a river");
		assert!((s.y - (h as f32 + 2.5)).abs() < 0.001, "eye above surface");
		assert!(!carved(x, h, z, seed), "spawn surface is not a cave mouth");
		for dz in -2..=2 {
			for dx in -2..=2 {
				let n = column_shape(x + dx, z + dz, seed);
				let top = n.height.max(n.water);
				assert!(
					top <= h + 1 || (dx, dz) == (0, 0),
					"seed {value}: wall at eye level ({dx},{dz})"
				);
			}
		}
		assert!(
			!digblock_core::decoration::box_obstructed(
				x - 2,
				x + 2,
				h + 1,
				h + 3,
				z - 2,
				z + 2,
				seed
			),
			"seed {value}: decoration block in the spawn clearing"
		);
	}
}

/// ASCII cave cross-section — run with `--nocapture` to eyeball cave size
/// and spacing: `cargo test -p digblock-core cave_slice_dump -- --nocapture`
/// Prints an x/y slice ('.' = carved void, '#' = solid) plus the carved
/// fraction of below-surface land cells over a larger sample.
#[test]
fn cave_slice_dump() {
	let seed = Seed::current(0.0);
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

/// `BlockProps::crafted` claims worldgen places none of these — the claim the craft gate rests on
/// (`unlock::pickup` refuses to dig one below its recipe's level, which would be a wall across
/// natural terrain if the world grew them). Asked of worldgen itself, over a spread of seeds and
/// columns touching every generation path: ocean, mountains, caves, rivers, decorated surface.
#[test]
fn worldgen_never_places_a_crafted_block() {
	for value in [42.0, 7.5, -300.25] {
		let seed = Seed::current(value);
		for &(cx, cz) in &[(0, 0), (1, -1), (7, 3), (-12, 5), (40, -33)] {
			for chunk in digblock_core::worldgen::build_column(cx, cz, seed) {
				for &cell in chunk.blocks.iter() {
					let b = BlockType::from_id(digblock_core::chunk::cell_id(cell));
					assert!(
						!b.is_crafted_only(),
						"worldgen placed craft-only {b:?} at seed {value} chunk ({cx},{cz})"
					);
				}
			}
		}
	}
}

/// One pinned hash per generator this build carries. A terrain change adds a `TerrainVersion` row
/// with a hash beside it, and that hash never moves again: a world made under the row regenerates
/// identically forever. A hash moves in place only for a change small enough that every world on
/// its row is to wear it — ridden by a `SIM_VERSION` bump, so two builds cannot disagree about one
/// world. The count assert catches a row shipped unpinned, the one way this stops covering `ALL`.
const TERRAIN_GOLDENS: &[(TerrainVersion, u64)] = &[(TerrainVersion::V1, 2130581501045034260)];

#[test]
fn a_shipped_generator_never_moves() {
	assert_eq!(
		TERRAIN_GOLDENS.len(),
		TerrainVersion::ALL.len(),
		"every supported generator owes a golden — add the new row rather than editing one"
	);
	for version in TerrainVersion::ALL {
		let (_, want) = TERRAIN_GOLDENS
			.iter()
			.find(|(v, _)| *v == version)
			.unwrap_or_else(|| panic!("{version:?} has no golden"));
		// FNV-1a over the raw block bytes of a spread of columns (spawn area,
		// ocean, mountains — chosen to touch every generation path). The last
		// three are water's: (11, -17) carries the sample's densest run of
		// weirs, (38, -17) the cascade the walk's re-resolution moves the
		// most, and (7, -35) the marsh puddles a cave breaches — without them
		// the pin cannot see a cascade or a puddle's wall change at all.
		let mut hash: u64 = 0xcbf29ce484222325;
		for &(cx, cz) in &[
			(0, 0),
			(1, -1),
			(7, 3),
			(-12, 5),
			(40, -33),
			(11, -17),
			(38, -17),
			(7, -35),
		] {
			for chunk in digblock_core::worldgen::build_column(cx, cz, Seed::new(42.0, version)) {
				for &b in chunk.blocks.iter() {
					hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
				}
			}
		}
		assert_eq!(
			hash, *want,
			"{version:?} produces different terrain than it shipped with — every world made under it \
			 would be rewritten under its edits. Add a new generator; or, for a change every such \
			 world is to wear, move this hash AND bump digblock_core::SIM_VERSION."
		);
	}
}

/// A generator this build does not carry is refused rather than mistaken for one it does, and the
/// number a header written before the slot existed holds decodes as the FIRST generator — which
/// is what makes the field free of a format bump and every existing world open untouched.
#[test]
fn an_unknown_generator_is_refused_and_an_absent_one_is_the_first() {
	assert_eq!(TerrainVersion::from_wire(0), Some(TerrainVersion::V1));
	let highest = TerrainVersion::ALL
		.iter()
		.map(|v| v.to_wire())
		.max()
		.expect("the roster is never empty");
	assert_eq!(TerrainVersion::from_wire(highest + 1), None);
	for version in TerrainVersion::ALL {
		assert_eq!(
			TerrainVersion::from_wire(version.to_wire()),
			Some(version),
			"{version:?} does not survive the wire"
		);
	}
}
