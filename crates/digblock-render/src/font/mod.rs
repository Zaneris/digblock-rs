//! Text: two embedded typefaces, a glyph atlas, and word wrapping.
//!
//! Everything here is CPU-side and headless-testable. The GPU consumer is
//! [`crate::overlay`], which owns the atlas texture and uploads the rows
//! [`GlyphAtlas::take_dirty`] reports.
//!
//! Sizes are expressed as a **layout box height** in device pixels, never
//! as an em — see [`Face::em_px`]. `y` coordinates passed to text drawing
//! are the **top of that box**, not a baseline, which is what lets every
//! `(rect.h - glyph) / 2.0` centering site keep working unchanged.

mod atlas;
mod face;
mod wrap;

pub use atlas::{ATLAS_H, ATLAS_W, GlyphAtlas};
pub use face::{Face, RasterGlyph};
pub use wrap::wrap;

use std::sync::LazyLock;

/// Which embedded face a piece of text is drawn with.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum FaceId {
	/// Proportional. Everything the player reads.
	#[default]
	Ui,
	/// Monospace. The debug overlay and spike log only — they align
	/// columns with printf padding, which needs a fixed advance.
	Mono,
}

// Layout box height → em size. The UI face maps 1:1 — a box of a given height
// is that many pixels of Inter, the ordinary reading of a UI type size. Note
// this IS visibly smaller than the bitmap face it replaces, whose capitals
// filled nearly the whole box; that is the intended look, and the
// interface-scale slider is there for anyone who wants the old heft back.
//
// The mono face is trimmed slightly so its taller line box still fits the debug
// panel's row pitch.
//
// These are THE tuning knobs for a font swap — nothing else depends on their
// values.
const UI_EM_RATIO: f32 = 1.0;
const MONO_EM_RATIO: f32 = 0.95;

static UI: LazyLock<Face> = LazyLock::new(|| Face::load(digblock_assets::font::UI, UI_EM_RATIO));
static MONO: LazyLock<Face> =
	LazyLock::new(|| Face::load(digblock_assets::font::MONO, MONO_EM_RATIO));

pub fn ui() -> &'static Face {
	&UI
}

pub fn mono() -> &'static Face {
	&MONO
}

pub fn face(id: FaceId) -> &'static Face {
	match id {
		FaceId::Ui => ui(),
		FaceId::Mono => mono(),
	}
}

/// Every character the wire alphabet allows, in code point order. The
/// ranges live in `digblock-net` because the server is what enforces
/// them; this crate's job is proving the embedded face can draw them.
#[cfg(test)]
pub use digblock_net::protocol::text_charset as repertoire;

#[cfg(test)]
mod tests {
	use super::*;

	// The contract SOURCES.md advertises: swap in a face that can't draw
	// the repertoire and this fires, instead of players seeing `?` where
	// their name's accents should be.
	#[test]
	fn ui_face_covers_the_whole_repertoire() {
		let missing: Vec<char> = repertoire().filter(|&c| !ui().covers(c)).collect();
		assert!(missing.is_empty(), "ui.ttf is missing {missing:?}");
		assert!(ui().covers('?'), "the fallback glyph must exist");
	}

	// The mono face only ever draws engine-generated ASCII (the debug
	// overlay), so it is held to a lower bar on purpose — JetBrains Mono
	// has no U+0132/0133, and that is fine.
	#[test]
	fn mono_face_covers_printable_ascii() {
		let missing: Vec<char> = (' '..='~').filter(|&c| !mono().covers(c)).collect();
		assert!(missing.is_empty(), "mono.ttf is missing {missing:?}");
	}

	// Layout runs in the screens, emission runs in the overlay, and they
	// are separated by a whole frame — if their advance walks ever
	// disagree, panels clip their own text. Both go through Face::run;
	// this is what keeps it that way.
	#[test]
	fn measure_matches_the_pen_walk() {
		let samples = [
			"",
			" ",
			"INVENTORY",
			"Draw Distance: 32",
			"héllo wörld — příliš žluťoučký kůň",
			"WWWWiiii....",
		];
		for f in [ui(), mono()] {
			for px in [8.0, 12.0, 16.0, 20.0, 33.0, 64.0] {
				for s in samples {
					let mut walked = 0.0;
					for (c, pen) in f.run(s, px) {
						assert!((pen - walked).abs() < 1e-4, "{s:?} @{px}");
						walked += f.advance(c, px);
					}
					assert!(
						(f.measure(s, px) - walked).abs() < 1e-4,
						"measure != pen walk for {s:?} @{px}"
					);
				}
			}
		}
	}

	#[test]
	fn unsupported_characters_fall_back_to_a_visible_marker() {
		let px = 16.0;
		let (c, _) = ui().run("😀", px).next().unwrap();
		assert_eq!(c, '?');
		assert!((ui().measure("😀", px) - ui().measure("?", px)).abs() < 1e-4);
	}

	// Sign errors here read as "the font is off by a line" and invite a
	// wrong fix in baseline(). `l` is all above the baseline; `g` reaches
	// below it; `.` sits on it.
	#[test]
	fn glyph_bitmaps_sit_on_the_baseline() {
		let px = 32.0;
		let l = ui().raster('l', px);
		let g = ui().raster('g', px);
		let dot = ui().raster('.', px);
		assert!(l.top < 0, "ascender must start above the baseline");
		assert!(l.top + l.h as i32 <= 0, "'l' must not cross the baseline");
		assert!(g.top + g.h as i32 > 0, "'g' must descend past the baseline");
		assert!(dot.top + dot.h as i32 <= 1, "'.' must rest on the baseline");
		assert!(ui().raster(' ', px).cov.is_empty(), "space has no ink");
	}

	// Ink overflows the nominal box — accents reach above it, descenders
	// below — exactly as a CSS line box works. What must hold is that it
	// stays inside the LINE box, because that is what surfaces drawing a
	// backdrop behind text (name tags, the chat feed) size themselves to.
	#[test]
	fn ink_stays_within_the_line_box() {
		for (id, f) in [(FaceId::Ui, ui()), (FaceId::Mono, mono())] {
			for px in [12.0, 16.0, 24.0, 48.0] {
				let base = f.baseline(px);
				// A few percent of headroom on top of the line box: some
				// glyphs' accents (the ring on Å) sit a hair above the face's
				// declared ascent, which is normal and invisible. Gross
				// errors — a flipped sign, a baseline off by a line — are
				// still an order of magnitude outside this.
				let slack = (f.line_h(px) - px) / 2.0 + 1.0 + px * 0.05;
				for c in ['g', 'É', 'Å', 'j', 'W', 'ĝ', 'Ź', 'Q'] {
					if !f.covers(c) {
						continue;
					}
					let r = f.raster(c, px);
					let top = base + r.top as f32;
					assert!(top >= -slack, "{id:?} {c:?} @{px} clips the line box top");
					assert!(
						top + r.h as f32 <= px + slack,
						"{id:?} {c:?} @{px} clips the line box bottom"
					);
				}
			}
		}
	}

	// The debug panel stacks rows at a fixed pitch; a face whose line box
	// exceeds it would overlap them. This is the tripwire for swapping in
	// a new mono face.
	#[test]
	fn debug_panel_rows_dont_overlap() {
		let pitch = crate::ui::scale::FIXED_GLYPH_PX * 1.25;
		let line = mono().line_h(crate::ui::scale::FIXED_GLYPH_PX);
		assert!(
			line <= pitch,
			"mono line box {line} exceeds the debug panel's {pitch}px pitch"
		);
	}
}
