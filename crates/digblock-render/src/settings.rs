//! User settings: plain data shared by the menu's Settings screen and
//! the engine. Deliberately serde-free — each launcher owns a persisted
//! mirror struct (the `ListServer` ↔ `ServerEntry` pattern) and injects
//! load results / a save hook, so this crate stays wasm- and IO-clean.

/// Draw-distance slider bounds, in chunks. The floor is deliberately
/// modern-hardware-sized; the server clamps its own sync radius
/// independently of whatever a client asks for.
pub const DRAW_MIN: i32 = 10;
pub const DRAW_MAX: i32 = 32;
pub const DRAW_STEP: i32 = 2;

/// Volume sliders: multipliers on the authored gains. 1.0 (= 100%) is
/// the slider midpoint so quiet mixes can be boosted, not just cut.
pub const VOL_MAX: f32 = 2.0;
pub const VOL_STEP: f32 = 0.1;

/// Interface-scale slider: a multiplier on the scale the engine derives
/// from window size and display density. The step matches the quarter
/// quantization in `ui::scale::ui_scale` — a finer one would be a dead
/// slider for several notches.
pub const UI_SCALE_MIN: f32 = 0.75;
pub const UI_SCALE_MAX: f32 = 2.0;
pub const UI_SCALE_STEP: f32 = 0.25;

/// The launcher-injected persistence hook (desktop: data-dir JSON;
/// web: localStorage). Called with each change batch.
pub type SettingsStore = Box<dyn FnMut(&Settings)>;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
	pub draw_dist: i32,
	pub music_vol: f32,
	pub sfx_vol: f32,
	/// Screen-space water reflections (a per-frame uniform bit).
	pub ssr: bool,
	/// Multiplier on the derived UI scale, so players can size the
	/// interface to taste (and to their eyesight).
	pub ui_scale: f32,
	/// Last-used multiplayer display name; pre-fills the browser's name
	/// field. Empty = never joined. Canonical validation still runs at
	/// join time.
	pub name: String,
}

impl Default for Settings {
	/// Desktop defaults (mobile launchers override before constructing
	/// the engine).
	fn default() -> Self {
		Self {
			draw_dist: 20,
			music_vol: 1.0,
			sfx_vol: 1.0,
			ssr: true,
			ui_scale: 1.0,
			name: String::new(),
		}
	}
}

impl Settings {
	/// Normalize untrusted values (a hand-edited settings file, an old
	/// format) into the legal ranges; an invalid name degrades to empty.
	pub fn clamped(mut self) -> Self {
		self.draw_dist = self.draw_dist.clamp(DRAW_MIN, DRAW_MAX);
		self.music_vol = self.music_vol.clamp(0.0, VOL_MAX);
		self.sfx_vol = self.sfx_vol.clamp(0.0, VOL_MAX);
		if !self.music_vol.is_finite() {
			self.music_vol = 1.0;
		}
		if !self.sfx_vol.is_finite() {
			self.sfx_vol = 1.0;
		}
		self.ui_scale = if self.ui_scale.is_finite() {
			self.ui_scale.clamp(UI_SCALE_MIN, UI_SCALE_MAX)
		} else {
			1.0
		};
		if digblock_net::protocol::validate_name(&self.name).is_err() {
			self.name.clear();
		}
		self
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clamped_normalizes_everything() {
		let s = Settings {
			draw_dist: 100,
			music_vol: -3.0,
			sfx_vol: f32::NAN,
			ssr: true,
			ui_scale: 9.0,
			name: "bad name!".into(),
		}
		.clamped();
		assert_eq!(s.draw_dist, DRAW_MAX);
		assert_eq!(s.music_vol, 0.0);
		assert_eq!(s.sfx_vol, 1.0);
		assert_eq!(s.ui_scale, UI_SCALE_MAX);
		assert!(s.name.is_empty());

		assert_eq!(
			Settings {
				ui_scale: f32::NAN,
				..Default::default()
			}
			.clamped()
			.ui_scale,
			1.0
		);

		let ok = Settings {
			draw_dist: 12,
			music_vol: 1.5,
			sfx_vol: 0.2,
			ssr: false,
			ui_scale: 1.25,
			name: "Miner42".into(),
		};
		assert_eq!(ok.clone().clamped(), ok);
	}
}
