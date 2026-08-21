//! Where players appear: the pristine anchor, and the edit-aware search that
//! keeps them out of holes players have dug.
//!
//! [`anchor`] is a pure function of the seed — it never sees edits, so it is
//! the fixed point everything else is measured from. [`resolve`] asks whether
//! players have ruined it and searches outward if so. Anchoring every resolve
//! at [`anchor`] rather than at the CURRENT spawn is what stops drift
//! compounding: a griefer can push spawn aside but never walk it across the
//! map, and a filled crater snaps it back. Nothing is stored.
//!
//! The search never looks downward, and [`SPAWN_FLOOR`] keeps it out of pits.

use crate::decoration::box_obstructed;
use crate::protect::{Region, Sphere};
use crate::worldgen::{SEA_LEVEL, carved, column_shape_above, column_shapes_in};
use glam::Vec3;

/// Clearance radius around the spawn column, in columns: the square disc
/// of ground a spawning player stands on.
pub const CLEAR: i32 = 2;

/// Height of the air cylinder above the floor, in cells. The player
/// capsule stands on `h + 1`, so cells `h+1 ..= h+CAPSULE` must be clear.
pub const CAPSULE: i32 = 3;

/// Radius of the protected sphere around an arrival pad — the spawn
/// lock, and a waystone pad. Sized to CONTAIN the validated core (the
/// [`CLEAR`] disc plus its [`CAPSULE`]): the box's farthest cells are its
/// top corners, and this is the smallest whole-block sphere that keeps
/// every one of them, with the lower hemisphere as the anti-undermining
/// depth the box never had. Shrinking it below that containment breaks
/// the "locked ⇒ permanently valid" fast path.
pub const LOCK_RADIUS: i32 = 6;

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

/// The cells an arrival lock protects: a [`LOCK_RADIUS`] sphere centred
/// on the pad's floor cell.
///
/// The sphere CONTAINS every cell [`accepts`] inspects — that
/// containment, pinned by `lock_region_covers_what_the_test_reads`, is
/// what makes a locked spawn *permanently* valid: no edit the lock
/// permits can change the search's answer, so callers holding a lock can
/// skip [`resolve`] outright. The cells beyond the validated core are
/// protected without being validated — whatever terrain they are,
/// frozen, the lower half against undermining the pad.
pub fn lock_region(eye: Vec3) -> Region {
	let (x, h, z) = column(eye);
	Region::Sphere(Sphere {
		cx: x,
		cy: h,
		cz: z,
		r: LOCK_RADIUS,
	})
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
/// judged from the world as it ACTUALLY is (terrain plus every landed edit),
/// not from pristine worldgen.
///
/// The operator's half of the spawn rules. [`accepts`] can only judge a column
/// whose *pristine* terrain is already level, which is right for an automatic
/// search and useless for an admin who levelled a hillside. The cells read here
/// are exactly [`lock_region`]'s, so "valid to set" and "protected by the lock"
/// describe the same box — which is what makes locking a hand-picked spawn as
/// permanent as locking a searched one.
///
/// Level ground falls out of the two rules rather than needing its own. `None`
/// from `cell` means not resident, and is a rejection. [`SPAWN_FLOOR`] is
/// deliberately NOT applied: that rule stops the *search* descending into a
/// pit, and an operator aiming at a block has made a choice.
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
	anchor_walk(&|x, z| pristine_floor(x, z, seed))
		// Pathological seed: no clear land anywhere the walk reaches —
		// tread water.
		.unwrap_or_else(|| Vec3::new(0.5, SEA_LEVEL as f32 + 3.5, 0.5))
}

/// The pristine ring walk, generalized: the nearest column to the origin
/// that `accept` answers a floor for. Spawn's own anchor and the waystone
/// anchors share it, so "a pad the search likes" means exactly one thing
/// — an accept that WRAPS [`pristine_floor`] inherits every clearance
/// rule, and tightening only ever narrows the set.
pub(crate) fn anchor_walk(accept: &dyn Fn(i32, i32) -> Option<i32>) -> Option<Vec3> {
	for r in 0..=ANCHOR_RINGS {
		for (x, z) in ring(r * ANCHOR_STEP, ANCHOR_STEP) {
			if let Some(h) = accept(x, z) {
				return Some(eye(x, h, z));
			}
		}
	}
	None
}

/// [`pristine_floor`], for sibling modules building accepts over it —
/// the clearance rule itself stays private.
pub(crate) fn pristine_pad(x: i32, z: i32, seed: f64) -> Option<i32> {
	pristine_floor(x, z, seed)
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
mod tests;
