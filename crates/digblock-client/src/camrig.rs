//! Cinematic camera rigs: poses computed from a rig's own state instead
//! of the player, with the terrain-aware boom that keeps them out of the
//! ground. The rig writes into `FlyCamera` each frame — every render
//! consumer keeps reading the one camera — and the player sim keeps
//! ticking untouched underneath. Blends run between POSITIONS and a
//! virtual look target, never raw angles, so the horizon stays put; the
//! exit blend converges on the LIVE first-person pose and then the mode
//! switches branches, which is what restores the exact-rest invariants
//! (GI history, light-list equality) by construction.

use digblock_core::camera_probe::CAM_RADIUS;
use digblock_core::ease::Curve;
use glam::Vec3;

/// A camera pose in `FlyCamera`'s convention (yaw 0 => +z, +yaw turns
/// left, degrees, pitch clamped inside ±89).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CamPose {
	pub pos: Vec3,
	pub yaw_deg: f32,
	pub pitch_deg: f32,
}

/// Seconds of the enter/exit blend between first person and a rig.
const BLEND_SECS: f32 = 1.2;
/// The virtual look-target distance blends interpolate through.
const BLEND_TARGET_DIST: f32 = 8.0;
/// Orbit radii above this shift the streaming disc anchored on the body.
const ORBIT_RADIUS_MAX: f32 = 24.0;

impl CamPose {
	/// The pose at `pos` looking at `target` — the inverse of
	/// `camera::dir_from`, shared convention with `Engine::face`.
	pub(crate) fn look_at(pos: Vec3, target: Vec3) -> Self {
		let d = target - pos;
		let flat = (d.x * d.x + d.z * d.z).sqrt();
		Self {
			pos,
			yaw_deg: d.x.atan2(d.z).to_degrees(),
			pitch_deg: d.y.atan2(flat).to_degrees().clamp(-89.0, 89.0),
		}
	}

	fn dir(&self) -> Vec3 {
		crate::camera::dir_from(self.yaw_deg, self.pitch_deg)
	}

	/// Blend two poses through positions and a virtual look target
	/// (angle lerp swings the horizon; this keeps it level).
	pub(crate) fn blend(a: Self, b: Self, t: f32) -> Self {
		let pos = a.pos.lerp(b.pos, t);
		let ta = a.pos + a.dir() * BLEND_TARGET_DIST;
		let tb = b.pos + b.dir() * BLEND_TARGET_DIST;
		Self::look_at(pos, ta.lerp(tb, t))
	}
}

/// Boom-length smoothing: instantaneous pull-in (a clip is worse than a
/// cut — `len <= clearance` is the invariant), eased extension, and a
/// small hysteresis so a grazed corner cannot flutter the length.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Boom {
	len: f32,
}

/// Extension rate (per second toward the target; tau ~0.4 s).
const EXTEND_RATE: f32 = 2.5;
/// Blocks of slack before an extension starts chasing.
const EXTEND_HYSTERESIS: f32 = 0.05;

impl Boom {
	pub(crate) fn step(&mut self, target: f32, dt: f32) -> f32 {
		if target < self.len {
			self.len = target;
		} else if target - self.len > EXTEND_HYSTERESIS {
			self.len = digblock_core::ease::exp_decay(self.len, target, EXTEND_RATE, dt);
		}
		self.len
	}
}

/// A slow orbit about a world point, boom pulled in by the terrain probe
/// re-run every frame — dug or placed blocks change next frame's answer
/// with no extra machinery.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct OrbitRig {
	pub center: Vec3,
	pub radius: f32,
	pub height: f32,
	pub yaw_deg: f32,
	pub angular_vel_deg: f32,
	boom: Boom,
}

impl OrbitRig {
	pub(crate) fn new(center: Vec3, radius: f32, height: f32, angular_vel_deg: f32) -> Self {
		Self {
			center,
			radius: radius.clamp(1.0, ORBIT_RADIUS_MAX),
			height,
			yaw_deg: 0.0,
			angular_vel_deg,
			// From zero: the entry reads as a fly-out along the boom.
			boom: Boom { len: 0.0 },
		}
	}

	/// Advance the azimuth, probe from the CENTER outward (a wall between
	/// subject and camera pulls the camera in front of it, never behind),
	/// smooth the boom, look at the center.
	pub(crate) fn step(
		&mut self,
		dt: f32,
		clearance: &dyn Fn(Vec3, Vec3, f32, f32) -> f32,
	) -> CamPose {
		self.yaw_deg = (self.yaw_deg + self.angular_vel_deg * dt).rem_euclid(360.0);
		let (s, c) = self.yaw_deg.to_radians().sin_cos();
		let offset = Vec3::new(s, 0.0, c) * self.radius + Vec3::Y * self.height;
		let dist = offset.length();
		let dir = offset / dist;
		let clear = clearance(self.center, dir, dist, CAM_RADIUS);
		let len = self.boom.step(clear, dt);
		CamPose::look_at(self.center + dir * len, self.center)
	}
}

/// The cinematic state machine: blend in from the captured first-person
/// pose, run the rig, blend back toward the LIVE first-person pose on
/// leave (the player may have kept moving under a server), then hand the
/// mode back to first person by returning None.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Cinematic {
	pub rig: OrbitRig,
	/// Captured first-person pose the entry blends out of.
	from: CamPose,
	/// 0..=1 into the rig.
	enter: f32,
	/// Some = blending back out; at 1.0 the mode returns to first person.
	leaving: Option<f32>,
}

impl Cinematic {
	pub(crate) fn start(from: CamPose, rig: OrbitRig) -> Self {
		Self {
			rig,
			from,
			enter: 0.0,
			leaving: None,
		}
	}

	/// Begin the blend back to first person (idempotent).
	pub(crate) fn leave(&mut self) {
		self.leaving.get_or_insert(0.0);
	}

	/// One frame: `fp` is the live first-person pose. None = finished
	/// leaving; the caller switches the mode back.
	pub(crate) fn step(
		&mut self,
		dt: f32,
		fp: CamPose,
		clearance: &dyn Fn(Vec3, Vec3, f32, f32) -> f32,
	) -> Option<CamPose> {
		let rig_pose = self.rig.step(dt, clearance);
		self.enter = (self.enter + dt / BLEND_SECS).min(1.0);
		let base = CamPose::blend(self.from, rig_pose, Curve::InOutCubic.apply(self.enter));
		match &mut self.leaving {
			None => Some(base),
			Some(t) => {
				*t += dt / BLEND_SECS;
				if *t >= 1.0 {
					None
				} else {
					Some(CamPose::blend(base, fp, Curve::InOutCubic.apply(*t)))
				}
			}
		}
	}
}

#[cfg(test)]
mod tests;
