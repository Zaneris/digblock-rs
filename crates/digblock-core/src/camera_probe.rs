//! Camera-vs-terrain clearance: the sphere probe a cinematic boom pulls
//! in against. Pure functions over the same `cell_at` closure the
//! raycast and the body take, judging COLLISION boxes (an open door lets
//! the camera through exactly as it lets the body through) with unloaded
//! cells passing, matching physics. A sphere rather than a ray because a
//! ray is too skinny: it threads crevices the camera's near plane does
//! not fit, and the frustum clips the corner. Deterministic by
//! construction — a fixed-step march plus a fixed bisection — so a
//! capture run replays bit-for-bit.

use crate::block::BlockType;
use crate::chunk::cell_id;
use glam::Vec3;

/// The camera's clearance radius: past the near-plane half-diagonal
/// (~0.16 at near 0.1, fov 75, 16:9) so geometry never crosses the near
/// plane, and under the body's RADIUS 0.3 so anywhere the player fits,
/// the camera fits.
pub const CAM_RADIUS: f32 = 0.25;

/// Bisection rounds after the march finds a blocked step — resolves the
/// boom length to `radius / 2^ROUNDS`, well under a pixel's worth.
const BISECT_ROUNDS: u32 = 8;

/// Cell coordinates a span of world units touches.
fn span(lo: f32, hi: f32) -> std::ops::RangeInclusive<i32> {
	(lo.floor() as i32)..=(hi.floor() as i32)
}

/// True when a sphere at `center` intersects any collision box. Unloaded
/// (`None`) cells pass — the physics rule, and the right one for a
/// camera: geometry that is not there cannot clip.
pub fn sphere_overlaps_solid(
	center: Vec3,
	radius: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> bool {
	let r2 = radius * radius;
	let hits = |lo: Vec3, hi: Vec3| center.clamp(lo, hi).distance_squared(center) < r2;
	for y in span(center.y - radius, center.y + radius) {
		for x in span(center.x - radius, center.x + radius) {
			for z in span(center.z - radius, center.z + radius) {
				let Some(v) = cell_at(x, y, z) else { continue };
				if !crate::shapes::collides(v) {
					continue;
				}
				let base = Vec3::new(x as f32, y as f32, z as f32);
				if BlockType::from_id(cell_id(v)).props().shape.is_none() {
					if hits(base, base + Vec3::ONE) {
						return true;
					}
					continue;
				}
				for sb in crate::shapes::collision_boxes(v) {
					let (lo, hi) = sb.aabb((x, y, z));
					if hits(lo, hi) {
						return true;
					}
				}
			}
		}
	}
	false
}

/// Farthest `d` in `[0, max]` such that the sphere swept from `origin`
/// along `dir` stays clear; 0.0 when the origin sphere is already
/// blocked. March step = `radius`: consecutive spheres overlap, so no
/// wall crossing the path can slip between two probes.
pub fn clearance(
	origin: Vec3,
	dir: Vec3,
	max: f32,
	radius: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> f32 {
	if max <= 0.0 || sphere_overlaps_solid(origin, radius, cell_at) {
		return 0.0;
	}
	let dir = dir.normalize_or_zero();
	if dir == Vec3::ZERO {
		return 0.0;
	}
	let step = radius.max(1e-3);
	let mut clear = 0.0f32;
	while clear < max {
		let next = (clear + step).min(max);
		if sphere_overlaps_solid(origin + dir * next, radius, cell_at) {
			// `clear` is clear, `next` blocked: bisect the boundary.
			let (mut lo, mut hi) = (clear, next);
			for _ in 0..BISECT_ROUNDS {
				let mid = 0.5 * (lo + hi);
				if sphere_overlaps_solid(origin + dir * mid, radius, cell_at) {
					hi = mid;
				} else {
					lo = mid;
				}
			}
			return lo;
		}
		clear = next;
	}
	max
}

#[cfg(test)]
mod tests;
