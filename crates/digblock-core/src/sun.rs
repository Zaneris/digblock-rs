//! Day/night sun + the dual shadow-cascade rig ("Shadow LoD").
//!
//! Java behavior (World.updateWorldTime), made time-based instead of
//! per-frame: `tod` ping-pongs in [−80°, +80°] about world Z at 0.06°/s
//! (0.001°/frame at 60fps) — the sun never goes below the horizon. The
//! near cascade (ortho 24×24) follows the player and re-renders every
//! frame; the wide cascade (ortho depth×depth) is world-anchored,
//! refreshes every ~3.33s (Java's 200 frames) with a snapshotted sun angle
//! `tod2`, and re-anchors only when the player drifts >32 units from the
//! anchor. The Java 3° X tilt is deliberately dropped — it patched
//! heightmap-shadow acne, superseded by hardware depth bias.

use glam::{Mat4, Vec3};

const TOD_RATE_DEG_PER_SEC: f32 = 0.06;
const TOD_LIMIT: f32 = 80.0;
const WIDE_PERIOD_SECS: f32 = 3.33;
const REANCHOR_DIST: f32 = 32.0;
pub const NEAR_EXTENT: f32 = 24.0;

pub struct Sun {
	pub tod: f32,
	flip: bool,
	tod2: f32,
	pub anchor: Vec3,
	wide_timer: f32,
}

impl Default for Sun {
	fn default() -> Self {
		Self::new()
	}
}

impl Sun {
	pub fn new() -> Self {
		// Java starts at tod = 80 (sunrise at the horizon).
		Self {
			tod: TOD_LIMIT,
			flip: false,
			tod2: TOD_LIMIT,
			anchor: Vec3::ZERO,
			wide_timer: f32::MAX, // wide refresh due immediately
		}
	}

	/// Fixed sun angle for deterministic captures.
	pub fn with_tod(tod: f32) -> Self {
		let mut s = Self::new();
		s.tod = tod;
		s.tod2 = tod;
		s
	}

	/// Advance time. Returns true when the wide cascade must re-render
	/// this frame (period elapsed; snapshots tod2 and maybe re-anchors).
	pub fn update(&mut self, dt: f32, player: Vec3) -> bool {
		self.tod -= TOD_RATE_DEG_PER_SEC * dt * if self.flip { -1.0 } else { 1.0 };
		if self.tod < -TOD_LIMIT {
			self.flip = true;
		} else if self.tod > TOD_LIMIT {
			self.flip = false;
		}
		self.wide_timer = if self.wide_timer == f32::MAX {
			WIDE_PERIOD_SECS
		} else {
			self.wide_timer + dt
		};
		if self.wide_timer >= WIDE_PERIOD_SECS {
			self.wide_timer = 0.0;
			if self.anchor.distance(player) > REANCHOR_DIST {
				self.anchor = player;
			}
			self.tod2 = self.tod;
			return true;
		}
		false
	}

	fn dir_of(tod_deg: f32) -> Vec3 {
		// Straight-down light rotated about world Z (Java rotateAround):
		// (0,-1,0) → (sin tod, −cos tod, 0).
		let (s, c) = tod_deg.to_radians().sin_cos();
		Vec3::new(s, -c, 0.0)
	}

	/// Sunlight direction (toward the ground) — the near cascade's angle,
	/// also used for diffuse lighting (Java u_LightVector).
	pub fn dir(&self) -> Vec3 {
		Self::dir_of(self.tod)
	}

	fn cascade_vp(extent: f32, tod: f32, target: Vec3, depth: f32) -> Mat4 {
		let dir = Self::dir_of(tod);
		let eye = target - dir * depth;
		let view = Mat4::look_at_rh(eye, target, Vec3::Z);
		let half = extent / 2.0;
		let proj = Mat4::orthographic_rh(-half, half, -half, half, 0.0, depth * 2.0);
		proj * view
	}

	/// Near cascade: 24×24 units, follows the player, current sun angle.
	pub fn near_vp(&self, player: Vec3, depth: f32) -> Mat4 {
		Self::cascade_vp(NEAR_EXTENT, self.tod, player, depth)
	}

	/// Wide cascade: (draw_dist·32)² units, world-anchored, snapshot angle.
	pub fn wide_vp(&self, depth: f32) -> Mat4 {
		Self::cascade_vp(depth, self.tod2, self.anchor, depth)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tod_ping_pongs_within_limits() {
		let mut sun = Sun::new();
		// Simulate a long stretch; tod must stay in [−80−ε, 80+ε] and flip.
		let mut saw_negative_travel = false;
		for _ in 0..400_000 {
			sun.update(0.016, Vec3::ZERO);
			assert!(sun.tod.abs() <= TOD_LIMIT + 0.01);
			if sun.tod < 0.0 {
				saw_negative_travel = true;
			}
		}
		assert!(saw_negative_travel, "sun should cross noon over ~35min sim");
	}

	#[test]
	fn wide_refresh_cadence_and_anchor() {
		let mut sun = Sun::new();
		assert!(
			sun.update(0.001, Vec3::new(100.0, 35.0, 0.0)),
			"first update forces refresh"
		);
		assert_eq!(
			sun.anchor,
			Vec3::new(100.0, 35.0, 0.0),
			"anchors immediately (>32 from origin)"
		);
		// No refresh again until the period elapses.
		let mut refreshes = 0;
		for _ in 0..100 {
			if sun.update(0.1, Vec3::new(105.0, 35.0, 0.0)) {
				refreshes += 1;
			}
		}
		// Timer resets on each refresh: crossings land at ~3.4s and ~6.8s
		// within the 10s window (the third lands just past it).
		assert_eq!(refreshes, 2, "two full periods fit in 10s of 0.1s steps");
		// Player only 5 units away — anchor must not have moved.
		assert_eq!(sun.anchor, Vec3::new(100.0, 35.0, 0.0));
	}

	#[test]
	fn noon_light_points_down() {
		let mut sun = Sun::new();
		sun.tod = 0.0;
		assert!((sun.dir() - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-6);
	}
}
