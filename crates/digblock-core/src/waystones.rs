//! Waystone sites: one canonical arrival pad per biome per world, and
//! the structure every arrival pad wears (spawn's included).
//!
//! [`anchor`] is spawn's own pristine ring walk with a per-kind gate, so
//! every rule that makes a spawn standable makes a waystone standable —
//! derived from the seed, never stored HERE. The authority resolves an
//! anchor against its edits once, stamps the structure as ORDINARY
//! journaled edits, stores the answer and locks the sphere; that model
//! (and why worldgen never changes) is MAGIC-DESIGN §5's record.
//!
//! [`stamp`] is a pure builder in the debug-plan shape: it RETURNS
//! writes, and each side applies them through its own authoritative path
//! — the worldjob precedent. It deliberately builds from EXISTING block
//! rows: a pad reads as masonry somebody raised, the marker spiral
//! carries the arcane identity, and the append-only table stays open for
//! a bespoke stone later.

use crate::block::BlockType;
use crate::spawn::{CLEAR, LOCK_RADIUS};
use crate::worldgen::SurfaceKind;
use glam::Vec3;

/// How far a waystone stands from the world origin at minimum, in
/// blocks. Spawn's own walk starts at the origin too, so without a floor
/// the first waystone lands within sight of spawn and "the first port"
/// is a sentence about nothing — a trip has to be a trip.
pub const MIN_DIST: i32 = 96;

/// Where the four standing stones rise, as offsets from the pad centre:
/// outside the validated CLEAR disc (the site must pass its own
/// `stands_clear`), inside the LOCK_RADIUS sphere (the lock must cover
/// every cell the stamp writes).
const STONE_OUT: i32 = CLEAR + 2;
/// Shaft height above the platform, capstone on top.
const STONE_H: i32 = 2;

const _: () = assert!(STONE_OUT > CLEAR, "stones must clear the validated disc");
// The farthest stamped cell — a capstone's top at (STONE_OUT, STONE_H + 1)
// — stays inside the lock sphere, so protection covers the whole build.
const _: () =
	assert!(STONE_OUT * STONE_OUT + (STONE_H + 1) * (STONE_H + 1) <= LOCK_RADIUS * LOCK_RADIUS);

/// The canonical anchor for `kind`, derived from the seed alone: the
/// nearest column past [`MIN_DIST`] whose pristine surface classifies as
/// `kind` and whose pad passes every spawn clearance rule. `None` when
/// the walk finds no such column — a seed's map simply may not offer a
/// kind, and a port to nowhere must not exist.
pub fn anchor(kind: SurfaceKind, seed: f64) -> Option<Vec3> {
	crate::spawn::anchor_walk(&|x, z| {
		if x * x + z * z < MIN_DIST * MIN_DIST {
			return None;
		}
		if crate::worldgen::column_shape(x, z, seed).surface != kind {
			return None;
		}
		crate::spawn::pristine_pad(x, z, seed)
	})
}

/// The structure every arrival pad wears, as writes against the world:
/// a polished platform REPLACING the ground under the validated disc,
/// and four stone shafts at the cardinals, capped with lamps. Pure — the
/// caller applies through its own authoritative path, and skips cells
/// already holding the value so a re-stamp is a no-op.
pub fn stamp(eye: Vec3) -> Vec<(i32, i32, i32, u16)> {
	let (x, z) = (eye.x.floor() as i32, eye.z.floor() as i32);
	let h = eye.y.floor() as i32 - 2;
	let mut out = Vec::new();
	// The platform: the exact floor cells `stands_clear` requires solid,
	// so the stamp can never invalidate its own pad.
	for dz in -CLEAR..=CLEAR {
		for dx in -CLEAR..=CLEAR {
			out.push((x + dx, h, z + dz, BlockType::PolishedStone as u16));
		}
	}
	// The four standing stones, at the cardinals.
	for (dx, dz) in [
		(STONE_OUT, 0),
		(-STONE_OUT, 0),
		(0, STONE_OUT),
		(0, -STONE_OUT),
	] {
		// A footing under the shaft, flush with the platform's plane.
		out.push((x + dx, h, z + dz, BlockType::PolishedStone as u16));
		for dy in 1..=STONE_H {
			out.push((x + dx, h + dy, z + dz, BlockType::StoneBrick as u16));
		}
		out.push((
			x + dx,
			h + STONE_H + 1,
			z + dz,
			BlockType::Aetherlamp as u16,
		));
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::protect::Region;

	const SEED: f64 = 4242.0;

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
		for &(x, y, z, _) in &writes {
			assert!(
				region.contains(x, y, z),
				"stamped cell ({x},{y},{z}) is outside the lock"
			);
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
		for &(x, y, z, v) in &stamp(eye) {
			let in_disc = (x - cx).abs() <= CLEAR && (z - cz).abs() <= CLEAR;
			if in_disc {
				assert_eq!(y, h, "a disc write must be the floor itself");
				assert!(
					crate::block::BlockType::try_from_id(crate::chunk::cell_id(v))
						.is_some_and(|b| b.props().collides),
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
			if let Some(&(_, _, _, v)) = writes
				.iter()
				.find(|&&(wx, wy, wz, _)| (wx, wy, wz) == (x, y, z))
			{
				return Some(v);
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

	/// The walk really answers per kind: on a real seed, each anchor it
	/// finds classifies as its own kind, sits past MIN_DIST, and the
	/// common lowland kind is always on offer (a map without plains is
	/// not a map the terrain generator makes). Counted, so a sweep where
	/// every kind came back None could not pass vacuously.
	#[test]
	fn every_found_anchor_sits_on_its_own_surface() {
		let mut found = 0u32;
		for kind in SurfaceKind::ALL {
			let Some(eye) = anchor(kind, SEED) else {
				continue;
			};
			found += 1;
			let (x, z) = (eye.x.floor() as i32, eye.z.floor() as i32);
			assert!(x * x + z * z >= MIN_DIST * MIN_DIST, "{kind:?} too near");
			assert_eq!(
				crate::worldgen::column_shape(x, z, SEED).surface,
				kind,
				"the {kind:?} anchor is not standing on {kind:?}"
			);
		}
		assert!(found > 0, "no kind anchored at all — the walk is broken");
		assert!(
			anchor(SurfaceKind::Plains, SEED).is_some(),
			"plains always exists"
		);
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
