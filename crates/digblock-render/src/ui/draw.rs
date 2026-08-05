//! Retained-per-frame draw commands. Screens emit into a [`DrawList`];
//! two GPU consumers execute it: the overlay (quads + text) and
//! [`super::icons::UiIcons`] (block-texture icons). Commands are
//! bucketed by [`Layer`] — no sorting, emission order within a layer is
//! draw order.

use super::Rect;

/// Draw order: Backdrop < Icons < Content < Ghost. Quads/text are legal
/// in Backdrop/Content/Ghost; icons in Icons/Ghost (the ghost stage
/// draws a held icon above everything, including Content text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
	Backdrop = 0,
	Icons = 1,
	Content = 2,
	Ghost = 3,
}

pub enum Cmd {
	/// Solid quad, 0xAABBGGRR.
	Quad { rect: Rect, color: u32 },
	/// Square glyphs of side `px`; `range` indexes the shared text arena.
	Text {
		x: f32,
		y: f32,
		px: f32,
		color: u32,
		range: (u32, u32),
	},
	/// Block-material icon: texture-array layer + 0xAABBGGRR tint.
	Icon {
		rect: Rect,
		tex_layer: u16,
		tint: u32,
	},
	/// One projected face of a 3D item icon: four screen-px corners
	/// (TL,TR,BL,BR — `ShapeQuad` slot order) with per-corner UVs into
	/// the texture layer. Emitted far-to-near by `ui::shaped_icon`.
	IconQuad {
		corners: [[f32; 2]; 4],
		uvs: [[f32; 2]; 4],
		tex_layer: u16,
		tint: u32,
	},
}

#[derive(Default)]
pub struct DrawList {
	cmds: [Vec<Cmd>; 4],
	/// Shared arena for Text command payloads (no per-command alloc).
	text: String,
}

impl DrawList {
	pub fn clear(&mut self) {
		for bucket in &mut self.cmds {
			bucket.clear();
		}
		self.text.clear();
	}

	pub fn quad(&mut self, layer: Layer, rect: Rect, color: u32) {
		debug_assert!(
			layer != Layer::Icons,
			"quads don't belong in the icon layer"
		);
		self.cmds[layer as usize].push(Cmd::Quad { rect, color });
	}

	pub fn text(&mut self, layer: Layer, x: f32, y: f32, px: f32, color: u32, s: &str) {
		debug_assert!(
			layer != Layer::Icons,
			"text doesn't belong in the icon layer"
		);
		let start = self.text.len() as u32;
		self.text.push_str(s);
		self.cmds[layer as usize].push(Cmd::Text {
			x,
			y,
			px,
			color,
			range: (start, self.text.len() as u32),
		});
	}

	/// `write!` straight into the arena — zero-alloc formatted text
	/// (stack counts, live values).
	pub fn text_fmt(
		&mut self,
		layer: Layer,
		x: f32,
		y: f32,
		px: f32,
		color: u32,
		args: std::fmt::Arguments<'_>,
	) {
		debug_assert!(
			layer != Layer::Icons,
			"text doesn't belong in the icon layer"
		);
		use std::fmt::Write as _;
		let start = self.text.len() as u32;
		let _ = self.text.write_fmt(args);
		self.cmds[layer as usize].push(Cmd::Text {
			x,
			y,
			px,
			color,
			range: (start, self.text.len() as u32),
		});
	}

	pub fn icon(&mut self, layer: Layer, rect: Rect, tex_layer: u16, tint: u32) {
		debug_assert!(
			matches!(layer, Layer::Icons | Layer::Ghost),
			"icons draw in the Icons or Ghost layer"
		);
		self.cmds[layer as usize].push(Cmd::Icon {
			rect,
			tex_layer,
			tint,
		});
	}

	pub fn icon_quad(
		&mut self,
		layer: Layer,
		corners: [[f32; 2]; 4],
		uvs: [[f32; 2]; 4],
		tex_layer: u16,
		tint: u32,
	) {
		debug_assert!(
			matches!(layer, Layer::Icons | Layer::Ghost),
			"icons draw in the Icons or Ghost layer"
		);
		self.cmds[layer as usize].push(Cmd::IconQuad {
			corners,
			uvs,
			tex_layer,
			tint,
		});
	}

	/// A layer's commands plus the text arena its ranges index.
	pub fn cmds(&self, layer: Layer) -> (&[Cmd], &str) {
		(&self.cmds[layer as usize], &self.text)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn text_arena_ranges_stay_valid_across_pushes() {
		let mut dl = DrawList::default();
		dl.text(Layer::Content, 0.0, 0.0, 16.0, 0xFFFFFFFF, "hello");
		dl.text_fmt(
			Layer::Content,
			0.0,
			20.0,
			16.0,
			0xFFFFFFFF,
			format_args!("{}+{}", 1, 2),
		);
		let (cmds, arena) = dl.cmds(Layer::Content);
		let texts: Vec<&str> = cmds
			.iter()
			.map(|c| match c {
				Cmd::Text { range, .. } => &arena[range.0 as usize..range.1 as usize],
				_ => panic!("expected text"),
			})
			.collect();
		assert_eq!(texts, ["hello", "1+2"]);
	}

	#[test]
	fn buckets_keep_layers_separate_and_clear_resets() {
		let mut dl = DrawList::default();
		let r = Rect::new(0.0, 0.0, 10.0, 10.0);
		dl.quad(Layer::Backdrop, r, 1);
		dl.icon(Layer::Icons, r, 7, 2);
		dl.quad(Layer::Content, r, 3);
		dl.icon(Layer::Ghost, r, 8, 4);
		assert_eq!(dl.cmds(Layer::Backdrop).0.len(), 1);
		assert_eq!(dl.cmds(Layer::Icons).0.len(), 1);
		assert_eq!(dl.cmds(Layer::Content).0.len(), 1);
		assert_eq!(dl.cmds(Layer::Ghost).0.len(), 1);
		dl.clear();
		for l in [Layer::Backdrop, Layer::Icons, Layer::Content, Layer::Ghost] {
			assert!(dl.cmds(l).0.is_empty());
		}
		assert!(dl.cmds(Layer::Content).1.is_empty());
	}
}
