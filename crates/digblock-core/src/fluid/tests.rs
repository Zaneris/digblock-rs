use super::*;
use rustc_hash::FxHashMap;

/// A bounded box of air over one flat stone floor course at the bottom,
/// overridable per cell; outside → None (unloaded). `xmax` shrinks the
/// loaded region to model residency boundaries.
struct MapWorld {
	cells: FxHashMap<(i32, i32, i32), BlockType>,
	xmax: i32,
}

impl MapWorld {
	fn new() -> Self {
		Self {
			cells: FxHashMap::default(),
			xmax: 24,
		}
	}

	fn water_cells(&self) -> Vec<((i32, i32, i32), u8)> {
		let mut v: Vec<_> = self
			.cells
			.iter()
			.filter(|(_, b)| b.is_water())
			.map(|(&p, &b)| (p, b as u8))
			.collect();
		v.sort();
		v
	}
}

impl FluidWorld for MapWorld {
	fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		if x.abs() > 24 || x > self.xmax || z.abs() > 24 || !(0..=24).contains(&y) {
			return None;
		}
		Some(*self.cells.get(&(x, y, z)).unwrap_or(if y == 0 {
			&BlockType::Stone
		} else {
			&BlockType::Air
		}))
	}

	fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool {
		if self.get(x, y, z).is_none() {
			return false;
		}
		self.cells.insert((x, y, z), b);
		true
	}
}

fn settle(sim: &mut FluidSim, world: &mut MapWorld) {
	for _ in 0..10_000 {
		if sim.step(world, 512) == 0 {
			return;
		}
	}
	panic!("sim never converged");
}

#[test]
fn source_spreads_to_a_plus_shaped_pool() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	w.set(0, 1, 0, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	settle(&mut sim, &mut w);
	// The amount halves with each Manhattan step, and the pool stops
	// where the halving runs out.
	for (dx, dz, expect) in [
		(1, 0, Some(4u8)),
		(0, -2, Some(2)),
		(3, 0, Some(1)),
		(2, 1, Some(1)),
		(4, 0, None),
		(2, 2, None),
	] {
		let b = w.get(dx, 1, dz).unwrap();
		assert_eq!(b.water_amount().filter(|_| b != BlockType::Water), expect);
	}
	// The source itself plus the full Manhattan ball the halving reaches.
	assert_eq!(
		w.water_cells().len(),
		1 + (1..=3).map(|d| 4 * d).sum::<usize>()
	);
}

#[test]
fn removing_the_source_drains_everything() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	w.set(0, 1, 0, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	settle(&mut sim, &mut w);
	w.set(0, 1, 0, BlockType::Air);
	sim.schedule_around(0, 1, 0);
	settle(&mut sim, &mut w);
	assert_eq!(w.water_cells(), vec![], "fully drained");
}

#[test]
fn water_falls_into_a_pit_and_spreads_on_its_floor() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	// A one-cell shaft in the floor, within reach of the source: below
	// the floor course is the world edge (None, never solid), so the
	// shaft is bottomless — the floor around it stays solid and the
	// source goes beside the hole at surface level.
	w.set(3, 0, 0, BlockType::Air); // hole in the floor... bottomless
	w.set(0, 1, 0, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	settle(&mut sim, &mut w);
	// Flow reaches the hole and pours in.
	assert!(w.get(3, 1, 0).unwrap().is_water(), "flow crosses the hole");
	assert_eq!(
		w.get(3, 0, 0).unwrap(),
		BlockType::WaterFall,
		"water falls through the opened floor"
	);
}

#[test]
fn landed_fall_spreads_at_full_strength() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	// Floating source high up: falls straight down, lands on the
	// floor, then spreads a full-strength pool there (falls are full
	// strength, so the landing restarts the halving ladder).
	w.set(0, 6, 0, BlockType::Water);
	sim.schedule_around(0, 6, 0);
	settle(&mut sim, &mut w);
	for y in 1..6 {
		assert_eq!(w.get(0, y, 0).unwrap(), BlockType::WaterFall, "y={y}");
	}
	assert_eq!(w.get(1, 1, 0).unwrap().water_amount(), Some(4));
	assert_eq!(w.get(3, 1, 0).unwrap().water_amount(), Some(1));
	assert_eq!(w.get(4, 1, 0).unwrap().water_amount(), None);
}

#[test]
fn budget_bounds_each_step() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	w.set(0, 1, 0, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	assert!(sim.step(&mut w, 3) <= 3);
	assert!(sim.pending() > 0, "remaining work stays queued");
}

/// The load-boundary contract: a front stalls where the world ends
/// (unloaded cells drop), and rescheduling the surviving water — what
/// the server does when the region loads — resumes it to exactly the
/// state an always-loaded run reaches.
#[test]
fn stalled_front_resumes_on_load_and_matches_reference() {
	let reference = {
		let mut w = MapWorld::new();
		let mut sim = FluidSim::new();
		w.set(0, 1, 0, BlockType::Water);
		sim.schedule_around(0, 1, 0);
		settle(&mut sim, &mut w);
		w.water_cells()
	};

	// The world now ends inside the pool: its far half cannot form.
	let mut w = MapWorld::new();
	w.xmax = 1;
	let mut sim = FluidSim::new();
	w.set(0, 1, 0, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	settle(&mut sim, &mut w);
	assert!(
		w.water_cells().len() < reference.len(),
		"front stalled at the boundary"
	);

	// The region loads; the sim hook reschedules surviving water.
	w.xmax = 24;
	for ((x, y, z), _) in w.water_cells() {
		sim.schedule_around(x, y, z);
	}
	settle(&mut sim, &mut w);
	assert_eq!(w.water_cells(), reference, "front completed identically");
}

#[test]
fn two_sources_one_apart_regenerate_the_middle() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	w.set(0, 1, 0, BlockType::Water);
	w.set(2, 1, 0, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	sim.schedule_around(2, 1, 0);
	settle(&mut sim, &mut w);
	assert_eq!(
		w.get(1, 1, 0).unwrap(),
		BlockType::Water,
		"the flanked middle becomes a source"
	);
}

#[test]
fn diagonal_pair_chain_fills_the_pool_and_is_renewable() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	// A 2×2 pool seeded with two diagonal sources: each empty corner
	// touches both, so the whole pool converts (the pinned chain
	// reaction).
	w.set(0, 1, 0, BlockType::Water);
	w.set(1, 1, 1, BlockType::Water);
	sim.schedule_around(0, 1, 0);
	sim.schedule_around(1, 1, 1);
	settle(&mut sim, &mut w);
	assert_eq!(w.get(1, 1, 0).unwrap(), BlockType::Water);
	assert_eq!(w.get(0, 1, 1).unwrap(), BlockType::Water);

	// Scooping any cell of the converted pool refills it: renewable.
	w.set(1, 1, 0, BlockType::Air);
	sim.schedule_around(1, 1, 0);
	settle(&mut sim, &mut w);
	assert_eq!(w.get(1, 1, 0).unwrap(), BlockType::Water, "pool refilled");
}

#[test]
fn deep_water_scoop_hole_heals() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	// A 3×3 two-deep pond of sources: the flow rule alone cannot fill
	// a surface hole here (its side sources rest on water, so they
	// don't count as supported feeders) — regen must do it, via the
	// source-below floor.
	for x in 0..3 {
		for z in 0..3 {
			for y in 1..=2 {
				w.set(x, y, z, BlockType::Water);
			}
		}
	}
	w.set(1, 2, 1, BlockType::Air); // scoop the surface center
	sim.schedule_around(1, 2, 1);
	settle(&mut sim, &mut w);
	assert_eq!(w.get(1, 2, 1).unwrap(), BlockType::Water, "hole healed");
}

#[test]
fn floating_source_pair_grows_no_hanging_shelf() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	// Two sources in mid-air, one apart: falls pour beneath them and
	// flow pools on the floor below the gap — which must NOT count as
	// a whole floor, or the pair would grow a hanging source shelf.
	w.set(0, 2, 0, BlockType::Water);
	w.set(2, 2, 0, BlockType::Water);
	sim.schedule_around(0, 2, 0);
	sim.schedule_around(2, 2, 0);
	settle(&mut sim, &mut w);
	assert_ne!(
		w.get(1, 2, 0).unwrap(),
		BlockType::Water,
		"no source materializes over a non-whole floor"
	);
}

/// Nothing standing over air spreads outward — a SOURCE included.
/// Poured against a wall face, the block runs straight down the wall
/// and only its landing spreads; the placed cell's own level stays
/// dry on every side. Its own top face still renders full height all round
/// (`mesher::tests::a_spilling_source_keeps_its_full_height`), which
/// is the half of this arrangement that WAS broken.
#[test]
fn an_unsupported_source_pours_straight_down() {
	let mut w = MapWorld::new();
	let mut sim = FluidSim::new();
	w.set(0, 2, 0, BlockType::Water); // placed in mid-air over the floor
	sim.schedule_around(0, 2, 0);
	settle(&mut sim, &mut w);
	for (dx, dz) in SIDES {
		assert_eq!(
			w.get(dx, 2, dz).unwrap(),
			BlockType::Air,
			"side ({dx}, {dz}) at the source's own level stays dry"
		);
	}
	assert_eq!(
		w.get(0, 1, 0).unwrap(),
		BlockType::WaterFall,
		"straight down"
	);
	// The landing is on the stone floor, so THAT spreads.
	assert_eq!(w.get(1, 1, 0).unwrap().water_amount(), Some(4));
}
