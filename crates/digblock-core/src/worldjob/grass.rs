//! Grass creeps onto dirt that can see the sun.
//!
//! Three registrations of one rule, each with a clock hour's sun angle
//! baked in — `apply` never reads a live clock, so a sweep sliced over
//! many ticks judges every cell at ONE angle and stays reproducible from
//! `(seed, cell, run)`. The frontier candidate source finds player-made
//! dirt; the subsoil shell finds the only pristine kind, dirt bared by
//! the cave carve. Both arms register through [`jobs`], so schedule,
//! thresholds and angles cannot drift between the server and offline.

use glam::Vec3;

use super::{Candidates, CellSource, Schedule, Scope, WorldJob};
use crate::block::{BlockType, RenderClass};
use crate::chunk::{WORLD_VBLOCK, cell_id, cell_state};
use crate::dir::Dir;
use crate::gi::{self, GiOpacity};
use crate::raycast;
use crate::shapes;
use crate::sun::{Sun, TOD_START};
use crate::worldgen;

/// The clock hours a spread runs at — all in daylight, spanning enough of
/// the morning-to-afternoon arc that east- and west-facing slopes each
/// get a run with the sun on their side.
const SPREAD_HOURS: [f32; 3] = [9.0, 12.0, 15.0];

const DEG_PER_HOUR: f32 = 360.0 / 24.0;

/// Chunk radius around each player a sweep covers. Modest on purpose:
/// within typical residency, so the sun rays mostly read real cells and
/// the worldgen fallback stays the exception.
const RADIUS_CHUNKS: i32 = 3;

/// Qualifying neighbors of the same kind a dirt cell needs before that
/// kind spreads to it. DryGrass is deliberately the harder spreader, so
/// savanna creep is slow and green grass takes contested ground.
const GRASS_MIN_NEIGHBORS: u32 = 1;
const DRY_MIN_NEIGHBORS: u32 = 4;

/// Per-cell perturbation of the sun ray, in sun angle and in a small
/// out-of-plane slope. Both must be nonzero: every caster direction has
/// z = 0 exactly, and a lattice-rational ray confined to that plane
/// aliases identically in every z slice (`sun` module docs) — an
/// unjittered 45° registration would judge whole grid diagonals as one.
const JITTER_TOD_DEG: f32 = 1.5;
const JITTER_Z: f32 = 0.03;
const JITTER_SALT: u64 = 0x0067_7261_7373; // "grass"

/// The sun angle at a clock hour: 0 = noon, 15°/hour, morning positive —
/// the `sun` module's convention.
fn tod_of_hour(hour: f32) -> f32 {
	(12.0 - hour) * DEG_PER_HOUR
}

/// The day fraction at which `Sun::tod_of_secs` passes `tod` — the
/// inverse of that formula, so the schedule and the sky cannot drift.
fn offset_of_tod(tod: f32) -> f32 {
	(TOD_START - tod) / 360.0
}

/// One registration of the spread rule, at one baked sun angle.
pub struct GrassSpread {
	tod: f32,
	name: &'static str,
}

impl GrassSpread {
	/// Panics if the hour puts the sun at or below the horizon even
	/// before jitter — such a ray could never rise past the world top,
	/// and the cap in `apply` divides by its upward slope.
	pub fn at_hour(hour: f32, name: &'static str) -> Self {
		let tod = tod_of_hour(hour);
		assert!(
			tod.abs() + JITTER_TOD_DEG < 90.0,
			"{name}: the sun must be above the horizon"
		);
		Self { tod, name }
	}
}

/// The three production registrations, morning to afternoon.
pub fn jobs() -> [Box<dyn WorldJob>; 3] {
	let [morning, noon, afternoon] = SPREAD_HOURS;
	[
		Box::new(GrassSpread::at_hour(morning, "grass-9am")),
		Box::new(GrassSpread::at_hour(noon, "grass-noon")),
		Box::new(GrassSpread::at_hour(afternoon, "grass-3pm")),
	]
}

/// The rule's inverse half: a placed cell that seals the cell below off
/// from the sky starves the grass there back to Dirt. "Seals" is the
/// mesher's culling answer — a cover smothers exactly when the grass top
/// face stops being drawn (pinned against a real mesh) — so water and
/// open undersides spare it. Both arms apply the write as a
/// CONSEQUENCE of a place, never a predicted or validated part of one.
pub fn smothered_by(placed: u16, below: u16) -> Option<u16> {
	let b = BlockType::from_id(cell_id(placed));
	let seals = match b.props().class {
		RenderClass::Opaque => true,
		RenderClass::Shaped => shapes::shape_of(b, cell_state(placed))
			.is_some_and(|v| v.occludes_full[Dir::Bottom as usize]),
		_ => false,
	};
	let living = matches!(
		BlockType::from_id(cell_id(below)),
		BlockType::Grass | BlockType::DryGrass
	);
	(seals && living).then_some(BlockType::Dirt as u16)
}

/// Strictly the empty cell: water or any occupant refuses, and so does
/// unloaded (`None`) — an unjudgeable candidate is retried on a later
/// run rather than guessed at.
fn open_above(world: &dyn CellSource, x: i32, y: i32, z: i32) -> bool {
	world
		.cell_at(x, y + 1, z)
		.is_some_and(|c| cell_id(c) == BlockType::Air as u16)
}

/// What the sun ray meets outside loaded cells: pure worldgen, solid at
/// or below the column's height. Only its opacity is ever read, and the
/// closure lives here in core, so both arms judge unloaded terrain
/// identically by construction.
fn synth(x: i32, y: i32, z: i32, seed: f64) -> u16 {
	if y <= worldgen::terrain_height(x, z, seed) {
		BlockType::Stone as u16
	} else {
		BlockType::Air as u16
	}
}

/// Low bits of a hash to a signed unit float.
fn signed_unit(bits: u64) -> f32 {
	(bits & u64::from(u16::MAX)) as f32 / f32::from(u16::MAX) * 2.0 - 1.0
}

impl WorldJob for GrassSpread {
	fn name(&self) -> &'static str {
		self.name
	}

	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 1,
			offset: offset_of_tod(self.tod),
		}
	}

	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Dirt],
			radius_chunks: RADIUS_CHUNKS,
			y: (0, WORLD_VBLOCK),
			candidates: Candidates::Frontier {
				// The shell is what lets cave-mouth dirt green over; deep
				// tunnel dirt still fails the neighbor and sun gates.
				surface_shell: true,
			},
		}
	}

	fn apply(
		&self,
		(x, y, z): (i32, i32, i32),
		world: &dyn CellSource,
		seed: f64,
		run: u64,
	) -> Option<u16> {
		// Cheapest gate first: the cell above must be empty.
		if !open_above(world, x, y, z) {
			return None;
		}
		// Neighbor thresholds over the 26-neighborhood. A neighbor
		// qualifies only if ITS cell above is empty too — buried grass
		// cannot seed spread, so grass never leaks out from under a
		// player's build.
		let (mut grass, mut dry) = (0u32, 0u32);
		for dy in -1..=1 {
			for dz in -1..=1 {
				for dx in -1..=1 {
					if (dx, dy, dz) == (0, 0, 0) {
						continue;
					}
					let (nx, ny, nz) = (x + dx, y + dy, z + dz);
					let Some(c) = world.cell_at(nx, ny, nz) else {
						continue;
					};
					let count = if cell_id(c) == BlockType::Grass as u16 {
						&mut grass
					} else if cell_id(c) == BlockType::DryGrass as u16 {
						&mut dry
					} else {
						continue;
					};
					if open_above(world, nx, ny, nz) {
						*count += 1;
					}
				}
			}
		}
		// Grass wins when both thresholds are met: the easier spreader
		// takes contested borders.
		let winner = if grass >= GRASS_MIN_NEIGHBORS {
			BlockType::Grass
		} else if dry >= DRY_MIN_NEIGHBORS {
			BlockType::DryGrass
		} else {
			return None;
		};
		// The sun ray, cast from the center of the empty cell above (an
		// origin inside a stopping cell is an instant hit). Jittered per
		// (seed, cell, run) — see JITTER_TOD_DEG.
		let bits = worldgen::ore::mix(x, y, z, seed.to_bits(), JITTER_SALT ^ run);
		let dtod = signed_unit(bits) * JITTER_TOD_DEG;
		let dz = signed_unit(bits >> u16::BITS) * JITTER_Z;
		let mut dir = -Sun::with_tod(self.tod + dtod).sun_dir();
		dir.z = dz;
		let dir = dir.normalize();
		let origin = Vec3::new(x as f32 + 0.5, y as f32 + 1.5, z as f32 + 0.5);
		// Far enough to rise past the world top, and no further.
		let cap = (WORLD_VBLOCK + 1 - y) as f32 / dir.y + 1.0;
		raycast::raycast_with(
			origin,
			dir,
			cap,
			&|cx, cy, cz| {
				world
					.cell_at(cx, cy, cz)
					.or_else(|| Some(synth(cx, cy, cz, seed)))
			},
			// Opacity, not collision: leaves shade, water passes.
			&|c| {
				matches!(
					gi::opacity(BlockType::from_id(cell_id(c))),
					GiOpacity::Opaque
				)
			},
		)
		.is_none()
		.then_some(winner as u16)
	}
}

#[cfg(test)]
mod tests;
