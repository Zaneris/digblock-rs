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
						|| wy <= n.height + 2
						|| n.fall.is_some_and(|(b, t)| (b..=t).contains(&wy));
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
	let seed = 11.0;
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
