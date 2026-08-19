use super::*;
use crate::worldgen::column_shape;

/// The altitude presieve may only reject columns no family would have
/// accepted anyway — nothing grows and nothing piles at or below the
/// waterline band — and a survivor must classify exactly as the full
/// five-evaluation path does.
#[test]
fn presieve_only_rejects_barren_columns() {
	let seed = 1234.5678;
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
	let seeds = [42.0f64, 1234.5678, -7.0];
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
fn candidates_are_deterministic_and_spaced() {
	let seed = 1234.5678;
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

/// Every structure keeps the apron contract and reports an EXACT
/// top(): build_column's sky-chunk skipping truncates canopies at
/// chunk-stack seams if top() ever under-reports.
#[test]
fn structures_respect_apron_and_exact_tops() {
	let seed = 1234.5678;
	let mut seen = 0u32;
	for ccz in -16..16 {
		for ccx in -16..16 {
			for s in structures_near(ccx * 16, ccz * 16, seed) {
				seen += 1;
				let (ox, oz) = s.origin();
				let mut max_y = i32::MIN;
				s.for_each_block(|x, y, z, _| {
					assert!((x - ox).abs() <= STRUCTURE_APRON, "apron x {s:?}");
					assert!((z - oz).abs() <= STRUCTURE_APRON, "apron z {s:?}");
					max_y = max_y.max(y);
				});
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
	let seed = 42.0;
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
	let seed = 1234.5678;
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
	let seed = 42.0;
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
