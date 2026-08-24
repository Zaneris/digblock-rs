use super::classify::{column_block, height_ceiling, holds_water};
use super::terrain::{TerrainParams, fields, spline};
use super::*;
use crate::chunk::WORLD_VBLOCK;
use crate::worldgen::Seed;

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
	let seed = Seed::current(77.7);
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
	let seed = Seed::current(1234.5678);
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
/// channels, and never hold water below sea level. Containment may take
/// a channel column's water away where no bank could wall it — the bed
/// then stands at exactly the level it used to hold — but it must only
/// NARROW the river: the drained columns stay a small minority of the
/// channel, or the fix has quietly drained the rivers it was contained
/// to keep.
#[test]
fn rivers_exist_inland() {
	let seed = Seed::current(1234.5678);
	let mut inland = 0u32;
	let mut wet = 0u32;
	let mut drained = 0u32;
	for x in (-4096..4096).step_by(13) {
		for z in (-4096..4096).step_by(11) {
			let s = column_shape(x, z, seed);
			assert!(s.water >= SEA_LEVEL, "water never below sea at ({x},{z})");
			if s.surface == SurfaceKind::River {
				assert!(
					s.water >= s.height,
					"river bed under its level at ({x},{z})"
				);
				if s.water > s.height {
					wet += 1;
					if s.water > SEA_LEVEL {
						inland += 1;
					}
				} else {
					drained += 1;
				}
			}
		}
	}
	assert!(inland > 0, "sample contains inland river columns");
	assert!(
		drained * 10 < wet,
		"containment drained {drained} river columns against {wet} still wet"
	);
}

/// Wherever a pool steps down, the upper pool's edge is a weir: one
/// block of water standing on solid ground, which is the only shape the
/// fluid sim will spill sideways — `update`'s spread rule reads the
/// FEEDING cell's floor, so a pool deeper than one block never pours at
/// all. Everything below the lip is ground, so exactly one cell per weir
/// column faces the drop, and that cell is what the settle pours through.
#[test]
fn a_pool_step_ends_in_a_weir() {
	let seed = Seed::current(1234.5678);
	let mut found = 0u32;
	'scan: for x in (-2048..2048).step_by(4) {
		for z in (-2048..2048).step_by(4) {
			let f = fields(P, x, z, seed.value);
			if !holds_water(&f) {
				continue;
			}
			let wl = f.water.floor() as i32;
			let below = [(x - 1, z), (x + 1, z), (x, z - 1), (x, z + 1)]
				.iter()
				.map(|&(nx, nz)| fields(P, nx, nz, seed.value))
				.filter(holds_water)
				.any(|n| (n.water.floor() as i32) < wl);
			let s = column_shape(x, z, seed);
			// A column drained by a low BANK reports no water at all; the
			// weir rule speaks only for the ones that kept theirs.
			if !below || s.water <= s.height {
				continue;
			}
			assert_eq!(s.water, wl, "the weir keeps its pool's level at ({x},{z})");
			assert_eq!(s.height, wl - 1, "one block deep at ({x},{z})");
			assert_eq!(column_block(wl, s), BlockType::Water);
			assert!(
				column_block(wl - 1, s).props().collides,
				"a weir stands on solid ground at ({x},{z})"
			);
			found += 1;
			if found >= 5 {
				break 'scan;
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
	let seed = Seed::current(1234.5678);
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
	let seed = Seed::current(1234.5678);
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

/// Lakes exist, hold their water above the terrain, and keep bounded beds.
#[test]
fn lakes_exist_and_hold_bounded_beds() {
	let seed = Seed::current(1234.5678);
	let mut found = 0u32;
	for x in (-8192..8192).step_by(32) {
		for z in (-8192..8192).step_by(32) {
			let s = column_shape(x, z, seed);
			if s.surface != SurfaceKind::Lake || s.water <= s.height {
				continue;
			}
			found += 1;
			assert!(s.water >= SEA_LEVEL, "lake level above sea at ({x},{z})");
			assert!(s.height >= s.water - 7, "bounded bed at ({x},{z})");
			if found >= 200 {
				return;
			}
		}
	}
	assert!(found > 0, "sample contains lakes");
}

/// THE containment invariant: every water cell worldgen emits is walled
/// by water or ground on all four sides, with one deliberate exception —
/// a weir's lip, the cell the settle pours through. Swept over every
/// water-holding column rather than one surface kind, and with no
/// tolerance: a bank's-height slop is exactly the hole this used to pass.
#[test]
fn worldgen_water_is_walled_but_for_its_weirs() {
	let seed = Seed::current(1234.5678);
	let mut water = 0u64;
	let mut weirs = 0u64;
	for x in (-1024..1024).step_by(3) {
		for z in (-1024..1024).step_by(3) {
			let s = column_shape(x, z, seed);
			if s.water <= s.height {
				continue;
			}
			water += 1;
			let nbrs: Vec<ColumnShape> = [(x - 1, z), (x + 1, z), (x, z - 1), (x, z + 1)]
				.iter()
				.map(|&(nx, nz)| column_shape(nx, nz, seed))
				.collect();
			// A weir is one deep and spills from its lip alone; every other
			// water column is walled through its whole span. What fails to
			// cover us may be a lower pool OR ground that falls away — a
			// river ending at a cliff pours over it just the same.
			let lip =
				s.height + 1 == s.water && nbrs.iter().any(|n| n.height.max(n.water) < s.water);
			if lip {
				weirs += 1;
				continue;
			}
			for wy in (s.height + 1)..=s.water {
				for n in &nbrs {
					assert!(
						wy <= n.height || wy <= n.water,
						"water at ({x},{wy},{z}) stands against air"
					);
				}
			}
		}
	}
	// Neither half may pass by examining nothing, and the exemption may
	// not swallow the sweep: a weir is an edge, so it stays a sliver of
	// the water. Without this the test passes by calling everything a lip.
	assert!(water > 10_000, "only {water} water columns sampled");
	assert!(weirs > 0, "the sample contains no weir");
	assert!(
		weirs * 20 < water,
		"{weirs} of {water} water columns claim the weir exemption"
	);
}

/// `contain` may raise a column, and `column_shape_above` rejects on the
/// centre field alone — so every raise has to stay inside the ceiling
/// that reject computes, or decoration and spawn silently lose columns.
#[test]
fn containment_stays_inside_the_reject_bound() {
	let seed = Seed::current(1234.5678);
	let mut raised = 0u32;
	for x in (-2048..2048).step_by(7) {
		for z in (-2048..2048).step_by(7) {
			let f = fields(P, x, z, seed.value);
			let s = column_shape(x, z, seed);
			let ceiling = height_ceiling(&f);
			assert!(
				s.height <= ceiling,
				"({x},{z}) classified to {} over a ceiling of {ceiling}",
				s.height
			);
			// And the reject must agree with the real thing at the bound.
			assert!(
				super::column_shape_above(x, z, seed, ceiling - 1).is_some(),
				"({x},{z}) rejected at its own ceiling"
			);
			if s.height > f.h.floor() as i32 {
				raised += 1;
			}
		}
	}
	assert!(raised > 0, "the sample contains no raised column");
}

/// The sandstone stratum sits under sandy floors, and scree patches
/// break up rock faces.
#[test]
fn strata_and_scree_exist() {
	let seed = Seed::current(1234.5678);
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
	let seed = Seed::current(1234.5678);
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
	let seed = Seed::current(1234.5678);
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
				SurfaceKind::Beach | SurfaceKind::Marsh | SurfaceKind::GravelShore => beach += 1,
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

#[test]
fn subsoil_span_bounds_every_pristine_dirt_cell() {
	// The frontier sweep's surface shell scans exactly this span, so a
	// Dirt cell outside it is a cell no scheduled rule can ever reach —
	// cave-exposed dirt would silently stop greening over.
	let seed = Seed::current(11.0);
	let n = CHUNK_SIZE as i32;
	let (mut dirt, mut exposed) = (0u32, 0u32);
	for cx in -4..4 {
		for cz in -4..4 {
			let stack = build_column(cx, cz, seed);
			for lx in 0..CHUNK_SIZE {
				for lz in 0..CHUNK_SIZE {
					let (x, z) = (cx * n + lx as i32, cz * n + lz as i32);
					let span = subsoil_span(x, z, seed);
					for (cy, data) in stack.iter().enumerate() {
						for ly in 0..CHUNK_SIZE {
							if data.get(lx, ly, lz) != BlockType::Dirt {
								continue;
							}
							let y = cy as i32 * n + ly as i32;
							assert!(
								(span.0..=span.1).contains(&y),
								"dirt at ({x},{y},{z}) outside subsoil span {span:?}"
							);
							dirt += 1;
							let above = if ly + 1 < CHUNK_SIZE {
								stack[cy].get(lx, ly + 1, lz)
							} else {
								stack
									.get(cy + 1)
									.map_or(BlockType::Air, |d| d.get(lx, 0, lz))
							};
							if above == BlockType::Air {
								exposed += 1;
							}
						}
					}
				}
			}
		}
	}
	// Vacuity guards: the region must actually contain buried dirt AND
	// cave-exposed dirt, or the sweep above proved nothing.
	assert!(dirt > 1000, "only {dirt} dirt cells examined");
	assert!(exposed > 0, "no cave-exposed dirt in the scanned region");
}
