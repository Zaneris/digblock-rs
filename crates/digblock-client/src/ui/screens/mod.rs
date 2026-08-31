//! Screens built on the ui framework (state machine + build/emit +
//! input resolution, all pure). The engine owns when they open and
//! applies the actions they return.

pub mod alchemy;
pub mod character;
pub mod hiscores;
pub mod inventory;
pub mod spellbook;

#[cfg(test)]
mod tests {
	use crate::ui::draw::{Cmd, DrawList, Layer};
	use crate::ui::{HitMap, InvId, Ui, scale, widgets::Style};
	use digblock_core::block::{BlockType as B, Essence};
	use digblock_core::inv_grid::InvGrid;
	use digblock_core::inventory::Inventory;

	/// What the shared hotbar holds, so a bar-slot hover has a name to
	/// show — and not one the grid also holds, or the two hover fixtures
	/// could not be told apart.
	const BAR_ITEM: B = B::Rubble;

	fn stocked() -> Inventory {
		let mut inv = Inventory::default();
		inv.add(B::Wood, 5);
		inv.add(B::Stone, 11);
		inv.add(B::Rubble, 24);
		inv.add_sub(Essence::Vitae, 54);
		inv.add_sub(Essence::Terra, 120);
		inv
	}

	/// Every name a frame put on [`Layer::Ghost`] — a tooltip's, and
	/// nothing else's on a page with no drag running.
	fn ghost_names(dl: &DrawList) -> Vec<String> {
		let (cmds, arena) = dl.cmds(Layer::Ghost);
		cmds.iter()
			.filter_map(|c| match c {
				Cmd::Text { range, .. } => {
					Some(arena[range.0 as usize..range.1 as usize].to_string())
				}
				_ => None,
			})
			.collect()
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
		// The bar holds something: the pages name its slots as well as
		// their own, and an empty bar cannot show that.
		let mut hotbar = digblock_core::hotbar::Hotbar::new();
		hotbar.auto_assign(BAR_ITEM);
		let bar =
			crate::hotbar::HotbarView::build(&hotbar, &inv, &crate::hotbar::ToolSwap::default());
		let mut out = Vec::new();

		// Mid-curve skills, so the alchemy page has real content (counts,
		// costs, an XP bar). Two states of the ONE page: freshly opened
		// (the family row alone), and mid-batch with a family picked.
		let mut skills = digblock_core::skills::SkillSet::default();
		skills.add_xp(digblock_core::skills::Skill::Alchemy, 20_000);

		// Three states of the ONE page: freshly opened, mid-batch with a
		// family picked, and hovering the first family tile — the hover
		// state exists because a tooltip draws NOTHING in the other two,
		// and a sweep that never sees one is not sweeping it.
		for (name, batch, hover) in [
			("alchemy", false, false),
			("alchemy-transmuting", true, false),
			("alchemy-hover", false, true),
		] {
			let mut alch = super::alchemy::AlchScreen::default();
			alch.open();
			if batch {
				alch.debug_batch(B::WoodSlab, 5, 0.4);
			}
			let view = super::alchemy::AlchView::build(&inv, &skills);
			let mut dl = DrawList::default();
			let mut hits: HitMap<InvId> = HitMap::default();
			let build =
				|alch: &super::alchemy::AlchScreen, dl: &mut DrawList, hits: &mut HitMap<InvId>| {
					hits.clear();
					alch.build(&mut Ui { dl, hits, style }, size, &view, &bar, 1.0);
				};
			build(&alch, &mut dl, &mut hits);
			if hover {
				let r = hits.rect_of(InvId::Family(0)).expect("a family tile");
				alch.handle_input(
					&digblock_platform::InputEvent::CursorMoved {
						x: r.x + r.w / 2.0,
						y: r.y + r.h / 2.0,
					},
					&hits,
					&view,
				);
				dl = DrawList::default();
				build(&alch, &mut dl, &mut hits);
			}
			out.push((name, dl));
		}

		// Filled, not default: an empty grid draws no stack counts, and
		// those are exactly the text this screen once got wrong.
		let mut grid = InvGrid::default();
		grid.file(B::Wood);
		grid.file(B::Stone);
		// Two hovers, because the page answers them on two different
		// branches: `over_grid` for a cell of its own, `over_bar` for a
		// slot of the bar it is drawn over. `DebugScreen::InventoryHover`
		// photographs the BAR one, so that is what `inventory-hover` is.
		for (name, hover) in [
			("inventory", None),
			("inventory-hover", Some(InvId::Bar(0))),
			("inventory-grid-hover", Some(InvId::Grid(0))),
		] {
			let mut screen = super::inventory::InvScreen::default();
			screen.open();
			let mut dl = DrawList::default();
			let mut hits: HitMap<InvId> = HitMap::default();
			let build = |screen: &super::inventory::InvScreen,
			             dl: &mut DrawList,
			             hits: &mut HitMap<InvId>| {
				hits.clear();
				screen.build(&mut Ui { dl, hits, style }, size, &grid, &inv, &bar, 1.0);
			};
			build(&screen, &mut dl, &mut hits);
			if let Some(id) = hover {
				// The bar's rects belong to the HUD, which is not drawn
				// here (it draws at the WINDOW's scale and would put a
				// second text size in a fitted page's frame) — so they come
				// from the same two functions the page itself asks.
				let r = match id {
					InvId::Bar(i) => crate::ui::hud::bar_slot_rect(
						&crate::ui::hud::row_grid(size, s),
						i as usize,
						s,
					),
					id => hits.rect_of(id).expect("the hovered slot"),
				};
				screen.handle_input(
					&digblock_platform::InputEvent::CursorMoved {
						x: r.x + r.w / 2.0,
						y: r.y + r.h / 2.0,
					},
					&hits,
					&style,
					&grid,
					&bar,
				);
				dl = DrawList::default();
				build(&screen, &mut dl, &mut hits);
			}
			out.push((name, dl));
		}

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
			&super::character::CharView::build(
				&skills,
				digblock_core::skills::DEPTH_XP_TO,
				&super::character::sample_ranks(&skills),
			),
			&bar,
			1.0,
		);
		out.push(("character", dl));

		// The hiscores page over its sample board: a full page, both ends
		// of the pager, and the own-row wash.
		let mut dl = DrawList::default();
		let mut hits: HitMap<InvId> = HitMap::default();
		let mut board = super::hiscores::BoardModel::default();
		board.debug_fill(super::hiscores::sample_board(None));
		let mut screen = super::hiscores::HiscoresScreen::default();
		screen.open();
		screen.build(
			&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style,
			},
			size,
			&super::hiscores::HiscoresView::build(&board, true),
			&bar,
			1.0,
		);
		out.push(("hiscores", dl));

		// Two states of the spellbook: the page, and the page with the
		// cursor on the first fare chip. It grew hover and tooltips, so it
		// needs the hover fixture every page with a tooltip needs — the
		// sweeps see a tooltip through these and nowhere else.
		let view = spell_view(&inv);
		for (name, hover) in [("spellbook", false), ("spellbook-hover", true)] {
			let mut screen = super::spellbook::SpellScreen::default();
			screen.open();
			let mut dl = DrawList::default();
			let mut hits: HitMap<InvId> = HitMap::default();
			let build = |screen: &super::spellbook::SpellScreen,
			             dl: &mut DrawList,
			             hits: &mut HitMap<InvId>| {
				hits.clear();
				screen.build(&mut Ui { dl, hits, style }, size, &view, &bar, 1.0);
			};
			build(&screen, &mut dl, &mut hits);
			if hover {
				let (slot, _) = view.fares()[0];
				let r = hits
					.rect_of(InvId::SpellFare(slot))
					.expect("the first fare's chip");
				screen.handle_input(
					&digblock_platform::InputEvent::CursorMoved {
						x: r.x + r.w / 2.0,
						y: r.y + r.h / 2.0,
					},
					&hits,
					&view,
				);
				dl = DrawList::default();
				build(&screen, &mut dl, &mut hits);
			}
			out.push((name, dl));
		}

		out
	}

	/// The spellbook fixture's view. One construction, so the sweep and
	/// the assertion about what its hover NAMES read the same rows.
	fn spell_view(inv: &Inventory) -> super::spellbook::SpellView {
		// Past the first rung, so a passed threshold and the next teaser
		// both draw; a live countdown on the home row for the state text.
		let mut skills = digblock_core::skills::SkillSet::default();
		skills.add_xp(
			digblock_core::skills::Skill::Arcana,
			digblock_core::skills::total_xp_for(digblock_core::unlock::TELEPORT_SWIFT_AT + 1),
		);
		super::spellbook::SpellView::build(&skills, 12, &super::spellbook::all_ports(), inv, 0)
	}

	/// The sweep's roster is hand-kept, and what keeps it honest is the capture roster every page
	/// joins anyway: a new `DebugScreen` variant fails to COMPILE here until this map says which
	/// fixture draws it — or says, explicitly, that it is a held-item state and not a page.
	/// Without this, a new screen escapes the one-text-size and dim/panel sweeps below.
	fn fixture_of(d: crate::DebugScreen) -> Option<&'static str> {
		use crate::DebugScreen as D;
		match d {
			D::Menu => Some("menu"),
			D::Inventory => Some("inventory"),
			D::InventoryHover => Some("inventory-hover"),
			D::Alchemy => Some("alchemy"),
			D::AlchemyHover => Some("alchemy-hover"),
			D::Skills => Some("character"),
			D::Hiscores => Some("hiscores"),
			D::Transmute | D::Transmuting => Some("alchemy-transmuting"),
			D::Spellbook => Some("spellbook"),
			D::SpellbookHover => Some("spellbook-hover"),
			// Held-item states, not pages: nothing draws a panel.
			D::Hand | D::Bucket { .. } | D::Tool(_) | D::Torch => None,
		}
	}

	/// The hover fixtures exist so the sweeps SEE a tooltip; this keeps
	/// them hovering something real — the derived name of the tile the
	/// cursor was parked on — and keeps every other fixture silent.
	#[test]
	fn a_hover_fixture_names_what_it_points_at() {
		let mut checked = 0;
		for (name, dl) in each_screen((1280, 800), 1.0) {
			let names = ghost_names(&dl);
			match name {
				"inventory-hover" => {
					assert_eq!(names, vec![BAR_ITEM.name()]);
					checked += 1;
				}
				"inventory-grid-hover" => {
					assert_eq!(names, vec![B::Wood.name()]);
					checked += 1;
				}
				"alchemy-hover" => {
					assert_eq!(names, vec![super::alchemy::families()[0].name]);
					checked += 1;
				}
				"spellbook-hover" => {
					let (_, essence) = spell_view(&stocked()).fares()[0];
					assert_eq!(names, vec![essence.name()]);
					checked += 1;
				}
				_ => assert!(names.is_empty(), "{name} names something unhovered"),
			}
		}
		assert_eq!(checked, 4);
	}

	/// Every page drawn OVER the hotbar names the slot under the pointer, all five of them. The
	/// bar is drawn after whichever page is open, at the window's scale, so a page that stepped
	/// the fit ladder is a rung smaller and a frame has exactly one text size — which is why the
	/// naming is the PAGE's job and not the bar's. Swept rather than spot-checked, because the
	/// page nobody thought of is the one that bites.
	#[test]
	fn every_page_over_the_bar_names_its_slots() {
		let size = (1280u32, 800u32);
		let s = 1.0f32;
		let style = Style::new(s);
		let inv = stocked();
		let mut hotbar = digblock_core::hotbar::Hotbar::new();
		hotbar.auto_assign(BAR_ITEM);
		let bar =
			crate::hotbar::HotbarView::build(&hotbar, &inv, &crate::hotbar::ToolSwap::default());
		// The bar's own geometry, from the two functions every page asks.
		let slot = crate::ui::hud::bar_slot_rect(&crate::ui::hud::row_grid(size, s), 0, s);
		let moved = digblock_platform::InputEvent::CursorMoved {
			x: slot.x + slot.w / 2.0,
			y: slot.y + slot.h / 2.0,
		};
		let mut skills = digblock_core::skills::SkillSet::default();
		skills.add_xp(digblock_core::skills::Skill::Alchemy, 20_000);
		skills.add_xp(
			digblock_core::skills::Skill::Arcana,
			digblock_core::skills::total_xp_for(digblock_core::unlock::TELEPORT_SWIFT_AT + 1),
		);
		let mut named: Vec<(&str, Vec<String>)> = Vec::new();
		let frame = |draw: &mut dyn FnMut(&mut Ui<'_, InvId>)| {
			let mut dl = DrawList::default();
			let mut hits: HitMap<InvId> = HitMap::default();
			draw(&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style,
			});
			ghost_names(&dl)
		};

		let grid = InvGrid::default();
		let mut screen = super::inventory::InvScreen::default();
		screen.open();
		screen.handle_input(&moved, &HitMap::default(), &style, &grid, &bar);
		named.push((
			"inventory",
			frame(&mut |ui| screen.build(ui, size, &grid, &inv, &bar, 1.0)),
		));

		let mut screen = super::alchemy::AlchScreen::default();
		screen.open();
		let view = super::alchemy::AlchView::build(&inv, &skills);
		screen.handle_input(&moved, &HitMap::default(), &view);
		named.push((
			"alchemy",
			frame(&mut |ui| screen.build(ui, size, &view, &bar, 1.0)),
		));

		let mut screen = super::character::CharScreen::default();
		screen.open();
		screen.handle_input(&moved);
		let view =
			super::character::CharView::build(&skills, digblock_core::skills::DEPTH_XP_TO, &[]);
		named.push((
			"character",
			frame(&mut |ui| screen.build(ui, size, &view, &bar, 1.0)),
		));

		let mut screen = super::hiscores::HiscoresScreen::default();
		screen.open();
		screen.handle_input(&moved, &HitMap::default());
		let mut board = super::hiscores::BoardModel::default();
		board.debug_fill(super::hiscores::sample_board(None));
		let view = super::hiscores::HiscoresView::build(&board, true);
		named.push((
			"hiscores",
			frame(&mut |ui| screen.build(ui, size, &view, &bar, 1.0)),
		));

		let mut screen = super::spellbook::SpellScreen::default();
		screen.open();
		let view = spell_view(&inv);
		screen.handle_input(&moved, &HitMap::default(), &view);
		named.push((
			"spellbook",
			frame(&mut |ui| screen.build(ui, size, &view, &bar, 1.0)),
		));

		assert_eq!(named.len(), 5, "every page over the bar is listed");
		for (page, names) in named {
			assert_eq!(
				names,
				vec![BAR_ITEM.name()],
				"{page} does not name the bar slot under the cursor"
			);
		}
	}

	/// The two surfaces that tease the Arcana ladder read the SAME unlock
	/// tables and must not read the same prose: the spellbook says what
	/// casting gains, the character screen names the rung. Three rungs
	/// were once authored word for word on both — the one-place rule
	/// broken in the only place a screen's own labels can break it.
	#[test]
	fn the_two_arcana_teaser_surfaces_share_no_prose() {
		use digblock_core::skills::Skill;
		let page = super::character::teasers(Skill::Arcana);
		// Walk the spellbook's ladder by asking it from each rung it
		// names — the only way to enumerate a "next above" table.
		let mut book = Vec::new();
		let mut level = 0;
		while let Some((lvl, what)) = super::spellbook::next_rung(level) {
			book.push((lvl, what));
			level = lvl;
		}
		assert!(!book.is_empty(), "the spellbook teases nothing");
		for &(lvl, what) in &book {
			// Same tables: every rung the book teases is a rung the
			// character screen lists.
			assert!(
				page.iter().any(|&(l, _)| l == lvl),
				"LV {lvl} is teased only by the spellbook"
			);
			assert!(
				!page.iter().any(|&(_, w)| w == what),
				"LV {lvl} is teased in the same words on both surfaces: {what:?}"
			);
		}
	}

	#[test]
	fn the_sweep_covers_every_page_the_capture_roster_names() {
		use crate::DebugScreen as D;
		let drawn: Vec<&str> = each_screen((800, 600), 1.0)
			.iter()
			.map(|(n, _)| *n)
			.collect();
		for d in [
			D::Menu,
			D::Inventory,
			D::Alchemy,
			D::Skills,
			D::Hiscores,
			D::Transmute,
			D::Transmuting,
			D::Spellbook,
		] {
			let name = fixture_of(d).expect("a page maps to a fixture");
			assert!(drawn.contains(&name), "{name} missing from the sweep");
		}
	}

	/// A screen is a panel over a dimmed world, and every screen should present that the same
	/// way. The menu laid a heavier dim behind a lighter panel while the in-game screens did the
	/// opposite — two separate `PANEL_BG` constants that had drifted apart, which read as the
	/// pages having different transparency when you tabbed between them.
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

	/// The invariant the whole `ui::theme` module exists to hold: a screen draws all of its text
	/// at the frame's one text size. The alchemy screen broke it twice at once — an enlarged
	/// title, and a shrink-to-fit that scaled its glyph by an arbitrary fraction — and read as
	/// "the alchemy page uses different fonts". The only sanctioned exception in the entire UI is
	/// [`scale::FIXED_GLYPH_PX`], the debug overlay's pinned size, which no screen here draws.
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
					if name.starts_with("alchemy") || name.starts_with("inventory") {
						// The screens that can outgrow their window, so a fit pass may
						// step the whole page down. What a fit pass may not do is invent
						// a size: every step is one `theme::fit_scale` rung off the
						// frame's own scale, not a fraction of it, and the last one is
						// `theme::FIT_FLOOR`.
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
