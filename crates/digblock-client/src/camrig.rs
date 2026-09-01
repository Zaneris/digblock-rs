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
/// with no extra machinery. `center` must sit in clear air (the probe
/// casts from it); `target` is what the lens holds, and may be a solid
/// block the center stands beside — the two split exactly so a subject
/// that IS terrain can still be looked at.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct OrbitRig {
	pub center: Vec3,
	pub target: Vec3,
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
			target: center,
			radius: radius.clamp(1.0, ORBIT_RADIUS_MAX),
			height,
			yaw_deg: 0.0,
			angular_vel_deg,
			// From zero: the entry reads as a fly-out along the boom.
			boom: Boom { len: 0.0 },
		}
	}

	/// Start with the boom already run out: for a center that IS the
	/// subject block, the eased fly-out would crawl the camera through
	/// it; the pose blend supplies the entry motion instead.
	pub(crate) fn extended(mut self) -> Self {
		self.boom = Boom { len: f32::MAX };
		self
	}

	/// Advance the azimuth, probe from the CENTER outward (a wall between
	/// subject and camera pulls the camera in front of it, never behind),
	/// smooth the boom, look at the target.
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
		CamPose::look_at(self.center + dir * len, self.target)
	}
}

/// A tracking dolly: the eye slides `from` -> `to` over `secs` with the
/// lens CHASING `target` every frame — the classic move for a subject
/// the camera passes at a distance. Do not run it through (or near
/// over) the subject: the bearing swings hard as the eye passes, which
/// is [`DroneRig`]'s job to avoid. The boom probes from the target
/// toward the eye and starts fully extended (a dolly cuts in rather
/// than blending out of its subject).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct DollyRig {
	pub from: Vec3,
	pub to: Vec3,
	pub target: Vec3,
	pub secs: f32,
	t: f32,
	boom: Boom,
}

impl DollyRig {
	pub(crate) fn new(from: Vec3, to: Vec3, target: Vec3, secs: f32) -> Self {
		Self {
			from,
			to,
			target,
			secs: secs.max(0.01),
			t: 0.0,
			boom: Boom { len: f32::MAX },
		}
	}

	pub(crate) fn step(
		&mut self,
		dt: f32,
		clearance: &dyn Fn(Vec3, Vec3, f32, f32) -> f32,
	) -> CamPose {
		self.t = (self.t + dt / self.secs).min(1.0);
		let eye = self.from.lerp(self.to, Curve::InOutCubic.apply(self.t));
		let offset = eye - self.target;
		let dist = offset.length().max(1e-3);
		let dir = offset / dist;
		let clear = clearance(self.target, dir, dist, CAM_RADIUS);
		let len = self.boom.step(clear, dt);
		CamPose::look_at(self.target + dir * len, self.target)
	}
}

/// A drone glide: the same slide with a FIXED gimbal — the frame never
/// rotates, the world drifts through it, which is what lets the path
/// pass directly over the subject (a look-at there spins the frame).
/// The boom probes from `target` toward the eye; an embedded target is
/// fine under `clearance_out_of`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct DroneRig {
	pub from: Vec3,
	pub to: Vec3,
	pub target: Vec3,
	pub yaw_deg: f32,
	pub pitch_deg: f32,
	pub secs: f32,
	t: f32,
	boom: Boom,
}

impl DroneRig {
	pub(crate) fn new(
		from: Vec3,
		to: Vec3,
		target: Vec3,
		secs: f32,
		yaw_deg: f32,
		pitch_deg: f32,
	) -> Self {
		Self {
			from,
			to,
			target,
			yaw_deg,
			pitch_deg: pitch_deg.clamp(-89.0, 89.0),
			secs: secs.max(0.01),
			t: 0.0,
			boom: Boom { len: f32::MAX },
		}
	}

	pub(crate) fn step(
		&mut self,
		dt: f32,
		clearance: &dyn Fn(Vec3, Vec3, f32, f32) -> f32,
	) -> CamPose {
		self.t = (self.t + dt / self.secs).min(1.0);
		// Linear on purpose: a drone crosses at constant speed — the
		// eased ramp reads as the camera hesitating, not as flight.
		let eye = self.from.lerp(self.to, self.t);
		let offset = eye - self.target;
		let dist = offset.length().max(1e-3);
		let dir = offset / dist;
		let clear = clearance(self.target, dir, dist, CAM_RADIUS);
		let len = self.boom.step(clear, dt);
		CamPose {
			pos: self.target + dir * len,
			yaw_deg: self.yaw_deg,
			pitch_deg: self.pitch_deg,
		}
	}
}

/// One rig a shot can run.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum CineRig {
	Orbit(OrbitRig),
	Dolly(DollyRig),
	Drone(DroneRig),
}

impl CineRig {
	fn step(&mut self, dt: f32, clearance: &dyn Fn(Vec3, Vec3, f32, f32) -> f32) -> CamPose {
		match self {
			CineRig::Orbit(r) => r.step(dt, clearance),
			CineRig::Dolly(r) => r.step(dt, clearance),
			CineRig::Drone(r) => r.step(dt, clearance),
		}
	}
}

/// One shot of a sequence: a rig held for `hold` seconds before the CUT
/// to the next (or the leave blend, on the last). `f32::INFINITY` runs
/// until something calls `leave` — the dev orbit's mode.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Shot {
	pub rig: CineRig,
	pub hold: f32,
}

/// The cinematic state machine: blend in from the captured first-person
/// pose, run the shot sequence (hard cuts between shots), blend back
/// toward the LIVE first-person pose on leave (the player may have kept
/// moving under a server), then hand the mode back to first person by
/// returning None.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Cinematic {
	shot: Shot,
	/// Shots still to come; each cuts in when the current hold expires.
	queue: Vec<Shot>,
	/// Seconds into the current shot.
	age: f32,
	/// Captured first-person pose the entry blends out of.
	from: CamPose,
	/// 0..=1 into the rig.
	enter: f32,
	/// Some = blending back out; at 1.0 the mode returns to first person.
	leaving: Option<f32>,
}

impl Cinematic {
	/// A single unbounded orbit — the dev knob's mode, ended by `leave`.
	pub(crate) fn start(from: CamPose, rig: OrbitRig) -> Self {
		Self::start_shots(
			from,
			vec![Shot {
				rig: CineRig::Orbit(rig),
				hold: f32::INFINITY,
			}],
		)
	}

	/// A scripted sequence. `shots` must be non-empty; each shot runs its
	/// hold and CUTS to the next, and the last one's expiry leaves.
	pub(crate) fn start_shots(from: CamPose, mut shots: Vec<Shot>) -> Self {
		assert!(!shots.is_empty(), "a cinematic needs at least one shot");
		let shot = shots.remove(0);
		Self {
			shot,
			queue: shots,
			age: 0.0,
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
		self.age += dt;
		if self.age >= self.shot.hold && self.leaving.is_none() {
			if self.queue.is_empty() {
				self.leave();
			} else {
				// A hard cut: the next rig takes over whole, no residual
				// entry blend softening it into the old pose.
				self.shot = self.queue.remove(0);
				self.age = 0.0;
				self.enter = 1.0;
			}
		}
		let rig_pose = self.shot.rig.step(dt, clearance);
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
