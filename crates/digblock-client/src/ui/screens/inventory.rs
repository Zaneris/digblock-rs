//! The inventory screen: a fixed grid of slots behind the hotbar
//! (`digblock_core::inv_grid`) with drag-and-drop between grid and bar.
//! Slots are *assignments* over the per-type counts, so drops move
//! assignments — counts never move or split — and an item dropped on a
//! grid slot lands in exactly that slot (swapping with any occupant).
//! The hotbar row itself stays on screen (emitted by `ui::hud`); its hit
//! rects are the drop targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem};
use digblock_core::inv_grid::{GRID_SLOTS, InvGrid};
use digblock_core::inventory::Inventory;
use digblock_platform::{InputEvent, MouseButton};

use crate::hotbar::{HotbarView, SlotView};
use crate::ui::drag::{DRAG_THRESHOLD_PX, DragEvent, DragState};
use crate::ui::draw::Layer;
use crate::ui::hud;
use crate::ui::widgets::{self, GridSpec, Style, Ui, WHITE};
use crate::ui::{HitMap, InvId, Rect, theme};

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
		Self::grid_spec_cols(style, origin, HOTBAR_SLOTS)
	}

	/// The same grid at a chosen column count — see [`InvScreen::reflow`]
	/// for the one case that is not the bar's.
	fn grid_spec_cols(style: &Style, origin: (f32, f32), cols: usize) -> GridSpec {
		GridSpec {
			origin,
			cols,
			cell: hud::SLOT_PX * style.scale,
			gap: hud::SLOT_GAP * style.scale,
		}
	}

	/// How many columns to lay `GRID_SLOTS` out in, given the height actually
	/// available for the grid.
	///
	/// Normally the bar's own count, which is what puts each column over the
	/// slot it drops into. But the ladder bottoms out ([`theme::FIT_FLOOR`]) and
	/// a small enough window at a large enough interface scale still has no room
	/// for seven rows — and the page has already given up bar alignment by then,
	/// because a fitted page CENTRES. So the same slots go into fewer, longer
	/// rows instead.
	///
	/// Fewer rows and not smaller boxes, which is the whole argument: the glyph
	/// size has a floor, so buying height by stepping below it shrinks every box
	/// around text that cannot follow.
	///
	/// Never NARROWER than the bar: a page with room to spare must lay out
	/// exactly as it always did, so the normal case comes out of the same
	/// expression rather than being special-cased around it.
	fn reflow(style: &Style, grid_budget: f32) -> usize {
		let (cell, gap) = (hud::SLOT_PX * style.scale, hud::SLOT_GAP * style.scale);
		let full_rows = GRID_SLOTS.div_ceil(HOTBAR_SLOTS);
		let fits = ((grid_budget + gap) / (cell + gap)).floor().max(1.0) as usize;
		let rows = fits.min(full_rows);
		HOTBAR_SLOTS.max(GRID_SLOTS.div_ceil(rows))
	}

	/// The panel's content height: one title row over the grid. The
	/// measure half of [`InvScreen::build`]'s layout — `fit_scale` calls
	/// it at candidate scales, so it must derive everything from the
	/// style it is handed and read nothing already committed at the
	/// frame's own.
	///
	/// Measured at the BAR's column count, never a reflowed one: this is
	/// what the ladder steps against, and a height that shrank as the grid
	/// re-shaped would stop the ladder stepping at all. Reflow is the last
	/// resort AFTER the ladder has spent itself, not an alternative to it.
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
		bar_view: &HotbarView,
		daylight: f32,
	) {
		let window_scale = ui.style.scale;

		// Full-screen dim under everything this screen adds.
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);

		// The slot rows and a title band make a page taller than a short window
		// at a big interface scale has to give: the panel's top edge crosses
		// the top of the window, and further up the scale the first row of
		// slots goes with it and cannot be clicked at all. This screen ran off
		// the TOP once the grid gained its extra rows, because it positioned
		// itself by subtraction from the bar and never checked.
		//
		// So it walks the same ladder the alchemy screen does: the WHOLE style
		// steps down a rung at a time, re-measured per rung, never a fractional
		// glyph. The bar is not ours to shrink — it is drawn after us at the
		// frame's own scale — so every measurement taken against it stays in
		// `window_scale`.
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
		// What is left for the grid once the title and the panel's own
		// padding are paid, at the scale the ladder settled on. Equal to
		// the full seven rows in every ordinary window, so `reflow`
		// answers the bar's column count and nothing below here changes.
		let grid_budget = budget - title_h - 2.0 * pad;
		let cols = Self::reflow(&ui.style, grid_budget);
		let (row_w, grid_h) = Self::grid_spec_cols(&ui.style, (0.0, 0.0), cols).extent(GRID_SLOTS);
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
		let spec = Self::grid_spec_cols(&ui.style, (row_x, panel_y + pad + title_h), cols);
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
		let (cx, cy) = self.drag.cursor;
		if let Some(&item) = self.drag.ghost() {
			// Dragging the tool carries its charge: the ghost is the same
			// item, so it has to be the same picture.
			let (kind, tint) = hud::item_icon(item, inv.bucket_full());
			hud::draw_icon(
				ui.dl,
				Layer::Ghost,
				Rect::new(cx - icon_px / 2.0, cy - icon_px / 2.0, icon_px, icon_px),
				kind,
				tint,
			);
		} else {
			// The name of the slot under the cursor — grid or bar — at
			// THIS frame's glyph, which is why the bar's slots are named
			// here rather than by the bar, drawn later at the window's.
			let over_grid = (0..GRID_SLOTS)
				.map(|i| (spec.rect(i), grid.get(i)))
				.find(|(r, _)| r.contains(cx, cy))
				.and_then(|(r, b)| b.map(|b| (r, b.name())));
			let over_bar = || hud::hovered_bar_slot(size, window_scale, bar_view, (cx, cy));
			if let Some((anchor, name)) = over_grid.or_else(over_bar) {
				widgets::tooltip(ui.dl, &ui.style, size, anchor, &name);
			}
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
					// Alchemy and spellbook ids never appear while this
					// screen is open.
					InvId::Craft(_)
					| InvId::Family(_)
					| InvId::CraftGo
					| InvId::Spell(_)
					| InvId::SpellFare(_) => {
						return None;
					}
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
mod tests;
