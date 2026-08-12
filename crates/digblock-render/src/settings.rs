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

/// Render-scale bounds. **No slider exposes this today** — render scale is
/// locked at 100% for players and only `--render-scale` / `?renderscale=`
/// move it, because the upscale is a plain bilinear blit: dropping it to
/// 0.7 measured mean 18.61/255 against the GI notch's 0.11 for the same
/// 30% (BENCH.md 2026-08-12). The whole path stays built, wired and
/// reachable from the flag so it does not rot, and re-exposing it is a
/// slider row plus an EASU/RCAS filter in the upscale pass.
///
/// `RENDER_SCALE_STEP` is what that row would step by; it is unused while
/// the row is hidden.
pub const RENDER_SCALE_MIN: f32 = 0.5;
pub const RENDER_SCALE_MAX: f32 = 1.0;
pub const RENDER_SCALE_STEP: f32 = 0.05;

/// GI-scale notches: the GI resolve and à-trous run at
/// `render_size / GI_SCALE_DIVISORS[i]`.
///
/// DIVISORS, not a continuous slider, and that is forced rather than
/// chosen: `chunk.wgsl`'s bilateral upsample is a 4-tap gather at render
/// resolution, so the render:GI ratio has to be a clean integer or the
/// footprint it walks stops matching the texels it reads. It is also why
/// GI scale is expressed against the RENDER size and not the surface —
/// anchored to native, render 0.7 with a half-res GI would ask for a
/// ratio of 0.714.
///
/// 2 is the shipped default, so a default frame is unchanged.
pub const GI_SCALE_DIVISORS: [u32; 4] = [1, 2, 3, 4];
pub const GI_SCALE_DEFAULT: u32 = 2;

/// Interface-scale slider: a multiplier on the scale the engine derives
/// from window size and display density. Defined next to the function
/// that gives the numbers meaning, and re-exported here because the
/// launchers clamp CLI/query overrides against them.
pub use crate::ui::scale::{UI_SCALE_MAX, UI_SCALE_MIN, UI_SCALE_STEP};

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
	/// Fraction of the surface the world renders at (the upscale pass
	/// takes it to native). Never touches the UI.
	pub render_scale: f32,
	/// The GI resolve runs at `render_size / gi_scale_div`. One of
	/// [`GI_SCALE_DIVISORS`].
	pub gi_scale_div: u32,
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
			// 1.0, NOT the high-DPI default. This struct is also what
			// bench/screenshot/drill runs use (they skip settings load
			// entirely), and those must stay at native or every recorded
			// baseline shifts under them. The density-derived default is
			// applied by the LAUNCHER, for interactive runs only.
			render_scale: 1.0,
			gi_scale_div: GI_SCALE_DEFAULT,
			name: String::new(),
		}
	}
}

/// The `gi_scale_div` to START an interactive session at, from the
/// display's backing scale. Applied once, and only when the player has
/// expressed no preference — see `EngineConfig::gfx_defaults_auto`.
///
/// A high-density panel spends ~4x the pixels of its logical window, and
/// the GI resolve is 60% of this frame, so something has to give there.
/// **Which** term gives is a measured call, not a taste one (BENCH.md
/// 2026-08-12, interleaved at 5.43 MP):
///
/// | | avg ms | vs native | image delta |
/// |---|---|---|---|
/// | render 1.0, gi 2 | 12.50 | — | — |
/// | render 0.7, gi 2 | 8.74 | −30% | mean 18.61/255 |
/// | render 1.0, gi 3 | 8.72 | −30% | mean **0.11**/255 |
///
/// The two buy the same time and the GI notch costs ~170x less picture,
/// because the a-trous filter and the bilateral upsample already
/// reconstruct that signal — so a dense display keeps the world at NATIVE
/// resolution and drops lighting detail instead. Render scale is not a
/// knob players have at all today; see `RENDER_SCALE_MIN`.
pub fn density_gi_div(backing_scale: f32) -> u32 {
	if backing_scale >= 1.5 {
		3
	} else {
		GI_SCALE_DEFAULT
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
		self.render_scale = if self.render_scale.is_finite() {
			self.render_scale.clamp(RENDER_SCALE_MIN, RENDER_SCALE_MAX)
		} else {
			1.0
		};
		// An unknown divisor degrades to the shipped default rather than
		// to the nearest legal one: a 4-tap gather against a ratio it was
		// not built for is a rendering bug, not a quality setting.
		if !GI_SCALE_DIVISORS.contains(&self.gi_scale_div) {
			self.gi_scale_div = GI_SCALE_DEFAULT;
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
			ui_scale: 9.0,
			render_scale: 4.0,
			gi_scale_div: 7,
			name: "bad name!".into(),
		}
		.clamped();
		assert_eq!(s.draw_dist, DRAW_MAX);
		assert_eq!(s.music_vol, 0.0);
		assert_eq!(s.sfx_vol, 1.0);
		assert_eq!(s.ui_scale, UI_SCALE_MAX);
		assert_eq!(s.render_scale, RENDER_SCALE_MAX);
		// An illegal divisor goes to the DEFAULT, not the nearest legal
		// value — see `clamped`.
		assert_eq!(s.gi_scale_div, GI_SCALE_DEFAULT);
		assert!(s.name.is_empty());

		// A capture run builds Settings::default() and must stay at the
		// shipped values: bench/screenshot/drill skip the settings load,
		// so a density-derived default living HERE would silently move
		// every recorded baseline.
		assert_eq!(Settings::default().render_scale, 1.0);
		assert_eq!(Settings::default().gi_scale_div, GI_SCALE_DEFAULT);
		// High density drops LIGHTING DETAIL, not resolution — the two
		// measured the same win and this one costs ~170x less picture.
		assert_eq!(density_gi_div(2.0), 3);
		assert_eq!(density_gi_div(1.0), GI_SCALE_DEFAULT);

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
			render_scale: 0.75,
			gi_scale_div: 3,
			name: "Miner42".into(),
		};
		assert_eq!(ok.clone().clamped(), ok);
	}
}
