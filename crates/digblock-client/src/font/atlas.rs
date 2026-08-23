//! Glyph cache: rasterize on first sight, shelf-pack into one R8 page,
//! hand the dirty rows to whoever owns the GPU texture.
//!
//! wgpu-free on purpose — the packing, the padding invariant and the
//! reserved white block are all testable headless.

use super::{Face, FaceId, RasterGlyph};
use rustc_hash::FxHashMap;

pub(crate) const ATLAS_W: u32 = 2048;
pub(crate) const ATLAS_H: u32 = 2048;

/// Side of the all-white block reserved at the origin for untextured
/// quads. Several texels, not one: the overlay samples it with a Linear
/// sampler, and a lone white texel would let a half-texel-off tap make
/// every panel backdrop and the crosshair partly transparent.
const SOLID: u32 = 4;
/// Transparent ring around every packed glyph, so a boundary-exact
/// bilinear tap reads empty space instead of the neighboring glyph.
const PAD: u32 = 1;

/// Where a glyph lives in the page and where it sits relative to the pen.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glyph {
	pub uv0: [f32; 2],
	pub uv1: [f32; 2],
	/// Bitmap size in texels — also its size on screen. A cached glyph is
	/// never stretched.
	pub w: f32,
	pub h: f32,
	/// Pen-on-baseline → quad top-left, y-down.
	pub left: f32,
	pub top: f32,
}

pub(crate) struct GlyphAtlas {
	px: Vec<u8>,
	/// `None` = the glyph has no ink (space) or didn't fit.
	map: FxHashMap<(FaceId, u32, char), Option<Glyph>>,
	shelf_y: u32,
	shelf_h: u32,
	shelf_x: u32,
	dirty: Option<(u32, u32)>,
	live: Vec<(FaceId, u32)>,
	overflow: bool,
}

impl Default for GlyphAtlas {
	fn default() -> Self {
		Self::new()
	}
}

impl GlyphAtlas {
	pub(crate) fn new() -> Self {
		let mut a = Self {
			px: vec![0; (ATLAS_W * ATLAS_H) as usize],
			map: FxHashMap::default(),
			shelf_y: 0,
			shelf_h: 0,
			shelf_x: 0,
			dirty: None,
			live: Vec::new(),
			overflow: false,
		};
		a.reset();
		a
	}

	/// Center of the reserved white block — the UV every untextured quad
	/// samples.
	pub(crate) fn solid_uv(&self) -> [f32; 2] {
		let h = SOLID as f32 * 0.5;
		[h / ATLAS_W as f32, h / ATLAS_H as f32]
	}

	/// Start a frame. Wipes the page when the set of (face, size) pairs in use changed — i.e. the
	/// UI scale moved — or when the previous frame ran out of room. This is the ONLY place a reset
	/// may happen: resetting mid-frame would invalidate UVs already written into the vertex buffer
	/// and flash one frame of scrambled text.
	pub(crate) fn begin_frame(&mut self, live: &[(FaceId, u32)]) {
		if !self.overflow && self.live == live {
			return;
		}
		self.reset();
		self.live.clear();
		self.live.extend_from_slice(live);
	}

	fn reset(&mut self) {
		self.px.fill(0);
		self.map.clear();
		self.overflow = false;
		for y in 0..SOLID {
			let row = (y * ATLAS_W) as usize;
			self.px[row..row + SOLID as usize].fill(255);
		}
		// First shelf clears the white block and its pad ring.
		self.shelf_y = SOLID + PAD;
		self.shelf_h = 0;
		self.shelf_x = PAD;
		self.dirty = Some((0, SOLID + PAD));
	}

	/// Cached glyph, rasterizing and packing on first sight. `None` when
	/// the glyph has no ink, or when the page is full (in which case the
	/// next [`begin_frame`](Self::begin_frame) wipes and retries).
	pub(crate) fn glyph(&mut self, id: FaceId, face: &Face, c: char, px: f32) -> Option<Glyph> {
		let key = (id, face.size_key(px), c);
		if let Some(g) = self.map.get(&key) {
			return *g;
		}
		let r = face.raster(c, px);
		let g = if r.w == 0 || r.h == 0 {
			None
		} else {
			self.pack(&r)
		};
		self.map.insert(key, g);
		g
	}

	fn pack(&mut self, r: &RasterGlyph) -> Option<Glyph> {
		let (w, h) = (r.w, r.h);
		if w + PAD * 2 > ATLAS_W {
			self.overflow = true;
			return None;
		}
		if self.shelf_x + w + PAD > ATLAS_W {
			self.shelf_y += self.shelf_h + PAD;
			self.shelf_h = 0;
			self.shelf_x = PAD;
		}
		if self.shelf_y + h + PAD > ATLAS_H {
			self.overflow = true;
			return None;
		}
		let (x0, y0) = (self.shelf_x, self.shelf_y);
		for y in 0..h {
			let dst = ((y0 + y) * ATLAS_W + x0) as usize;
			let src = (y * w) as usize;
			self.px[dst..dst + w as usize].copy_from_slice(&r.cov[src..src + w as usize]);
		}
		self.shelf_x += w + PAD;
		self.shelf_h = self.shelf_h.max(h);
		self.mark_dirty(y0, y0 + h);
		Some(Glyph {
			uv0: [x0 as f32 / ATLAS_W as f32, y0 as f32 / ATLAS_H as f32],
			uv1: [
				(x0 + w) as f32 / ATLAS_W as f32,
				(y0 + h) as f32 / ATLAS_H as f32,
			],
			w: w as f32,
			h: h as f32,
			left: r.left as f32,
			top: r.top as f32,
		})
	}

	fn mark_dirty(&mut self, y0: u32, y1: u32) {
		self.dirty = Some(match self.dirty {
			Some((a, b)) => (a.min(y0), b.max(y1)),
			None => (y0, y1),
		});
	}

	/// Rows written since the last call, as `(y0, y1, full-width pixels)`.
	pub(crate) fn take_dirty(&mut self) -> Option<(u32, u32, &[u8])> {
		let (y0, y1) = self.dirty.take()?;
		let (a, b) = ((y0 * ATLAS_W) as usize, (y1 * ATLAS_W) as usize);
		Some((y0, y1, &self.px[a..b]))
	}

	#[cfg(test)]
	fn at(&self, x: u32, y: u32) -> u8 {
		self.px[(y * ATLAS_W + x) as usize]
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::font;

	fn interned(sizes: &[f32]) -> GlyphAtlas {
		let mut a = GlyphAtlas::new();
		for &px in sizes {
			for c in font::repertoire() {
				a.glyph(FaceId::Ui, font::ui(), c, px);
			}
			for c in ' '..='~' {
				a.glyph(FaceId::Mono, font::mono(), c, px);
			}
		}
		a
	}

	// Every untextured overlay quad in the game samples solid_uv(). Under
	// a Linear sampler a lone white texel would bleed transparency in from
	// its neighbors and wash out every panel backdrop, the crosshair and
	// every slider — so the whole tap footprint must be opaque.
	#[test]
	fn solid_block_is_opaque_across_the_filter_footprint() {
		let a = GlyphAtlas::new();
		let uv = a.solid_uv();
		let (cx, cy) = (
			(uv[0] * ATLAS_W as f32) as u32,
			(uv[1] * ATLAS_H as f32) as u32,
		);
		for y in cy - 1..=cy + 1 {
			for x in cx - 1..=cx + 1 {
				assert_eq!(a.at(x, y), 255, "solid neighborhood at {x},{y}");
			}
		}
	}

	// The white-line-above-'o' regression from the bitmap font, restated
	// for a packed atlas: a fragment landing exactly on a glyph boundary
	// must read empty space, never the glyph next door.
	#[test]
	fn packed_glyphs_are_ringed_by_transparent_padding() {
		let a = interned(&[16.0, 24.0]);
		let glyphs: Vec<Glyph> = a.map.values().flatten().copied().collect();
		assert!(glyphs.len() > 300, "expected a full page of glyphs");
		for g in glyphs {
			let x0 = (g.uv0[0] * ATLAS_W as f32).round() as u32;
			let y0 = (g.uv0[1] * ATLAS_H as f32).round() as u32;
			let (w, h) = (g.w as u32, g.h as u32);
			// UVs span exactly the bitmap — 1:1 at draw time, no resample.
			assert!(((g.uv1[0] - g.uv0[0]) * ATLAS_W as f32 - g.w).abs() < 1e-3);
			assert!(((g.uv1[1] - g.uv0[1]) * ATLAS_H as f32 - g.h).abs() < 1e-3);
			for y in y0..y0 + h {
				assert_eq!(a.at(x0 - 1, y), 0, "left ring");
				assert_eq!(a.at(x0 + w, y), 0, "right ring");
			}
			for x in x0..x0 + w {
				assert_eq!(a.at(x, y0 - 1), 0, "top ring");
				assert_eq!(a.at(x, y0 + h), 0, "bottom ring");
			}
		}
	}

	// The page has to hold the worst case the UI can actually ask for:
	// both faces at the largest glyph size any legal scale produces, with
	// the whole repertoire interned. Overflow is handled gracefully, but
	// it would mean text silently dropping out at high UI scale.
	#[test]
	fn worst_case_size_set_fits_the_page() {
		let max = crate::ui::scale::glyph_px(crate::ui::scale::UI_SCALE_CAP);
		let mut a = interned(&[max, crate::ui::scale::FIXED_GLYPH_PX]);
		assert!(!a.overflow, "atlas overflowed at glyph_px {max}");
		// And the rows actually consumed leave headroom.
		assert!(a.shelf_y + a.shelf_h < ATLAS_H, "no headroom left");
		let _ = a.take_dirty();
	}

	#[test]
	fn resets_only_when_the_live_set_changes() {
		let mut a = GlyphAtlas::new();
		let ui16 = [(FaceId::Ui, font::ui().size_key(16.0))];
		a.begin_frame(&ui16);
		a.glyph(FaceId::Ui, font::ui(), 'A', 16.0);
		let before = a.map.len();
		a.begin_frame(&ui16);
		assert_eq!(a.map.len(), before, "same live set must not wipe the cache");
		a.begin_frame(&[(FaceId::Ui, font::ui().size_key(24.0))]);
		assert_eq!(a.map.len(), 0, "a scale change wipes the page");
	}

	#[test]
	fn dirty_rows_cover_every_written_glyph() {
		let mut a = GlyphAtlas::new();
		a.take_dirty();
		let g = a.glyph(FaceId::Ui, font::ui(), 'g', 32.0).unwrap();
		let (y0, y1, _) = a.take_dirty().expect("a new glyph dirties rows");
		let gy = (g.uv0[1] * ATLAS_H as f32).round() as u32;
		assert!(y0 <= gy && gy + g.h as u32 <= y1);
		assert!(a.take_dirty().is_none(), "dirty range drains");
	}
}
