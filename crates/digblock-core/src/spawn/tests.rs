
use super::*;
use crate::block::BlockType;
use std::collections::HashMap;

const SEED: f64 = 7.0;

/// An overlay backed by an explicit cell map.
fn overlay(cells: HashMap<(i32, i32, i32), u16>) -> impl Fn(i32, i32, i32) -> Option<u16> {
	move |x, y, z| cells.get(&(x, y, z)).copied()
}

fn anchor_column(seed: f64) -> (i32, i32, i32) {
	let a = anchor(seed);
	(a.x.floor() as i32, a.y.floor() as i32, a.z.floor() as i32)
}

/// The whole point of factoring the predicate out: with nothing
/// edited, the search must agree with the pristine walk exactly. If
/// this drifts, spawn silently moves for every world at once.
#[test]
fn resolve_with_no_edits_is_the_anchor() {
	for seed in [1.0, 7.0, 42.0, 1234.5] {
		assert_eq!(
			resolve(anchor(seed), seed, no_edits()),
			anchor(seed),
			"seed {seed}"
		);
	}
}

/// The bug this module exists for: dig the floor out from under spawn
/// and the spawn must move sideways, not fall down the hole.
#[test]
fn digging_the_floor_moves_spawn_aside() {
	let a = anchor(SEED);
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	let h = a.y.floor() as i32 - 2; // back out the floor from the eye (see `eye`)
	let mut cells = HashMap::new();
	cells.insert((ax, h, az), BlockType::Air as u16);
	let moved = resolve(a, SEED, &overlay(cells));
	assert_ne!(moved, a, "spawn stayed on a dug-out floor");
	// Sideways, never downward: the new floor is a pristine surface,
	// so it is at or above sea level and never inside the hole.
	assert!(
		moved.y >= SPAWN_FLOOR as f32,
		"spawn dropped below sea level"
	);
	let d = ((moved.x - a.x).abs()).max((moved.z - a.z).abs());
	assert!(d > 0.0, "spawn did not move horizontally");
}

/// Fine-grained near the anchor: a single dug block should nudge
/// spawn a few columns, not bounce it a full anchor stride.
#[test]
fn a_small_hole_moves_spawn_only_a_few_columns() {
	let a = anchor(SEED);
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	let h = a.y.floor() as i32 - 2;
	let mut cells = HashMap::new();
	cells.insert((ax, h, az), BlockType::Air as u16);
	let moved = resolve(a, SEED, &overlay(cells));
	let d = ((moved.x - a.x).abs()).max((moved.z - a.z).abs());
	assert!(
		d <= ANCHOR_STEP as f32,
		"a 1x1 hole bounced spawn {d} blocks"
	);
}

/// Building into the standing space is as disqualifying as digging
/// the floor away — that is what makes a locked region always valid.
#[test]
fn building_into_the_capsule_moves_spawn_aside() {
	let a = anchor(SEED);
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	let h = a.y.floor() as i32 - 2;
	let mut cells = HashMap::new();
	cells.insert((ax, h + 1, az), BlockType::Stone as u16);
	assert_ne!(
		resolve(a, SEED, &overlay(cells)),
		a,
		"spawn stayed inside a placed block"
	);
}

/// Edits that don't hurt must not move spawn: filling a hollow at
/// floor level, and anything above the capsule.
#[test]
fn harmless_edits_leave_spawn_alone() {
	let a = anchor(SEED);
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	let h = a.y.floor() as i32 - 2;
	let mut cells = HashMap::new();
	// Filling in at floor level, out at the disc rim.
	cells.insert((ax + CLEAR, h, az + CLEAR), BlockType::Stone as u16);
	// A roof well clear of the capsule.
	cells.insert((ax, h + CAPSULE + 4, az), BlockType::Stone as u16);
	assert_eq!(
		resolve(a, SEED, &overlay(cells)),
		a,
		"spawn moved for nothing"
	);
}

/// Digging just outside the disc is none of spawn's business.
#[test]
fn digging_outside_the_disc_leaves_spawn_alone() {
	let a = anchor(SEED);
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	let h = a.y.floor() as i32 - 2;
	let mut cells = HashMap::new();
	cells.insert((ax + CLEAR + 1, h, az), BlockType::Air as u16);
	assert_eq!(
		resolve(a, SEED, &overlay(cells)),
		a,
		"spawn moved for nothing"
	);
}

/// The load-bearing equivalence: the lock protects exactly the cells
/// the search inspects. If the region ever became smaller than the
/// read set, a locked spawn could be invalidated through the gap and
/// the "locked ⇒ skip the search" fast path would start handing out a
/// broken spawn.
#[test]
fn lock_region_covers_what_the_test_reads() {
	let a = anchor(SEED);
	let region = lock_region(a);
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	let h = a.y.floor() as i32 - 2;
	// Every cell an edit could be placed in to break this spawn.
	for cz in az - CLEAR..=az + CLEAR {
		for cx in ax - CLEAR..=ax + CLEAR {
			for cy in h..=h + CAPSULE {
				let mut cells = HashMap::new();
				// Floor cells break by being dug, capsule cells by
				// being filled — whichever applies at this height.
				let cell = if cy == h {
					BlockType::Air as u16
				} else {
					BlockType::Stone as u16
				};
				cells.insert((cx, cy, cz), cell);
				if resolve(a, SEED, &overlay(cells)) != a {
					assert!(
						region.contains(cx, cy, cz),
						"edit at ({cx},{cy},{cz}) moves spawn but is not protected"
					);
				}
			}
		}
	}
}

/// The property the whole module exists to provide, asserted against
/// worldgen directly rather than by trusting the predicate that chose
/// the spot: the disc really is level solid ground, and the cylinder
/// above it really is empty.
///
/// The predicate used to permit neighbours at `h + 1` and ignore
/// neighbours below `h`, so spawns could be boxed in by rock at head
/// height or perched on the lip of a drop. Every earlier test asked
/// only whether spawn *moved*, never whether where it landed was any
/// good — which is exactly why that survived.
#[test]
fn every_anchor_stands_on_level_ground_under_open_sky() {
	for seed in [
		1.0, 3.0, 7.0, 42.0, 99.25, 555.0, 1234.5, 31337.0, 8675309.0,
	] {
		let a = anchor(seed);
		let (x, h, z) = (
			a.x.floor() as i32,
			a.y.floor() as i32 - 2,
			a.z.floor() as i32,
		);
		assert!(h >= SPAWN_FLOOR, "seed {seed}: spawn below sea level");
		let near = column_shapes_in(x - CLEAR, x + CLEAR, z - CLEAR, z + CLEAR, seed);
		for (i, n) in near.iter().enumerate() {
			let (dx, dz) = (
				i as i32 % (2 * CLEAR + 1) - CLEAR,
				i as i32 / (2 * CLEAR + 1) - CLEAR,
			);
			assert_eq!(
				n.height, h,
				"seed {seed}: column ({dx},{dz}) of the disc sits at {}, not {h} \
					 — spawn is on a step or a ledge",
				n.height
			);
			assert!(
				n.water <= n.height && n.fall.is_none(),
				"seed {seed}: column ({dx},{dz}) of the disc holds water"
			);
			assert!(
				!carved(x + dx, h, z + dz, seed),
				"seed {seed}: column ({dx},{dz}) of the disc is a cave mouth"
			);
		}
		assert!(
			!crate::decoration::box_obstructed(
				x - CLEAR,
				x + CLEAR,
				h + 1,
				h + CAPSULE,
				z - CLEAR,
				z + CLEAR,
				seed
			),
			"seed {seed}: decoration intrudes into the spawn cylinder"
		);
	}
}

/// A *relocated* spawn is held to the same standard as the anchor.
/// The reported bug was about where the search moved the player to,
/// not where it started, so checking anchors alone would miss it.
#[test]
fn a_relocated_spawn_is_also_level_ground() {
	for seed in [1.0, 7.0, 42.0, 555.0, 1234.5] {
		let a = anchor(seed);
		let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
		let h = a.y.floor() as i32 - 2;
		// Dig the anchor's floor out to force a relocation.
		let mut cells = HashMap::new();
		cells.insert((ax, h, az), BlockType::Air as u16);
		let moved = resolve(a, seed, &overlay(cells));
		assert_ne!(moved, a, "seed {seed}: spawn did not relocate");
		let (mx, mz) = (moved.x.floor() as i32, moved.z.floor() as i32);
		let mh = moved.y.floor() as i32 - 2;
		assert!(mh >= SPAWN_FLOOR, "seed {seed}: relocated below sea level");
		let near = column_shapes_in(mx - CLEAR, mx + CLEAR, mz - CLEAR, mz + CLEAR, seed);
		for (i, n) in near.iter().enumerate() {
			let (dx, dz) = (
				i as i32 % (2 * CLEAR + 1) - CLEAR,
				i as i32 / (2 * CLEAR + 1) - CLEAR,
			);
			assert_eq!(
				n.height, mh,
				"seed {seed}: relocated onto a step/ledge — disc column \
					 ({dx},{dz}) sits at {}, not {mh}",
				n.height
			);
		}
	}
}

/// A flat world reading from an explicit cell map, defaulting to stone
/// at or below `top` and air above — the shape `stands_clear` is meant
/// to accept.
fn world(top: i32, patch: HashMap<(i32, i32, i32), u16>) -> impl Fn(i32, i32, i32) -> Option<u16> {
	move |x, y, z| {
		Some(patch.get(&(x, y, z)).copied().unwrap_or({
			if y <= top {
				BlockType::Stone as u16
			} else {
				BlockType::Air as u16
			}
		}))
	}
}

fn eye_over(top: i32) -> Vec3 {
	eye(0, top, 0)
}

/// The happy path, and the two rules stated the way the doc comment
/// states them: the disc is ground, the cylinder above it is air.
#[test]
fn stands_clear_accepts_level_ground_and_rejects_a_step_or_a_hole() {
	let top = 100; // deliberately below SPAWN_FLOOR: it does not apply here
	assert!(stands_clear(eye_over(top), &world(top, HashMap::new())));

	// A step up at the disc rim puts stone in the capsule.
	let mut up = HashMap::new();
	up.insert((CLEAR, top + 1, 0), BlockType::Stone as u16);
	assert!(!stands_clear(eye_over(top), &world(top, up)));

	// A hole at the rim leaves air in the floor.
	let mut hole = HashMap::new();
	hole.insert((0, top, CLEAR), BlockType::Air as u16);
	assert!(!stands_clear(eye_over(top), &world(top, hole)));

	// Just outside the disc is none of its business.
	let mut outside = HashMap::new();
	outside.insert((CLEAR + 1, top, 0), BlockType::Air as u16);
	outside.insert((0, top + 1, CLEAR + 1), BlockType::Stone as u16);
	assert!(stands_clear(eye_over(top), &world(top, outside)));
}

/// Water is not solid, so nothing else in the predicate would catch a
/// spawn set in the shallows — this rule is what keeps a hand-picked
/// spawn out of the sea now that [`SPAWN_FLOOR`] does not apply to it.
#[test]
fn stands_clear_rejects_standing_water_in_the_capsule() {
	let top = 100;
	let mut wet = HashMap::new();
	wet.insert((0, top + 1, 0), BlockType::Water as u16);
	assert!(!stands_clear(eye_over(top), &world(top, wet)));
}

/// An unreadable cell is a rejection, never a pass: refusing to set a
/// spawn we cannot see is the safe direction.
#[test]
fn stands_clear_rejects_an_unreadable_cell() {
	let top = 100;
	let cell = move |x: i32, y: i32, z: i32| {
		// One corner column of the disc is not resident.
		if (x, z) == (CLEAR, CLEAR) {
			return None;
		}
		Some(if y <= top {
			BlockType::Stone as u16
		} else {
			BlockType::Air as u16
		})
	};
	assert!(!stands_clear(eye_over(top), &cell));
}

/// The load-bearing alignment: the cells `stands_clear` reads are
/// exactly the ones the lock protects, so locking a hand-picked spawn
/// makes it permanently valid the same way locking a searched one does.
#[test]
fn stands_clear_reads_exactly_the_locked_region() {
	let top = 100;
	let e = eye_over(top);
	let region = lock_region(e);
	for cz in -CLEAR - 1..=CLEAR + 1 {
		for cx in -CLEAR - 1..=CLEAR + 1 {
			for cy in top - 1..=top + CAPSULE + 1 {
				let mut patch = HashMap::new();
				// Break the cell whichever way applies at this height.
				let cell = if cy <= top {
					BlockType::Air as u16
				} else {
					BlockType::Stone as u16
				};
				patch.insert((cx, cy, cz), cell);
				let broke = !stands_clear(e, &world(top, patch));
				assert_eq!(
					broke,
					region.contains(cx, cy, cz),
					"cell ({cx},{cy},{cz}): broke={broke}, protected={}",
					region.contains(cx, cy, cz)
				);
			}
		}
	}
}

/// Anchors are above sea level by construction; pin it so a future
/// change to the altitude pre-gate can't quietly sink spawn.
#[test]
fn anchors_sit_at_or_above_sea_level() {
	for seed in [1.0, 7.0, 42.0, 1234.5, 99.25] {
		let (_, y, _) = anchor_column(seed);
		assert!(y - 2 >= SPAWN_FLOOR, "seed {seed} anchored below sea level");
	}
}
