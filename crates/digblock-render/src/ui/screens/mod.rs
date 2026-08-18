//! Screens built on the ui framework (state machine + build/emit +
//! input resolution, all pure). The engine owns when they open and
//! applies the actions they return.

pub mod alchemy;
pub mod character;
pub mod inventory;

#[cfg(test)]
mod tests {
	use crate::ui::draw::{Cmd, DrawList, Layer};
	use crate::ui::{HitMap, InvId, Ui, scale, widgets::Style};
	use digblock_core::block::{BlockType as B, Essence};
	use digblock_core::inv_grid::InvGrid;
	use digblock_core::inventory::Inventory;

	fn stocked() -> Inventory {
		let mut inv = Inventory::default();
		inv.add(B::Wood, 5);
		inv.add(B::Stone, 11);
		inv.add(B::Rubble, 24);
		inv.add_sub(Essence::Flora, 54);
		inv.add_sub(Essence::Earth, 120);
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

		// Mid-curve skills, so the alchemy page has real content (counts,
		// costs, an XP bar). Two states of the ONE page: freshly opened
		// (the family row alone), and mid-batch with a family picked.
		let mut skills = digblock_core::skills::SkillSet::default();
		skills.add_xp(digblock_core::skills::Skill::Alchemy, 20_000);

		for (name, batch) in [("alchemy", false), ("alchemy-transmuting", true)] {
			let mut dl = DrawList::default();
			let mut hits: HitMap<InvId> = HitMap::default();
			let mut alch = super::alchemy::AlchScreen::default();
			alch.open();
			if batch {
				alch.debug_batch(B::WoodSlab, 5, 0.4);
			}
			alch.build(
				&mut Ui {
					dl: &mut dl,
					hits: &mut hits,
					style,
				},
				size,
				&super::alchemy::AlchView::build(&inv, &skills),
				1.0,
			);
			out.push((name, dl));
		}

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
		let menu = crate::menu::Menu::new(true, Default::default(), None, None);
		menu.view(size, s, false).emit(&mut dl, &style, size, 1.0);
		out.push(("menu", dl));

		// The account-flow pages. They are worth listing individually
		// rather than trusting the root menu to stand for all of them:
		// each carries text the others do not (a full account id, a
		// status line, an availability line), and it is exactly that kind
		// of one-off row that has broken the one-text-size rule before.
		for (name, page) in [
			("menu-signin", crate::menu::TestPage::SignIn),
			("menu-name", crate::menu::TestPage::ChooseName),
			("menu-account", crate::menu::TestPage::Account),
		] {
			let mut dl = DrawList::default();
			let mut menu = crate::menu::Menu::new(true, Default::default(), None, None);
			menu.test_open(page);
			menu.view(size, s, false).emit(&mut dl, &style, size, 1.0);
			out.push((name, dl));
		}

		let mut dl = DrawList::default();
		let mut hits: HitMap<InvId> = HitMap::default();
		// Mid-curve XP, not zero: fractional bars and the XP detail
		// lines are exactly the text this screen could get wrong.
		let mut skills = digblock_core::skills::SkillSet::default();
		skills.add_xp(digblock_core::skills::Skill::Mining, 150_000);
		skills.add_xp(digblock_core::skills::Skill::Alchemy, 30_000);
		let mut screen = super::character::CharScreen::default();
		screen.open();
		screen.build(
			&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style,
			},
			size,
			// Deep, so the sweep sees the depth-reward readout in its
			// live form rather than its "none yet" placeholder.
			&super::character::CharView::build(&skills, digblock_core::skills::DEPTH_XP_TO),
			1.0,
		);
		out.push(("character", dl));

		out
	}

	/// A screen is a panel over a dimmed world, and every screen should
	/// present that the same way. The menu laid a heavier dim behind a
	/// lighter panel while the in-game screens did the opposite — two
	/// separate `PANEL_BG` constants that had quietly drifted apart, which
	/// read as the pages having different transparency when you tabbed
	/// between them.
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
			// Full `daylight` leaves `scaled_dim` a no-op, so this is the
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
	/// screen broke it twice at once — an enlarged title, and a
	/// shrink-to-fit that scaled its glyph by an arbitrary fraction — and
	/// the result was immediately visible as "the alchemy page uses
	/// different fonts".
	///
	/// The only sanctioned exception in the entire UI is
	/// [`scale::FIXED_GLYPH_PX`], the debug overlay's pinned diagnostic
	/// size, which no screen here draws.
	#[test]
	fn every_screen_draws_text_at_one_size() {
		use crate::ui::theme;
		for size in [(1280u32, 800u32), (1280, 720), (2560, 1600), (960, 600)] {
			// Includes the off-grid scales a fine-stepped interface-scale
			// preference now produces: the frame's size is whatever
			// `glyph_px` gives, not a value off a grid.
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
					if name.starts_with("alchemy") || name == "inventory" {
						// The screens that can outgrow their window, so a
						// fit pass may step the whole page down. Both draw
						// the same inventory grid — the inventory screen
						// with a title row on top, which makes it the
						// TALLER of the two, and it went without a ladder
						// long enough to hang its first row off the top of
						// an ordinary window.
						//
						// What a fit pass may not do is invent a size:
						// every step is one `theme::fit_scale` rung off the
						// frame's own scale, not a fraction of it, and the
						// last one is the floor it bottoms out on —
						// `theme::FIT_FLOOR`, which sits BELOW the
						// interface scale's own floor because the
						// alternative to a cramped page is an unreachable
						// one.
						let mut rungs: Vec<f32> = (0..16)
							.map(|k| scale::glyph_px(s - k as f32 * 0.25))
							.collect();
						rungs.push(scale::glyph_px(theme::FIT_FLOOR));
						assert!(
							rungs.contains(&px),
							"{name} drew {px}px, not a rung down from {s}: {rungs:?}"
						);
					} else {
						assert_eq!(px, want, "{name} shrank to {px}");
					}
				}
			}
		}
	}
}
