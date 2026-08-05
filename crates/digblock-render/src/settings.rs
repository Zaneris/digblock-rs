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
			name: "bad name!".into(),
		}
		.clamped();
		assert_eq!(s.draw_dist, DRAW_MAX);
		assert_eq!(s.music_vol, 0.0);
		assert_eq!(s.sfx_vol, 1.0);
		assert!(s.name.is_empty());

		let ok = Settings {
			draw_dist: 12,
			music_vol: 1.5,
			sfx_vol: 0.2,
			ssr: false,
			name: "Miner42".into(),
		};
		assert_eq!(ok.clone().clamped(), ok);
	}
}
