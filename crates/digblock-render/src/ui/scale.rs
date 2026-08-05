//! UI scale: device scale factor × window-size factor. Rects scale
//! continuously; text snaps to integer multiples of the 8px bitmap font
//! (Nearest sampler — only integer scales stay crisp).

use crate::font;

/// Whole-UI scale, quantized to quarter steps so live-resize walks
/// stable increments instead of shimmering. Reference: a 1280×800
/// logical window at 1× is exactly the pre-scaling design (scale 1.0);
/// bigger windows and denser displays both grow the UI from there.
pub fn ui_scale(size: (u32, u32), scale_factor: f32) -> f32 {
	let sf = scale_factor.max(0.5);
	let (lw, lh) = (size.0 as f32 / sf, size.1 as f32 / sf);
	let win = (lw / 1280.0).min(lh / 800.0).clamp(0.75, 1.5);
	((sf * win) * 4.0).round().max(3.0) / 4.0
}

/// On-screen glyph advance at `scale` (square glyphs). The base is the
/// historical 2× (8px font → 16px on screen).
pub fn glyph_px(scale: f32) -> f32 {
	font::GLYPH_W as f32 * (2.0 * scale).round().max(1.0)
}

/// Surfaces pinned to the pre-scaling size: hotbar stack counts (small
/// by design, per explicit request) and the debug panel (a diagnostic,
/// not UI).
pub const FIXED_GLYPH_PX: f32 = font::GLYPH_W as f32 * 2.0;

/// Horizontal inset between a row's edge and its text; shared by the
/// menu's measurement pass and the overlay's rendering.
pub fn text_pad_x(scale: f32) -> f32 {
	8.0 * scale
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reference_window_is_unit_scale() {
		assert_eq!(ui_scale((1280, 800), 1.0), 1.0);
		// The pre-scaling default window (1280×720) sits just under the
		// reference height: still a sane near-1 step, never below 0.75.
		assert!(ui_scale((1280, 720), 1.0) >= 0.75);
	}

	#[test]
	fn retina_doubles_and_quantizes() {
		let s = ui_scale((2560, 1600), 2.0); // same logical window, 2× glass
		assert_eq!(s, 2.0);
		// Quarter-step quantization: scan window sizes, every result is
		// a multiple of 0.25 and never below the floor.
		for w in (640..=3840).step_by(160) {
			for &sf in &[1.0f32, 1.5, 2.0, 3.0] {
				let s = ui_scale((w, w * 5 / 8), sf);
				assert!((s * 4.0).fract().abs() < 1e-4, "{s} not a quarter step");
				assert!(s >= 0.75);
			}
		}
	}

	#[test]
	fn window_factor_caps_both_ways() {
		// A tiny window can't shrink the UI below 0.75×...
		assert_eq!(ui_scale((640, 400), 1.0), 0.75);
		// ...and a huge 1× display caps at 1.5× the device factor.
		assert_eq!(ui_scale((3840, 2400), 1.0), 1.5);
	}

	#[test]
	fn glyphs_snap_to_integer_multiples() {
		assert_eq!(glyph_px(1.0), 16.0);
		assert_eq!(glyph_px(1.1), 16.0); // 2.2 rounds to 2
		assert_eq!(glyph_px(1.25), 24.0); // 2.5 rounds up
		assert_eq!(glyph_px(2.0), 32.0);
		assert_eq!(glyph_px(0.25), 8.0); // never below 1× the font
		assert_eq!(FIXED_GLYPH_PX, 16.0);
	}
}
