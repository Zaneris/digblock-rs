//! Where players appear: the pristine anchor, and the edit-aware search
//! that keeps them out of holes players have dug.
//!
//! Two layers, deliberately separated:
//!
//! * [`anchor`] is the original seed-derived walk — a pure function of the
//!   seed alone, scanning pristine worldgen for a spot that is dry, open,
//!   and not a cave mouth. It never sees player edits, so it is stable for
//!   the life of a world and is the fixed point everything else is
//!   measured against.
//! * [`resolve`] takes the anchor and asks whether players have since
//!   ruined it, searching outward for the nearest intact column if so.
//!
//! Anchoring the search at [`anchor`] rather than at the *current* spawn
//! is what stops drift compounding: every resolve is measured from the
//! same fixed point, so a griefer can push spawn aside but never walk it
//! across the map, and the moment their crater is filled or built over the
//! spawn snaps back on its own. Nothing is stored — spawn is a pure
//! function of `(seed, edits)`, so a restart reproduces whatever the live
//! server picked.
//!
//! The search never looks downward. A dug-out spawn relocates sideways
//! rather than descending into the hole, and [`SPAWN_FLOOR`] keeps it out
//! of excavated pits regardless.

use crate::decoration::box_obstructed;
use crate::protect::Region;
use crate::worldgen::{SEA_LEVEL, carved, column_shape_above, column_shapes_in};
use glam::Vec3;

/// Clearance radius around the spawn column, in columns: the square disc
/// of ground a spawning player stands on.
pub const CLEAR: i32 = 2;

/// Height of the air cylinder above the floor, in cells. The player
/// capsule stands on `h + 1`, so cells `h+1 ..= h+CAPSULE` must be clear.
pub const CAPSULE: i32 = 3;

/// Spawn never sits below sea level — a beach at the water's edge is
/// fine, an excavated pit is not.
///
/// Currently implied rather than enforced: the floor is always a
/// *pristine* surface height, and [`anchor`]'s altitude pre-gate already
/// rejects columns at or below `SEA_LEVEL + 1`. It is stated and checked
/// anyway because it is a design rule, not a coincidence of the pre-gate —
/// if the search ever learns to follow a built-up surface, this is the
/// line that stops it following a dug-out one.
pub const SPAWN_FLOOR: i32 = SEA_LEVEL;

/// Stride of the pristine walk. Wide on purpose: it is scanning a
/// continent for somewhere pleasant, not looking for a specific column.
const ANCHOR_STEP: i32 = 8;

/// Rings of [`ANCHOR_STEP`], enough of them to cover at least one full
/// continent wavelength — so the walk crosses any ocean the warped
/// continent field can produce.
const ANCHOR_RINGS: i32 = 192;

/// Radius around the anchor searched column-by-column before widening.
/// Fine-grained because the damage being routed around is usually small:
/// a single dug block fails the whole disc, but a column just outside it
/// is already clear, and stepping by [`ANCHOR_STEP`] there would bounce
/// spawn a full stride for a one-block hole.
const FINE_RADIUS: i32 = 16;

/// How far the coarse phase looks before giving up, in blocks.
const COARSE_RADIUS: i32 = 512;

/// A read-only view of player edits: the final value of a cell that has
/// been edited, or `None` for a cell no player has ever touched.
///
/// This is edits ONLY — never a chunk-store lookup. A store answers
/// `None` for anything non-resident, and the support test reads `None` as
/// "never edited, trust worldgen"; wiring a store in would make every
/// unloaded column read as pristine-intact in one direction and as void
/// in the other. The server's journal is the correct source and a
/// complete one: every landed write, including fluid, is journalled.
pub type Edits<'a> = &'a dyn Fn(i32, i32, i32) -> Option<u16>;

/// No edits — pristine worldgen answers everything.
pub fn no_edits() -> Edits<'static> {
	&|_, _, _| None
}

/// The eye position for a floor at `h` in column `(x, z)`: centred, feet
/// resting on the block top at `h + 1`.
fn eye(x: i32, h: i32, z: i32) -> Vec3 {
	Vec3::new(x as f32 + 0.5, h as f32 + 2.5, z as f32 + 0.5)
}

/// Split an eye position back into its column and floor.
fn column(eye: Vec3) -> (i32, i32, i32) {
	(
		eye.x.floor() as i32,
		eye.y.floor() as i32 - 2,
		eye.z.floor() as i32,
	)
}

/// The cells a spawn lock protects: one block down (the floor the player
/// stands on), [`CLEAR`] out, [`CAPSULE`] high.
///
/// Deliberately the exact set of cells [`accepts`] inspects — that
/// equivalence, pinned by `lock_region_covers_what_the_test_reads`, is
/// what makes a locked spawn *permanently* valid: no edit the lock
/// permits can change the search's answer, so callers holding a lock can
/// skip [`resolve`] outright.
pub fn lock_region(eye: Vec3) -> Region {
	let (x, h, z) = column(eye);
	Region {
		x0: x - CLEAR,
		x1: x + CLEAR,
		y0: h,
		y1: h + CAPSULE,
		z0: z - CLEAR,
		z1: z + CLEAR,
	}
}

/// The pristine half of the test: `Some(h)` when worldgen alone would be
/// happy to spawn a player on column `(x, z)`.
///
/// The disc is **level**: every column within [`CLEAR`] has its surface at
/// exactly `h`, so the player stands on solid ground `CLEAR` blocks in
/// every direction with a genuinely empty cylinder above it.
///
/// This used to permit neighbours at `h + 1` and say nothing at all about
/// neighbours *below* `h`, which is not the stated rule and produced the
/// two failures it was supposed to prevent: spawning walled in by rock at
/// head height, and spawning on the lip of a drop. "Nothing reaches eye
/// level" is a much weaker claim than "the disc is ground".
fn pristine_floor(x: i32, z: i32, seed: f64) -> Option<i32> {
	// The dry-land gate goes first, as the presieve it can be: an
	// ocean-heavy seed walks thousands of candidates and essentially all
	// of them are sea.
	let s = column_shape_above(x, z, seed, SEA_LEVEL + 1)?;
	let h = s.height;
	if !s.surface.props().spawnable {
		return None;
	}
	let near = column_shapes_in(x - CLEAR, x + CLEAR, z - CLEAR, z + CLEAR, seed);
	for n in &near {
		// Level ground: not a step up (rock at head height), not a step
		// down (a ledge to walk off).
		if n.height != h {
			return None;
		}
		// No standing water and no cascade sheet over any of it — `water`
		// is the local water surface, above `height` only where water
		// actually stands.
		if n.water > n.height || n.fall.is_some() {
			return None;
		}
	}
	// A cave mouth anywhere in the disc is a hole in the floor, not just
	// under the centre. Checked after the cheap height scan, which has
	// already rejected the overwhelming majority of candidates.
	for cz in z - CLEAR..=z + CLEAR {
		for cx in x - CLEAR..=x + CLEAR {
			if carved(cx, h, cz, seed) {
				return None;
			}
		}
	}
	// Decoration (trees, boulders, bushes) writes into the capsule cells
	// h+1.. that pristine terrain never touches.
	let blocked = box_obstructed(
		x - CLEAR,
		x + CLEAR,
		h + 1,
		h + CAPSULE,
		z - CLEAR,
		z + CLEAR,
		seed,
	);
	(!blocked).then_some(h)
}

/// The edit half of the test: has anyone ruined this floor?
///
/// [`pristine_floor`] guarantees a level disc — solid at `h`, air at
/// `h+1 ..= h+CAPSULE` — across the whole footprint, so the rules here are
/// absolute rather than relative to what terrain happens to do: a floor
/// cell turned non-solid is a dig under the standing disc, and a capsule
/// cell turned solid is something built into the standing space. An
/// unedited cell is pristine and therefore already correct, which is what
/// keeps this exactly equivalent to [`pristine_floor`] on an empty
/// overlay.
fn edits_clear(x: i32, z: i32, h: i32, edits: Edits) -> bool {
	for cz in z - CLEAR..=z + CLEAR {
		for cx in x - CLEAR..=x + CLEAR {
			if let Some(cell) = edits(cx, h, cz)
				&& !solid(cell)
			{
				return false; // floor dug out from under the disc
			}
			for cy in h + 1..=h + CAPSULE {
				if let Some(cell) = edits(cx, cy, cz)
					&& solid(cell)
				{
					return false; // built into the standing space
				}
			}
		}
	}
	true
}

/// Whether an edited cell counts as ground/obstruction here.
///
/// Raw cell, decoded id-only: a state-carrying cell (a stair, a slab) is
/// as solid as a full cube for this purpose, and anything unrecognised is
/// treated as solid rather than letting an unknown id read as open air.
fn solid(cell: u16) -> bool {
	use crate::block::BlockType;
	let id = crate::chunk::cell_id(cell);
	match BlockType::try_from_id(id) {
		Some(b) => b.props().collides,
		None => true,
	}
}

/// Whether an edited cell holds water. Standing water in the capsule is
/// disqualifying for the same reason [`pristine_floor`] rejects it: it is
/// not somewhere to be dropped, even though water does not collide.
fn watery(cell: u16) -> bool {
	use crate::block::BlockType;
	BlockType::try_from_id(crate::chunk::cell_id(cell)).is_some_and(|b| b.is_water())
}

/// Whether a player dropped at `eye` would stand on clear, level ground —
/// judged from the world as it ACTUALLY is (terrain plus every landed
/// edit), not from pristine worldgen.
///
/// This is the operator's half of the spawn rules, for a hand-picked
/// spawn. [`accepts`] can only judge a column whose *pristine* terrain is
/// already level, which is exactly right for an automatic search over
/// untouched land and useless for an admin who levelled a hillside or
/// built a platform. The cells read here are exactly [`lock_region`]'s, so
/// "valid to set" and "protected by the lock" describe the same box —
/// which is what makes locking a hand-picked spawn as permanent as
/// locking a searched one.
///
/// Level ground falls out of the two rules rather than needing its own:
/// a step up puts a solid cell in the capsule, a step down leaves a
/// non-solid cell in the floor.
///
/// `cell` answers the world; `None` means the cell cannot be read (not
/// resident) and is a rejection, not a pass. Callers pick a spot within
/// a few blocks of a live player, where everything is loaded.
///
/// [`SPAWN_FLOOR`] is deliberately NOT applied. That rule exists to stop
/// the *search* descending into a pit players excavated; an operator
/// aiming at a specific block has made a choice, and a hall cut into a
/// mountain is a perfectly good spawn. The water rule is what actually
/// keeps a hand-picked spawn out of the sea.
pub fn stands_clear(eye: Vec3, cell: &dyn Fn(i32, i32, i32) -> Option<u16>) -> bool {
	let (x, h, z) = column(eye);
	for cz in z - CLEAR..=z + CLEAR {
		for cx in x - CLEAR..=x + CLEAR {
			// The disc is ground.
			if !matches!(cell(cx, h, cz), Some(c) if solid(c)) {
				return false;
			}
			// The cylinder above it is air.
			for cy in h + 1..=h + CAPSULE {
				if !matches!(cell(cx, cy, cz), Some(c) if !solid(c) && !watery(c)) {
					return false;
				}
			}
		}
	}
	true
}

/// Whether column `(x, z)` is a valid spawn given the current edits.
fn accepts(x: i32, z: i32, seed: f64, edits: Edits) -> Option<i32> {
	let h = pristine_floor(x, z, seed)?;
	(h >= SPAWN_FLOOR && edits_clear(x, z, h, edits)).then_some(h)
}

/// Whether `eye` is a spawn point that currently passes every rule.
///
/// [`resolve`] hands back the anchor unchanged when it exhausts its
/// search, so a caller that must not act on a bad answer — locking the
/// region, say — asks this before committing.
pub fn valid(eye: Vec3, seed: f64, edits: Edits) -> bool {
	let (x, h, z) = column(eye);
	accepts(x, z, seed, edits) == Some(h)
}

/// The perimeter of the square ring at radius `d`, stepping by `step`.
fn ring(d: i32, step: i32) -> impl Iterator<Item = (i32, i32)> {
	let s = step.max(1) as usize;
	(-d..=d).step_by(s).flat_map(move |z| {
		(-d..=d)
			.step_by(s)
			.filter(move |x| x.abs().max(z.abs()) == d)
			.map(move |x| (x, z))
	})
}

/// Eye spawn position from the seed alone: the nearest open land to the
/// origin, ignoring everything players have built or dug.
///
/// Walks expanding square rings until a column clears the beach line, so
/// spawning tracks the terrain instead of hardcoding a height — and never
/// drops the player into the ocean.
pub fn anchor(seed: f64) -> Vec3 {
	for r in 0..=ANCHOR_RINGS {
		for (x, z) in ring(r * ANCHOR_STEP, ANCHOR_STEP) {
			if let Some(h) = pristine_floor(x, z, seed) {
				return eye(x, h, z);
			}
		}
	}
	// Pathological seed: no clear land anywhere the walk reaches — tread
	// water.
	Vec3::new(0.5, SEA_LEVEL as f32 + 3.5, 0.5)
}

/// The spawn point to actually use: the anchor if players have left it
/// alone, else the nearest column that still passes.
///
/// `a` is the world's [`anchor`], passed in rather than recomputed — the
/// ring walk crosses a continent and callers resolve on every join, so it
/// belongs in a field, not in this function.
///
/// Cheap in the common case — validating the anchor is one presieve plus
/// an overlay probe of the lock region — and the outward search only runs
/// once the anchor is genuinely broken. Callers holding a protected spawn
/// region can skip this entirely: an unmodifiable region cannot have
/// stopped being valid.
pub fn resolve(a: Vec3, seed: f64, edits: Edits) -> Vec3 {
	let (ax, az) = (a.x.floor() as i32, a.z.floor() as i32);
	if let Some(h) = accepts(ax, az, seed, edits) {
		return eye(ax, h, az);
	}
	// Column-by-column first: the break is usually a few blocks across,
	// and the nearest intact column is a far less jarring answer than the
	// next one on a coarse lattice.
	for r in 1..=FINE_RADIUS {
		for (dx, dz) in ring(r, 1) {
			if let Some(h) = accepts(ax + dx, az + dz, seed, edits) {
				return eye(ax + dx, h, az + dz);
			}
		}
	}
	// Something enormous has been excavated. Widen to the anchor stride.
	let mut r = FINE_RADIUS + ANCHOR_STEP;
	while r <= COARSE_RADIUS {
		for (dx, dz) in ring(r, ANCHOR_STEP) {
			if let Some(h) = accepts(ax + dx, az + dz, seed, edits) {
				return eye(ax + dx, h, az + dz);
			}
		}
		r += ANCHOR_STEP;
	}
	// Nowhere within half a kilometre is intact. Hand back the anchor and
	// let the caller's landing rule do what it can — this needs a quarry
	// the size of a small country to reach.
	a
}

#[cfg(test)]
mod tests {
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
	fn world(
		top: i32,
		patch: HashMap<(i32, i32, i32), u16>,
	) -> impl Fn(i32, i32, i32) -> Option<u16> {
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
}
