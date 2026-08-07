//! Screens built on the ui framework (state machine + build/emit +
//! input resolution, all pure). The engine owns when they open and
//! applies the actions they return.

pub mod alchemy;
pub mod inventory;

#[cfg(test)]
mod tests {
	use crate::ui::draw::{Cmd, DrawList, Layer};
	use crate::ui::{scale, widgets::Style, HitMap, InvId, Ui};
	use digblock_core::block::{BlockType as B, Material};
	use digblock_core::inv_grid::InvGrid;
	use digblock_core::inventory::Inventory;

	fn stocked() -> Inventory {
		let mut inv = Inventory::default();
		inv.add(B::Wood, 5);
		inv.add(B::Stone, 11);
		inv.add_sub(Material::Wood, 54);
		inv
	}

	/// Every text size a screen draws at, across every layer.
	fn text_sizes(dl: &DrawList) -> Vec<f32> {
		let mut out = Vec::new();
		for layer in [Layer::Backdrop, Layer::Content, Layer::Ghost] {
			for cmd in dl.cmds(layer).0 {
				if let Cmd::Text { px, .. } = cmd {
					out.push(*px);
				}
			}
		}
		out
	}

	/// One frame of each screen the player can be looking at, drawn at the
	/// same window and scale — which is the only way to compare them, since
	/// "does this page look like that page" is a statement about two draw
	/// lists produced under identical conditions.
	fn each_screen(size: (u32, u32), s: f32) -> Vec<(&'static str, DrawList)> {
		let style = Style::new(s);
		let inv = stocked();
		let mut out = Vec::new();

		let mut dl = DrawList::default();
		let mut hits: HitMap<InvId> = HitMap::default();
		let mut alch = super::alchemy::AlchScreen::default();
		alch.open();
		alch.build(
			&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style,
			},
			size,
			&super::alchemy::AlchView::build(&inv),
			1.0,
		);
		out.push(("alchemy", dl));

		let mut dl = DrawList::default();
		let mut hits: HitMap<InvId> = HitMap::default();
		let mut screen = super::inventory::InvScreen::default();
		screen.open();
		// Filled, not default: an empty grid draws no stack counts, and
		// those are exactly the text this screen once got wrong.
		let mut grid = InvGrid::default();
		grid.file(B::Wood);
		grid.file(B::Stone);
		screen.build(
			&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style,
			},
			size,
			&grid,
			&inv,
			1.0,
		);
		out.push(("inventory", dl));

		let mut dl = DrawList::default();
		let menu = crate::menu::Menu::new(true, Default::default(), None);
		menu.view(size, s, false).emit(&mut dl, &style, size, 1.0);
		out.push(("menu", dl));

		out
	}

	/// A screen is a panel over a dimmed world, and every screen should
	/// present that the same way. The menu dimmed to `0xA0` behind a
	/// `0x90000000` panel while the in-game screens dimmed to `0x70`
	/// behind a `0xC0101010` one — two separate `PANEL_BG` constants that
	/// had quietly drifted apart, which read as the pages having different
	/// transparency when you tabbed between them.
	#[test]
	fn every_screen_dims_and_panels_the_same_way() {
		use crate::ui::theme;
		let size = (1280u32, 800u32);
		let full = crate::ui::Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32);
		for (name, dl) in each_screen(size, 1.0) {
			let quads: Vec<(crate::ui::Rect, u32)> = [Layer::Backdrop, Layer::Content]
				.into_iter()
				.flat_map(|l| dl.cmds(l).0)
				.filter_map(|c| match c {
					Cmd::Quad { rect, color } => Some((*rect, *color)),
					_ => None,
				})
				.collect();
			let dim = quads
				.iter()
				.find(|(rect, _)| *rect == full)
				.unwrap_or_else(|| panic!("{name} drew no full-screen dim"));
			// `daylight` 1.0 leaves `scaled_dim` a no-op, so this is the
			// authored constant.
			assert_eq!(dim.1, theme::SCREEN_DIM, "{name} dim");
			assert!(
				quads
					.iter()
					.any(|(rect, color)| *color == theme::PANEL_BG && *rect != full),
				"{name} drew no panel in the shared color"
			);
		}
	}

	/// The invariant the whole `ui::theme` module exists to hold: a screen
	/// draws all of its text at the frame's one text size. The alchemy
	/// screen broke it twice at once — a 1.35× title, and a shrink-to-fit
	/// that scaled its glyph by an arbitrary fraction — and the result was
	/// immediately visible as "the alchemy page uses different fonts".
	///
	/// The only sanctioned exception in the entire UI is
	/// [`scale::FIXED_GLYPH_PX`], the debug overlay's pinned diagnostic
	/// size, which no screen here draws.
	#[test]
	fn every_screen_draws_text_at_one_size() {
		for size in [(1280u32, 800u32), (1280, 720), (2560, 1600), (960, 600)] {
			// Includes the off-quarter scales a 5%-step interface-scale
			// preference now produces (1.0 × 0.7, 2.0 × 0.85): the frame's
			// size is whatever `glyph_px` gives, not a value off a grid.
			for &s in &[0.7f32, 0.75, 1.0, 1.7, 1.75, 2.5] {
				let want = Style::new(s).glyph;
				for (name, dl) in each_screen(size, s) {
					let sizes = text_sizes(&dl);
					assert!(!sizes.is_empty(), "{name} drew no text at {size:?}/{s}");
					let px = sizes[0];
					assert!(
						sizes.iter().all(|p| *p == px),
						"{name} mixed sizes {sizes:?}"
					);
					assert!(px <= want, "{name} grew its text: {px} > {want}");
					if name == "alchemy" {
						// The one screen that can outgrow its window, so its
						// fit pass may step the whole page down. What it may
						// not do is invent a size: every step is a quarter
						// off the frame's own scale, not a fraction of it,
						// and the last one is the floor it bottoms out on.
						let mut rungs: Vec<f32> = (0..16)
							.map(|k| scale::glyph_px(s - k as f32 * 0.25))
							.collect();
						rungs.push(scale::glyph_px(scale::UI_SCALE_FLOOR));
						assert!(
							rungs.contains(&px),
							"alchemy drew {px}px, not a rung down from {s}: {rungs:?}"
						);
					} else {
						assert_eq!(px, want, "{name} shrank to {px}");
					}
				}
			}
		}
	}
}
