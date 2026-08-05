//! The inventory screen: a grid of every owned block type (the
//! overflow beyond the 9 hotbar slots) with drag-and-drop between grid
//! and bar. Hotbar slots are *assignments* over the per-type counts, so
//! drops move assignments — counts never move or split. The hotbar row
//! itself stays on screen (emitted by `ui::hud`); its hit rects are the
//! drop targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS};
use digblock_platform::{InputEvent, MouseButton};

use crate::hotbar::{HotbarView, SlotView};
use crate::ui::drag::{DragEvent, DragState, DRAG_THRESHOLD_PX};
use crate::ui::draw::Layer;
use crate::ui::hud;
use crate::ui::widgets::{self, GridSpec, Style, Ui, DIM, WHITE};
use crate::ui::{HitMap, InvId, Rect};

/// Grid/bar mutations the engine applies to its core `Hotbar` (the
/// screen never mutates game state). Core owns the invariants
/// (move-not-duplicate, WaterTool protection).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InvAction {
	/// Grid → bar drop: assign `block` to `slot`.
	Assign { slot: usize, block: BlockType },
	/// Bar → bar drop.
	SwapBar { a: usize, b: usize },
	/// Bar → grid/nothing drop: unassign (core refuses the tool).
	ClearBar { slot: usize },
}

const PANEL_BG: u32 = 0xC0101010;
const SCREEN_DIM: u32 = 0x70000000;

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

	/// The grid: 9 columns aligned with the hotbar, rows stacked above
	/// it, wrapped in a panel. `grid` is the engine's `iter_nonzero`
	/// snapshot — the same slice `handle_input` resolves ids against.
	fn grid_spec(size: (u32, u32), scale: f32) -> GridSpec {
		let bar = hud::row_grid(size, scale);
		GridSpec {
			// Rows grow upward from just above the bar; origin is set
			// per-build once the row count is known (see `build`).
			origin: (bar.origin.0, bar.origin.1),
			..bar
		}
	}

	/// Emit the screen: dim, panel, title, grid slots + counts, and the
	/// drag ghost. Call before `hud::hotbar` so the dim sits under the
	/// bar's own backdrops.
	pub fn build(&self, ui: &mut Ui<'_, InvId>, size: (u32, u32), grid: &[(BlockType, u32)]) {
		let scale = ui.style.scale;
		let mut spec = Self::grid_spec(size, scale);
		let rows = grid.len().div_ceil(spec.cols).max(1);
		let gap_above_bar = 24.0 * scale;
		let (_, grid_h) = spec.extent(grid.len().max(1));
		let grid_h = grid_h.max(spec.cell);
		spec.origin.1 -= gap_above_bar + grid_h;
		debug_assert_eq!(
			rows,
			((grid_h + spec.gap) / (spec.cell + spec.gap)).round() as usize
		);

		// Full-screen dim under everything this screen adds.
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			SCREEN_DIM,
		);
		let pad = 12.0 * scale;
		let title_h = ui.style.glyph + 8.0 * scale;
		let bar_w = hud::row_grid(size, scale).extent(HOTBAR_SLOTS).0;
		let panel = Rect::new(
			spec.origin.0 - pad,
			spec.origin.1 - title_h - pad,
			bar_w + pad * 2.0,
			title_h + grid_h + pad * 2.0,
		);
		widgets::panel(ui.dl, Layer::Backdrop, panel, PANEL_BG);
		widgets::label(
			ui.dl,
			Layer::Content,
			panel.x + pad,
			panel.y + pad / 2.0 + 2.0 * scale,
			ui.style.glyph,
			WHITE,
			"INVENTORY",
		);
		if grid.is_empty() {
			widgets::label(
				ui.dl,
				Layer::Content,
				spec.origin.0,
				spec.origin.1 + (spec.cell - ui.style.glyph) / 2.0,
				ui.style.glyph,
				DIM,
				"nothing collected yet",
			);
		}
		let icon_px = hud::ICON_PX * scale;
		for (i, &(block, count)) in grid.iter().enumerate() {
			let rect = spec.rect(i);
			let icon = hud::item_icon(HotbarItem::Block(block));
			widgets::slot(ui, InvId::Grid(i as u16), rect, Some(icon), icon_px, false);
			// Same pinned-size badge as the hotbar — the rows sit
			// stacked, so a size mismatch would read as a glitch.
			widgets::count_badge(ui.dl, Layer::Content, rect, count, WHITE);
		}
		// The held item rides the cursor, above everything.
		if let Some(&item) = self.drag.ghost() {
			let (tex, tint) = hud::item_icon(item);
			let (cx, cy) = self.drag.cursor;
			ui.dl.icon(
				Layer::Ghost,
				Rect::new(cx - icon_px / 2.0, cy - icon_px / 2.0, icon_px, icon_px),
				tex,
				tint,
			);
		}
	}

	/// Feed one input event; returns the action the engine should apply.
	/// `hits` is the frame's hit map (grid cells + the HUD's bar slots);
	/// `grid`/`bar` resolve a pressed id to its payload.
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
		style: &Style,
		grid: &[(BlockType, u32)],
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
					InvId::Grid(i) => HotbarItem::Block(grid.get(i as usize)?.0),
					InvId::Bar(i) => match bar.slots.get(i as usize)? {
						SlotView::Empty => return None,
						SlotView::Tool => HotbarItem::WaterTool,
						SlotView::Block { block, .. } => HotbarItem::Block(*block),
					},
					// Alchemy ids never appear while this screen is open.
					InvId::Convert(_) | InvId::Craft(_) => return None,
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
				// Off the bar (grid or nothing) = unassign; the tool
				// snaps back instead (core would refuse anyway).
				DragEvent::Drop {
					from: InvId::Bar(slot),
					payload,
					to: None | Some(InvId::Grid(_)),
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
	use crate::ui::draw::DrawList;
	use digblock_core::block::BlockType as B;
	use digblock_core::hotbar::Hotbar;
	use digblock_core::inventory::Inventory;

	const SIZE: (u32, u32) = (1280, 800);

	struct Rig {
		screen: InvScreen,
		hits: HitMap<InvId>,
		grid: Vec<(B, u32)>,
		bar: HotbarView,
		style: Style,
	}

	fn rig() -> Rig {
		let mut hotbar = Hotbar::new();
		hotbar.auto_assign(B::Dirt); // bar slot 0
		let mut inv = Inventory::default();
		inv.add(B::Dirt, 5);
		inv.add(B::Stone, 2);
		let grid: Vec<_> = inv.iter_nonzero().collect();
		let bar = HotbarView::build(&hotbar, &inv);
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
		screen.build(&mut ui, SIZE, &grid);
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
		// Grid is iter_nonzero order: Dirt then Stone.
		assert_eq!(r.grid[1].0, B::Stone);
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
	fn bar_off_bar_clears_but_tool_snaps_back() {
		let mut r = rig();
		let grid0 = center(r.hits.rect_of(InvId::Grid(0)).unwrap());
		assert_eq!(
			drag(&mut r, InvId::Bar(0), grid0),
			Some(InvAction::ClearBar { slot: 0 })
		);
		// Nowhere at all also clears.
		assert_eq!(
			drag(&mut r, InvId::Bar(0), (5.0, 5.0)),
			Some(InvAction::ClearBar { slot: 0 })
		);
		// The tool refuses both.
		let tool = InvId::Bar((HOTBAR_SLOTS - 1) as u8);
		assert_eq!(drag(&mut r, tool, grid0), None);
		assert_eq!(drag(&mut r, tool, (5.0, 5.0)), None);
		// But swaps within the bar.
		let bar2 = center(r.hits.rect_of(InvId::Bar(2)).unwrap());
		assert_eq!(
			drag(&mut r, tool, bar2),
			Some(InvAction::SwapBar {
				a: HOTBAR_SLOTS - 1,
				b: 2
			})
		);
	}

	#[test]
	fn empty_slots_and_clicks_do_nothing() {
		let mut r = rig();
		// Bar slot 4 is empty: press never arms, release is inert.
		let bar4 = center(r.hits.rect_of(InvId::Bar(4)).unwrap());
		assert_eq!(drag(&mut r, InvId::Bar(4), bar4), None);
		// Sub-threshold press+release on a full slot: a click, no action.
		let g0 = center(r.hits.rect_of(InvId::Grid(0)).unwrap());
		let near = (g0.0 + 1.0, g0.1 + 1.0);
		assert_eq!(drag(&mut r, InvId::Grid(0), near), None);
	}

	#[test]
	fn focus_lost_cancels_the_drag() {
		let mut r = rig();
		let src = center(r.hits.rect_of(InvId::Grid(0)).unwrap());
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
	fn ghost_appears_only_mid_drag_and_build_emits_grid() {
		let mut r = rig();
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: r.style,
		};
		r.screen.build(&mut ui, SIZE, &r.grid);
		assert!(dl.cmds(Layer::Ghost).0.is_empty(), "no ghost before a drag");
		// Two grid cells emitted with hit rects.
		assert!(hits.rect_of(InvId::Grid(0)).is_some());
		assert!(hits.rect_of(InvId::Grid(1)).is_some());

		// Mid-drag: ghost icon rides the cursor.
		let src = center(r.hits.rect_of(InvId::Grid(0)).unwrap());
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
		r.screen.build(&mut ui, SIZE, &r.grid);
		assert_eq!(dl.cmds(Layer::Ghost).0.len(), 1, "ghost mid-drag");
	}
}
