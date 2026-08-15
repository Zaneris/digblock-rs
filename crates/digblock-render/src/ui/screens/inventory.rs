//! The inventory screen: a fixed grid of slots behind the hotbar
//! (`digblock_core::inv_grid`) with drag-and-drop between grid and bar.
//! Slots are *assignments* over the per-type counts, so drops move
//! assignments — counts never move or split — and an item dropped on a
//! grid slot lands in exactly that slot (swapping with any occupant).
//! The hotbar row itself stays on screen (emitted by `ui::hud`); its hit
//! rects are the drop targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS};
use digblock_core::inv_grid::{InvGrid, GRID_SLOTS};
use digblock_core::inventory::Inventory;
use digblock_platform::{InputEvent, MouseButton};

use crate::hotbar::{HotbarView, SlotView};
use crate::ui::drag::{DragEvent, DragState, DRAG_THRESHOLD_PX};
use crate::ui::draw::Layer;
use crate::ui::hud;
use crate::ui::widgets::{self, GridSpec, Style, Ui, WHITE};
use crate::ui::{theme, HitMap, InvId, Rect};

/// Grid/bar mutations the engine applies to its core `Hotbar`/`InvGrid`
/// (the screen never mutates game state). Core owns the invariants
/// (move-not-duplicate, WaterTool protection).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InvAction {
	/// Grid → bar drop: assign `block` to bar `slot` (the source grid
	/// slot clears via reconcile).
	Assign { slot: usize, block: BlockType },
	/// Bar → bar drop.
	SwapBar { a: usize, b: usize },
	/// Bar → grid drop: the item lands in exactly `grid_slot`, swapping
	/// with any occupant (which takes the vacated bar slot).
	PlaceGrid { bar_slot: usize, grid_slot: usize },
	/// Grid → grid drop: move/swap between two grid slots.
	MoveGrid { from: usize, to: usize },
	/// Bar → nothing drop: unassign (core refuses the tool); the item
	/// re-files into the first free grid slot.
	ClearBar { slot: usize },
}

#[derive(Default)]
pub struct InvScreen {
	open: bool,
	drag: DragState<InvId, HotbarItem>,
}

impl InvScreen {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		self.open = true;
	}

	pub fn close(&mut self) {
		self.open = false;
		self.drag.cancel();
	}

	/// Abandon an in-flight drag without closing (focus loss — the
	/// screen survives a cmd-tab, like the pause menu does).
	pub fn cancel_drag(&mut self) {
		self.drag.cancel();
	}

	/// The grid: as many columns as the hotbar has slots, at the drawn
	/// style's own slot size, over however many rows `GRID_SLOTS` fills.
	/// The bar's column count because the grid is aligned to the bar, not
	/// merely the same shape as it. Takes a `Style` rather than a bare
	/// scale because the fit ladder may draw the page at a smaller one
	/// than the frame's, and the measure pass has to ask about candidate
	/// scales; `origin` is set per-build (see [`InvScreen::build`]).
	fn grid_spec_at(style: &Style, origin: (f32, f32)) -> GridSpec {
		GridSpec {
			origin,
			cols: HOTBAR_SLOTS,
			cell: hud::SLOT_PX * style.scale,
			gap: hud::SLOT_GAP * style.scale,
		}
	}

	/// The panel's content height: one title row over the grid. The
	/// measure half of [`InvScreen::build`]'s layout — `fit_scale` calls
	/// it at candidate scales, so it must derive everything from the
	/// style it is handed and read nothing already committed at the
	/// frame's own.
	fn content_h(style: &Style) -> f32 {
		theme::ROW_H * style.scale + Self::grid_spec_at(style, (0.0, 0.0)).extent(GRID_SLOTS).1
	}

	/// Emit the screen: dim, panel, title, every grid slot (occupied
	/// ones with icon + count), and the drag ghost. Call before
	/// `hud::hotbar` so the dim sits under the bar's own backdrops.
	/// `daylight` fades the full-screen dim only — the panel keeps its
	/// fixed backdrop.
	pub fn build(
		&self,
		ui: &mut Ui<'_, InvId>,
		size: (u32, u32),
		grid: &InvGrid,
		inv: &Inventory,
		daylight: f32,
	) {
		let window_scale = ui.style.scale;

		// Full-screen dim under everything this screen adds.
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);

		// The slot rows and a title band make a page taller than a short
		// window at a big interface scale has to give: the panel's top
		// edge crosses the top of the window, and further up the scale the
		// first row of slots goes with it and cannot be clicked at all.
		// This screen grew into that when the grid gained its extra rows
		// and simply ran off the TOP, because it positioned itself by
		// subtraction from the bar and never checked.
		//
		// So it walks the same ladder the alchemy screen does: the WHOLE
		// style steps down a rung at a time until the page fits between
		// the bar and the top margin, re-measured per rung, never a
		// fractional glyph. The bar is not ours to shrink — it is drawn
		// after us at the frame's own scale — so every measurement taken
		// against it stays in `window_scale`.
		let bar = hud::row_grid(size, window_scale);
		let budget = bar.origin.1 - (theme::GAP_ABOVE_BAR + theme::TOP_MARGIN) * window_scale;
		let needed = |s: f32| Self::content_h(&Style::new(s)) + 2.0 * theme::PAD * s;
		let restore = ui.style;
		let fitted = theme::fit_scale(window_scale, budget, needed);
		if fitted != window_scale {
			ui.style = Style::new(fitted);
		}
		let s = ui.style.scale;
		let pad = theme::PAD * s;
		// One shared text row for the title band — the same box, and so
		// the same leading, every other screen's rows sit in.
		let title_h = theme::ROW_H * s;
		let (row_w, grid_h) = Self::grid_spec_at(&ui.style, (0.0, 0.0)).extent(GRID_SLOTS);
		// The left column is the BAR's, so the grid's cells land on the
		// slots they drop into. That alignment only exists while the two
		// share a scale, though: once the ladder has stepped there is
		// nothing left to align to, and hanging a narrower panel off the
		// bar's rail would follow it off the window — on a small window at
		// a large interface scale the bar's own first column sits off the
		// left edge. A fitted page therefore centres, like every other
		// screen.
		let row_x = if fitted == window_scale {
			bar.origin.0
		} else {
			(size.0 as f32 - row_w) / 2.0
		};
		// Bottom-anchored to the bar, then clamped off the top edge: when
		// even `theme::FIT_FLOOR` cannot buy the room, a page crowding the
		// bar it stands above is still a page you can read, and one whose
		// title has left the window is not.
		let panel_y =
			(bar.origin.1 - theme::GAP_ABOVE_BAR * window_scale - (title_h + grid_h + 2.0 * pad))
				.max(theme::TOP_MARGIN * window_scale);
		let panel = theme::panel_of(row_x, panel_y + pad, row_w, title_h + grid_h, s);
		let spec = Self::grid_spec_at(&ui.style, (row_x, panel_y + pad + title_h));
		widgets::panel(ui.dl, Layer::Backdrop, panel, theme::PANEL_BG);
		// Title text left-aligns with the first slot column, not at the
		// row widget's inset: this panel's content is a grid, and a title
		// that didn't sit over column one would read as a misalignment.
		let title_y = panel.y + pad + (title_h - ui.style.glyph) / 2.0;
		widgets::label(
			ui.dl,
			Layer::Content,
			panel.x + pad,
			title_y,
			ui.style.glyph,
			WHITE,
			"INVENTORY",
		);
		let icon_px = hud::ICON_PX * s;
		for i in 0..GRID_SLOTS {
			let rect = spec.rect(i);
			let icon = grid
				.get(i)
				.map(|b| hud::item_icon(HotbarItem::Block(b), false));
			widgets::slot(
				ui,
				InvId::Grid(i as u16),
				rect,
				icon,
				icon_px,
				widgets::SlotRing::None,
				rect.contains(self.drag.cursor.0, self.drag.cursor.1),
			);
			if let Some(b) = grid.get(i) {
				// Same badge as the hotbar — the rows sit stacked, so a
				// size mismatch would read as a glitch.
				widgets::count_badge(ui.dl, Layer::Content, &ui.style, rect, inv.count(b), WHITE);
			}
		}
		// The held item rides the cursor, above everything.
		if let Some(&item) = self.drag.ghost() {
			// Dragging the tool carries its charge: the ghost is the same
			// item, so it has to be the same picture.
			let (kind, tint) = hud::item_icon(item, inv.bucket_full());
			let (cx, cy) = self.drag.cursor;
			hud::draw_icon(
				ui.dl,
				Layer::Ghost,
				Rect::new(cx - icon_px / 2.0, cy - icon_px / 2.0, icon_px, icon_px),
				kind,
				tint,
			);
		}
		// The frame's own style back: the hotbar draws after us, at the
		// scale the window asked for and not the one we had to fit into.
		ui.style = restore;
	}

	/// Feed one input event; returns the action the engine should apply.
	/// `hits` is the frame's hit map (grid cells + the HUD's bar slots);
	/// `grid`/`bar` resolve a pressed id to its payload.
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
		style: &Style,
		grid: &InvGrid,
		bar: &HotbarView,
	) -> Option<InvAction> {
		match *event {
			InputEvent::CursorMoved { x, y } => {
				self.drag.on_move((x, y), DRAG_THRESHOLD_PX * style.scale);
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => {
				let pos = self.drag.cursor;
				let id = hits.hit(pos.0, pos.1)?;
				let payload = match id {
					InvId::Grid(i) => HotbarItem::Block(grid.get(i as usize)?),
					InvId::Bar(i) => match bar.slots.get(i as usize)? {
						// §6 pinned the tool slot in place: it can't be
						// dragged anywhere, so a press on it never arms.
						SlotView::Empty | SlotView::Tool { .. } => return None,
						SlotView::Block { block, .. } => HotbarItem::Block(*block),
					},
					// Alchemy ids never appear while this screen is open.
					InvId::Craft(_)
					| InvId::Family(_)
					| InvId::CraftGo
					| InvId::Tab(_)
					| InvId::ExtractSlot
					| InvId::BucketTile => return None,
				};
				self.drag.on_press(pos, id, payload);
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: false,
			} => match self.drag.on_release(hits) {
				DragEvent::Drop {
					from: InvId::Grid(_),
					payload: HotbarItem::Block(block),
					to: Some(InvId::Bar(slot)),
				} => Some(InvAction::Assign {
					slot: slot as usize,
					block,
				}),
				DragEvent::Drop {
					from: InvId::Bar(a),
					to: Some(InvId::Bar(b)),
					..
				} if a != b => Some(InvAction::SwapBar {
					a: a as usize,
					b: b as usize,
				}),
				// Onto a specific grid slot = land exactly there (the
				// tool snaps back instead — core would refuse anyway).
				DragEvent::Drop {
					from: InvId::Bar(slot),
					payload,
					to: Some(InvId::Grid(g)),
				} if payload != HotbarItem::WaterTool => Some(InvAction::PlaceGrid {
					bar_slot: slot as usize,
					grid_slot: g as usize,
				}),
				DragEvent::Drop {
					from: InvId::Grid(a),
					to: Some(InvId::Grid(b)),
					..
				} if a != b => Some(InvAction::MoveGrid {
					from: a as usize,
					to: b as usize,
				}),
				// Off everything = unassign (re-filed into the grid).
				DragEvent::Drop {
					from: InvId::Bar(slot),
					payload,
					to: None,
				} if payload != HotbarItem::WaterTool => Some(InvAction::ClearBar {
					slot: slot as usize,
				}),
				_ => None,
			},
			InputEvent::FocusLost => {
				self.drag.cancel();
				None
			}
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
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
		screen.build(&mut ui, SIZE, &grid, &inv, 1.0);
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
		screen.build(&mut ui, size, &grid, &inv, 1.0);
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

	/// This panel takes the BAR's geometry instead of centring like every
	/// other screen, and the alignment is the whole reason: its columns
	/// have to sit over the slots they drop into. The tool slot is not one
	/// of them — §6 stands it apart off the right-hand end — so the eight
	/// block slots are what the claim is about. Both sides read the one
	/// `hud::row_grid`, which is what makes the bar's re-centring move the
	/// grid with it rather than away from it.
	#[test]
	fn the_grid_columns_sit_over_the_bar_slots_they_drop_into() {
		// Only where the fit ladder has NOT stepped: at a smaller cell
		// size there is no column alignment left to hold, which is exactly
		// why a fitted page centres instead.
		for size in [(1280u32, 800u32), (1600, 900)] {
			for scale in [0.75f32, 1.0, 1.5] {
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
				screen.build(&mut ui, size, &grid, &inv, 1.0);
				crate::ui::hud::hotbar(&mut ui, size, &bar, |i| InvId::Bar(i as u8));
				assert!(
					(drawn_scale(&hits) - scale).abs() < 1e-4,
					"{size:?} at {scale}x was supposed to be a case with room"
				);
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
		r.screen.build(&mut ui, SIZE, &r.grid, &inv, 1.0);
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
		r.screen.build(&mut ui, SIZE, &r.grid, &inv, 1.0);
		// One cube icon = three projected faces.
		assert_eq!(dl.cmds(Layer::Ghost).0.len(), 3, "ghost mid-drag");
	}
}
