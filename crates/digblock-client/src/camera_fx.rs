//! Camera feel: trauma-based shake, the dig kick, and the FOV pulse —
//! all additive on the FINAL view matrix, never on `camera.pos`, so
//! streaming, the GI carried origin and the wire never see a tremor.
//! Deterministic (three incommensurate sines over an owned phase, no
//! RNG), and EXACTLY idle at rest: every decay snaps to literal zero
//! below an epsilon and [`CameraFx::offset`] returns the bitwise
//! identity offset, because the GI history and the light list compare
//! camera positions with `==` and must stay asleep on a still frame.

use glam::Vec3;

/// This frame's additive camera adjustment. `IDLE` is the exact rest
/// identity; consumers may compare against it to skip work.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CamOffset {
	pub pos: Vec3,
	pub pitch_deg: f32,
	pub roll_deg: f32,
	pub fov_scale: f32,
}

impl CamOffset {
	pub(crate) const IDLE: CamOffset = CamOffset {
		pos: Vec3::ZERO,
		pitch_deg: 0.0,
		roll_deg: 0.0,
		fov_scale: 1.0,
	};
}

/// Full-trauma shake amplitudes: position in blocks, angles in degrees.
const SHAKE_POS: f32 = 0.09;
const SHAKE_PITCH: f32 = 0.9;
const SHAKE_ROLL: f32 = 1.2;
/// Trauma drained per second (a full charge lasts under a second).
const TRAUMA_DRAIN: f32 = 1.4;
/// The kick's and pulse's exponential decay rate, and the snap floor.
const DECAY_RATE: f32 = 9.0;
const SNAP: f32 = 1e-3;

#[derive(Default)]
pub(crate) struct CameraFx {
	/// 0..=1; amplitude rides trauma squared (the classic model), so
	/// small hits barely whisper and stacked ones roar.
	trauma: f32,
	/// The shake's own clock; only advances while active, wraps like
	/// `time_secs`.
	phase: f32,
	/// One-shot downward pitch impulse, degrees.
	kick_deg: f32,
	/// One-shot FOV widen, as a fraction over 1.0.
	pulse: f32,
}

impl CameraFx {
	pub(crate) fn add_trauma(&mut self, amount: f32) {
		self.trauma = (self.trauma + amount).clamp(0.0, 1.0);
	}

	pub(crate) fn kick(&mut self, degrees: f32) {
		self.kick_deg += degrees;
	}

	pub(crate) fn pulse_fov(&mut self, amount: f32) {
		self.pulse = (self.pulse + amount).min(0.5);
	}

	/// Advance the decays; everything snaps to EXACT zero near rest.
	pub(crate) fn tick(&mut self, dt: f32) {
		if !self.active() {
			return;
		}
		self.phase = (self.phase + dt) % 600.0;
		self.trauma = (self.trauma - TRAUMA_DRAIN * dt).max(0.0);
		self.kick_deg = digblock_core::ease::exp_decay(self.kick_deg, 0.0, DECAY_RATE, dt);
		if self.kick_deg.abs() < SNAP {
			self.kick_deg = 0.0;
		}
		self.pulse = digblock_core::ease::exp_decay(self.pulse, 0.0, DECAY_RATE, dt);
		if self.pulse < SNAP {
			self.pulse = 0.0;
		}
	}

	pub(crate) fn active(&self) -> bool {
		self.trauma > 0.0 || self.kick_deg != 0.0 || self.pulse != 0.0
	}

	/// This frame's offset — the bitwise [`CamOffset::IDLE`] at rest.
	pub(crate) fn offset(&self) -> CamOffset {
		if !self.active() {
			return CamOffset::IDLE;
		}
		let a = self.trauma * self.trauma;
		let t = self.phase;
		// Three incommensurate frequencies per axis family: dense enough
		// to read as noise, cheap enough to be three sines.
		let wobble = |f: f32, o: f32| (t * f + o).sin();
		CamOffset {
			pos: Vec3::new(
				a * SHAKE_POS * wobble(31.0, 0.0),
				a * SHAKE_POS * wobble(37.3, 1.7),
				a * SHAKE_POS * wobble(41.9, 3.1),
			),
			pitch_deg: a * SHAKE_PITCH * wobble(43.7, 0.9) - self.kick_deg,
			roll_deg: a * SHAKE_ROLL * wobble(29.1, 2.3),
			fov_scale: 1.0 + self.pulse,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The exact-rest contract, the `a_resting_camera_does_not_drift`
	/// sibling: once every impulse has drained, the offset is BITWISE the
	/// identity, forever, so the GI history and light list stay asleep.
	#[test]
	fn a_spent_shake_is_bitwise_idle() {
		let mut fx = CameraFx::default();
		assert_eq!(fx.offset(), CamOffset::IDLE);
		fx.add_trauma(0.6);
		fx.kick(1.5);
		fx.pulse_fov(0.1);
		let excited = fx.offset();
		assert_ne!(excited, CamOffset::IDLE);
		for _ in 0..600 {
			fx.tick(1.0 / 60.0);
		}
		assert!(!fx.active());
		let rest = fx.offset();
		assert_eq!(rest.pos, Vec3::ZERO);
		assert_eq!(rest.pitch_deg, 0.0);
		assert_eq!(rest.roll_deg, 0.0);
		assert_eq!(rest.fov_scale, 1.0);
		// And it STAYS idle: further ticks change nothing (the phase
		// itself is frozen at rest).
		fx.tick(1.0 / 60.0);
		assert_eq!(fx.offset(), CamOffset::IDLE);
	}

	/// Amplitude rides trauma squared: half trauma is a quarter shake.
	#[test]
	fn trauma_squares_into_amplitude() {
		let mut a = CameraFx::default();
		let mut b = CameraFx::default();
		a.add_trauma(1.0);
		b.add_trauma(0.5);
		let (oa, ob) = (a.offset(), b.offset());
		let ratio = ob.pos.length() / oa.pos.length();
		assert!((ratio - 0.25).abs() < 1e-3, "{ratio}");
	}
}
