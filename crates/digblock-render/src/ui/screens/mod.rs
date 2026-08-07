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

	/// The invariant the whole `ui::theme` module exists to hold: a screen
	/// draws all of its text at the frame's one text size. The alchemy
	/// screen broke it twice at once — a 1.35× title, and a shrink-to-fit
	/// that scaled its glyph by an arbitrary fraction — and the result was
	/// immediately visible as "the alchemy page uses different fonts".
	///
	/// The only sanctioned exception in the entire UI is
	/// [`scale::FIXED_GLYPH_PX`] (hotbar stack counts and the debug
	/// overlay), which no screen here draws.
	#[test]
	fn every_screen_draws_text_at_one_size() {
		for size in [(1280u32, 800u32), (1280, 720), (2560, 1600), (960, 600)] {
			for &s in &[0.75f32, 1.0, 1.75, 2.5] {
				let style = Style::new(s);
				let inv = stocked();

				// Alchemy.
				let mut dl = DrawList::default();
				let mut hits: HitMap<InvId> = HitMap::default();
				let mut screen = super::alchemy::AlchScreen::default();
				screen.open();
				screen.build(
					&mut Ui {
						dl: &mut dl,
						hits: &mut hits,
						style,
					},
					size,
					&super::alchemy::AlchView::build(&inv),
					1.0,
				);
				let sizes = text_sizes(&dl);
				assert!(!sizes.is_empty(), "alchemy drew no text");
				// The fit pass may step the whole screen down a rung; what
				// it may never do is draw two sizes in one frame, or a size
				// off the ladder `glyph_px` produces.
				let px = sizes[0];
				assert!(sizes.iter().all(|p| *p == px), "alchemy mixed {sizes:?}");
				assert!(px <= style.glyph, "alchemy grew its text: {px}");
				assert!(
					(3..=12).any(|q| scale::glyph_px(q as f32 / 4.0) == px),
					"alchemy drew {px}px, off the glyph_px ladder"
				);

				// Inventory.
				let mut dl = DrawList::default();
				let mut hits: HitMap<InvId> = HitMap::default();
				let mut screen = super::inventory::InvScreen::default();
				screen.open();
				screen.build(
					&mut Ui {
						dl: &mut dl,
						hits: &mut hits,
						style,
					},
					size,
					&InvGrid::default(),
					&inv,
					1.0,
				);
				let sizes = text_sizes(&dl);
				assert!(!sizes.is_empty(), "inventory drew no text");
				assert!(
					sizes.iter().all(|p| *p == style.glyph),
					"inventory drew {sizes:?}, want {}",
					style.glyph
				);

				// The menu, for the same frame — the page the others are
				// compared against by eye.
				let mut dl = DrawList::default();
				let menu = crate::menu::Menu::new(true, Default::default(), None);
				menu.view(size, s, false).emit(&mut dl, &style, size, 1.0);
				let sizes = text_sizes(&dl);
				assert!(!sizes.is_empty(), "menu drew no text");
				assert!(
					sizes.iter().all(|p| *p == style.glyph),
					"menu drew {sizes:?}, want {}",
					style.glyph
				);
			}
		}
	}
}
