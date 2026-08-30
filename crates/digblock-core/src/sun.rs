//! Day/night sun + moon.
//!
//! `tod` is the angle of the sun's light vector from straight-down, rotating
//! about world Z and wrapping continuously in (−180°, 180°]: 0 = noon, ±90 =
//! horizons, 180 = midnight. A full cycle takes [`DAY_SECS`]. The moon sits
//! exactly opposite and takes over at night; near the horizon the caster's
//! strength fades to zero, so the handoff is invisible.
//!
//! Because `dir_of` rotates about world Z alone, the light direction ALWAYS has
//! z = 0 — load-bearing for anything tracing the voxel lattice, since a ray
//! confined to a plane on an exactly rational slope aliases identically in
//! every slice of the third axis. Sun rays are jittered per-pixel for exactly
//! this reason.

use glam::Vec3;

/// Full 360° day length in seconds.
pub const DAY_SECS: f32 = 1200.0;
pub const TOD_RATE_DEG_PER_SEC: f32 = 360.0 / DAY_SECS;
/// Where a fresh world's clock starts: mid-morning (the same angle the
/// Java original started at, where it read as sunrise). Shared with the
/// server, whose authoritative clock is `tod_of_secs` over its tick
/// counter.
pub const TOD_START: f32 = 80.0;

/// The active light caster (sun by day, moon by night).
pub struct CasterState {
	/// Light direction, toward the ground.
	pub dir: Vec3,
	/// Caster strength 0..1: fades out at low altitude so the sun→moon
	/// handoff at the horizon never pops; moonlight caps below full.
	pub strength: f32,
	pub is_moon: bool,
}

/// How much of the sun's strength the moon casts at. Below 1 so a night
/// reads as a night; named because the twilight-handoff test must assert
/// the cap it was given rather than a number beside it.
pub const MOON_STRENGTH: f32 = 0.6;

pub struct Sun {
	pub tod: f32,
}

impl Default for Sun {
	fn default() -> Self {
		Self::new()
	}
}

impl Sun {
	pub fn new() -> Self {
		Self { tod: TOD_START }
	}

	/// Fixed sun angle for deterministic captures.
	pub fn with_tod(tod: f32) -> Self {
		Self {
			tod: Self::normalize(tod),
		}
	}

	/// The world clock as a pure function of elapsed seconds — the
	/// server's authoritative time of day (`secs` = tick / TICK_RATE).
	/// f64 math so precision holds over months of uptime; per-frame f32
	/// integration is fine client-side because heartbeats re-anchor it.
	pub fn tod_of_secs(secs: f64) -> f32 {
		let deg = (f64::from(TOD_START) - f64::from(TOD_RATE_DEG_PER_SEC) * secs).rem_euclid(360.0);
		Self::normalize(deg as f32)
	}

	/// Wrap an angle into (−180, 180].
	fn normalize(deg: f32) -> f32 {
		let t = deg.rem_euclid(360.0);
		if t > 180.0 { t - 360.0 } else { t }
	}

	/// Advance the world clock.
	pub fn update(&mut self, dt: f32) {
		self.tod = Self::normalize(self.tod - TOD_RATE_DEG_PER_SEC * dt);
	}

	fn dir_of(tod_deg: f32) -> Vec3 {
		// Straight-down light rotated about world Z (Java rotateAround):
		// (0,-1,0) → (sin tod, −cos tod, 0).
		let (s, c) = tod_deg.to_radians().sin_cos();
		Vec3::new(s, -c, 0.0)
	}

	/// Sunlight direction (toward the ground), independent of who casts.
	pub fn sun_dir(&self) -> Vec3 {
		Self::dir_of(self.tod)
	}

	/// Moonlight direction (toward the ground) — exactly opposite the sun.
	pub fn moon_dir(&self) -> Vec3 {
		-self.sun_dir()
	}

	/// Sin of the sun's altitude: +1 noon, 0 at the horizons, −1 midnight.
	pub fn elevation(&self) -> f32 {
		self.tod.to_radians().cos()
	}

	/// The tod whose light vector points down: the sun's by day, the
	/// moon's (sun + 180°) by night.
	fn caster_tod(tod: f32) -> f32 {
		if tod.to_radians().cos() >= 0.0 {
			tod
		} else {
			tod + 180.0
		}
	}

	/// The active shadow caster and its strength (twilight fade).
	pub fn caster(&self) -> CasterState {
		let e = self.elevation();
		let is_moon = e < 0.0;
		// Smoothstep from just above the horizon up to a low altitude; the
		// bounds are sines of those altitudes, to match `e`.
		let t = ((e.abs() - 0.035) / (0.174 - 0.035)).clamp(0.0, 1.0);
		let ramp = t * t * (3.0 - 2.0 * t);
		CasterState {
			dir: Self::dir_of(Self::caster_tod(self.tod)),
			strength: ramp * if is_moon { MOON_STRENGTH } else { 1.0 },
			is_moon,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every caster direction lies in the XY plane — the property the GI
	/// sun ray has to jitter around (see the module docs).
	#[test]
	fn the_caster_never_leaves_the_xy_plane() {
		for tod in [-179.0, -90.0, -12.0, 0.0, 37.3, 90.0, 180.0] {
			let sun = Sun::with_tod(tod);
			assert_eq!(sun.sun_dir().z, 0.0, "sun dir at tod {tod}");
			assert_eq!(sun.caster().dir.z, 0.0, "caster dir at tod {tod}");
		}
	}

	#[test]
	fn tod_wraps_through_a_full_cycle() {
		let mut sun = Sun::new();
		let mut saw_night = false;
		// Simulate > one full day; tod must stay normalized and pass
		// through midnight (elevation < 0).
		for _ in 0..80_000 {
			sun.update(0.016);
			assert!(
				sun.tod > -180.0 && sun.tod <= 180.0,
				"tod out of range: {}",
				sun.tod
			);
			if sun.elevation() < -0.5 {
				saw_night = true;
			}
		}
		assert!(
			saw_night,
			"sun should pass through deep night over a full cycle"
		);
	}

	#[test]
	fn tod_of_secs_matches_integration() {
		assert_eq!(Sun::tod_of_secs(0.0), TOD_START);
		// A whole day wraps back to the start.
		assert!((Sun::tod_of_secs(f64::from(DAY_SECS)) - TOD_START).abs() < 1e-4);
		// Stays normalized and precise after ~a year of uptime.
		let t = Sun::tod_of_secs(3.0e7);
		assert!(t > -180.0 && t <= 180.0);
		// Matches frame integration over a short horizon.
		let mut sun = Sun::new();
		for _ in 0..600 {
			sun.update(0.1);
		}
		assert!((Sun::tod_of_secs(60.0) - sun.tod).abs() < 1e-3);
	}

	#[test]
	fn noon_light_points_down() {
		let sun = Sun::with_tod(0.0);
		assert!((sun.sun_dir() - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-6);
	}

	#[test]
	fn caster_handoff_at_twilight() {
		// At the horizons shadows are fully faded out.
		for tod in [90.0, -90.0] {
			let sun = Sun::with_tod(tod);
			assert!(
				sun.caster().strength < 1e-6,
				"strength at horizon (tod {tod})"
			);
		}
		// At midnight the moon casts, pointing down, at reduced strength.
		let sun = Sun::with_tod(180.0);
		let caster = sun.caster();
		assert!(caster.is_moon);
		assert!((caster.dir - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-5);
		assert!((caster.strength - MOON_STRENGTH).abs() < 1e-6);
		// Mid-day: sun casts at full strength.
		let caster = Sun::with_tod(30.0).caster();
		assert!(!caster.is_moon);
		assert!((caster.strength - 1.0).abs() < 1e-6);
	}
}
