//! The audio seam: the engine emits [`AudioCmd`]s into an injected
//! [`AudioSink`] and never touches a device — mirroring the
//! `MenuServices`/`NetTransport` pattern that keeps the engine
//! platform-free. Desktop fills the seam with rodio on its own thread
//! (`apps/desktop/src/audio.rs`); web applies commands synchronously to
//! a WebAudio graph (`apps/web/src/audio.rs`); iOS injects nothing and
//! stays silent. A `None` sink costs one branch per would-be emit, so
//! bench/screenshot runs are untouched.
//!
//! This module also owns the pure, headless-testable emission logic
//! that doesn't fit a single call site: footstep cadence and the
//! ambient-bed classifier.

use digblock_core::block::BlockType;

/// Sound identifiers, 1:1 with the OGGs in `digblock_assets::sfx`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sfx {
	DigStone,
	DigEarth,
	DigWood,
	BreakStone,
	BreakEarth,
	BreakWood,
	Place,
	StepStone,
	StepEarth,
	StepWood,
	Waterfall,
	AmbientWind,
	AmbientWater,
	AmbientCave,
}

/// The embedded bytes for a sound — the enum↔asset pairing lives here
/// and nowhere else; both platform fillers key their decode caches off
/// this.
pub fn bytes(s: Sfx) -> &'static [u8] {
	use digblock_assets::sfx as a;
	match s {
		Sfx::DigStone => a::DIG_STONE,
		Sfx::DigEarth => a::DIG_EARTH,
		Sfx::DigWood => a::DIG_WOOD,
		Sfx::BreakStone => a::BREAK_STONE,
		Sfx::BreakEarth => a::BREAK_EARTH,
		Sfx::BreakWood => a::BREAK_WOOD,
		Sfx::Place => a::PLACE,
		Sfx::StepStone => a::STEP_STONE,
		Sfx::StepEarth => a::STEP_EARTH,
		Sfx::StepWood => a::STEP_WOOD,
		Sfx::Waterfall => a::WATERFALL,
		Sfx::AmbientWind => a::AMBIENT_WIND,
		Sfx::AmbientWater => a::AMBIENT_WATER,
		Sfx::AmbientCave => a::AMBIENT_CAVE,
	}
}

/// Every playable [`Sfx`], for filler decode caches and tests.
pub const ALL_SFX: [Sfx; 14] = [
	Sfx::DigStone,
	Sfx::DigEarth,
	Sfx::DigWood,
	Sfx::BreakStone,
	Sfx::BreakEarth,
	Sfx::BreakWood,
	Sfx::Place,
	Sfx::StepStone,
	Sfx::StepEarth,
	Sfx::StepWood,
	Sfx::Waterfall,
	Sfx::AmbientWind,
	Sfx::AmbientWater,
	Sfx::AmbientCave,
];

/// Sound-material class of a block (dig/break/step variants).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Material {
	Stone,
	Earth,
	Wood,
}

/// Which material a block sounds like. Total over every block — new
/// blocks fall back to Earth rather than panicking.
pub fn material_of(b: BlockType) -> Material {
	match b {
		BlockType::Stone => Material::Stone,
		BlockType::Wood | BlockType::Leaves => Material::Wood,
		_ => Material::Earth,
	}
}

pub fn dig_sfx(m: Material) -> Sfx {
	match m {
		Material::Stone => Sfx::DigStone,
		Material::Earth => Sfx::DigEarth,
		Material::Wood => Sfx::DigWood,
	}
}

pub fn break_sfx(m: Material) -> Sfx {
	match m {
		Material::Stone => Sfx::BreakStone,
		Material::Earth => Sfx::BreakEarth,
		Material::Wood => Sfx::BreakWood,
	}
}

pub fn step_sfx(m: Material) -> Sfx {
	match m {
		Material::Stone => Sfx::StepStone,
		Material::Earth => Sfx::StepEarth,
		Material::Wood => Sfx::StepWood,
	}
}

/// Stable handle for a running loop voice.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LoopId(pub u32);

/// The hold-to-dig loop (one at a time).
pub const LOOP_DIG: LoopId = LoopId(1);
/// The non-positional ambient bed (one at a time; restarted on change).
pub const LOOP_AMBIENT: LoopId = LoopId(2);
/// Water voices allocate upward from here (see `water_audio.rs`).
pub const LOOP_WATER_BASE: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioCmd {
	/// Where the ears are. Sent once per frame after the camera settles.
	Listener {
		pos: [f32; 3],
		forward: [f32; 3],
	},
	/// One-shot. `pos: None` = non-positional (UI-ish, full volume).
	Sfx {
		sound: Sfx,
		pos: Option<[f32; 3]>,
		gain: f32,
	},
	/// Start (or restart) a loop voice under a stable id.
	LoopStart {
		id: LoopId,
		sound: Sfx,
		pos: Option<[f32; 3]>,
		gain: f32,
	},
	/// Move/retune a running loop voice; ignored for unknown ids.
	LoopMove {
		id: LoopId,
		pos: [f32; 3],
		gain: f32,
	},
	/// Playback-rate multiplier for a running loop (dig pitch ramp).
	LoopPitch {
		id: LoopId,
		rate: f32,
	},
	LoopStop {
		id: LoopId,
	},
	/// App suspended / tab hidden: silence everything, keep state.
	Pause,
	Resume,
}

/// The injected backend. Deliberately not `Send` — the web filler holds
/// JS values (precedent: `NetTransport`).
pub trait AudioSink {
	fn send(&mut self, cmd: AudioCmd);
}

// ---- footstep cadence -------------------------------------------------

/// Distance-driven footstep emitter: pure accumulator logic, engine
/// feeds it per-frame movement facts and forwards the returned step.
#[derive(Default)]
pub struct Footsteps {
	accum: f32,
	was_airborne: bool,
}

/// Horizontal distance between steps, blocks (walk speed 4 → ~2 steps/s).
const STRIDE: f32 = 2.2;

impl Footsteps {
	/// Advance by one frame. `moved` = horizontal distance this frame;
	/// `grounded` = standing on `ground` (the block under the feet);
	/// `in_water` suppresses steps entirely. Returns a step's material
	/// and gain when one lands this frame.
	pub fn advance(
		&mut self,
		moved: f32,
		grounded: bool,
		ground: Option<BlockType>,
		in_water: bool,
	) -> Option<(Material, f32)> {
		if in_water || !grounded {
			self.accum = 0.0;
			self.was_airborne = !grounded && !in_water;
			return None;
		}
		let ground = ground?;
		// Landing: one immediate, slightly heavier step.
		if std::mem::take(&mut self.was_airborne) {
			self.accum = 0.0;
			return Some((material_of(ground), 0.5));
		}
		self.accum += moved;
		if self.accum >= STRIDE {
			self.accum -= STRIDE;
			Some((material_of(ground), 0.35))
		} else {
			None
		}
	}
}

// ---- ambient bed ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AmbientZone {
	Surface,
	Underwater,
	Cave,
}

pub fn ambient_sfx(z: AmbientZone) -> Sfx {
	match z {
		AmbientZone::Surface => Sfx::AmbientWind,
		AmbientZone::Underwater => Sfx::AmbientWater,
		AmbientZone::Cave => Sfx::AmbientCave,
	}
}

/// Ambient-bed state machine: classification comes from the engine
/// (submersion test + worldgen surface height); this owns the
/// hysteresis so boundary flicker never restarts the bed.
pub struct Ambient {
	current: AmbientZone,
	pending: AmbientZone,
	pending_secs: f32,
	started: bool,
}

/// A new classification must hold this long before the bed switches.
const AMBIENT_HYSTERESIS_SECS: f32 = 1.0;
pub const AMBIENT_GAIN: f32 = 0.25;

impl Default for Ambient {
	fn default() -> Self {
		Self {
			current: AmbientZone::Surface,
			pending: AmbientZone::Surface,
			pending_secs: 0.0,
			started: false,
		}
	}
}

impl Ambient {
	/// Feed this frame's classification; returns a zone when the bed
	/// should (re)start — the first call always starts the current bed.
	pub fn advance(&mut self, zone: AmbientZone, dt: f32) -> Option<AmbientZone> {
		if !self.started {
			self.started = true;
			self.current = zone;
			self.pending = zone;
			return Some(zone);
		}
		if zone != self.pending {
			self.pending = zone;
			self.pending_secs = 0.0;
			return None;
		}
		if zone == self.current {
			self.pending_secs = 0.0;
			return None;
		}
		self.pending_secs += dt;
		if self.pending_secs >= AMBIENT_HYSTERESIS_SECS {
			self.current = zone;
			self.pending_secs = 0.0;
			Some(zone)
		} else {
			None
		}
	}

	/// Reset so the next `advance` restarts the bed (world swap).
	pub fn reset(&mut self) {
		self.started = false;
		self.pending_secs = 0.0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn material_is_total_over_diggable_blocks() {
		for b in BlockType::ALL {
			if b.hardness_ticks().is_some() {
				let _ = material_of(b); // must not panic
			}
		}
		assert_eq!(material_of(BlockType::Stone), Material::Stone);
		assert_eq!(material_of(BlockType::Grass), Material::Earth);
		assert_eq!(material_of(BlockType::Wood), Material::Wood);
	}

	#[test]
	fn every_sfx_has_nonempty_bytes() {
		for s in ALL_SFX {
			assert!(!bytes(s).is_empty(), "{s:?}");
		}
	}

	#[test]
	fn footsteps_pace_by_distance_and_stop_when_airborne() {
		let mut f = Footsteps::default();
		let ground = Some(BlockType::Grass);
		let mut steps = 0;
		// 60 frames of walking at 4 blocks/s: ~4 blocks → 1-2 steps.
		for _ in 0..60 {
			if f.advance(4.0 / 60.0, true, ground, false).is_some() {
				steps += 1;
			}
		}
		assert!((1..=2).contains(&steps), "{steps}");
		// Airborne: accumulator resets, no steps.
		assert!(f.advance(1.0, false, None, false).is_none());
		assert!(f.advance(1.0, false, None, false).is_none());
		// Landing emits one heavier step immediately.
		let landed = f.advance(0.0, true, ground, false);
		assert_eq!(landed, Some((Material::Earth, 0.5)));
		// Swimming: silent even while "grounded".
		assert!(f.advance(1.0, true, ground, true).is_none());
	}

	#[test]
	fn ambient_switches_only_after_hysteresis() {
		let mut a = Ambient::default();
		// First frame starts the surface bed.
		assert_eq!(
			a.advance(AmbientZone::Surface, 0.016),
			Some(AmbientZone::Surface)
		);
		// A brief dip underwater (shorter than the hysteresis) is ignored.
		for _ in 0..30 {
			assert_eq!(a.advance(AmbientZone::Underwater, 0.016), None);
		}
		assert_eq!(a.advance(AmbientZone::Surface, 0.016), None);
		// A sustained dive switches once.
		let mut switched = 0;
		for _ in 0..90 {
			if a.advance(AmbientZone::Underwater, 0.016).is_some() {
				switched += 1;
			}
		}
		assert_eq!(switched, 1);
	}
}
