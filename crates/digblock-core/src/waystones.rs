//! Waystone sites: one canonical arrival pad per biome per world, and the structure every arrival
//! pad wears (spawn's included).
//!
//! [`Roster`] is spawn's own pristine ring walk with a per-kind gate, so every rule that makes a
//! spawn standable makes a waystone standable — derived from the seed, never stored HERE. It is a
//! roster rather than a function per kind because [`MIN_SEPARATION`] is a MUTUAL rule: a pad can
//! only be judged against the pads already placed, so a per-kind answer would depend on who asked
//! first. The authority resolves the set against its edits once, stamps each structure as ORDINARY
//! journaled edits, stores the answers and locks the spheres.
//!
//! [`stamp`] is a pure builder that RETURNS writes, each side applying them through its own
//! authoritative path — the worldjob precedent. It deliberately builds from EXISTING block rows,
//! so the append-only table stays open for a bespoke stone later.

use crate::block::BlockType;
use crate::spawn::{CLEAR, LOCK_RADIUS};
use crate::worldgen::Seed;
use crate::worldgen::SurfaceKind;
use glam::Vec3;

/// How far every arrival pad stands from every other, in blocks, spawn included — measured
/// horizontally, because a pad is a place on the map rather than a point in the air. Generous on
/// purpose: a port that sets you down within sight of the pad you left is not a trip, and it sits
/// comfortably past the draw distance a player actually runs, so arriving at one pad never puts
/// another on screen.
pub const MIN_SEPARATION: i32 = 320;

/// The placement rule's version — bumped whenever a change here would
/// move a site an authority has already resolved and stored, so a stored
/// roster written under an older rule is discarded and re-placed once.
pub const PLACEMENT_RULE: u32 = 2;

/// Where the four standing stones rise, as the offset on BOTH axes from
/// the pad centre — one diagonally off each platform corner: outside the
/// validated CLEAR disc (the site must pass its own `stands_clear`) and
/// inside the LOCK_RADIUS sphere (the lock must cover every cell the
/// stamp writes — and a corner reaches √2 farther out than a cardinal).
const STONE_OUT: i32 = CLEAR + 1;
/// Shaft height above the platform, capstone on top.
const STONE_H: i32 = 2;

const _: () = assert!(STONE_OUT > CLEAR, "stones must clear the validated disc");
// The farthest stamped cell — a capstone's top at (STONE_OUT, STONE_H + 1, STONE_OUT)
// — stays inside the lock sphere, so protection covers the whole build.
const _: () =
	assert!(2 * STONE_OUT * STONE_OUT + (STONE_H + 1) * (STONE_H + 1) <= LOCK_RADIUS * LOCK_RADIUS);

/// Where the stones stood before they moved to the corners: at the
/// cardinals, this far out. A MIGRATION rather than part of the build —
/// a pad stamped under that design still wears four stones there and
/// the stamp is the only thing that may take them down. Delete with
/// [`retired`] once no world stands on it.
const RETIRED_STONE_OUT: i32 = CLEAR + 2;
/// The retired design's shaft height — its own figure, so a retune of
/// `STONE_H` cannot make the migration miss a cell it wrote.
const RETIRED_STONE_H: i32 = 2;

/// One cell of an arrival pad's structure, as a write against the
/// world: `to` is what the pad wants there. A BUILD write lands whatever
/// stands there; an UNBUILD write (`from` set) lands only while the
/// retired design's own block still stands, so ground the old stamp
/// never touched, or anything built since, is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pub from: Option<u16>,
	pub to: u16,
}

impl Write {
	/// Whether this write is still owed against what the cell holds —
	/// `None` for a cell no journal has ever written. Either kind is a
	/// no-op once the cell holds `to`, which is what makes a re-stamp
	/// converge.
	pub fn owed(&self, cur: Option<u16>) -> bool {
		match self.from {
			None => cur != Some(self.to),
			Some(from) => cur == Some(from),
		}
	}
}

/// The world's waystones, resolved rung by rung. The roster resolves as a SET because
/// [`MIN_SEPARATION`] is mutual — see the module header. Placement order is the ladder's, lowest
/// rung first (ties by kind index, so `unlock::waystone` alone decides it), and spawn's anchor is
/// placed before any of them. Rungs resolve LAZILY and in order for the same answer, since a
/// rung's site depends only on the rungs below it, so a prefix is exact.
pub struct Roster {
	seed: Seed,
	/// Every pad standing so far, spawn's included — the exclusion set.
	/// Empty until the first rung resolves: constructing a roster must
	/// stay free, since a world swap builds one whether or not anybody
	/// ever casts.
	taken: Vec<Vec3>,
	/// The rungs still unresolved, lowest first.
	pending: Vec<(u8, SurfaceKind)>,
	/// What each resolved rung answered. `None` = this seed's map never
	/// offered the kind outside the pads already standing, and a port to
	/// nowhere must not exist.
	placed: Vec<(u8, Option<Vec3>)>,
}

impl Roster {
	pub fn new(seed: Seed) -> Self {
		let mut pending: Vec<(u8, SurfaceKind, u32)> = SurfaceKind::ALL
			.iter()
			.enumerate()
			.filter_map(|(i, &k)| crate::unlock::waystone(k).map(|lvl| (i as u8, k, lvl)))
			.collect();
		pending.sort_by_key(|&(idx, _, level)| (level, idx));
		Self {
			seed,
			taken: Vec::new(),
			pending: pending.into_iter().map(|(i, k, _)| (i, k)).collect(),
			placed: Vec::new(),
		}
	}

	/// The site for one `SurfaceKind` index, resolving every rung below
	/// it that is not resolved yet. A kind off the ladder answers `None`
	/// without walking anything — asking for one would otherwise resolve
	/// the whole roster only to find it was never in it.
	pub fn site(&mut self, idx: u8) -> Option<Vec3> {
		if !self.pending.iter().any(|&(i, _)| i == idx)
			&& !self.placed.iter().any(|&(i, _)| i == idx)
		{
			return None;
		}
		while !self.placed.iter().any(|&(i, _)| i == idx) {
			self.resolve_next()?;
		}
		self.placed
			.iter()
			.find(|&&(i, _)| i == idx)
			.and_then(|&(_, eye)| eye)
	}

	/// Every site this seed offers, in kind order — the whole ladder
	/// resolved. What an authority asks at boot, once.
	pub fn all(&mut self) -> Vec<(u8, Vec3)> {
		while self.resolve_next().is_some() {}
		let mut out: Vec<(u8, Vec3)> = self
			.placed
			.iter()
			.filter_map(|&(i, eye)| eye.map(|e| (i, e)))
			.collect();
		out.sort_by_key(|&(idx, _)| idx);
		out
	}

	/// Place the next rung, or `None` once the ladder is exhausted.
	fn resolve_next(&mut self) -> Option<()> {
		let (idx, kind) = self.pending.first().copied()?;
		self.pending.remove(0);
		if self.taken.is_empty() {
			// Spawn is an arrival pad like any other, and it stands
			// first — nothing may be placed before it is in the set.
			self.taken.push(crate::spawn::anchor(self.seed));
		}
		let eye = anchor(kind, self.seed, &self.taken);
		if let Some(eye) = eye {
			self.taken.push(eye);
		}
		self.placed.push((idx, eye));
		Some(())
	}
}

/// Every waystone this seed offers, as `(SurfaceKind` index, eye)` pairs
/// in kind order — [`Roster::all`] for a caller with nothing to cache.
pub fn sites(seed: Seed) -> Vec<(u8, Vec3)> {
	Roster::new(seed).all()
}

/// The nearest column to the origin that classifies as `kind`, passes
/// every spawn clearance rule, and clears every pad in `taken`.
fn anchor(kind: SurfaceKind, seed: Seed, taken: &[Vec3]) -> Option<Vec3> {
	crate::spawn::anchor_walk(&|x, z| {
		// Cheapest gate first — the classify and the pad walk both read
		// terrain, and an excluded column can be rejected on arithmetic.
		if taken.iter().any(|&t| crowds(x, z, t)) {
			return None;
		}
		if crate::worldgen::column_shape(x, z, seed).surface != kind {
			return None;
		}
		crate::spawn::pristine_pad(x, z, seed)
	})
}

/// Whether column `(x, z)` sits inside `other`'s exclusion.
fn crowds(x: i32, z: i32, other: Vec3) -> bool {
	let dx = x - other.x.floor() as i32;
	let dz = z - other.z.floor() as i32;
	dx * dx + dz * dz < MIN_SEPARATION * MIN_SEPARATION
}

/// The structure every arrival pad wears, as writes against the world:
/// a polished platform REPLACING the ground under the validated disc,
/// four stone shafts at the corners capped with lamps, and the retired
/// design's stones taken down. Pure — the caller applies each write
/// its [`Write::owed`] says is due through its own authoritative path.
pub fn stamp(eye: Vec3) -> Vec<Write> {
	let (x, z) = (eye.x.floor() as i32, eye.z.floor() as i32);
	let h = eye.y.floor() as i32 - 2;
	let build = |x, y, z, b: BlockType| Write {
		x,
		y,
		z,
		from: None,
		to: b as u16,
	};
	let mut out = Vec::new();
	// The platform: the exact floor cells `stands_clear` requires solid,
	// so the stamp can never invalidate its own pad.
	for dz in -CLEAR..=CLEAR {
		for dx in -CLEAR..=CLEAR {
			out.push(build(x + dx, h, z + dz, BlockType::PolishedStone));
		}
	}
	// The four standing stones, one off each corner of the platform.
	for (dx, dz) in [
		(STONE_OUT, STONE_OUT),
		(-STONE_OUT, STONE_OUT),
		(STONE_OUT, -STONE_OUT),
		(-STONE_OUT, -STONE_OUT),
	] {
		// A footing under the shaft, flush with the platform's plane.
		out.push(build(x + dx, h, z + dz, BlockType::PolishedStone));
		for dy in 1..=STONE_H {
			out.push(build(x + dx, h + dy, z + dz, BlockType::StoneBrick));
		}
		out.push(build(
			x + dx,
			h + STONE_H + 1,
			z + dz,
			BlockType::Aetherlamp,
		));
	}
	out.extend(retired(x, h, z));
	out
}

/// The unbuild half of the stamp: every cell the retired design wrote
/// that the current one does not, guarded on the block it put there.
/// Dismantled to air — no reader of the pristine cell exists, and the
/// guard already keeps this off anything the old stamp did not touch.
fn retired(x: i32, h: i32, z: i32) -> impl Iterator<Item = Write> {
	let unbuild = move |x, y, z, b: BlockType| Write {
		x,
		y,
		z,
		from: Some(b as u16),
		to: BlockType::Air as u16,
	};
	[
		(RETIRED_STONE_OUT, 0),
		(-RETIRED_STONE_OUT, 0),
		(0, RETIRED_STONE_OUT),
		(0, -RETIRED_STONE_OUT),
	]
	.into_iter()
	.flat_map(move |(dx, dz)| {
		let (sx, sz) = (x + dx, z + dz);
		std::iter::once(unbuild(sx, h, sz, BlockType::PolishedStone))
			.chain(
				(1..=RETIRED_STONE_H).map(move |dy| unbuild(sx, h + dy, sz, BlockType::StoneBrick)),
			)
			.chain(std::iter::once(unbuild(
				sx,
				h + RETIRED_STONE_H + 1,
				sz,
				BlockType::Aetherlamp,
			)))
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::protect::Region;

	const SEED: Seed = Seed::current(4242.0);

	fn eye_at(x: i32, h: i32, z: i32) -> Vec3 {
		Vec3::new(x as f32 + 0.5, h as f32 + 2.5, z as f32 + 0.5)
	}

	/// Every cell the stamp writes sits inside the lock sphere — the
	/// protection must cover the whole build, or a griefer dismantles
	/// the stones the pad is recognized by. Counted, so a stamp that
	/// wrote nothing could not pass.
	#[test]
	fn a_stamp_fits_its_protection_sphere() {
		let eye = eye_at(10, 100, -7);
		let region = crate::spawn::lock_region(eye);
		let writes = stamp(eye);
		assert!(!writes.is_empty());
		for w in &writes {
			assert!(
				region.contains(w.x, w.y, w.z),
				"stamped cell ({},{},{}) is outside the lock",
				w.x,
				w.y,
				w.z
			);
		}
	}

	/// The build and the unbuild name disjoint cells: were a retired
	/// cell also a built one, the two writes would fight over it and
	/// the stamp could never converge.
	#[test]
	fn the_build_and_the_unbuild_never_share_a_cell() {
		let writes = stamp(eye_at(0, 100, 0));
		let built: std::collections::HashSet<_> = writes
			.iter()
			.filter(|w| w.from.is_none())
			.map(|w| (w.x, w.y, w.z))
			.collect();
		let mut unbuilt = 0u32;
		for w in writes.iter().filter(|w| w.from.is_some()) {
			assert!(!built.contains(&(w.x, w.y, w.z)), "{w:?} is also built");
			unbuilt += 1;
		}
		assert!(unbuilt > 0, "the migration names nothing");
	}

	/// An unbuild write is owed only while the retired design's own
	/// block stands there — never against pristine ground, a player's
	/// block, or a cell already cleared; a build write is owed against
	/// anything but its own value. Swept over the whole stamp.
	#[test]
	fn a_write_is_owed_against_exactly_what_it_may_change() {
		let foreign = BlockType::Dirt as u16;
		for w in stamp(eye_at(5, 100, -5)) {
			assert!(!w.owed(Some(w.to)), "{w:?} owed against itself");
			match w.from {
				None => {
					assert!(w.owed(None), "{w:?} not owed on pristine ground");
					assert!(w.owed(Some(foreign)), "{w:?} not owed over a foreign block");
				}
				Some(from) => {
					assert!(w.owed(Some(from)), "{w:?} not owed over its own relic");
					assert!(!w.owed(None), "{w:?} would carve pristine ground");
					assert!(!w.owed(Some(foreign)), "{w:?} would clear a foreign block");
				}
			}
		}
	}

	/// The stamp never writes into the validated standing space: the
	/// floor cells it replaces are the ones `stands_clear` wants SOLID,
	/// and nothing lands in the capsule above them. Counted per class.
	#[test]
	fn a_stamp_keeps_its_own_pad_clear() {
		let eye = eye_at(0, 130, 0);
		let (cx, cz, h) = (0, 0, 130);
		let mut floor = 0u32;
		for w in stamp(eye).iter().filter(|w| w.from.is_none()) {
			let (x, y, z) = (w.x, w.y, w.z);
			let in_disc = (x - cx).abs() <= CLEAR && (z - cz).abs() <= CLEAR;
			if in_disc {
				assert_eq!(y, h, "a disc write must be the floor itself");
				assert!(
					crate::block::BlockType::try_from_id(crate::chunk::cell_id(w.to))
						.is_some_and(|b| b.props().solid),
					"the floor must stay solid ground"
				);
				floor += 1;
			} else {
				// A stone stands on its own footing at floor level and
				// rises no higher than its capstone.
				assert!(
					y >= h && y <= h + STONE_H + 1,
					"a stone write strays vertically: ({x},{y},{z})"
				);
			}
		}
		let disc = ((2 * CLEAR + 1) * (2 * CLEAR + 1)) as u32;
		assert_eq!(floor, disc, "the platform must pave the whole disc");
	}

	/// Applied to a synthetic world, the stamped pad still passes the
	/// standing rule — the site validates AGAINST its own structure.
	#[test]
	fn a_stamped_pad_stands_clear() {
		let eye = eye_at(3, 100, 3);
		let h = 98;
		let writes = stamp(eye);
		let cell = |x: i32, y: i32, z: i32| -> Option<u16> {
			if let Some(w) = writes
				.iter()
				.find(|w| w.from.is_none() && (w.x, w.y, w.z) == (x, y, z))
			{
				return Some(w.to);
			}
			// Flat synthetic ground: solid at and below the floor.
			Some(if y <= h {
				BlockType::Stone as u16
			} else {
				BlockType::Air as u16
			})
		};
		assert!(crate::spawn::stands_clear(eye, &cell));
	}

	/// The walk really answers per kind: on a real seed, each site it
	/// finds classifies as its own kind, only laddered kinds are placed,
	/// and the common lowland kind is always on offer (a map without
	/// plains is not a map the terrain generator makes). Counted, so a
	/// seed that placed nothing could not pass vacuously.
	#[test]
	fn every_found_site_sits_on_its_own_surface() {
		let sites = sites(SEED);
		assert!(
			!sites.is_empty(),
			"nothing placed at all — the walk is broken"
		);
		for &(idx, eye) in &sites {
			let kind = SurfaceKind::ALL[idx as usize];
			assert!(
				crate::unlock::waystone(kind).is_some(),
				"{kind:?} is not on the ladder and must not be placed"
			);
			let (x, z) = (eye.x.floor() as i32, eye.z.floor() as i32);
			assert_eq!(
				crate::worldgen::column_shape(x, z, SEED).surface,
				kind,
				"the {kind:?} site is not standing on {kind:?}"
			);
		}
		let plains = SurfaceKind::ALL
			.iter()
			.position(|&k| k == SurfaceKind::Plains)
			.expect("plains is a kind") as u8;
		assert!(
			sites.iter().any(|&(idx, _)| idx == plains),
			"plains always exists"
		);
	}

	/// The lazy path and the eager one are the same placement: asking
	/// for one rung resolves exactly the prefix that `all` would have,
	/// so an interactive caller's cheap answer is never a different
	/// answer. Swept over every laddered kind, counted.
	#[test]
	fn asking_one_rung_places_it_where_the_whole_ladder_would() {
		let eager = sites(SEED);
		let mut asked = 0u32;
		for (i, &kind) in SurfaceKind::ALL.iter().enumerate() {
			if crate::unlock::waystone(kind).is_none() {
				continue;
			}
			asked += 1;
			let i = i as u8;
			// A fresh roster per kind: the point is that asking for this
			// rung ALONE lands it where the full resolve put it.
			let lazy = Roster::new(SEED).site(i);
			assert_eq!(
				lazy,
				eager.iter().find(|&&(j, _)| j == i).map(|&(_, e)| e),
				"{kind:?} moved between the lazy and the eager resolve"
			);
		}
		assert_eq!(asked as usize, eager.len(), "every laddered kind placed");
		// A kind off the ladder is not a port, and asking costs nothing.
		let ocean = SurfaceKind::ALL
			.iter()
			.position(|&k| crate::unlock::waystone(k).is_none())
			.expect("some kind is off the ladder") as u8;
		assert_eq!(Roster::new(SEED).site(ocean), None);
	}

	/// The separation rule, swept over every pair the roster forms —
	/// spawn included, since spawn is an arrival pad like any other.
	/// Pairs are counted, because a roster that placed one site (or
	/// none) would satisfy this vacuously.
	#[test]
	fn no_two_arrival_pads_crowd_each_other() {
		let mut pads = vec![crate::spawn::anchor(SEED)];
		pads.extend(sites(SEED).into_iter().map(|(_, eye)| eye));
		let mut pairs = 0u32;
		for (i, &a) in pads.iter().enumerate() {
			for &b in &pads[i + 1..] {
				pairs += 1;
				let (x, z) = (b.x.floor() as i32, b.z.floor() as i32);
				assert!(
					!crowds(x, z, a),
					"{a} and {b} are closer than the separation rule allows"
				);
			}
		}
		// Spawn plus every laddered kind this seed placed: the sweep is
		// only meaningful once it has seen a site-to-site pair too.
		assert_eq!(pairs, (pads.len() * (pads.len() - 1) / 2) as u32);
		assert!(pads.len() > 2, "the roster is too thin to test");
	}

	/// The lock region a stamped site takes is the same shape spawn's is
	/// — one concept, asserted rather than assumed.
	#[test]
	fn a_site_lock_is_a_sphere() {
		assert!(matches!(
			crate::spawn::lock_region(eye_at(0, 100, 0)),
			Region::Sphere(_)
		));
	}
}
