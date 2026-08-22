use super::*;
use crate::ui::Rect;
use crate::ui::draw::{Cmd, DrawList};
use crate::ui::hit::HitMap;
use crate::ui::widgets::Style;
use digblock_core::hotbar::Hotbar;
use digblock_core::inventory::Inventory;

const SIZE: (u32, u32) = (1280, 720);

/// `icon_block` is a HAND-WRITTEN inverse, and the compiler only checks that
/// an arm exists. `IconKind` is closed, so a new variant cannot be
/// forgotten — but naming the wrong block in one compiles perfectly, and the
/// two questions this answers are the ones nobody would notice being wrong:
/// an item would wear another item's fullbright ink, and carry another
/// item's light.
///
/// So walk the real catalog both ways rather than restating the match. Tools
/// go through `tool_icon` because that is the function keyed by BLOCK;
/// `item_icon(WaterTool, _)` is keyed by the SLOT.
#[test]
fn icon_block_names_the_block_its_icon_was_built_from() {
	for b in BlockType::ALL {
		if b.is_tool() {
			for full in [false, true] {
				assert_eq!(
					icon_block(tool_icon(b, full)),
					Some(b),
					"{b:?} (full={full}) does not survive the round trip"
				);
			}
		} else if b.is_slottable() {
			assert_eq!(
				icon_block(item_icon(HotbarItem::Block(b), false).0),
				Some(b),
				"{b:?} does not survive the round trip"
			);
		}
	}
}

/// What the sample bar is stocked with. Named so the test that checks
/// the counts REACH the screen reads them from the fixture rather than
/// keeping a second copy of the same two numbers.
const DIRT_STOCK: u32 = 3;
const STONE_STOCK: u32 = 12;

fn sample_view() -> HotbarView {
	let mut hotbar = Hotbar::new();
	hotbar.auto_assign(BlockType::Dirt);
	hotbar.auto_assign(BlockType::Stone);
	let mut inv = Inventory::default();
	inv.add(BlockType::Dirt, DIRT_STOCK);
	inv.add(BlockType::Stone, STONE_STOCK);
	// A settled bucket in the slot, the way the engine's entry path
	// settles it — these tests are about the drawn bar, not the
	// fresh-player bare slot.
	let mut swap = crate::hotbar::ToolSwap::default();
	swap.advance(crate::hotbar::ToolSwap::SECS, Some(BlockType::Bucket));
	HotbarView::build(&hotbar, &inv, &swap)
}

fn emit_at(size: (u32, u32), scale: f32) -> (DrawList, HitMap<usize>) {
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: Style::new(scale),
	};
	hotbar(&mut ui, size, &sample_view(), |i| i);
	(dl, hits)
}

fn emit(scale: f32) -> (DrawList, HitMap<usize>) {
	emit_at(SIZE, scale)
}

/// The DRAWN row is centered — and the drawn row is the block-slot lattice
/// PLUS the split the tool slot steps out by, which is a real
/// [`TOOL_SPLIT`] wider than anything that measures the lattice.
///
/// `right` has to come off the LAST DRAWN SLOT. Taking it from
/// `grid.origin.0 + grid.extent(HOTBAR_SLOTS).0` reduces algebraically to
/// `left`, so the assertion becomes `left - left == 0` and cannot fail —
/// which is exactly how the split shipped folded into the drawing but not
/// into the centring, leaving the row half a split right of centre with its
/// right edge off the window first. Swept over sizes and scales because the
/// bar takes more of the window as the interface scale grows.
#[test]
fn row_is_bottom_centered_and_scales() {
	for size in [
		(1280u32, 720u32),
		(1920, 1080),
		(960, 600),
		(800, 600),
		(640, 480),
	] {
		for scale in [1.0f32, 1.5, 2.0, 2.5, 3.0] {
			let (_, hits) = emit_at(size, scale);
			let grid = row_grid(size, scale);
			let first = hits.rect_of(0).unwrap();
			let last = hits.rect_of(HOTBAR_SLOTS - 1).unwrap();
			let left = first.x;
			let right = size.0 as f32 - (last.x + last.w);
			assert!(
				(left - right).abs() < 0.5,
				"{size:?} at {scale}x: {left} of air on the left, {right} on the right"
			);
			// Every slot the lattice owns is ON it — the split is one
			// slot stepping out at the end, never a wider seam applied
			// to the row, and never the whole row shifted by hand.
			for i in 0..HOTBAR_SLOTS - 1 {
				assert_eq!(
					hits.rect_of(i).unwrap().x,
					grid.rect(i).x,
					"block slot {i} is off the lattice at {size:?}/{scale}x"
				);
			}
			assert_eq!(
				last.x,
				grid.rect(HOTBAR_SLOTS - 1).x + TOOL_SPLIT * scale,
				"the tool slot is its lattice column plus the split"
			);
			assert_eq!(first.w, SLOT_PX * scale, "slot size scales");
			assert_eq!(
				first.y + first.h,
				size.1 as f32 - BOTTOM_MARGIN * scale,
				"bottom anchor scales"
			);
		}
	}
}

/// The rail grows rightward from the bar into whatever the centred bar
/// leaves — which is LESS the bigger the interface scale, because the
/// bar itself takes more of the window. Nothing it draws may cross the
/// right edge, at any scale, on any window, with the full roster owned.
#[test]
fn the_tool_rail_never_runs_off_the_window() {
	use digblock_core::block::BlockType as B;
	let owned: Vec<B> = B::ALL.into_iter().filter(|b| b.is_tool()).collect();
	assert!(owned.len() >= 3, "the roster has to be able to overflow");
	for size in [(1280u32, 720u32), (960, 600), (800, 600), (640, 480)] {
		for scale in [1.0f32, 1.5, 2.0, 2.5, 3.0] {
			let mut dl = DrawList::default();
			let style = Style::new(scale);
			tool_rail(&mut dl, &style, size, &owned, Some(B::Bucket), false, 1.0);
			// Horizontal extent of every mark the rail makes — flat
			// quads, block icons, and the projected faces of a 3D one.
			let spans = [Layer::Backdrop, Layer::Icons, Layer::Content, Layer::Ghost]
				.into_iter()
				.flat_map(|l| dl.cmds(l).0)
				.filter_map(|cmd| match cmd {
					Cmd::Quad { rect, .. } | Cmd::Icon { rect, .. } => {
						Some((rect.x, rect.x + rect.w))
					}
					Cmd::IconQuad { corners, .. } => Some((
						corners.iter().fold(f32::MAX, |m, c| m.min(c[0])),
						corners.iter().fold(f32::MIN, |m, c| m.max(c[0])),
					)),
					_ => None,
				});
			let mut drawn = 0;
			for (lo, hi) in spans {
				drawn += 1;
				assert!(
					hi <= size.0 as f32 + 0.5,
					"{size:?} at {scale}x: rail reaches {hi} past {}",
					size.0
				);
				assert!(lo >= 0.0, "{size:?} at {scale}x: rail starts off-screen");
			}
			// Not vacuous: where there IS room, the rail still draws.
			if size == (1280, 720) && scale <= 1.5 {
				assert!(drawn > 0, "{size:?} at {scale}x drew nothing at all");
			}
		}
	}
}

/// §6: the last slot is the TOOL slot, not the ninth block slot, and
/// it says so by standing apart. It steps out of the lattice to the
/// right rather than pushing the row over, so nothing that was
/// aligned to the bar has to move — and the gap it leaves is a real
/// break, not a slightly wider seam.
#[test]
fn the_tool_slot_stands_apart_from_the_block_row() {
	for scale in [1.0f32, 2.0, 3.0] {
		let (_, hits) = emit(scale);
		let seam = |a: usize, b: usize| {
			let (a, b) = (hits.rect_of(a).unwrap(), hits.rect_of(b).unwrap());
			b.x - (a.x + a.w)
		};
		let between_blocks = seam(0, 1);
		let before_tool = seam(HOTBAR_SLOTS - 2, HOTBAR_SLOTS - 1);
		assert_eq!(between_blocks, SLOT_GAP * scale, "the row's own seam");
		assert_eq!(before_tool, (SLOT_GAP + TOOL_SPLIT) * scale);
		assert!(
			before_tool > between_blocks * 3.0,
			"a break, not a seam: {before_tool} vs {between_blocks}"
		);
	}
}

/// Device-shaped (size, scale) pairs: on real hardware the interface
/// scale rides the display density, which also multiplies the physical
/// size, so the two grow together. A huge scale in a tiny window is
/// not a device, and the touch HUD is only drawn on devices.
const TOUCH_SWEEP: [((u32, u32), f32); 5] = [
	((1280, 720), 1.0),
	((1688, 780), 1.5),
	((1920, 1080), 2.0),
	((2532, 1170), 2.25),
	((2778, 1284), 2.5),
];

fn sweep_insets() -> [SafeArea; 2] {
	[
		SafeArea::default(),
		SafeArea {
			top: 20.0,
			right: 44.0,
			bottom: 20.0,
			left: 44.0,
		},
	]
}

/// The drawn buttons and the hit test are the same rect functions, so
/// they cannot drift — this pins that every mark `touch_buttons` makes
/// lands inside a rect `touch_hit` answers for, and that each rect's
/// center answers its own id.
#[test]
fn touch_hit_matches_the_drawn_buttons() {
	for ((w, h), scale) in TOUCH_SWEEP {
		for insets in sweep_insets() {
			for jump_button in [false, true] {
				let size = (w, h);
				let corner = corner_button_rect(size, scale, &insets);
				let place = place_button_rect(size, scale, &insets);
				let jump = jump_button_rect(size, scale, &insets);
				let center = |r: super::super::Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
				let hit = |x: f32, y: f32| touch_hit(size, scale, &insets, jump_button, x, y);
				let (cx, cy) = center(corner);
				assert_eq!(hit(cx, cy), Some(HudButton::Corner));
				let (px, py) = center(place);
				assert_eq!(hit(px, py), Some(HudButton::Place));
				let (jx, jy) = center(jump);
				assert_eq!(
					hit(jx, jy),
					jump_button.then_some(HudButton::Jump),
					"the jump rect is only live in Button mode"
				);
				// The screen's middle belongs to look/dig, never a button.
				assert_eq!(hit(w as f32 / 2.0, h as f32 / 2.0), None);

				let mut dl = DrawList::default();
				touch_buttons(&mut dl, scale, size, &insets, false, jump_button);
				let rects = [corner, place, jump];
				for layer in [Layer::Backdrop, Layer::Content] {
					for cmd in dl.cmds(layer).0 {
						let Cmd::Quad { rect, .. } = cmd else {
							panic!("touch buttons draw quads only")
						};
						assert!(
							rects
								.iter()
								.any(|r| rect.x >= r.x - 0.5
									&& rect.y >= r.y - 0.5 && rect.x + rect.w
									<= r.x + r.w + 0.5 && rect.y + rect.h <= r.y + r.h + 0.5),
							"a mark at {rect:?} lies outside every button"
						);
					}
				}
			}
		}
	}
}

/// The thumb cluster hangs above the hotbar row and the corner button
/// above everything — nothing the touch HUD owns may overlap a bar
/// slot (whose taps mean "select"), and every button stays on screen
/// inside the safe area.
#[test]
fn touch_buttons_stay_on_screen_and_clear_of_the_bar() {
	for ((w, h), scale) in TOUCH_SWEEP {
		for insets in sweep_insets() {
			let size = (w, h);
			let grid = row_grid(size, scale);
			let buttons = [
				corner_button_rect(size, scale, &insets),
				place_button_rect(size, scale, &insets),
				jump_button_rect(size, scale, &insets),
			];
			for b in buttons {
				assert!(b.x >= insets.left && b.y >= insets.top, "{b:?} off-screen");
				assert!(
					b.x + b.w <= w as f32 - insets.right && b.y + b.h <= h as f32 - insets.bottom,
					"{b:?} crosses the safe-area edge on {size:?}/{scale}x"
				);
				for i in 0..HOTBAR_SLOTS {
					let s = bar_slot_rect(&grid, i, scale);
					let overlap =
						b.x < s.x + s.w && s.x < b.x + b.w && b.y < s.y + s.h && s.y < b.y + b.h;
					assert!(!overlap, "{b:?} overlaps bar slot {i}");
				}
			}
		}
	}
}

/// Counts used to be pinned at `scale::FIXED_GLYPH_PX` while the slots
/// around them scaled, so on a dense display the stack numbers stayed
/// put while the UI around them grew. They ride the frame's text size
/// now.
#[test]
fn counts_scale_with_the_slots() {
	const SHADOW: u32 = 0xE0000000;
	let (dl, _) = emit(2.0);
	let (cmds, arena) = dl.cmds(Layer::Content);
	let texts: Vec<(&str, f32, u32)> = cmds
		.iter()
		.filter_map(|c| match c {
			Cmd::Text {
				px, range, color, ..
			} => Some((&arena[range.0 as usize..range.1 as usize], *px, *color)),
			_ => None,
		})
		.collect();
	// Both stocked block slots print their counts (the tool and the
	// empties don't), each as a shadow + digit pair.
	let (shadows, counts): (Vec<_>, Vec<_>) = texts.iter().partition(|(_, _, c)| *c == SHADOW);
	assert_eq!(counts.len(), 2);
	assert_eq!(shadows.len(), 2, "every count carries a drop shadow");
	let want = Style::new(2.0).glyph;
	for (text, px, _) in &texts {
		assert_eq!(*px, want, "count {text} should ride the frame's size");
	}
	for stock in [DIRT_STOCK, STONE_STOCK] {
		let want = stock.to_string();
		assert!(
			counts.iter().any(|(t, _, _)| *t == want),
			"{want} never reached the bar: {counts:?}"
		);
	}
}

#[test]
fn icons_and_selection_ride_the_right_layers() {
	let (dl, _) = emit(1.0);
	// Two cubes at three visible faces each, then the tool bucket.
	let icons = dl.cmds(Layer::Icons).0;
	assert!(icons.iter().all(|c| matches!(c, Cmd::IconQuad { .. })));
	assert!(icons.len() > 6, "two cubes plus a bucket");
	// Backdrop: one bg per slot, plus the selection's border quads.
	assert_eq!(dl.cmds(Layer::Backdrop).0.len(), HOTBAR_SLOTS + 4);
	// Every stocked slot draws at full tint — the grey zero-count
	// state is gone; an exhausted type leaves the bar entirely.
	let dirt: Vec<u32> = icons[..3]
		.iter()
		.map(|c| match c {
			Cmd::IconQuad { tint, .. } => *tint,
			_ => unreachable!(),
		})
		.collect();
	assert!(dirt.contains(&FULL_TINT), "top face is untinted: {dirt:?}");
}

/// An essence is the one flat icon: a single sprite quad in its own
/// layer, filling the rect it was given, with no block behind it.
#[test]
fn an_essence_icon_is_one_flat_sprite() {
	use digblock_core::block::Essence;
	let rect = Rect::new(10.0, 20.0, 24.0, 24.0);
	let mut seen = 0;
	for e in Essence::ALL {
		let mut dl = DrawList::default();
		draw_icon(
			&mut dl,
			Layer::Icons,
			rect,
			IconKind::Essence(e),
			0xFFFF_FFFF,
		);
		let (cmds, _) = dl.cmds(Layer::Icons);
		let flats: Vec<(Rect, u16)> = cmds
			.iter()
			.filter_map(|c| match c {
				Cmd::Icon {
					rect, tex_layer, ..
				} => Some((*rect, *tex_layer)),
				_ => None,
			})
			.collect();
		assert_eq!(flats, vec![(rect, e.icon_tex())], "{e:?}");
		assert_eq!(cmds.len(), 1, "{e:?} emitted geometry too");
		assert_eq!(icon_block(IconKind::Essence(e)), None);
		seen += 1;
	}
	assert_eq!(seen, Essence::COUNT);
}
