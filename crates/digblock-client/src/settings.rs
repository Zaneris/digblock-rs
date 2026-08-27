//! User settings: plain data shared by the menu's Settings screen and
//! the engine. Deliberately serde-free — each launcher owns a persisted
//! mirror struct (the `ListServer` ↔ `ServerEntry` pattern) and injects
//! load results / a save hook, so this crate stays wasm- and IO-clean.

/// Draw-distance slider bounds, in chunks. `DRAW_MIN` is the default UI
/// floor, deliberately modern-hardware-sized; the server clamps its own
/// sync radius independently of whatever a client asks for.
pub const DRAW_MIN: i32 = 10;
pub const DRAW_MAX: i32 = 32;
pub const DRAW_STEP: i32 = 2;
/// The absolute floor `clamped` accepts, and the slider minimum on a platform whose GPU has not
/// proven it can afford `DRAW_MIN` (`EngineConfig::draw_min` — mobile web today). On `DRAW_MIN`'s
/// own step grid, so lowering the floor moves no existing value. Accepted by `clamped` on EVERY
/// platform: the same browser can flip between the mobile and desktop profiles across sessions
/// over one saved file, and a value legal where it was written must survive being read anywhere.
pub const DRAW_FLOOR: i32 = 6;

/// Light-Sources slider bounds: a runtime cap on the GI light list. The maximum IS the list's
/// compile-time capacity — derived, so the slider cannot offer a rung the buffer was not sized
/// for. The resolve's NEE scans the whole list per pixel, which is why this bounds what a torch
/// field or a cave wall of revealed veins can cost; under the cap the nearest-to-the-view
/// survivors win (`gi::select_lights`).
pub const LIGHT_CAP_MIN: i32 = 64;
pub const LIGHT_CAP_STEP: i32 = 64;
pub const LIGHT_CAP_MAX: i32 = crate::gi::MAX_LIGHTS as i32;
/// Where an interactive desktop (and desktop-web) session STARTS, applied beside the
/// density-derived lighting detail and only when the player has expressed no graphics preference
/// — `EngineConfig::gfx_defaults_auto`, which is false on every capture path, so the shipped
/// `Settings::default` stays at the capacity every recorded baseline was taken with. The full
/// list is the slider's opt-in rather than the default because the cap sweep priced it: the scan
/// is linear in the list (BENCH.md 2026-08-27), and the top half of it buys picture only where a
/// scene actually oversubscribes. On the step grid by construction, so `clamped` cannot move it.
pub const LIGHT_CAP_DEFAULT: i32 = LIGHT_CAP_MIN + LIGHT_CAP_STEP;

/// Volume sliders: multipliers on the authored gains. Unity gain sits at
/// the slider midpoint so quiet mixes can be boosted, not just cut.
pub const VOL_MAX: f32 = 2.0;
pub const VOL_STEP: f32 = 0.1;

/// Render-scale bounds. **No slider exposes this today** — render scale is locked at native for
/// players and only `--render-scale` / `?renderscale=` move it, because the upscale is a plain
/// bilinear blit: notching it down measured an image delta orders of magnitude worse than the GI
/// notch that buys the same time (BENCH.md 2026-08-12). The path stays wired so it does not rot;
/// re-exposing it is a slider row plus an EASU/RCAS filter, which `RENDER_SCALE_STEP` waits for.
pub const RENDER_SCALE_MIN: f32 = 0.5;
pub const RENDER_SCALE_MAX: f32 = 1.0;
pub const RENDER_SCALE_STEP: f32 = 0.05;

/// GI-scale notches: the GI resolve and à-trous run at `render_size / GI_SCALE_DIVISORS[i]`.
/// DIVISORS rather than a continuous slider, and forced: `chunk.wgsl`'s bilateral upsample is a
/// 4-tap gather at render resolution, so the render:GI ratio must be a clean integer — which is
/// also why GI scale is expressed against the RENDER size, not the surface. Full resolution is
/// deliberately not a rung; it buys almost no picture for the largest step (BENCH.md 2026-08-12).
pub const GI_SCALE_DIVISORS: [u32; 3] = [2, 3, 4];
pub const GI_SCALE_DEFAULT: u32 = 2;

/// The lever-only full-resolution divisor, and the shared answer to what
/// `--gi-scale` / `?giscale=` accept — one function, so the two launchers
/// cannot drift.
pub const GI_SCALE_DIV_FULL: u32 = 1;
pub fn lever_gi_div_ok(d: u32) -> bool {
	d == GI_SCALE_DIV_FULL || GI_SCALE_DIVISORS.contains(&d)
}

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
	/// Cap on the GI light list, on the [`LIGHT_CAP_STEP`] grid within
	/// [`LIGHT_CAP_MIN`]..=[`LIGHT_CAP_MAX`].
	pub max_lights: i32,
	/// Touch jump verb: the on-screen button instead of the double-tap.
	/// Only surfaced (and only meaningful) on touch-HUD configs; the row
	/// is hidden elsewhere but the value travels with the settings like
	/// any other, so a tablet and a desktop browser share one store.
	pub touch_jump_button: bool,
}
// There is no `name` here any more. It used to be the last multiplayer
// display name, re-typed per join and remembered as a convenience; a name
// now belongs to the master account and travels with it, so the client has
// nothing to remember and nothing to let drift.

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
			// Native, NOT the high-DPI default. This struct is also what
			// bench/screenshot/drill runs use (they skip settings load
			// entirely), and those must stay at native or every recorded
			// baseline shifts under them. The density-derived default is
			// applied by the LAUNCHER, for interactive runs only.
			render_scale: 1.0,
			gi_scale_div: GI_SCALE_DEFAULT,
			// The full capacity, for the same reason render_scale is
			// native: captures build this struct, and every recorded
			// baseline was taken with the whole list.
			max_lights: LIGHT_CAP_MAX,
			// The double-tap is the shipped jump — the old Java feel.
			touch_jump_button: false,
		}
	}
}

/// The `gi_scale_div` to START an interactive session at, from the display's backing scale.
/// Applied once, and only when the player has expressed no preference — see
/// `EngineConfig::gfx_defaults_auto`. Which term gives on a dense panel is a measured call: the
/// interleaved A/B in BENCH.md 2026-08-12 has a render-scale notch and a GI-scale notch buying
/// the SAME time, the GI notch costing far less picture. So a dense display stays NATIVE.
pub fn density_gi_div(backing_scale: f32) -> u32 {
	if backing_scale >= 1.5 {
		3
	} else {
		GI_SCALE_DEFAULT
	}
}

impl Settings {
	/// Normalize untrusted values (a hand-edited settings file, an old
	/// format) into the legal ranges.
	pub fn clamped(mut self) -> Self {
		// Down to the FLOOR, not the default UI minimum — see DRAW_FLOOR:
		// a save written under a mobile profile's lower slider must
		// survive a desktop read.
		self.draw_dist = self.draw_dist.clamp(DRAW_FLOOR, DRAW_MAX);
		// Clamp then snap DOWN onto the step grid: the grid spans the
		// range exactly, so the snap cannot leave it. Snapped rather than
		// kept because every rung is a re-flush boundary the slider was
		// built around, and an off-grid stored value would be a rung the
		// UI can display but never return to.
		let cap = self.max_lights.clamp(LIGHT_CAP_MIN, LIGHT_CAP_MAX);
		self.max_lights = cap - (cap - LIGHT_CAP_MIN) % LIGHT_CAP_STEP;
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
			max_lights: i32::MAX,
			touch_jump_button: false,
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
		// That includes the lever-only full-res divisor: a save written
		// when full-res was a slider rung degrades to the default, and the
		// levers still accept what the slider offers.
		assert_eq!(
			Settings {
				gi_scale_div: GI_SCALE_DIV_FULL,
				..Default::default()
			}
			.clamped()
			.gi_scale_div,
			GI_SCALE_DEFAULT
		);
		assert!(lever_gi_div_ok(GI_SCALE_DIV_FULL));
		assert!(GI_SCALE_DIVISORS.iter().all(|&d| lever_gi_div_ok(d)));
		assert_eq!(s.max_lights, LIGHT_CAP_MAX);

		// The draw floor, not the default UI minimum, is what an
		// out-of-range save clamps to — a mobile-web save must survive a
		// desktop read.
		assert_eq!(
			Settings {
				draw_dist: 0,
				..Default::default()
			}
			.clamped()
			.draw_dist,
			DRAW_FLOOR
		);
		// The floor sits on DRAW_MIN's own step grid, so opening the
		// lower rungs moved no existing value.
		assert_eq!((DRAW_MIN - DRAW_FLOOR) % DRAW_STEP, 0);

		// The light cap clamps to its bounds and snaps DOWN onto the step
		// grid; every on-grid rung survives a round-trip.
		assert_eq!(
			Settings {
				max_lights: 0,
				..Default::default()
			}
			.clamped()
			.max_lights,
			LIGHT_CAP_MIN
		);
		let mut rungs = 0;
		let mut v = LIGHT_CAP_MIN;
		while v <= LIGHT_CAP_MAX {
			let s = Settings {
				max_lights: v,
				..Default::default()
			}
			.clamped();
			assert_eq!(s.max_lights, v);
			// An off-grid value snaps down to the rung below it (v+1 can
			// only clamp back onto v at the top because the grid spans
			// the range exactly).
			let off = Settings {
				max_lights: v + 1,
				..Default::default()
			}
			.clamped();
			assert_eq!(off.max_lights, v);
			rungs += 1;
			v += LIGHT_CAP_STEP;
		}
		assert!(rungs > 1, "the slider must have more than one rung");

		// A capture run builds Settings::default() and must stay at the
		// shipped values: bench/screenshot/drill skip the settings load,
		// so a density-derived default living HERE would silently move
		// every recorded baseline.
		assert_eq!(Settings::default().render_scale, 1.0);
		assert_eq!(Settings::default().gi_scale_div, GI_SCALE_DEFAULT);
		assert_eq!(Settings::default().max_lights, LIGHT_CAP_MAX);
		// High density drops LIGHTING DETAIL, not resolution — the two
		// measured the same win and this one costs far less picture.
		assert_eq!(density_gi_div(2.0), 3);
		assert_eq!(density_gi_div(1.0), GI_SCALE_DEFAULT);
		// An interactive session starts SHORT of the capacity captures
		// keep, and above the floor the mobile profile starts at — so the
		// full list is an opt-in and the desktop default is not the
		// mobile one. On the step grid, or the slider could display a
		// rung it can never return to.
		const { assert!(LIGHT_CAP_MIN < LIGHT_CAP_DEFAULT && LIGHT_CAP_DEFAULT < LIGHT_CAP_MAX) };
		assert_eq!(
			Settings {
				max_lights: LIGHT_CAP_DEFAULT,
				..Default::default()
			}
			.clamped()
			.max_lights,
			LIGHT_CAP_DEFAULT,
			"the interactive default must survive its own clamp"
		);

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
			max_lights: LIGHT_CAP_MIN + LIGHT_CAP_STEP,
			touch_jump_button: true,
		};
		assert_eq!(ok.clone().clamped(), ok);
	}
}
