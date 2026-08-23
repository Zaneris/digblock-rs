use super::*;
use crate::worldjob::Run;
use rustc_hash::FxHashMap;

const SEED: f64 = 7.0;

/// A hand-built pocket of cells. `airy` answers Air everywhere unset, so
/// rays stay inside the pocket; `sealed` answers `None` there, so a ray
/// falls through to the worldgen synth.
struct Pocket {
	cells: FxHashMap<(i32, i32, i32), u16>,
	fill: Option<u16>,
}

impl Pocket {
	fn airy() -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: Some(BlockType::Air as u16),
		}
	}
	fn sealed() -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: None,
		}
	}
	fn set(&mut self, at: (i32, i32, i32), b: BlockType) {
		self.cells.insert(at, b as u16);
	}
}

impl CellSource for Pocket {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.cells.get(&(x, y, z)).copied().or(self.fill)
	}
}

const AT: (i32, i32, i32) = (0, 64, 0);

/// Dirt at `AT` with one exposed Grass beside it — the smallest world
/// the rule says yes to.
fn seeded() -> Pocket {
	let mut w = Pocket::airy();
	w.set(AT, BlockType::Dirt);
	w.set((1, 64, 0), BlockType::Grass);
	w
}

fn noon() -> GrassSpread {
	let [_, noon, _] = SPREAD_HOURS;
	GrassSpread::at_hour(noon, "t-noon")
}

#[test]
fn spreads_beside_exposed_grass() {
	assert_eq!(
		noon().apply(
			AT,
			&seeded(),
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		Some(BlockType::Grass as u16)
	);
}

#[test]
fn no_grass_neighbor_no_spread() {
	let mut w = Pocket::airy();
	w.set(AT, BlockType::Dirt);
	assert_eq!(
		noon().apply(
			AT,
			&w,
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		None
	);
}

#[test]
fn a_covered_target_never_grasses() {
	for lid in [BlockType::Stone, BlockType::Water] {
		let mut w = seeded();
		w.set((AT.0, AT.1 + 1, AT.2), lid);
		assert_eq!(
			noon().apply(
				AT,
				&w,
				SEED,
				Run {
					slot: 0,
					players: 1
				}
			),
			None,
			"{lid:?} lid"
		);
	}
}

#[test]
fn a_buried_neighbor_does_not_count() {
	let mut w = seeded();
	w.set((1, 65, 0), BlockType::Stone);
	assert_eq!(
		noon().apply(
			AT,
			&w,
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		None
	);
}

/// Exactly the threshold spreads; one fewer does not — for both kinds,
/// with the counts DERIVED from the consts.
#[test]
fn thresholds_bind_exactly() {
	// Distinct neighbor slots in the 26-box around AT.
	let slots = [
		(1, 64, 0),
		(-1, 64, 0),
		(0, 64, 1),
		(0, 64, -1),
		(1, 64, 1),
		(-1, 64, -1),
	];
	for (kind, min) in [
		(BlockType::Grass, GRASS_MIN_NEIGHBORS),
		(BlockType::DryGrass, DRY_MIN_NEIGHBORS),
	] {
		assert!((min as usize) <= slots.len(), "add slots for the sweep");
		for n in [min - 1, min] {
			let mut w = Pocket::airy();
			w.set(AT, BlockType::Dirt);
			for s in &slots[..n as usize] {
				w.set(*s, kind);
			}
			let want = (n >= min).then_some(kind as u16);
			assert_eq!(
				noon().apply(
					AT,
					&w,
					SEED,
					Run {
						slot: 0,
						players: 1
					}
				),
				want,
				"{kind:?} × {n}"
			);
		}
	}
}

#[test]
fn green_wins_when_both_thresholds_are_met() {
	let mut w = Pocket::airy();
	w.set(AT, BlockType::Dirt);
	let grass_slots = [(1, 64, 0), (-1, 64, 0), (0, 64, 1)];
	let dry_slots = [
		(0, 64, -1),
		(1, 64, 1),
		(-1, 64, 1),
		(1, 64, -1),
		(-1, 64, -1),
	];
	assert!(grass_slots.len() as u32 >= GRASS_MIN_NEIGHBORS);
	assert!(dry_slots.len() as u32 >= DRY_MIN_NEIGHBORS);
	for s in &grass_slots[..GRASS_MIN_NEIGHBORS as usize] {
		w.set(*s, BlockType::Grass);
	}
	for s in &dry_slots[..DRY_MIN_NEIGHBORS as usize] {
		w.set(*s, BlockType::DryGrass);
	}
	assert_eq!(
		noon().apply(
			AT,
			&w,
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		Some(BlockType::Grass as u16)
	);
}

#[test]
fn a_roof_blocks_noon() {
	let mut w = seeded();
	for dz in -1..=1 {
		for dx in -1..=1 {
			w.set((AT.0 + dx, 70, AT.2 + dz), BlockType::Stone);
		}
	}
	assert_eq!(
		noon().apply(
			AT,
			&w,
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		None
	);
}

/// A wall on the sun's side of the sky blocks the morning ray; the same
/// wall mirrored does not. The side is DERIVED from the caster, never
/// assumed — compass names are a trap.
#[test]
fn morning_light_comes_from_the_sunrise_side() {
	let [morning, _, _] = SPREAD_HOURS;
	let job = GrassSpread::at_hour(morning, "t-9am");
	let toward_sun = -Sun::with_tod(tod_of_hour(morning)).sun_dir();
	assert!(toward_sun.x != 0.0, "a morning sun cannot be overhead");
	let wall = |side: f32| {
		let mut w = seeded();
		let x0 = if side < 0.0 { -5 } else { 3 };
		for x in x0..=(x0 + 2) {
			for y in 64..=75 {
				for z in -2..=2 {
					w.set((x, y, z), BlockType::Stone);
				}
			}
		}
		w
	};
	assert_eq!(
		job.apply(
			AT,
			&wall(toward_sun.x),
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		None,
		"sun-side wall must shade"
	);
	assert_eq!(
		job.apply(
			AT,
			&wall(-toward_sun.x),
			SEED,
			Run {
				slot: 0,
				players: 1
			}
		),
		Some(BlockType::Grass as u16),
		"far-side wall must not"
	);
}

#[test]
fn leaves_block_sun_and_water_passes_it() {
	for (canopy, want) in [
		(BlockType::Leaves, None),
		(BlockType::Water, Some(BlockType::Grass as u16)),
	] {
		let mut w = seeded();
		for dz in -1..=1 {
			for dx in -1..=1 {
				w.set((AT.0 + dx, 70, AT.2 + dz), canopy);
			}
		}
		assert_eq!(
			noon().apply(
				AT,
				&w,
				SEED,
				Run {
					slot: 0,
					players: 1
				}
			),
			want,
			"{canopy:?} canopy"
		);
	}
}

/// Beyond loaded cells the ray reads pure worldgen: below the column's
/// height it is buried, above it is sky — expectations derived from
/// `terrain_height` at this seed, not from a chosen y.
#[test]
fn unloaded_columns_judge_by_worldgen() {
	let h = worldgen::terrain_height(AT.0, AT.2, SEED);
	for (y, want) in [(h - 5, None), (h + 5, Some(BlockType::Grass as u16))] {
		assert!(y > 1, "seed put the surface too low for this test");
		let mut w = Pocket::sealed();
		w.set((0, y, 0), BlockType::Dirt);
		w.set((0, y + 1, 0), BlockType::Air);
		w.set((1, y, 0), BlockType::Grass);
		w.set((1, y + 1, 0), BlockType::Air);
		assert_eq!(
			noon().apply(
				(0, y, 0),
				&w,
				SEED,
				Run {
					slot: 0,
					players: 1
				}
			),
			want,
			"y = h{:+}",
			y - h
		);
	}
}

#[test]
fn the_answer_is_a_pure_function_of_seed_cell_run() {
	let w = seeded();
	let [morning, _, _] = SPREAD_HOURS;
	for run in 0..20 {
		let a = GrassSpread::at_hour(morning, "t-a").apply(
			AT,
			&w,
			SEED,
			Run {
				slot: run,
				players: 1,
			},
		);
		let b = GrassSpread::at_hour(morning, "t-b").apply(
			AT,
			&w,
			SEED,
			Run {
				slot: run,
				players: 1,
			},
		);
		assert_eq!(a, b, "run {run}");
	}
}

/// Sweeping the production roster: every offset is a day fraction and
/// every registration survived `at_hour`'s horizon assert by being
/// constructed at all.
#[test]
fn every_registration_keeps_the_sun_above_the_horizon() {
	let roster = jobs();
	for job in &roster {
		let off = job.schedule().offset;
		assert!((0.0..1.0).contains(&off), "{}: offset {off}", job.name());
		assert_eq!(job.schedule().per_day, 1, "{}", job.name());
		assert_eq!(
			job.scope().candidates,
			Candidates::Frontier {
				surface_shell: true
			},
			"{}",
			job.name()
		);
	}
	let mut names: Vec<_> = roster.iter().map(|j| j.name()).collect();
	names.sort_unstable();
	names.dedup();
	assert_eq!(names.len(), roster.len(), "registrations share a name");
}

/// `smothered_by` and the mesher must name ONE set of covers: a cover smothers exactly when the
/// mesh stops drawing the grass top face under it. Swept over every block × valid state against a
/// real mesh — an independent oracle — so a shape whose underside opens moves both sides or fails
/// here. Both outcomes counted, or a sweep of spared covers would pass vacuously.
#[test]
fn a_cover_smothers_exactly_when_it_culls_the_grass_top_face() {
	use crate::chunk::{ChunkData, make_cell};
	use crate::mesher::{MeshInput, Src, mesh_chunk};
	let grass = BlockType::Grass as u16;
	let (mut smothering, mut sparing) = (0u32, 0u32);
	for b in BlockType::ALL {
		for state in 0..16u8 {
			let cell = make_cell(b, state);
			if b == BlockType::Air || !shapes::state_valid(cell) {
				continue;
			}
			let mut d = ChunkData::default();
			d.set(8, 8, 8, BlockType::Grass);
			d.set_cell(8, 9, 8, cell);
			let m = mesh_chunk(&MeshInput::build(&d, |_, _, _| Src::Missing));
			// The grass top face: dir Top, every corner on the shared
			// plane (y = 9 blocks, in twelfths).
			let top_drawn = m.solid.chunks(8).any(|q| {
				(q[0] >> 24) & 0x7 == Dir::Top as u32
					&& q.chunks(2).all(|p| (p[0] >> 8) & 0xff == 9 * 12)
			});
			let smothers = smothered_by(cell, grass).is_some();
			assert_eq!(smothers, !top_drawn, "{b:?} state {state}");
			if smothers {
				smothering += 1;
			} else {
				sparing += 1;
			}
		}
	}
	assert!(smothering > 0 && sparing > 0, "one-sided sweep");
}

/// The cycle closes for both spreaders: what the sweep greens, a
/// sealing place turns back into the very block the sweep hunts — so an
/// uncovered cell is a candidate again, and the covering edit itself
/// feeds the frontier. Winners DERIVED by running the rule, never named.
#[test]
fn a_smothered_spread_returns_to_the_candidate_set() {
	let job = noon();
	let slots = [(1, 64, 0), (-1, 64, 0), (0, 64, 1), (0, 64, -1)];
	for (kind, min) in [
		(BlockType::Grass, GRASS_MIN_NEIGHBORS),
		(BlockType::DryGrass, DRY_MIN_NEIGHBORS),
	] {
		let mut w = Pocket::airy();
		w.set(AT, BlockType::Dirt);
		for s in &slots[..min as usize] {
			w.set(*s, kind);
		}
		let win = job
			.apply(
				AT,
				&w,
				SEED,
				Run {
					slot: 0,
					players: 1,
				},
			)
			.expect("a lit spread");
		let dirt = smothered_by(BlockType::Stone as u16, win).expect("stone seals");
		assert!(
			job.scope()
				.blocks
				.contains(&BlockType::from_id(cell_id(dirt))),
			"{kind:?}"
		);
	}
}

/// The greenhouse rule, swept over the bucket: a see-through cover —
/// glass, crystal, a translucent shaped cover in ANY state — never
/// smothers, because the sun still reaches the turf through it. Counted,
/// so a future pane joins the pin by carrying the bucket.
#[test]
fn a_translucent_cover_never_smothers() {
	use crate::block::MeshBucket;
	use crate::chunk::make_cell;
	let mut examined = 0u32;
	for b in BlockType::ALL {
		if b.props().bucket != MeshBucket::Translucent {
			continue;
		}
		for state in 0..16u8 {
			let cell = make_cell(b, state);
			if !shapes::state_valid(cell) {
				continue;
			}
			examined += 1;
			assert_eq!(
				smothered_by(cell, BlockType::Grass as u16),
				None,
				"{b:?} state {state}"
			);
		}
	}
	assert!(examined > 0, "the bucket has members to sweep");
}

/// The deliberate exclusions, pinned by name: a cover you can see past
/// — a poured source, a torch — leaves grass alive. The mesh sweep
/// would follow a reclassified block's culling and agree with the bug;
/// this names the two the design calls out.
#[test]
fn water_and_torches_never_smother() {
	for b in [BlockType::Water, BlockType::Torch] {
		assert_eq!(
			smothered_by(b as u16, BlockType::Grass as u16),
			None,
			"{b:?}"
		);
	}
}
