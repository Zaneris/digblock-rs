//! One typeface: metrics for layout, rasterization for the atlas.
//!
//! Pure CPU — no wgpu, no atlas. Everything a screen needs at layout time
//! (widths, line heights) is here and callable without a GPU.

/// A rasterized glyph: 8-bit coverage plus where the bitmap sits relative
/// to the pen on the baseline, in y-down screen space.
pub struct RasterGlyph {
	pub w: u32,
	pub h: u32,
	/// Pen → bitmap left edge.
	pub left: i32,
	/// Baseline → bitmap top edge (negative for anything above the
	/// baseline, i.e. almost everything).
	pub top: i32,
	/// Row-major coverage, `w * h` bytes.
	pub cov: Vec<u8>,
}

pub struct Face {
	font: fontdue::Font,
	em_ratio: f32,
	ascent_em: f32,
	descent_em: f32,
	line_em: f32,
}

impl Face {
	/// `em_ratio` converts a layout box height into an em size — see the
	/// constants in [`super`] for why it isn't 1.0.
	pub(super) fn load(bytes: &'static [u8], em_ratio: f32) -> Self {
		let font = fontdue::Font::from_bytes(
			bytes,
			fontdue::FontSettings {
				// We never call rasterize_indexed, so the GSUB walk that
				// load_substitutions does is pure startup cost.
				load_substitutions: false,
				..fontdue::FontSettings::default()
			},
		)
		.expect("embedded face is a valid TTF (digblock-assets pins the magic)");
		// Metrics at 1px == metrics per em, so store them normalized once
		// and scale on demand.
		let m = font
			.horizontal_line_metrics(1.0)
			.expect("embedded face has horizontal line metrics");
		Self {
			font,
			em_ratio,
			ascent_em: m.ascent,
			descent_em: m.descent,
			line_em: m.new_line_size,
		}
	}

	/// Em size for a layout box of `px`. Rounded to a whole pixel: it is
	/// the atlas cache key, and rasterizing at the exact size we draw is
	/// what keeps the blit 1:1.
	pub fn em_px(&self, px: f32) -> f32 {
		(px * self.em_ratio).round().max(1.0)
	}

	pub fn size_key(&self, px: f32) -> u32 {
		self.em_px(px) as u32
	}

	/// Does this face have a real glyph for `c`? (Index 0 is `.notdef`.)
	pub fn covers(&self, c: char) -> bool {
		self.font.lookup_glyph_index(c) != 0
	}

	/// The char actually drawn for `c` — `'?'` when the face has no glyph,
	/// so an unexpected codepoint degrades to a visible marker instead of
	/// tofu. `'?'` is in every face we ship (the repertoire test pins it).
	fn resolve(&self, c: char) -> char {
		if self.covers(c) { c } else { '?' }
	}

	pub fn advance(&self, c: char, px: f32) -> f32 {
		self.font
			.metrics(self.resolve(c), self.em_px(px))
			.advance_width
	}

	/// Pen position of each char, then the total. THE advance walk:
	/// `measure`, `wrap` and the overlay's `push_text` all go through it,
	/// so a layout width and the pen the renderer actually walks cannot
	/// disagree. (`measure_matches_the_pen_walk` pins that.)
	pub fn run<'a>(&'a self, text: &'a str, px: f32) -> Run<'a> {
		Run {
			face: self,
			chars: text.chars(),
			em: self.em_px(px),
			pen: 0.0,
		}
	}

	pub fn measure(&self, text: &str, px: f32) -> f32 {
		let mut run = self.run(text, px);
		for _ in &mut run {}
		run.pen()
	}

	/// Positive, in px.
	pub fn ascent(&self, px: f32) -> f32 {
		self.ascent_em * self.em_px(px)
	}

	/// Negative, in px.
	pub fn descent(&self, px: f32) -> f32 {
		self.descent_em * self.em_px(px)
	}

	/// Baseline-to-baseline distance for stacked lines.
	pub fn line_h(&self, px: f32) -> f32 {
		self.line_em * self.em_px(px)
	}

	/// Top of a `px`-tall layout box → baseline, so callers keep passing a
	/// box top and get vertically centered text.
	///
	/// This centers the ascent..descent band in the box rather than
	/// hanging it from the top. Using bare `ascent` instead pushes
	/// descenders `|descent|` below the box, and several surfaces size
	/// their backdrop off the box height — a name tag's `g` would poke out
	/// the bottom of its own backdrop.
	pub fn baseline(&self, px: f32) -> f32 {
		0.5 * (px + self.ascent(px) + self.descent(px))
	}

	pub fn raster(&self, c: char, px: f32) -> RasterGlyph {
		let (m, cov) = self.font.rasterize(self.resolve(c), self.em_px(px));
		RasterGlyph {
			w: m.width as u32,
			h: m.height as u32,
			left: m.xmin,
			// fontdue's ymin is the bitmap's BOTTOM edge measured up from
			// the baseline (y-up). Screen space is y-down from the top
			// edge, hence the negated sum. Pinned by
			// `glyph_bitmaps_sit_on_the_baseline`.
			top: -(m.ymin + m.height as i32),
			cov,
		}
	}
}

pub struct Run<'a> {
	face: &'a Face,
	chars: std::str::Chars<'a>,
	em: f32,
	pen: f32,
}

impl Run<'_> {
	/// Total advance consumed so far — the string width once exhausted.
	pub fn pen(&self) -> f32 {
		self.pen
	}
}

impl Iterator for Run<'_> {
	/// `(glyph, pen x)` — the char is already resolved, so the atlas never
	/// sees one the face can't draw.
	type Item = (char, f32);

	fn next(&mut self) -> Option<Self::Item> {
		let c = self.face.resolve(self.chars.next()?);
		let x = self.pen;
		self.pen += self.face.font.metrics(c, self.em).advance_width;
		Some((c, x))
	}
}
