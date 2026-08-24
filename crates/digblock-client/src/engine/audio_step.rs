//! Cadence emitters. These pace by DISTANCE walked rather than by
//! time, so a slow walk still sounds right; strokes fire only in water
//! and off the bottom, exactly where footsteps stop.

use super::*;

impl Engine {
	/// The block just under the feet and whether it holds the player up.
	/// Shared by the footstep cadence and the held item's walk bob, which
	/// have to agree: the hand keeps time with the feet, so anything that
	/// stops the steps sounding stops the hand swinging. Cheap enough to
	/// call twice a frame — it is one `block_at`.
	pub(super) fn ground_below(&self) -> (Option<BlockType>, bool) {
		let feet = self.player.pos.y - digblock_core::player::EYE_TO_FEET;
		let below = self.pipeline.block_at(
			self.player.pos.x.floor() as i32,
			(feet - 0.05).floor() as i32,
			self.player.pos.z.floor() as i32,
		);
		(below, below.is_some_and(|b| b.props().collides))
	}

	/// Per-frame audio upkeep, after the camera settles: listener pose, footsteps, waterfall
	/// voices, ambient bed, with one early-out branch when no sink is attached. `prev_player_pos`
	/// is deliberately NOT advanced here — `update` owns it, because the viewmodel's bob reads the
	/// same delta and this function returns early on a silent run.
	pub(super) fn step_audio(&mut self, dt: f32) {
		use crate::audio::{AmbientZone, AudioCmd, LOOP_AMBIENT, ambient_sfx, step_sfx};
		/// Splash gain: a step into the shallows, plus this much per block
		/// of the fall that preceded it, capped.
		const SPLASH_GAIN_MIN: f32 = 0.3;
		const SPLASH_GAIN_PER_BLOCK: f32 = 0.06;
		const SPLASH_GAIN_MAX: f32 = 0.9;
		if self.audio.sink.is_none() {
			return;
		}
		let cam = self.camera.pos;
		self.emit(AudioCmd::Listener {
			pos: cam.to_array(),
			forward: self.camera.dir().to_array(),
		});

		// Footsteps and the water-entry splash, both driven by the feet.
		let submerged = self.eye_in_water();
		let d = self.player.pos - self.audio.prev_player_pos;
		let feet = self.player.pos.y - digblock_core::player::EYE_TO_FEET;
		let (bx, bz) = (
			self.player.pos.x.floor() as i32,
			self.player.pos.z.floor() as i32,
		);
		let (below, grounded) = self.ground_below();
		let feet_wet = self
			.pipeline
			.block_at(bx, feet.floor() as i32, bz)
			.is_some_and(|b| b.is_water());
		let wet = submerged || feet_wet;
		if self.input.walking {
			let moved = Vec3::new(d.x, 0.0, d.z).length();
			if let Some((m, gain)) = self.audio.footsteps.advance(moved, grounded, below, wet) {
				self.emit(AudioCmd::Sfx {
					sound: step_sfx(m),
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
			// One splash on the rising edge, scaled by the fall that got
			// you there — a step off a bank barely registers, a plunge
			// from a cliff does. Fly mode is excluded: the body isn't
			// being simulated, so drifting through a surface isn't an
			// entry.
			if wet && !self.audio.prev_wet {
				let drop = (self.audio.fall_from_y - feet).max(0.0);
				let gain = (SPLASH_GAIN_MIN + drop * SPLASH_GAIN_PER_BLOCK).min(SPLASH_GAIN_MAX);
				self.emit(AudioCmd::Sfx {
					sound: crate::audio::Sfx::Splash,
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
			// Strokes while actually swimming — in the water and off the
			// bottom. Wading is grounded, so it keeps its footsteps and
			// the two can never double up.
			if let Some(gain) = self.audio.swim_strokes.advance(moved, wet && !grounded) {
				self.emit(AudioCmd::Sfx {
					sound: crate::audio::Sfx::Swim,
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
		}
		self.audio.prev_wet = wet;
		self.audio.fall_from_y = if grounded || wet || !self.input.walking {
			feet
		} else {
			self.audio.fall_from_y.max(feet)
		};

		// Waterfall voices.
		let mut cmds = std::mem::take(&mut self.audio.audio_scratch);
		self.audio.water_audio.update(dt, cam, &mut cmds);
		for cmd in cmds.drain(..) {
			self.emit(cmd);
		}
		self.audio.audio_scratch = cmds;

		// Ambient bed: underwater beats cave beats surface. Cave = well
		// below the pure worldgen surface (dug pits near grade don't
		// count — that's fine, they're open to the sky anyway).
		let zone = if submerged {
			AmbientZone::Underwater
		} else {
			// `terrain_height` runs the whole continentalness/roughness/
			// ridge/detail stack, which is far too much to spend every
			// frame on a question whose answer is a pure function of the
			// column — and the camera changes column a few times a second
			// at a walk.
			let col = (cam.x.floor() as i32, cam.z.floor() as i32);
			let h = match self.ambient_height {
				Some((c, h)) if c == col => h,
				_ => {
					let h = digblock_core::worldgen::terrain_height(col.0, col.1, self.seed);
					self.ambient_height = Some((col, h));
					h
				}
			};
			if cam.y < h as f32 - 6.0 {
				AmbientZone::Cave
			} else {
				AmbientZone::Surface
			}
		};
		if let Some(zone) = self.audio.ambient.advance(zone, dt) {
			self.emit(AudioCmd::LoopStart {
				id: LOOP_AMBIENT,
				sound: ambient_sfx(zone),
				pos: None,
				gain: crate::audio::AMBIENT_GAIN,
			});
		}
		// The prospector's sense (SKILLS-DESIGN §5.5): a faint shimmer near a deposit whose tier
		// you cannot yet see, reaching further at the sense's second rank. Audio only — an overlay
		// would make every reveal level meaningless. It borrows the cave bed's voice as a
		// POSITIONAL one-shot, a stand-in; swapping this to a new `Sfx` variant is the whole change
		// when the design's own sourced shimmer exists.
		let mining = self.skills.level(digblock_core::skills::Skill::Mining);
		let near = digblock_core::unlock::prospector(mining)
			.then(|| {
				digblock_core::worldgen::ore::unseen_nearby(
					(
						cam.x.floor() as i32,
						cam.y.floor() as i32,
						cam.z.floor() as i32,
					),
					self.seed.value,
					mining,
				)
			})
			.flatten();
		if self.audio.prospect.advance(near.is_some(), dt)
			&& let Some(d) = near
		{
			self.emit(AudioCmd::Sfx {
				sound: crate::audio::Sfx::AmbientCave,
				pos: Some(cell_center((d.x, d.y, d.z))),
				gain: crate::audio::PROSPECT_GAIN,
			});
		}

		// Music pool follows the world clock. `tod` is the sun's angle from straight down, so the
		// night window is exactly "sun below the horizon". No hysteresis needed: tod is monotonic,
		// and the filler only acts on this at the next track boundary anyway.
		let night = self.sun.tod.abs() >= 90.0;
		if self.audio.music_night != Some(night) {
			self.audio.music_night = Some(night);
			self.emit(AudioCmd::MusicSet { night });
		}
	}

	/// Send one audio command if a sink is attached (free otherwise).
	pub(super) fn emit(&mut self, cmd: crate::audio::AudioCmd) {
		if let Some(a) = self.audio.sink.as_mut() {
			a.send(cmd);
		}
	}
}
