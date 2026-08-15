//! In-game HUD emission: the hotbar row, built from widgets. Pure — the
//! engine calls this each frame with its `HotbarView`; the same pass
//! registers the `Bar(i)` hit rects the inventory screen uses as drop
//! targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS};

use super::draw::Layer;
use super::widgets::{self, GridSpec, Ui, WHITE};
use crate::hotbar::{HotbarView, SlotView};

// Logical bases, × ui_scale.
pub const SLOT_PX: f32 = 56.0;
pub const SLOT_GAP: f32 = 4.0;
pub const ICON_PX: f32 = 48.0;
pub const BOTTOM_MARGIN: f32 = 12.0;
/// EXTRA space before the last slot (SKILLS-DESIGN §6): the tool slot is
/// not the ninth block slot, and standing it apart is the whole of how a
/// player is told so — the icon in it is a tool whatever the bar holds.
///
/// It is added at DRAW time rather than folded into [`row_grid`], which
/// stays the uniform nine-column geometry every panel measures itself
/// against: the inventory grid's columns keep landing on the block slots
/// they drop into, and the split is what the tool slot hangs off the
/// right-hand end by.
pub const TOOL_SPLIT: f32 = 20.0;

const FULL_TINT: u32 = 0xFFFF_FFFF;

/// What a 3D item icon renders as (see [`super::shaped_icon`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
	/// Plain block: a three-face dimetric cube.
	Cube(BlockType),
	/// Stairs/doors: the baked shape template, projected.
	Shaped(BlockType),
	/// The water tool: a little wooden bucket, with a source in it or
	/// not. The charge is drawn, not tinted — "have I got water?" is the
	/// question the bar has to answer at a glance.
	WaterTool { full: bool },
	/// The Essence Siphon: a funnel, a stem, and a glowing bulb. Its own
	/// geometry rather than the cube fallback, because a tool with no
	/// texture layers of its own would otherwise draw as a water cube.
	Siphon,
	/// The Resonant Pick: a haft with a broad resonant head.
	Pick,
	/// The Master's Sigil: a crystal set in a standing metal plate.
	Sigil,
}

/// The arrival cover: an OPAQUE full-screen fill with one centred line,
/// held over a join or a teleport until the server has shipped the
/// journaled columns around the destination.
///
/// Opaque rather than the screens' translucent dim on purpose — the whole
/// point is that what is behind it is worldgen the journal is about to
/// overwrite, so showing any of it through is showing the artifact. It is
/// emitted on `Ghost`, the topmost layer, because it has to cover the
/// hotbar and the crosshair too: nothing under it is real yet.
pub fn arrival_cover(
	dl: &mut super::DrawList,
	style: &super::Style,
	size: (u32, u32),
	label: &str,
) {
	const COVER: u32 = 0xFF0A0A0A;
	const INK: u32 = 0xFFE8E8E8;
	let (w, h) = (size.0 as f32, size.1 as f32);
	dl.quad(Layer::Ghost, super::Rect::new(0.0, 0.0, w, h), COVER);
	let glyph = style.glyph;
	let text_w = crate::font::ui().measure(label, glyph);
	dl.text(
		Layer::Ghost,
		(w - text_w) / 2.0,
		(h - glyph) / 2.0,
		glyph,
		INK,
		label,
	);
}

/// The aim crosshair (hidden while any screen owns the cursor).
pub fn crosshair(dl: &mut super::DrawList, size: (u32, u32), scale: f32) {
	const COLOR: u32 = 0xC0FFFFFF;
	let arm = 9.0 * scale; // half-length in px
	let thick = 2.0 * scale;
	let (cx, cy) = (size.0 as f32 / 2.0, size.1 as f32 / 2.0);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(cx - arm, cy - thick / 2.0, arm * 2.0, thick),
		COLOR,
	);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(cx - thick / 2.0, cy - arm, thick, arm * 2.0),
		COLOR,
	);
}

/// Icon (kind, tint) for a hotbar item — the ghost drawn under the
/// cursor mid-drag uses the same visuals as the slots.
///
/// `tool_full` is the water tool's charge, which is player state rather
/// than part of the slot assignment, so it has to arrive alongside the
/// item; every other item ignores it.
pub fn item_icon(item: HotbarItem, tool_full: bool) -> (IconKind, u32) {
	match item {
		HotbarItem::WaterTool => (IconKind::WaterTool { full: tool_full }, FULL_TINT),
		// A tool has no texture layers of its own, so `Cube` would draw it
		// as WATER — which is what the transmute screen did to the siphon
		// until this arm existed. Every path that can name a tool routes
		// through `tool_icon`, screens included.
		HotbarItem::Block(b) if b.is_tool() => (tool_icon(b, tool_full), FULL_TINT),
		HotbarItem::Block(b) if b.props().shape.is_some() => (IconKind::Shaped(b), FULL_TINT),
		HotbarItem::Block(b) => (IconKind::Cube(b), FULL_TINT),
	}
}

/// Draw one 3D item icon into `rect` (every icon path goes through the
/// dimetric projector — one consistent visual language).
pub fn draw_icon(
	dl: &mut super::DrawList,
	layer: Layer,
	rect: super::Rect,
	kind: IconKind,
	tint: u32,
) {
	super::shaped_icon::emit(dl, layer, rect, kind, tint);
}

/// The hotbar row's grid: bottom-centered. Shared with the inventory
/// screen so its panel columns align with the bar.
pub fn row_grid(size: (u32, u32), scale: f32) -> GridSpec {
	let (cell, gap) = (SLOT_PX * scale, SLOT_GAP * scale);
	let total_w = HOTBAR_SLOTS as f32 * cell + (HOTBAR_SLOTS as f32 - 1.0) * gap;
	GridSpec {
		origin: (
			(size.0 as f32 - total_w) / 2.0,
			size.1 as f32 - BOTTOM_MARGIN * scale - cell,
		),
		cols: HOTBAR_SLOTS,
		cell,
		gap,
	}
}

/// One bar slot's rect: the uniform grid, plus [`TOOL_SPLIT`] of clear
/// air before the last one. The single place that knows the bar is 8
/// block slots and a tool — the hit rects the inventory screen drops
/// into come from here too, so the target can't drift from the picture.
pub fn bar_slot_rect(grid: &GridSpec, i: usize, scale: f32) -> super::Rect {
	let mut rect = grid.rect(i);
	if i == HOTBAR_SLOTS - 1 {
		rect.x += TOOL_SPLIT * scale;
	}
	rect
}

/// Emit the hotbar: slot widgets (backdrops, icons, selection border,
/// hit rects under `id_of(i)`) plus stack counts, drawn at the frame's
/// text size like the rest of the UI.
pub fn hotbar<Id: Copy + PartialEq>(
	ui: &mut Ui<'_, Id>,
	size: (u32, u32),
	view: &HotbarView,
	id_of: impl Fn(usize) -> Id,
) {
	let scale = ui.style.scale;
	let grid = row_grid(size, scale);
	let base_icon = ICON_PX * scale;
	for i in 0..HOTBAR_SLOTS {
		let rect = bar_slot_rect(&grid, i, scale);
		let mut icon_px = base_icon;
		let icon = match view.slots[i] {
			SlotView::Empty => None,
			SlotView::Tool {
				tool,
				full,
				present,
			} => {
				// Mid-handover the icon shrinks and fades; the SLOT
				// itself doesn't move, so the exchange happens inside a
				// bar that stays put.
				icon_px *= present;
				Some((tool_icon(tool, full), faded(FULL_TINT, present)))
			}
			SlotView::Block { block, .. } => Some(item_icon(HotbarItem::Block(block), false)),
		};
		// The HUD bar has no cursor: rings only, never hover.
		let ring = if i == view.selected {
			widgets::SlotRing::Strong
		} else {
			widgets::SlotRing::None
		};
		widgets::slot(ui, id_of(i), rect, icon, icon_px, ring, false);
		if let SlotView::Block { count, .. } = view.slots[i] {
			widgets::count_badge(ui.dl, Layer::Content, &ui.style, rect, count, WHITE);
		}
	}
}

/// §6's faded tool rail: the owned tools OTHER than the active one,
/// drawn as small reduced-alpha icons just right of the tool slot, in
/// the roster's own deterministic order. A READOUT, not a target — no
/// hit rects; tapping 9 on the selected tool slot is the interaction.
/// With a single owned tool nothing draws at all.
/// The icon for a tool. Tools have no texture layers of their own (they
/// never render in the world), so every one of them needs authored
/// geometry — the old cube fallback drew a WATER cube for anything new.
pub fn tool_icon(tool: BlockType, bucket_full: bool) -> IconKind {
	match tool {
		BlockType::EssenceSiphon => IconKind::Siphon,
		BlockType::ResonantPick => IconKind::Pick,
		BlockType::MastersSigil => IconKind::Sigil,
		// The bucket, and anything a future row forgets to author: a cube
		// is wrong, but it is visibly wrong rather than invisible.
		BlockType::Bucket => IconKind::WaterTool { full: bucket_full },
		other => IconKind::Cube(other),
	}
}

pub fn tool_rail(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	owned: &[digblock_core::block::BlockType],
	shown: digblock_core::block::BlockType,
	bucket_full: bool,
	present: f32,
) {
	if owned.len() < 2 {
		return;
	}
	const RAIL_TINT: u32 = 0x70FF_FFFF;
	/// Rail slot size — subordinate to the bar's own [`SLOT_PX`] at a
	/// glance, but big enough that WHICH tool it is can be read, which is
	/// the only thing the rail is for.
	const RAIL_CELL: f32 = 34.0;
	const RAIL_ICON_PAD: f32 = 3.0;
	/// How far the cells may shrink to fit. Below this a tool icon is a
	/// smudge, and a rail you cannot read is worth less than a short one.
	const RAIL_MIN_SQUEEZE: f32 = 0.6;
	let scale = style.scale;
	let grid = row_grid(size, scale);
	let slot = bar_slot_rect(&grid, HOTBAR_SLOTS - 1, scale);
	let start = slot.x + slot.w + SLOT_GAP * scale * 2.0;
	// The rail grows rightward from the bar's last slot into whatever the
	// centred bar leaves — and that space SHRINKS as the interface scale
	// grows, because the bar takes more of the window. Three owned tools
	// at 1.5× on a narrow window ran the last icon clean off the edge.
	// Squeeze the cells to fit, down to a floor where they can still be
	// told apart, and then draw only what actually fits.
	let n = owned.iter().filter(|&&t| t != shown).count();
	let room = (size.0 as f32 - start - SLOT_GAP * scale * 2.0).max(0.0);
	let span = |c: f32, g: f32| n as f32 * c + (n as f32 - 1.0) * g;
	let full = span(RAIL_CELL * scale, 6.0 * scale);
	let squeeze = if full > room && full > 0.0 {
		(room / full).max(RAIL_MIN_SQUEEZE)
	} else {
		1.0
	};
	let cell = RAIL_CELL * scale * squeeze;
	let gap = 6.0 * scale * squeeze;
	let fits = if cell + gap > 0.0 {
		(((room + gap) / (cell + gap)) as usize).min(n)
	} else {
		0
	};
	let mut x = start;
	let y = grid.origin.1 + grid.cell - cell;
	// The rail rides the same handover as the slot: `shown` is the tool
	// being DRAWN, so membership changes at the midpoint — where every
	// icon is at nothing — and no tool is ever in the slot and the rail
	// at the same time.
	let inset = cell * (1.0 - present) / 2.0;
	for &tool in owned.iter().filter(|&&t| t != shown).take(fits) {
		let kind = tool_icon(tool, bucket_full);
		let box_ = super::Rect::new(x, y, cell, cell);
		// A faded slot, not a bare icon: these sit over open world, and
		// a small dark tool against dark terrain was a smudge. The
		// backdrop is the bar's own fill at the rail's alpha, so the
		// rail reads as smaller, quieter slots of the same row.
		dl.quad(Layer::Backdrop, box_, faded(widgets::SLOT_BG, present));
		draw_icon(
			dl,
			Layer::Icons,
			super::Rect::new(
				x + inset + RAIL_ICON_PAD * scale,
				y + inset + RAIL_ICON_PAD * scale,
				(cell - 2.0 * RAIL_ICON_PAD * scale) * present,
				(cell - 2.0 * RAIL_ICON_PAD * scale) * present,
			),
			kind,
			faded(RAIL_TINT, present),
		);
		x += cell + gap;
	}
}

/// One coalescing XP toast: rapid awards to the same skill accumulate
/// into a single "+N SKILL" line instead of a scrolling storm. Engine
/// state — [`xp_toasts`] just draws whatever is live.
pub struct XpToast {
	pub skill: digblock_core::skills::Skill,
	pub amount: u64,
	/// Seconds left; refreshed to [`XP_TOAST_TTL`] by each new award.
	pub ttl: f32,
}

pub const XP_TOAST_TTL: f32 = 2.0;
/// The fade window at the end of a toast's life.
const XP_TOAST_FADE: f32 = 0.5;

/// Scale a 0xAABBGGRR color's alpha byte.
fn faded(color: u32, f: f32) -> u32 {
	let a = ((color >> 24) as f32 * f.clamp(0.0, 1.0)) as u32;
	a << 24 | (color & 0x00FF_FFFF)
}

/// Draw the live XP toasts, stacked upward above the hotbar, centered
/// like the bar itself. Text rides the frame's size; the backdrop is
/// sized from the face's line height (ink legitimately overflows the
/// nominal box — accents above, descenders below).
pub fn xp_toasts(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	toasts: &[XpToast],
) {
	let scale = style.scale;
	let px = style.glyph;
	let line = crate::font::face(style.face).line_h(px);
	let pad_x = super::scale::text_pad_x(scale);
	let gap = 4.0 * scale;
	let mut bottom = row_grid(size, scale).origin.1 - super::theme::GAP_ABOVE_BAR * scale;
	for toast in toasts.iter().rev() {
		let fade = (toast.ttl / XP_TOAST_FADE).clamp(0.0, 1.0);
		let text = format!("+{} {}", toast.amount, toast.skill.name());
		let w = style.measure(&text) + 2.0 * pad_x;
		let rect = super::Rect::new((size.0 as f32 - w) / 2.0, bottom - line, w, line);
		dl.quad(Layer::Backdrop, rect, faded(super::theme::FILL_BG, fade));
		dl.text(
			Layer::Content,
			rect.x + pad_x,
			rect.y + (line - px) / 2.0,
			px,
			faded(WHITE, fade),
			&text,
		);
		bottom = rect.y - gap;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::{Cmd, DrawList};
	use crate::ui::hit::HitMap;
	use crate::ui::widgets::Style;
	use digblock_core::hotbar::Hotbar;
	use digblock_core::inventory::Inventory;

	const SIZE: (u32, u32) = (1280, 720);

	fn sample_view() -> HotbarView {
		let mut hotbar = Hotbar::new();
		hotbar.auto_assign(BlockType::Dirt);
		hotbar.auto_assign(BlockType::Stone);
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 3);
		inv.add(BlockType::Stone, 12);
		HotbarView::build(&hotbar, &inv, &crate::hotbar::ToolSwap::default())
	}

	fn emit(scale: f32) -> (DrawList, HitMap<usize>) {
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: Style::new(scale),
		};
		hotbar(&mut ui, SIZE, &sample_view(), |i| i);
		(dl, hits)
	}

	/// The nine-column lattice is centered and scales — and the block row
	/// sits ON it, which is what keeps the inventory grid's columns over
	/// the slots they drop into.
	#[test]
	fn row_is_bottom_centered_and_scales() {
		for scale in [1.0f32, 2.0] {
			let (_, hits) = emit(scale);
			let grid = row_grid(SIZE, scale);
			let first = hits.rect_of(0).unwrap();
			let left = first.x;
			let right = SIZE.0 as f32 - (grid.origin.0 + grid.extent(HOTBAR_SLOTS).0);
			assert!((left - right).abs() < 0.5, "centered at {scale}x");
			for i in 0..HOTBAR_SLOTS - 1 {
				assert_eq!(
					hits.rect_of(i).unwrap().x,
					grid.rect(i).x,
					"block slot {i} is on the lattice at {scale}x"
				);
			}
			assert_eq!(first.w, SLOT_PX * scale, "slot size scales");
			assert_eq!(
				first.y + first.h,
				SIZE.1 as f32 - BOTTOM_MARGIN * scale,
				"bottom anchor scales"
			);
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
				tool_rail(&mut dl, &style, size, &owned, B::Bucket, false, 1.0);
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

	/// Counts used to be pinned at `scale::FIXED_GLYPH_PX` while the slots
	/// around them scaled, so on a dense display a 32px UI carried 16px
	/// stack numbers. They ride the frame's text size now.
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
		// Dirt count 3 and Stone count 12 print (tool and empties don't),
		// each as a shadow + digit pair.
		let (shadows, counts): (Vec<_>, Vec<_>) = texts.iter().partition(|(_, _, c)| *c == SHADOW);
		assert_eq!(counts.len(), 2);
		assert_eq!(shadows.len(), 2, "every count carries a drop shadow");
		let want = Style::new(2.0).glyph;
		for (text, px, _) in &texts {
			assert_eq!(*px, want, "count {text} should ride the frame's size");
		}
		assert!(counts.iter().any(|(t, _, _)| *t == "3"));
		assert!(counts.iter().any(|(t, _, _)| *t == "12"));
	}

	#[test]
	fn icons_and_selection_ride_the_right_layers() {
		let (dl, _) = emit(1.0);
		// Two cubes at three visible faces each, then the tool bucket.
		let icons = dl.cmds(Layer::Icons).0;
		assert!(icons.iter().all(|c| matches!(c, Cmd::IconQuad { .. })));
		assert!(icons.len() > 6, "two cubes plus a bucket");
		// Backdrop: 9 slot bgs + 4 selection border quads.
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
}
