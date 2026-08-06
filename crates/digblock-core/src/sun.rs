//! Day/night sun + moon and the dual shadow-cascade rig ("Shadow LoD").
//!
//! `tod` is the angle of the sun's light vector from straight-down,
//! rotating about world Z and wrapping continuously in (−180°, 180°]:
//! 0 = noon, ±90 = horizons, 180 = midnight. A full cycle takes
//! [`DAY_SECS`]. The moon sits exactly opposite the sun and takes over
//! as shadow caster at night; near the horizon the caster's shadow
//! strength fades to zero, so the handoff at ±90° is invisible.
//!
//! Cascades: the near cascade (ortho 24×24) follows the player and
//! re-renders every frame; the wide cascade (ortho depth×depth) is
//! world-anchored, refreshes every ~3.33s with a snapshotted angle
//! `tod2`, and re-anchors only when the player drifts >32 units from
//! the anchor. Both cascade matrices are snapped to whole shadow-map
//! texels in light space, so a moving target translates the depth map
//! in texel steps instead of re-rasterizing every edge each frame
//! (shadow shimmer). The Java 3° X tilt is deliberately dropped — it
//! patched heightmap-shadow acne, superseded by hardware depth bias.

use glam::{Mat4, Vec3};

/// Full 360° day length in seconds (20 min real time).
pub const DAY_SECS: f32 = 1200.0;
pub const TOD_RATE_DEG_PER_SEC: f32 = 360.0 / DAY_SECS;
/// Where a fresh world's clock starts: mid-morning (the Java start was
/// tod = 80, sunrise). Shared with the server, whose authoritative clock
/// is `tod_of_secs` over its tick counter.
pub const TOD_START: f32 = 80.0;
const WIDE_PERIOD_SECS: f32 = 3.33;
const REANCHOR_DIST: f32 = 32.0;
pub const NEAR_EXTENT: f32 = 24.0;
/// Quantization step for the NEAR cascade's caster angle, in degrees.
///
/// The renderer re-rasterizes the near map whenever its VP changes, and
/// an unquantized `tod` changes it every single frame — so a stationary
/// camera redrew a visually identical map at full frame rate forever.
/// The texel snap already makes translation change in whole texels; this
/// does the same for rotation.
///
/// Sizing: the sun sweeps `TOD_RATE_DEG_PER_SEC` (0.3°/s at the shipping
/// `DAY_SECS`), which drifts the near-map edge about one texel per
/// second, so 0.25° is at most ~1 texel of step and re-rasterizes roughly
/// once a second instead of ~120 times. Diffuse lighting keeps the smooth
/// angle — only the shadow caster direction is stepped, and the renderer
/// patches `light_near` to whatever VP it actually rasterized, so
/// sampling can't disagree with the map.
pub const NEAR_TOD_STEP_DEG: f32 = 0.25;

/// The active shadow caster (sun by day, moon by night).
pub struct CasterState {
	/// Light direction, toward the ground.
	pub dir: Vec3,
	/// Shadow strength 0..1: fades out below ~10° altitude so the
	/// sun→moon handoff at the horizon never pops; moonlight caps at 0.6.
	pub strength: f32,
	pub is_moon: bool,
}

pub struct Sun {
	pub tod: f32,
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
		Self {
			tod: TOD_START,
			tod2: TOD_START,
			anchor: Vec3::ZERO,
			wide_timer: f32::MAX, // wide refresh due immediately
		}
	}

	/// Fixed sun angle for deterministic captures.
	pub fn with_tod(tod: f32) -> Self {
		let mut s = Self::new();
		s.tod = Self::normalize(tod);
		s.tod2 = s.tod;
		s
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
		if t > 180.0 {
			t - 360.0
		} else {
			t
		}
	}

	/// Advance time. Returns true when the wide cascade must re-render
	/// this frame (period elapsed; snapshots tod2 and maybe re-anchors).
	pub fn update(&mut self, dt: f32, player: Vec3) -> bool {
		self.tod = Self::normalize(self.tod - TOD_RATE_DEG_PER_SEC * dt);
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
		// 0 below ~2° altitude, 1 above ~10° (sin 2° ≈ 0.035, sin 10° ≈ 0.174).
		let t = ((e.abs() - 0.035) / (0.174 - 0.035)).clamp(0.0, 1.0);
		let ramp = t * t * (3.0 - 2.0 * t);
		CasterState {
			dir: Self::dir_of(Self::caster_tod(self.tod)),
			strength: ramp * if is_moon { 0.6 } else { 1.0 },
			is_moon,
		}
	}

	/// `texels` = shadow map resolution. The snap makes edges stable
	/// under a moving `target`. The near cascade additionally quantizes
	/// its tod (see [`NEAR_TOD_STEP_DEG`]) so a still camera holds one
	/// exact VP and the renderer can reuse the map it already drew.
	fn cascade_vp(extent: f32, tod: f32, target: Vec3, depth: f32, texels: f32) -> Mat4 {
		let dir = Self::dir_of(tod);
		let eye = target - dir * depth;
		// Up = world Z is safe for any tod: `dir` stays in the XY plane.
		// If the sun ever gains azimuth, switch to a dir-dependent basis.
		let view = Mat4::look_at_rh(eye, target, Vec3::Z);
		let half = extent / 2.0;
		let proj = Mat4::orthographic_rh(-half, half, -half, half, 0.0, depth * 2.0);
		let mut vp = proj * view;
		// Texel snap: for an ortho VP, w_axis.xy is the world origin in
		// NDC and the rotation part depends only on `tod`, so rounding
		// it to whole NDC texels (2/size) means any two targets yield
		// translations differing by exact texel multiples — the light-
		// space snap that kills shadow-edge shimmer while moving. Depth
		// (z) is left unsnapped; it has no rasterization footprint.
		let texel = 2.0 / texels;
		vp.w_axis.x = (vp.w_axis.x / texel).round() * texel;
		vp.w_axis.y = (vp.w_axis.y / texel).round() * texel;
		vp
	}

	/// Near cascade: 24×24 units, follows the player, quantized caster
	/// angle (see [`NEAR_TOD_STEP_DEG`]).
	pub fn near_vp(&self, player: Vec3, depth: f32, texels: f32) -> Mat4 {
		let tod = Self::caster_tod(self.tod);
		Self::cascade_vp(
			NEAR_EXTENT,
			(tod / NEAR_TOD_STEP_DEG).round() * NEAR_TOD_STEP_DEG,
			player,
			depth,
			texels,
		)
	}

	/// Wide cascade: (draw_dist·32)² units, world-anchored, snapshot
	/// angle. Thanks to the texel snap, a re-anchor at the same `tod2`
	/// shifts the map by whole texels (no sub-texel pop); across
	/// refreshes the angle rotates slightly and the ping-pong flip
	/// swaps in the re-rendered map atomically.
	pub fn wide_vp(&self, depth: f32, texels: f32) -> Mat4 {
		Self::cascade_vp(
			depth,
			Self::caster_tod(self.tod2),
			self.anchor,
			depth,
			texels,
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The near map is only reusable if its VP is EXACTLY stable frame to
	/// frame for a still camera — the renderer compares matrices exactly,
	/// because a tolerance would let shadow edges shift without a redraw.
	#[test]
	fn near_vp_is_stable_between_tod_steps() {
		let mut sun = Sun::new();
		sun.tod = 40.0;
		let at = Vec3::new(10.0, 70.0, -3.0);
		let vp = sun.near_vp(at, 120.0, 2048.0);
		// A tenth of the quantization step, many frames' worth of sun
		// movement, must not disturb the matrix at all.
		let mut held = 0;
		for i in 1..=10 {
			sun.tod = 40.0 + NEAR_TOD_STEP_DEG * 0.01 * i as f32;
			if sun.near_vp(at, 120.0, 2048.0) == vp {
				held += 1;
			}
		}
		assert_eq!(held, 10, "near VP must not move inside one tod step");
		// And a full step does move it, or the cascade would never track
		// the sun at all.
		sun.tod = 40.0 + NEAR_TOD_STEP_DEG;
		assert_ne!(
			sun.near_vp(at, 120.0, 2048.0),
			vp,
			"a whole step must re-aim the cascade"
		);
	}

	#[test]
	fn tod_wraps_through_a_full_cycle() {
		let mut sun = Sun::new();
		let mut saw_night = false;
		// Simulate > one full day; tod must stay normalized and pass
		// through midnight (elevation < 0).
		for _ in 0..80_000 {
			sun.update(0.016, Vec3::ZERO);
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
			sun.update(0.1, Vec3::ZERO);
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
		assert!((caster.strength - 0.6).abs() < 1e-6);
		// Mid-day: sun casts at full strength.
		let caster = Sun::with_tod(30.0).caster();
		assert!(!caster.is_moon);
		assert!((caster.strength - 1.0).abs() < 1e-6);
	}

	/// Texel coordinates of world point `p` in the shadow map of `vp`.
	fn texel_of(vp: Mat4, p: Vec3, size: f32) -> (f32, f32) {
		let ndc = vp * p.extend(1.0);
		((ndc.x * 0.5 + 0.5) * size, (ndc.y * -0.5 + 0.5) * size)
	}

	fn assert_whole_texel(v: f32) {
		assert!((v - v.round()).abs() < 1e-2, "not a whole texel: {v}");
	}

	#[test]
	fn near_vp_moves_in_whole_texels() {
		let sun = Sun::with_tod(37.3); // arbitrary non-axis-aligned angle
		let a = sun.near_vp(Vec3::new(100.3, 40.7, 20.1), 640.0, 2048.0);
		let b = sun.near_vp(Vec3::new(101.1, 40.2, 19.6), 640.0, 2048.0);
		// The snap puts the world origin on integer texel coordinates,
		// so nearby targets differ by exact texel multiples.
		for vp in [a, b] {
			let (tx, ty) = texel_of(vp, Vec3::ZERO, 2048.0);
			assert_whole_texel(tx);
			assert_whole_texel(ty);
		}
	}

	#[test]
	fn wide_vp_stable_across_reanchor() {
		let mut sun = Sun::with_tod(-12.0);
		sun.anchor = Vec3::new(100.0, 35.0, 40.0);
		let a = sun.wide_vp(640.0, 2048.0);
		sun.anchor = Vec3::new(150.0, 42.0, 10.0); // re-anchor, same tod2
		let b = sun.wide_vp(640.0, 2048.0);
		let (ax, ay) = texel_of(a, Vec3::ZERO, 2048.0);
		let (bx, by) = texel_of(b, Vec3::ZERO, 2048.0);
		assert_whole_texel(ax - bx);
		assert_whole_texel(ay - by);
	}
}
