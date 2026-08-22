use super::*;
use crate::ui::draw::{Cmd, DrawList};
use digblock_core::block::BlockType as B;
use digblock_core::hotbar::Hotbar;

const SIZE: (u32, u32) = (1280, 800);

/// Every window × interface scale the layout assertions sweep. The
/// short ones are the point: this panel's height rides the scale, and
/// it is the tall windows that never had a problem.
const WINDOWS: [(u32, u32); 5] = [
	(1280, 800),
	(1280, 720),
	(1600, 900),
	(960, 600),
	(800, 450),
];
const SCALES: [f32; 7] = [0.75, 1.0, 1.5, 1.75, 2.25, 2.5, 3.0];

struct Rig {
	screen: InvScreen,
	hits: HitMap<InvId>,
	grid: InvGrid,
	bar: HotbarView,
	style: Style,
}

/// A bar with nothing on it, for the rigs that are not about the bar.
fn empty_bar() -> HotbarView {
	HotbarView::build(
		&Hotbar::new(),
		&Inventory::default(),
		&crate::hotbar::ToolSwap::default(),
	)
}

fn rig() -> Rig {
	let mut hotbar = Hotbar::new();
	hotbar.auto_assign(B::Dirt); // bar slot 0
	let mut inv = Inventory::default();
	inv.add(B::Dirt, 5);
	inv.add(B::Stone, 2);
	let mut grid = InvGrid::default();
	grid.place(1, B::Stone); // parked in grid slot 1
	let bar = HotbarView::build(&hotbar, &inv, &crate::hotbar::ToolSwap::default());
	let style = Style::new(1.0);

	// Build the frame exactly as the engine does: screen first,
	// then the HUD bar — one hit map serves both.
	let mut screen = InvScreen::default();
	screen.open();
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style,
	};
	screen.build(&mut ui, SIZE, &grid, &inv, &bar, 1.0);
	crate::ui::hud::hotbar(&mut ui, SIZE, &bar, |i| InvId::Bar(i as u8));
	Rig {
		screen,
		hits,
		grid,
		bar,
		style,
	}
}

fn center(r: Rect) -> (f32, f32) {
	(r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// One frame of the screen ALONE (the HUD bar is a separate call),
/// plus the style the caller is left holding — the bar draws after
/// this screen and has to get the frame's own scale back, whatever
/// the fit pass did with it.
fn emit_at(size: (u32, u32), scale: f32) -> (DrawList, HitMap<InvId>, Style) {
	let mut inv = Inventory::default();
	inv.add(B::Dirt, 5);
	let mut grid = InvGrid::default();
	grid.place(0, B::Dirt);
	let mut screen = InvScreen::default();
	screen.open();
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: Style::new(scale),
	};
	screen.build(&mut ui, size, &grid, &inv, &empty_bar(), 1.0);
	let left_holding = ui.style;
	(dl, hits, left_holding)
}

fn panel_rect(dl: &DrawList) -> Rect {
	dl.cmds(Layer::Backdrop)
		.0
		.iter()
		.find_map(|c| match c {
			Cmd::Quad { rect, color } if *color == theme::PANEL_BG => Some(*rect),
			_ => None,
		})
		.expect("the screen draws a panel in the shared color")
}

/// The scale the page actually drew at, read back off a slot — the
/// fit pass is invisible from outside otherwise, and a rung is a
/// statement about the WHOLE style, not just its text.
fn drawn_scale(hits: &HitMap<InvId>) -> f32 {
	hits.rect_of(InvId::Grid(0)).unwrap().w / hud::SLOT_PX
}

fn glyph_of(dl: &DrawList) -> f32 {
	let (cmds, _) = dl.cmds(Layer::Content);
	cmds.iter()
		.find_map(|c| match c {
			Cmd::Text { px, .. } => Some(*px),
			_ => None,
		})
		.expect("the screen draws its title")
}

/// The slot rows under a title band make a page whose height rides the
/// scale, and this screen positioned itself by pure subtraction from
/// the bar without ever checking that the result was still on the
/// window. When the grid gained its extra rows it simply ran off the
/// TOP: the panel's top edge crossed the top of the window, further up
/// the scale the first row of slots went with it and could not be
/// clicked at all, and on the smallest window at the largest scale
/// most of the page was gone.
///
/// Nothing it draws may leave the window, and the page must still
/// stand ABOVE the bar rather than under it, at every window and
/// scale the game can produce.
#[test]
fn the_panel_always_fits_between_the_bar_and_the_top_of_the_window() {
	for size in WINDOWS {
		for scale in SCALES {
			let (dl, hits, left_holding) = emit_at(size, scale);
			let (w, h) = (size.0 as f32, size.1 as f32);
			let panel = panel_rect(&dl);
			let at = format!("{size:?} at {scale}x");
			assert!(panel.y >= 0.0, "{at}: panel top edge at {}", panel.y);
			assert!(panel.x >= 0.0, "{at}: panel left edge at {}", panel.x);
			assert!(
				panel.x + panel.w <= w + 0.5,
				"{at}: panel right edge at {}",
				panel.x + panel.w
			);
			assert!(
				panel.y + panel.h <= hud::row_grid(size, scale).origin.1 + 0.5,
				"{at}: the panel runs under the hotbar"
			);
			// Both ends of the grid are inside the window, so every
			// cell between them is a drop target the cursor can reach.
			for i in [0, GRID_SLOTS - 1] {
				let cell = hits.rect_of(InvId::Grid(i as u16)).unwrap();
				assert!(
					cell.x >= 0.0
						&& cell.y >= 0.0 && cell.x + cell.w <= w + 0.5
						&& cell.y + cell.h <= h + 0.5,
					"{at}: grid cell {i} at {cell:?} is off the window"
				);
			}
			assert_eq!(
				left_holding.scale, scale,
				"{at}: the frame's own style has to come back for the bar"
			);
		}
	}
}

/// The fit pass is a LADDER: the page draws either at the frame's own
/// scale or a whole rung down from it, glyph and boxes moving
/// together. The obvious implementation — multiply the style by
/// `budget / needed` — is what `ui::theme` exists to prevent, because
/// it lands the text on some fraction of the size the menu beside it
/// draws at. And a page with the room to spare must not step at all.
#[test]
fn the_page_steps_in_whole_rungs_and_only_when_it_has_to() {
	// Room to spare, from a small window at a small scale up to a
	// retina one: nothing shrinks.
	for (size, scale) in [
		((1280u32, 800u32), 1.0f32),
		((2560, 1600), 2.0),
		((1280, 800), 0.7),
	] {
		let (_, hits, _) = emit_at(size, scale);
		assert!(
			(drawn_scale(&hits) - scale).abs() < 1e-4,
			"{size:?} at {scale}x has the room and should not have shrunk"
		);
	}
	for size in WINDOWS {
		for scale in SCALES {
			let (dl, hits, _) = emit_at(size, scale);
			let drawn = drawn_scale(&hits);
			let at = format!("{size:?} at {scale}x");
			assert!(drawn <= scale + 1e-4, "{at}: grew to {drawn}");
			let rung = (scale - drawn) * 4.0;
			assert!(
				(rung - rung.round()).abs() < 1e-3 || (drawn - theme::FIT_FLOOR).abs() < 1e-4,
				"{at}: drew at {drawn}, which is neither a quarter rung down nor the floor"
			);
			// The glyph is that rung's own whole-pixel size — the text
			// stepped with the boxes, not by some fraction of its own.
			assert_eq!(
				glyph_of(&dl),
				crate::ui::scale::glyph_px(drawn),
				"{at}: text at {} against boxes at {drawn}x",
				glyph_of(&dl)
			);
		}
	}
}

/// This panel takes the BAR's geometry instead of centring like every other
/// screen, and the alignment is the whole reason: its columns have to sit
/// over the slots they drop into. The tool slot is not one of them — §6
/// stands it apart off the right-hand end. Both sides read the one
/// `hud::row_grid`, which is what makes the bar's re-centring move the grid
/// with it rather than away from it.
///
/// Alignment holds exactly when the page draws at the frame's own scale AND
/// in the bar's own columns; the two escape hatches for a page with no room
/// — stepping the style, and `reflow`ing to longer rows — each abandon it
/// deliberately, and a fitted page centres because there is nothing left to
/// align to.
///
/// So the sweep is EVERY window and scale and the expectation is DERIVED. A
/// hand-kept list of the configurations that happen to have room is the
/// fragile part: the seventh grid row pushed one entry over and the entry
/// was removed, which reads as the rule still holding everywhere it is
/// checked while the place it stopped holding is no longer checked.
#[test]
fn the_grid_columns_sit_over_the_bar_slots_they_drop_into() {
	let mut aligned = 0usize;
	for size in WINDOWS {
		for scale in SCALES {
			let mut inv = Inventory::default();
			inv.add(B::Dirt, 5);
			inv.add(B::Stone, 2);
			let mut hotbar = Hotbar::new();
			let mut grid = InvGrid::default();
			digblock_core::inv_grid::reconcile_layout(&inv, &mut hotbar, &mut grid);
			let bar = HotbarView::build(&hotbar, &inv, &crate::hotbar::ToolSwap::default());
			let mut screen = InvScreen::default();
			screen.open();
			let mut dl = DrawList::default();
			let mut hits = HitMap::default();
			let mut ui = Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(scale),
			};
			// The engine's own frame order: screen first, then the bar,
			// one hit map serving both.
			screen.build(&mut ui, size, &grid, &inv, &bar, 1.0);
			crate::ui::hud::hotbar(&mut ui, size, &bar, |i| InvId::Bar(i as u8));
			// The two conditions, read off the layout rather than
			// guessed: the ladder did not step, and the grid kept the
			// bar's column count.
			let drawn = drawn_scale(&hits);
			let budget = crate::ui::hud::row_grid(size, scale).origin.1
				- (theme::GAP_ABOVE_BAR + theme::TOP_MARGIN) * scale;
			let style = Style::new(drawn);
			let grid_budget = budget - theme::ROW_H * drawn - 2.0 * theme::PAD * drawn;
			let bar_columns = InvScreen::reflow(&style, grid_budget) == HOTBAR_SLOTS;
			if (drawn - scale).abs() > 1e-4 || !bar_columns {
				continue;
			}
			aligned += 1;
			for i in 0..HOTBAR_SLOTS - 1 {
				let slot = hits.rect_of(InvId::Bar(i as u8)).unwrap();
				let cell = hits.rect_of(InvId::Grid(i as u16)).unwrap();
				assert_eq!(
					cell.x, slot.x,
					"{size:?} at {scale}x: grid column {i} is off bar slot {i}"
				);
				assert_eq!(cell.w, slot.w, "{size:?} at {scale}x: column {i} width");
			}
		}
	}
	// Without this the derivation could exclude everything and the
	// test would pass by checking nothing — which is the failure mode
	// the hand-kept list actually had.
	assert!(
		aligned >= WINDOWS.len(),
		"only {aligned} configurations kept the bar's geometry — the \
			 ordinary window is supposed to be one of them"
	);
}

/// The grid re-shapes only as a LAST resort, and when it does the page
/// still holds every slot on screen.
///
/// Two halves, and the first is the one that guards the ordinary
/// window: at the reference size the grid must be the bar's own nine
/// columns, because `reflow` is expressed as a `max` against them and
/// an off-by-one in the row arithmetic would silently widen every
/// page rather than only the cramped ones.
#[test]
fn the_grid_only_reshapes_when_the_ladder_has_run_out() {
	let full_rows = GRID_SLOTS.div_ceil(HOTBAR_SLOTS);
	let style = Style::new(1.0);
	let spec = InvScreen::grid_spec_at(&style, (0.0, 0.0));
	let full = spec.extent(GRID_SLOTS).1;
	assert_eq!(
		InvScreen::reflow(&style, full),
		HOTBAR_SLOTS,
		"a page with room for every row keeps the bar's columns"
	);
	assert_eq!(
		InvScreen::reflow(&style, full * 4.0),
		HOTBAR_SLOTS,
		"and so does one with room to spare — never NARROWER than the bar"
	);
	// One row short of the full height: the slots have to go somewhere,
	// and the only direction left is sideways.
	let one_row = spec.cell + spec.gap;
	let cols = InvScreen::reflow(&style, full - one_row);
	assert!(
		cols > HOTBAR_SLOTS,
		"a page one row short still laid out in {cols} columns"
	);
	assert!(
		GRID_SLOTS.div_ceil(cols) < full_rows,
		"the reshape has to actually save the row it was asked for"
	);
	// Every rung the ladder can reach still holds the whole roster.
	for rows in 1..=full_rows {
		let budget = rows as f32 * (spec.cell + spec.gap) - spec.gap;
		let cols = InvScreen::reflow(&style, budget);
		assert!(
			GRID_SLOTS.div_ceil(cols) <= rows.max(1),
			"{rows} rows of room laid out in {cols} columns — a slot is off the page"
		);
	}
}

fn drag(rig: &mut Rig, from: InvId, to: (f32, f32)) -> Option<InvAction> {
	let src = rig.hits.rect_of(from).expect("source slot exists");
	let (sx, sy) = center(src);
	let feed = |rig: &mut Rig, ev: &InputEvent| {
		rig.screen
			.handle_input(ev, &rig.hits, &rig.style, &rig.grid, &rig.bar)
	};
	feed(rig, &InputEvent::CursorMoved { x: sx, y: sy });
	feed(
		rig,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
	);
	feed(rig, &InputEvent::CursorMoved { x: to.0, y: to.1 });
	feed(
		rig,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: false,
		},
	)
}

#[test]
fn grid_to_bar_assigns_and_bar_to_bar_swaps() {
	let mut r = rig();
	assert_eq!(r.grid.get(1), Some(B::Stone));
	let bar3 = center(r.hits.rect_of(InvId::Bar(3)).unwrap());
	assert_eq!(
		drag(&mut r, InvId::Grid(1), bar3),
		Some(InvAction::Assign {
			slot: 3,
			block: B::Stone
		})
	);
	let bar5 = center(r.hits.rect_of(InvId::Bar(5)).unwrap());
	assert_eq!(
		drag(&mut r, InvId::Bar(0), bar5),
		Some(InvAction::SwapBar { a: 0, b: 5 })
	);
}

#[test]
fn bar_to_grid_lands_in_the_exact_slot() {
	let mut r = rig();
	// Onto an EMPTY grid slot: place exactly there.
	let g7 = center(r.hits.rect_of(InvId::Grid(7)).unwrap());
	assert_eq!(
		drag(&mut r, InvId::Bar(0), g7),
		Some(InvAction::PlaceGrid {
			bar_slot: 0,
			grid_slot: 7
		})
	);
	// Onto an OCCUPIED slot: same action — the engine swaps the
	// occupant onto the vacated bar slot.
	let g1 = center(r.hits.rect_of(InvId::Grid(1)).unwrap());
	assert_eq!(
		drag(&mut r, InvId::Bar(0), g1),
		Some(InvAction::PlaceGrid {
			bar_slot: 0,
			grid_slot: 1
		})
	);
	// Grid → grid moves between slots.
	let g4 = center(r.hits.rect_of(InvId::Grid(4)).unwrap());
	assert_eq!(
		drag(&mut r, InvId::Grid(1), g4),
		Some(InvAction::MoveGrid { from: 1, to: 4 })
	);
	// Empty grid slots yield no drag payload.
	assert_eq!(drag(&mut r, InvId::Grid(7), g4), None);
}

#[test]
fn bar_off_everything_clears_but_tool_snaps_back() {
	let mut r = rig();
	// Nowhere at all clears (re-filed by the engine's reconcile).
	assert_eq!(
		drag(&mut r, InvId::Bar(0), (5.0, 5.0)),
		Some(InvAction::ClearBar { slot: 0 })
	);
	// The tool slot is pinned (§6): its drags never even arm — grid,
	// void, and bar targets alike produce nothing.
	let g0 = center(r.hits.rect_of(InvId::Grid(0)).unwrap());
	let tool = InvId::Bar((HOTBAR_SLOTS - 1) as u8);
	assert_eq!(drag(&mut r, tool, g0), None);
	assert_eq!(drag(&mut r, tool, (5.0, 5.0)), None);
	let bar2 = center(r.hits.rect_of(InvId::Bar(2)).unwrap());
	assert_eq!(drag(&mut r, tool, bar2), None);
}

#[test]
fn empty_slots_and_clicks_do_nothing() {
	let mut r = rig();
	// Bar slot 4 is empty: press never arms, release is inert.
	let bar4 = center(r.hits.rect_of(InvId::Bar(4)).unwrap());
	assert_eq!(drag(&mut r, InvId::Bar(4), bar4), None);
	// Sub-threshold press+release on a full slot: a click, no action.
	let g1 = center(r.hits.rect_of(InvId::Grid(1)).unwrap());
	let near = (g1.0 + 1.0, g1.1 + 1.0);
	assert_eq!(drag(&mut r, InvId::Grid(1), near), None);
}

#[test]
fn focus_lost_cancels_the_drag() {
	let mut r = rig();
	let src = center(r.hits.rect_of(InvId::Grid(1)).unwrap());
	let feed = |r: &mut Rig, ev: &InputEvent| {
		r.screen
			.handle_input(ev, &r.hits, &r.style, &r.grid, &r.bar)
	};
	feed(&mut r, &InputEvent::CursorMoved { x: src.0, y: src.1 });
	feed(
		&mut r,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
	);
	feed(&mut r, &InputEvent::CursorMoved { x: 400.0, y: 400.0 });
	feed(&mut r, &InputEvent::FocusLost);
	let bar0 = center(r.hits.rect_of(InvId::Bar(0)).unwrap());
	// Release after cancel: nothing happens.
	feed(
		&mut r,
		&InputEvent::CursorMoved {
			x: bar0.0,
			y: bar0.1,
		},
	);
	assert_eq!(
		feed(
			&mut r,
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: false,
			}
		),
		None
	);
}

#[test]
fn ghost_appears_only_mid_drag_and_build_emits_every_slot() {
	let mut r = rig();
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: r.style,
	};
	let inv = Inventory::default();
	r.screen.build(&mut ui, SIZE, &r.grid, &inv, &r.bar, 1.0);
	assert!(dl.cmds(Layer::Ghost).0.is_empty(), "no ghost before a drag");
	// Every grid slot has a hit rect (empty ones are drop targets).
	assert!(hits.rect_of(InvId::Grid(0)).is_some());
	assert!(hits.rect_of(InvId::Grid(GRID_SLOTS as u16 - 1)).is_some());

	// Mid-drag: ghost icon rides the cursor.
	let src = center(r.hits.rect_of(InvId::Grid(1)).unwrap());
	let feed = |r: &mut Rig, ev: &InputEvent| {
		r.screen
			.handle_input(ev, &r.hits, &r.style, &r.grid, &r.bar)
	};
	feed(&mut r, &InputEvent::CursorMoved { x: src.0, y: src.1 });
	feed(
		&mut r,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
	);
	feed(&mut r, &InputEvent::CursorMoved { x: 300.0, y: 300.0 });
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: r.style,
	};
	r.screen.build(&mut ui, SIZE, &r.grid, &inv, &r.bar, 1.0);
	// One cube icon = three projected faces.
	assert_eq!(dl.cmds(Layer::Ghost).0.len(), 3, "ghost mid-drag");
}

/// Every text on the Ghost layer — the tooltip's, and nothing else's
/// while no drag is running.
fn ghost_texts(dl: &DrawList) -> Vec<String> {
	let (cmds, arena) = dl.cmds(Layer::Ghost);
	cmds.iter()
		.filter_map(|c| match c {
			crate::ui::draw::Cmd::Text { range, .. } => {
				Some(arena[range.0 as usize..range.1 as usize].to_string())
			}
			_ => None,
		})
		.collect()
}

/// A slot names what it holds while the cursor is on it — grid or bar,
/// at the page's own glyph — and only then: an empty slot, a running
/// drag and a cursor that has left all show nothing.
#[test]
fn hovering_a_slot_names_it_and_leaving_clears_it() {
	let mut r = rig();
	let inv = {
		let mut inv = Inventory::default();
		inv.add(B::Dirt, 5);
		inv.add(B::Stone, 2);
		inv
	};
	let feed = |r: &mut Rig, ev: &InputEvent| {
		r.screen
			.handle_input(ev, &r.hits, &r.style, &r.grid, &r.bar)
	};
	let frame = |r: &Rig| {
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: r.style,
		};
		r.screen.build(&mut ui, SIZE, &r.grid, &inv, &r.bar, 1.0);
		let glyph = ui.style.glyph;
		(ghost_texts(&dl), glyph)
	};
	// The parked stone, in the grid.
	let at = center(r.hits.rect_of(InvId::Grid(1)).unwrap());
	feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
	let (names, glyph) = frame(&r);
	assert_eq!(names, vec![B::Stone.name().to_uppercase()]);
	assert_eq!(glyph, r.style.glyph, "named at the page's glyph");
	// The dirt on the bar: named by the PAGE, since the bar draws later
	// at the window's scale.
	let at = center(r.hits.rect_of(InvId::Bar(0)).unwrap());
	feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
	assert_eq!(frame(&r).0, vec![B::Dirt.name().to_uppercase()]);
	// An empty slot has no name.
	let at = center(r.hits.rect_of(InvId::Grid(5)).unwrap());
	feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
	assert!(frame(&r).0.is_empty(), "an empty slot has nothing to say");
	// Mid-drag the ghost rides the cursor and no name does.
	let src = center(r.hits.rect_of(InvId::Grid(1)).unwrap());
	feed(&mut r, &InputEvent::CursorMoved { x: src.0, y: src.1 });
	feed(
		&mut r,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
	);
	feed(
		&mut r,
		&InputEvent::CursorMoved {
			x: src.0 + 80.0,
			y: src.1,
		},
	);
	assert!(frame(&r).0.is_empty(), "no name under a drag");
	feed(
		&mut r,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: false,
		},
	);
	// A cursor that has left — what a lifted touch sends — clears it.
	feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
	feed(&mut r, &InputEvent::CursorMoved { x: -1.0, y: -1.0 });
	assert!(frame(&r).0.is_empty(), "a cursor that left names nothing");
}
