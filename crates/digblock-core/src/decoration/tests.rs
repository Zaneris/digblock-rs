use super::*;
use crate::worldgen::Seed;
use crate::worldgen::column_shape;

/// The altitude presieve may only reject columns no family would have
/// accepted anyway — nothing grows and nothing piles at or below the
/// waterline band — and a survivor must classify exactly as the full
/// five-evaluation path does.
#[test]
fn presieve_only_rejects_barren_columns() {
	let seed = Seed::current(1234.5678);
	let mut rejected = 0u32;
	for x in (-8192..8192).step_by(37) {
		for z in (-8192..8192).step_by(41) {
			let full = column_shape(x, z, seed);
			match candidate_column(x, z, seed) {
				Some(s) => assert_eq!(s, full, "presieved column at ({x},{z})"),
				None => {
					let p = full.surface.props();
					assert_eq!(p.tree_density, TreeDensity::None, "at ({x},{z})");
					assert!(!p.boulders, "boulders at ({x},{z})");
					rejected += 1;
				}
			}
		}
	}
	assert!(rejected > 0, "sample contains rejected columns");
}

/// Memo hits must be bit-identical to misses. Revisiting cells across
/// interleaved seeds exercises both the reuse the aprons rely on and
/// the key's seed component, which is what keeps two worlds from
/// leaking candidates into each other.
#[test]
fn memo_hits_match_misses() {
	let seeds = [42.0, 1234.5678, -7.0].map(Seed::current);
	for _ in 0..3 {
		for seed in seeds {
			for cell_z in -24..24 {
				for cell_x in -24..24 {
					let c = memoized(&FINE_MEMO, cell_x, cell_z, seed, || {
						cell_candidates(cell_x, cell_z, seed)
					});
					assert_eq!(
						c.tree,
						cell_tree(cell_x, cell_z, seed),
						"tree {cell_x},{cell_z}"
					);
					assert_eq!(c.boulder, cell_boulder(cell_x, cell_z, seed), "boulder");
					let bush = match c.tree {
						Some(_) => None,
						None => cell_bush(cell_x, cell_z, seed),
					};
					assert_eq!(c.bush, bush, "bush {cell_x},{cell_z}");
					let l = memoized(&LANDMARK_MEMO, cell_x, cell_z, seed, || {
						cell_landmark(cell_x, cell_z, seed)
					});
					assert_eq!(l, cell_landmark(cell_x, cell_z, seed), "landmark");
				}
			}
		}
	}
}

#[test]
fn candidates_are_deterministic_and_shared_across_the_apron() {
	let seed = Seed::current(1234.5678);
	let a = structures_near(0, 0, seed);
	let b = structures_near(0, 0, seed);
	assert_eq!(a, b, "deterministic");
	// Neighboring chunks agree about shared-apron candidates.
	let right = structures_near(16, 0, seed);
	for s in &a {
		if s.origin().0 >= 16 - STRUCTURE_APRON {
			assert!(right.contains(s), "border structure {s:?} seen by both");
		}
	}
}

/// Every structure keeps the apron contract and reports an EXACT y span. Both ends are
/// load-bearing: `build_column`'s sky-chunk skipping truncates canopies at chunk-stack seams if
/// top() under-reports, and `decorate_chunk`'s per-chunk skip drops a trunk root if base() over-.
#[test]
fn structures_respect_apron_and_their_exact_y_span() {
	let seed = Seed::current(1234.5678);
	let mut seen = 0u32;
	for ccz in -16..16 {
		for ccx in -16..16 {
			for s in structures_near(ccx * 16, ccz * 16, seed) {
				seen += 1;
				let (ox, oz) = s.origin();
				let (mut min_y, mut max_y) = (i32::MAX, i32::MIN);
				s.for_each_block(|x, y, z, _| {
					assert!((x - ox).abs() <= STRUCTURE_APRON, "apron x {s:?}");
					assert!((z - oz).abs() <= STRUCTURE_APRON, "apron z {s:?}");
					min_y = min_y.min(y);
					max_y = max_y.max(y);
				});
				// Both ends exact, not merely containing: `decorate_chunk` skips a whole
				// structure whose span misses the chunk, so a base above the lowest block
				// drops it and a top below the highest drops that.
				assert_eq!(min_y, s.base(), "exact base for {s:?}");
				assert_eq!(max_y, s.top() - 1, "exact top for {s:?}");
			}
		}
	}
	assert!(seen > 0, "sample contains structures");
}

#[test]
fn landmark_shape_sane() {
	for trunk_h in 8..=10 {
		for salt in [0u16, 0xbeef, 0xffff] {
			let l = Landmark {
				x: 0,
				y: 60,
				z: 0,
				trunk_h,
				salt,
			};
			let (mut wood, mut leaves) = (0, 0);
			l.for_each_block(|x, y, z, b| {
				assert!((-4..=5).contains(&x) && (-4..=5).contains(&z));
				assert!(y >= l.y - 1 && y <= l.y + trunk_h + 1);
				match b {
					BlockType::Wood => wood += 1,
					BlockType::Leaves => leaves += 1,
					other => panic!("unexpected {other:?}"),
				}
			});
			// One column per trunk cell, each rooted one block down.
			assert_eq!(wood, 4 * (trunk_h + 1));
			// The two wide corner-clipped layers (at the low end, fully
			// jittered), the smaller layer above them, and the cap.
			assert!((156..=220).contains(&leaves), "leaves {leaves}");
		}
	}
}

#[test]
fn boulder_and_bush_shapes() {
	for pattern in 0..4u8 {
		let b = Boulder {
			x: 0,
			y: 10,
			z: 0,
			pattern,
		};
		let mut count = 0;
		let mut max_y = i32::MIN;
		b.for_each_block(|x, y, z, block| {
			assert_eq!(block, BlockType::Stone);
			assert!((0..=1).contains(&x) && (0..=1).contains(&z));
			max_y = max_y.max(y);
			count += 1;
		});
		assert!((2..=3).contains(&count));
		assert_eq!(max_y, Structure::Boulder(b).top() - 1);
	}
	for (leaf, second) in [
		(BlockType::Leaves, None),
		(BlockType::PineLeaves, Some((1, 0))),
		(BlockType::Leaves, Some((0, -1))),
	] {
		let b = Bush {
			x: 0,
			y: 10,
			z: 0,
			leaf,
			second,
		};
		let mut count = 0;
		b.for_each_block(|_, y, _, block| {
			assert_eq!(block, leaf);
			assert_eq!(y, 10);
			count += 1;
		});
		assert_eq!(count, 1 + second.is_some() as i32);
	}
}

/// Terrain-placed candidates land where their rules say: boulders on
/// rock surfaces, bushes on tree-valid ground, landmarks on flat
/// tree-valid footprints.
#[test]
fn placements_match_surface_rules() {
	let seed = Seed::current(42.0);
	let (mut boulders, mut bushes) = (0u32, 0u32);
	for cz in -128..128 {
		for cx in -128..128 {
			if let Some(b) = cell_boulder(cx, cz, seed) {
				let s = column_shape(b.x, b.z, seed);
				assert!(s.surface.props().boulders, "boulder on {:?}", s.surface);
				assert_eq!(b.y, s.height + 1);
				boulders += 1;
			}
			if let Some(b) = cell_bush(cx, cz, seed) {
				let s = column_shape(b.x, b.z, seed);
				assert!(s.surface.props().tree_density != TreeDensity::None);
				bushes += 1;
			}
		}
	}
	assert!(boulders > 0, "sample contains boulders");
	assert!(bushes > 0, "sample contains bushes");
}

#[test]
fn landmarks_exist() {
	let seed = Seed::current(1234.5678);
	let mut found = 0u32;
	for cz in -96..96 {
		for cx in -96..96 {
			if let Some(l) = cell_landmark(cx, cz, seed) {
				assert!((8..=10).contains(&l.trunk_h));
				assert!(Structure::Landmark(l).top() < WORLD_VBLOCK);
				found += 1;
			}
		}
	}
	assert!(found > 0, "coarse sample contains landmarks");
}

/// Per-kind trunk range and leaf-count bounds: the maximum is the full
/// layer list; the minimum drops every jitterable rim cell of the wide
/// layers.
fn expect_shape(kind: TreeKind) -> (std::ops::RangeInclusive<i32>, std::ops::RangeInclusive<i32>) {
	match kind {
		TreeKind::Blob => (4..=6, 25..=49),
		TreeKind::TallNarrow => (6..=7, 33..=33),
		TreeKind::Pine => (6..=7, 41..=65),
		TreeKind::SquatWide => (4..=5, 57..=109),
		TreeKind::FlatTop => (4..=6, 17..=29),
	}
}

fn assert_sane(t: &Tree) {
	let (trunk, leaves_bounds) = expect_shape(t.kind);
	assert!(
		trunk.contains(&t.trunk_h),
		"{:?} trunk {}",
		t.kind,
		t.trunk_h
	);
	assert!(t.top() < WORLD_VBLOCK);
	let expect_leaf = match t.kind {
		TreeKind::Pine => BlockType::PineLeaves,
		_ => BlockType::Leaves,
	};
	let mut wood = 0;
	let mut leaves = 0;
	t.for_each_block(|x, y, z, b| {
		assert!((x - t.x).abs() <= STRUCTURE_APRON, "apron x");
		assert!((z - t.z).abs() <= STRUCTURE_APRON, "apron z");
		assert!(y >= t.y - 1 && y < t.top(), "vertical bounds");
		match b {
			BlockType::Wood => wood += 1,
			b if b == expect_leaf => leaves += 1,
			other => panic!("unexpected {other:?}"),
		}
	});
	assert_eq!(wood, t.trunk_h + 1, "{:?} wood incl. root", t.kind);
	assert!(
		leaves_bounds.contains(&leaves),
		"{:?} leaves {leaves}",
		t.kind
	);
}

/// Terrain-placed candidates are sane, and the broadleaf sample shows
/// all three of its silhouettes. Pine/FlatTop placement depends on
/// which climates the sample area holds, so their shapes are pinned
/// by `conifer_and_acacia_shapes` on synthetic trees instead.
#[test]
fn trees_have_sane_shape() {
	let seed = Seed::current(42.0);
	let mut broadleaf = [false; 3];
	for cz in -128..128 {
		for cx in -128..128 {
			let Some(t) = cell_tree(cx, cz, seed) else {
				continue;
			};
			match t.kind {
				TreeKind::Blob => broadleaf[0] = true,
				TreeKind::TallNarrow => broadleaf[1] = true,
				TreeKind::SquatWide => broadleaf[2] = true,
				_ => {}
			}
			assert_sane(&t);
		}
	}
	assert!(
		broadleaf.iter().all(|&k| k),
		"all broadleaf kinds in sample: {broadleaf:?}"
	);
}

#[test]
fn conifer_and_acacia_shapes() {
	for (kind, trunks) in [(TreeKind::Pine, 6..=7), (TreeKind::FlatTop, 4..=6)] {
		for trunk_h in trunks {
			for salt in [0u16, 0xbeef, 0xffff] {
				assert_sane(&Tree {
					x: 0,
					y: 60,
					z: 0,
					trunk_h,
					kind,
					salt,
				});
			}
		}
	}
}

/// The landmark fallback may only ADD trees. Every anchor the original single-roll placement
/// produced must still hold a landmark at its exact spot — existing worlds keep every tree they
/// had, which is the whole argument for shipping this on a protocol bump with saves kept. A
/// placement edit that moves one of these is a save-staling worldgen retune and needs the journal
/// `VERSION` bump this change deliberately avoided.
#[test]
fn landmark_fallback_only_adds() {
	const KEPT: &[(f64, i32, i32)] = &[
		(42.0, -223, -609),
		(42.0, 129, 552),
		(42.0, 151, -504),
		(42.0, 302, 623),
		(1.0, 79, -271),
		(1.0, 295, -547),
		(1.0, 422, 480),
		(7.5, -601, -407),
		(7.5, -217, -541),
		(7.5, 13, -579),
		(7.5, 144, -591),
		(7.5, 394, -232),
		(7.5, 625, -767),
		(123.0, -703, 708),
		(123.0, -528, 486),
		(123.0, -501, -43),
		(123.0, -366, 423),
		(123.0, -296, 197),
		(123.0, -283, 748),
		(123.0, 612, -412),
		(999.25, -149, 718),
		(999.25, -25, 632),
		(2024.0, 546, -560),
		(2024.0, 555, -354),
		(5.0, -22, 169),
		(5.0, 611, -130),
		(77.0, -208, -453),
		(77.0, 449, 631),
		(77.0, 607, 566),
		(77.0, 734, 566),
	];
	for &(value, x, z) in KEPT {
		let seed = Seed::current(value);
		let t = trunk_at(x, z, seed)
			.unwrap_or_else(|| panic!("({x},{z}) at seed {value} lost its landmark"));
		assert!(
			t.landmark,
			"({x},{z}) at seed {value} is no longer a landmark"
		);
		assert_eq!(t.anchor, (x, z), "the landmark at ({x},{z}) moved");
	}

	// And the fallback must actually add: the same region holds strictly
	// more landmarks than the single roll placed there. Two seeds keep
	// the sweep cheap; the mechanism is seed-blind.
	for (value, single_roll_count) in [(42.0, 4usize), (999.25, 2)] {
		let seed = Seed::current(value);
		let mut anchors = std::collections::HashSet::new();
		for x in -768..768 {
			for z in -768..768 {
				if let Some(t) = trunk_at(x, z, seed)
					&& t.landmark
				{
					anchors.insert(t.anchor);
				}
			}
		}
		assert!(
			anchors.len() > single_roll_count,
			"seed {value}: fallback added nothing ({} landmarks)",
			anchors.len()
		);
	}
}

/// The property `decorate_chunk`'s per-chunk skip rests on, asserted directly rather than through
/// the bounds: a structure emits into a chunk if and only if its y span reaches one. Both
/// directions, so a skip can neither drop a block nor stop paying its way, and across several
/// seeds — the span pin above runs one, and a bound exact there could still be wrong elsewhere.
#[test]
fn a_structure_emits_into_a_chunk_exactly_when_its_span_reaches_it() {
	let n = CHUNK_SIZE as i32;
	let mut checked = 0u32;
	let mut nonempty = 0u32;
	for seed in [42.0, 1234.5678, -7.25, 99991.0].map(Seed::current) {
		for ccz in -6..6 {
			for ccx in -6..6 {
				for s in structures_near(ccx * 16, ccz * 16, seed) {
					for cy in 0..crate::chunk::WORLD_VCHUNK {
						let py = cy * n;
						let skip = s.top() <= py || s.base() >= py + n;
						let mut hit = false;
						s.for_each_block(|_, y, _, _| hit |= (py..py + n).contains(&y));
						assert_eq!(!skip, hit, "seed {seed:?} chunk y {cy} for {s:?}");
						checked += 1;
						nonempty += u32::from(hit);
					}
				}
			}
		}
	}
	assert!(
		checked > 0 && nonempty > 0,
		"{checked} pairs, {nonempty} landed"
	);
}
