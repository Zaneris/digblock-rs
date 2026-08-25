//! In-game HUD emission: the hotbar row, built from widgets. Pure — the
//! engine calls this each frame with its `HotbarView`; the same pass
//! registers the `Bar(i)` hit rects the inventory screen uses as drop
//! targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem};

use super::draw::Layer;
use super::widgets::{self, GridSpec, Ui, WHITE};
use crate::hotbar::{HotbarView, SlotView};

// Logical bases, × ui_scale.
pub const SLOT_PX: f32 = 56.0;
pub const SLOT_GAP: f32 = 4.0;
pub const ICON_PX: f32 = 48.0;
pub const BOTTOM_MARGIN: f32 = 12.0;
/// EXTRA space before the last slot (SKILLS-DESIGN §6): the tool slot is not the ninth block slot,
/// and standing it apart is the whole of how a player is told so. Added to ONE slot at DRAW time
/// ([`bar_slot_rect`]) rather than to [`row_grid`]'s spacing, which stays the uniform lattice
/// every panel measures itself against. [`row_grid`]'s CENTRING has to know: the drawn row is a
/// split wider than the lattice, so the origin centres `lattice + split`.
pub const TOOL_SPLIT: f32 = 20.0;

const FULL_TINT: u32 = 0xFFFF_FFFF;

/// What a 3D item icon renders as (see [`super::shaped_icon`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
	/// Plain block: a three-face dimetric cube.
	Cube(BlockType),
	/// Stairs/doors: the baked shape template, projected.
	Shaped(BlockType),
	/// The water tool: a wooden bucket whose charge is DRAWN, not tinted — the bar must answer it.
	WaterTool { full: bool },
	/// The Essence Siphon: funnel, stem and glowing bulb — its own geometry, not the cube fallback.
	Siphon,
	/// The Resonant Pick: a haft with a broad resonant head.
	Pick,
	/// The Master's Sigil: a crystal set in a standing metal plate.
	Sigil,
	/// A skill's crest: the sigil's plate carrying that skill's emblem.
	Crest(digblock_core::skills::Skill),
	/// An essence: the one FLAT icon, since a count is not a thing in the world.
	Essence(digblock_core::block::Essence),
}

/// The arrival cover: an OPAQUE full-screen fill with one centred line, held over a join or a
/// teleport until the server has shipped the journaled columns around the destination. Opaque
/// rather than the screens' translucent dim, because what is behind it is worldgen the journal is
/// about to overwrite, so showing any of it through shows the artifact. Emitted on `Ghost`, the
/// topmost layer, since it must cover the hotbar and crosshair too: nothing under it is real yet.
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

/// Icon (kind, tint) for a hotbar item — the ghost drawn under the cursor mid-drag uses the same
/// visuals as the slots. `tool_full` is the water tool's charge, which is player state rather than
/// part of the slot assignment, so it arrives alongside the item; every other item ignores it.
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

/// The block behind an icon: the inverse of [`item_icon`] / [`tool_icon`], and the ONE place
/// "which block is this icon" is answered. Both questions derived from it are properties of the
/// block and never a special case on a type — whether the item draws as fullbright ink
/// (`props().emissive`), and whether holding it lights the room (`props().light > 0`). **Every
/// icon has a row, tools included**; `None` is left for an icon with genuinely nothing behind it.
pub fn icon_block(kind: IconKind) -> Option<BlockType> {
	match kind {
		IconKind::Cube(b) | IconKind::Shaped(b) => Some(b),
		IconKind::WaterTool { .. } => Some(BlockType::Bucket),
		IconKind::Siphon => Some(BlockType::EssenceSiphon),
		IconKind::Pick => Some(BlockType::ResonantPick),
		IconKind::Sigil => Some(BlockType::MastersSigil),
		IconKind::Crest(s) => Some(crest_block(s)),
		IconKind::Essence(_) => None,
	}
}

/// The crest row for a skill — `unlock::crest`'s inverse, walked over the
/// catalog so the two cannot name different rows.
fn crest_block(skill: digblock_core::skills::Skill) -> BlockType {
	BlockType::ALL
		.into_iter()
		.find(|&b| digblock_core::unlock::crest(b) == Some(skill))
		.expect("every skill has a crest row")
}

/// Draw one item icon into `rect`. Every THING goes through the dimetric
/// projector — one consistent visual language; an essence is the one
/// flat sprite, since a count has no geometry to project.
pub fn draw_icon(
	dl: &mut super::DrawList,
	layer: Layer,
	rect: super::Rect,
	kind: IconKind,
	tint: u32,
) {
	match kind {
		IconKind::Essence(e) => dl.icon(layer, rect, e.icon_tex(), tint),
		_ => super::shaped_icon::emit(dl, layer, rect, kind, tint),
	}
}

/// The hotbar row's grid: bottom-centered. Shared with the inventory screen so its panel columns
/// align with the bar. The lattice this returns is nine even columns and nothing else — what gets
/// CENTERED is the row as drawn, which is that lattice plus the [`TOOL_SPLIT`] the last slot
/// steps out by. Centring the lattice alone centres something narrower than the picture, and the
/// difference lands entirely on the right-hand edge.
pub fn row_grid(size: (u32, u32), scale: f32) -> GridSpec {
	let (cell, gap) = (SLOT_PX * scale, SLOT_GAP * scale);
	let total_w = HOTBAR_SLOTS as f32 * cell + (HOTBAR_SLOTS as f32 - 1.0) * gap;
	let drawn_w = total_w + TOOL_SPLIT * scale;
	GridSpec {
		origin: (
			(size.0 as f32 - drawn_w) / 2.0,
			size.1 as f32 - BOTTOM_MARGIN * scale - cell,
		),
		cols: HOTBAR_SLOTS,
		cell,
		gap,
	}
}

/// One bar slot's rect: the uniform grid, plus [`TOOL_SPLIT`] of clear
/// air before the last one. The single place that knows the bar's last
/// slot is a tool rather than a block — the hit rects the inventory
/// screen drops into come from here too, so the target can't drift from
/// the picture.
pub fn bar_slot_rect(grid: &GridSpec, i: usize, scale: f32) -> super::Rect {
	let mut rect = grid.rect(i);
	if i == HOTBAR_SLOTS - 1 {
		rect.x += TOOL_SPLIT * scale;
	}
	rect
}

/// The bar slot the cursor is over, and its name — `None` over an empty slot, a bare tool slot, or
/// nothing at all. A PAGE asks this, never the bar: the bar draws after every screen at the
/// WINDOW's scale, while a page that has stepped the fit ladder is a rung smaller, and a frame has
/// exactly one text size. So the open page names the bar's slots at its own glyph, and this is the
/// one place that rule is written down — CLAUDE.md states it as "bar slots included".
pub fn hovered_bar_slot(
	size: (u32, u32),
	window_scale: f32,
	view: &HotbarView,
	cursor: (f32, f32),
) -> Option<(super::Rect, String)> {
	let grid = row_grid(size, window_scale);
	(0..HOTBAR_SLOTS)
		.map(|i| (bar_slot_rect(&grid, i, window_scale), view.slots[i]))
		.find(|(r, _)| r.contains(cursor.0, cursor.1))
		.and_then(|(r, slot)| slot.name().map(|n| (r, n)))
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
				// bar that stays put. A bare slot — no vessel crafted
				// yet — draws the frame alone, with nothing in it.
				icon_px *= present;
				tool.map(|t| (tool_icon(t, full), faded(FULL_TINT, present)))
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

/// The icon for a tool, keyed by the BLOCK — the axis [`item_icon`] does not have, since
/// `HotbarItem::WaterTool` names the tool SLOT and the roster in it is a separate question. Every
/// path that can name a tool comes through here. Tools have no texture layers of their own, so
/// each needs authored geometry — the old cube fallback drew a WATER cube for anything new.
pub fn tool_icon(tool: BlockType, bucket_full: bool) -> IconKind {
	match tool {
		BlockType::EssenceSiphon => IconKind::Siphon,
		BlockType::ResonantPick => IconKind::Pick,
		BlockType::MastersSigil => IconKind::Sigil,
		BlockType::MinersCrest | BlockType::AlchemistsCrest | BlockType::ArcanistsCrest => {
			IconKind::Crest(digblock_core::unlock::crest(tool).expect("the three crest rows"))
		}
		// The bucket, and anything a future row forgets to author: a cube
		// is wrong, but it is visibly wrong rather than invisible.
		BlockType::Bucket => IconKind::WaterTool { full: bucket_full },
		other => IconKind::Cube(other),
	}
}

/// §6's faded tool rail: the owned tools OTHER than the one being shown
/// in the slot, drawn as small reduced-alpha icons just right of it, in
/// the roster's own deterministic order. A READOUT, not a target — no
/// hit rects; tapping 9 on the selected tool slot is the interaction.
/// With a single owned tool nothing draws at all.
pub fn tool_rail(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	owned: &[digblock_core::block::BlockType],
	shown: Option<digblock_core::block::BlockType>,
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
	// The rail grows rightward from the bar's last slot into whatever the centred bar leaves — and
	// that space SHRINKS as the interface scale grows. A full rail at a large interface scale on a
	// narrow window ran the last icon clean off the edge, so squeeze the cells to fit, down to a
	// floor where they can still be told apart, and then draw only what actually fits.
	let n = owned.iter().filter(|&&t| Some(t) != shown).count();
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
	for &tool in owned.iter().filter(|&&t| Some(t) != shown).take(fits) {
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

// ---------------------------------------------------------------------
// Touch HUD: the on-screen controls a finger-first device needs (gated on
// `EngineConfig::touch_hud`). Layout and hit-testing are PURE functions
// shared by the draw pass and the engine's input routing — deliberately
// not the render-pass `ui_hits`, whose rects are one frame stale exactly
// when a screen has just opened or closed under the finger.

/// Safe-area insets in physical px (notch, home indicator, rounded
/// corners), forwarded by shells that can measure them. Zero otherwise.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct SafeArea {
	pub top: f32,
	pub right: f32,
	pub bottom: f32,
	pub left: f32,
}

/// A tappable region of the touch HUD.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HudButton {
	/// Top-right: opens the pause menu in-game, closes an open screen.
	Corner,
	/// Thumb cluster: place/use at the crosshair (an edge, one per tap).
	Place,
	/// Thumb cluster: hold to jump. Drawn only when the jump-mode setting selects the button.
	Jump,
	/// A hotbar slot: tap to select; the tool slot cycles when already selected, as digits do.
	Slot(u8),
}

// Logical bases, × ui_scale — local to the touch HUD, so they live here
// rather than in theme.rs.
const CORNER_BTN_PX: f32 = 44.0;
/// Thumb-cluster buttons are bigger than the corner one: they are held
/// and hammered mid-motion, not visited.
const THUMB_BTN_PX: f32 = 64.0;
const TOUCH_MARGIN: f32 = 10.0;
const THUMB_GAP: f32 = 12.0;
/// Button glyph stroke thickness.
const GLYPH_STROKE: f32 = 4.0;
const BTN_INK: u32 = 0xC0FFFFFF;

pub fn corner_button_rect(size: (u32, u32), scale: f32, insets: &SafeArea) -> super::Rect {
	let px = CORNER_BTN_PX * scale;
	super::Rect::new(
		size.0 as f32 - insets.right - TOUCH_MARGIN * scale - px,
		insets.top + TOUCH_MARGIN * scale,
		px,
		px,
	)
}

/// The place button sits at the bottom of the thumb cluster — nearest the
/// resting thumb — anchored above the hotbar row so neither it nor the
/// tool rail can ever collide with it.
pub fn place_button_rect(size: (u32, u32), scale: f32, insets: &SafeArea) -> super::Rect {
	let px = THUMB_BTN_PX * scale;
	let bar_top = row_grid(size, scale).origin.1;
	super::Rect::new(
		size.0 as f32 - insets.right - TOUCH_MARGIN * scale - px,
		bar_top - THUMB_GAP * scale - px,
		px,
		px,
	)
}

/// Stacked directly above the place button.
pub fn jump_button_rect(size: (u32, u32), scale: f32, insets: &SafeArea) -> super::Rect {
	let place = place_button_rect(size, scale, insets);
	super::Rect::new(
		place.x,
		place.y - THUMB_GAP * scale - place.h,
		place.w,
		place.h,
	)
}

/// Which HUD button (if any) a touch at physical `(x, y)` lands on.
/// `jump_button` mirrors the jump-mode setting: when the double-tap is
/// the jump, there is no button to hit.
pub fn touch_hit(
	size: (u32, u32),
	scale: f32,
	insets: &SafeArea,
	jump_button: bool,
	x: f32,
	y: f32,
) -> Option<HudButton> {
	if corner_button_rect(size, scale, insets).contains(x, y) {
		return Some(HudButton::Corner);
	}
	if place_button_rect(size, scale, insets).contains(x, y) {
		return Some(HudButton::Place);
	}
	if jump_button && jump_button_rect(size, scale, insets).contains(x, y) {
		return Some(HudButton::Jump);
	}
	let grid = row_grid(size, scale);
	(0..HOTBAR_SLOTS)
		.find(|&i| bar_slot_rect(&grid, i, scale).contains(x, y))
		.map(|i| HudButton::Slot(i as u8))
}

/// A horizontal bar of the glyph stroke, centered spans expressed as
/// fractions of the button rect.
fn glyph_bar(dl: &mut super::DrawList, r: super::Rect, fx: f32, fw: f32, fy: f32, t: f32) {
	dl.quad(
		Layer::Content,
		super::Rect::new(r.x + r.w * fx, r.y + r.h * fy - t / 2.0, r.w * fw, t),
		BTN_INK,
	);
}

/// The corner button alone: a ✕ when `close` (an open screen, or a menu page with somewhere to go
/// back to), a hamburger otherwise. Public because the MENU draws it too, and draws nothing else
/// of the touch HUD — a finger needs the same way out of the server browser that it has out of
/// the inventory. One function, so the two cannot come to look like different controls.
pub fn corner_button(
	dl: &mut super::DrawList,
	scale: f32,
	size: (u32, u32),
	insets: &SafeArea,
	close: bool,
) {
	let t = GLYPH_STROKE * scale;
	let corner = corner_button_rect(size, scale, insets);
	dl.quad(Layer::Backdrop, corner, super::theme::FILL_BG);
	if close {
		// A pixel ✕: stepped squares along both diagonals — axis-aligned
		// quads only, in the UI's blocky idiom.
		const STEPS: i32 = 5;
		let (cx, cy) = (corner.x + corner.w / 2.0, corner.y + corner.h / 2.0);
		let span = corner.w * 0.28;
		for i in -(STEPS / 2)..=(STEPS / 2) {
			let d = i as f32 / (STEPS / 2) as f32 * span;
			for sx in [-1.0, 1.0] {
				dl.quad(
					Layer::Content,
					super::Rect::new(cx + d * sx - t / 2.0, cy + d - t / 2.0, t, t),
					BTN_INK,
				);
			}
		}
	} else {
		// Hamburger: the menu lives here.
		for fy in [0.32, 0.5, 0.68] {
			glyph_bar(dl, corner, 0.26, 0.48, fy, t);
		}
	}
}

/// Draw the touch HUD's buttons. `screen_open` swaps the corner glyph to
/// a close mark and suppresses the thumb cluster (the world verbs mean
/// nothing under a screen); `jump_button` mirrors the jump-mode setting.
pub fn touch_buttons(
	dl: &mut super::DrawList,
	scale: f32,
	size: (u32, u32),
	insets: &SafeArea,
	screen_open: bool,
	jump_button: bool,
) {
	let t = GLYPH_STROKE * scale;
	corner_button(dl, scale, size, insets, screen_open);
	if screen_open {
		return;
	}
	let place = place_button_rect(size, scale, insets);
	dl.quad(Layer::Backdrop, place, super::theme::FILL_BG);
	// A block outline with a centred plus: "put one here".
	let inset = place.w * 0.26;
	let b = super::Rect::new(
		place.x + inset,
		place.y + inset,
		place.w - 2.0 * inset,
		place.h - 2.0 * inset,
	);
	dl.quad(Layer::Content, super::Rect::new(b.x, b.y, b.w, t), BTN_INK);
	dl.quad(
		Layer::Content,
		super::Rect::new(b.x, b.y + b.h - t, b.w, t),
		BTN_INK,
	);
	dl.quad(Layer::Content, super::Rect::new(b.x, b.y, t, b.h), BTN_INK);
	dl.quad(
		Layer::Content,
		super::Rect::new(b.x + b.w - t, b.y, t, b.h),
		BTN_INK,
	);
	glyph_bar(dl, place, 0.42, 0.16, 0.5, t);
	dl.quad(
		Layer::Content,
		super::Rect::new(
			place.x + place.w / 2.0 - t / 2.0,
			place.y + place.h * 0.42,
			t,
			place.h * 0.16,
		),
		BTN_INK,
	);
	if jump_button {
		let jump = jump_button_rect(size, scale, insets);
		dl.quad(Layer::Backdrop, jump, super::theme::FILL_BG);
		// A pixel chevron pointing up: three stepped rows.
		let (cx, w) = (jump.x + jump.w / 2.0, jump.w * 0.14);
		for (i, fy) in [0.36, 0.48, 0.6].into_iter().enumerate() {
			let dx = i as f32 * w;
			for sx in [-1.0, 1.0] {
				dl.quad(
					Layer::Content,
					super::Rect::new(
						cx + dx * sx - if sx < 0.0 { w } else { 0.0 },
						jump.y + jump.h * fy,
						w,
						t,
					),
					BTN_INK,
				);
			}
		}
	}
}

/// The live joystick's visualization: an outline ring square at the
/// anchor the touch went down on, and a filled nub at the finger, clamped
/// to the ring. Presentation only — the driver's axes are the truth.
pub fn joystick_overlay(
	dl: &mut super::DrawList,
	scale: f32,
	anchor: (f32, f32),
	finger: (f32, f32),
) {
	const RING: u32 = 0x50FFFFFF;
	const NUB: u32 = 0x90FFFFFF;
	/// Visual ring half-size (logical px). A picture of the stick, not
	/// its calibration — the driver's deflection constant is the truth.
	const JOY_VIS_R: f32 = 44.0;
	const NUB_PX: f32 = 22.0;
	let r = JOY_VIS_R * scale;
	let t = 2.0 * scale;
	let (ax, ay) = anchor;
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(ax - r, ay - r, 2.0 * r, t),
		RING,
	);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(ax - r, ay + r - t, 2.0 * r, t),
		RING,
	);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(ax - r, ay - r, t, 2.0 * r),
		RING,
	);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(ax + r - t, ay - r, t, 2.0 * r),
		RING,
	);
	let (mut dx, mut dy) = (finger.0 - ax, finger.1 - ay);
	let len = (dx * dx + dy * dy).sqrt();
	if len > r {
		dx *= r / len;
		dy *= r / len;
	}
	let n = NUB_PX * scale;
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(ax + dx - n / 2.0, ay + dy - n / 2.0, n, n),
		NUB,
	);
}

/// What a toast counts: XP for a skill, or essence a draw paid. One
/// lane and one shape for both so a draw's two toasts (the XP and the
/// essence) stack in the order they arrived instead of fighting for the
/// same pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastLabel {
	Xp(digblock_core::skills::Skill),
	Essence(digblock_core::block::Essence),
}

/// One live "+N LABEL" toast above the hotbar. The engine owns the
/// state — [`toasts`] just draws whatever is live.
pub struct Toast {
	pub label: ToastLabel,
	pub amount: u64,
	/// Seconds left; refreshed to [`TOAST_TTL`] by each new award.
	pub ttl: f32,
}

pub const TOAST_TTL: f32 = 2.0;
/// The fade window at the end of a toast's life.
const TOAST_FADE: f32 = 0.5;

/// Scale a 0xAABBGGRR color's alpha byte.
fn faded(color: u32, f: f32) -> u32 {
	let a = ((color >> 24) as f32 * f.clamp(0.0, 1.0)) as u32;
	a << 24 | (color & 0x00FF_FFFF)
}

/// Draw the live toasts, stacked upward above the hotbar, centered
/// like the bar itself. Text rides the frame's size; the backdrop is
/// sized from the face's line height (ink legitimately overflows the
/// nominal box — accents above, descenders below).
pub fn toasts(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	toasts: &[Toast],
) {
	let scale = style.scale;
	let px = style.glyph;
	let line = crate::font::face(style.face).line_h(px);
	let pad_x = super::scale::text_pad_x(scale);
	let gap = 4.0 * scale;
	let mut bottom = row_grid(size, scale).origin.1 - super::theme::GAP_ABOVE_BAR * scale;
	let chip_gap = super::theme::CHIP_GAP * scale;
	for toast in toasts.iter().rev() {
		let fade = (toast.ttl / TOAST_FADE).clamp(0.0, 1.0);
		// An XP toast says the skill; an essence toast shows the chip —
		// an amount of essence is never a word.
		let (text, icon) = match toast.label {
			ToastLabel::Xp(skill) => (
				format!("+{} {}", toast.amount, skill.name().to_uppercase()),
				None,
			),
			ToastLabel::Essence(e) => (format!("+{}", toast.amount), Some(IconKind::Essence(e))),
		};
		let text_w = style.measure(&text);
		let w = text_w + icon.map_or(0.0, |_| chip_gap + px) + 2.0 * pad_x;
		let rect = super::Rect::new((size.0 as f32 - w) / 2.0, bottom - line, w, line);
		dl.quad(Layer::Backdrop, rect, faded(super::theme::FILL_BG, fade));
		let top = rect.y + (line - px) / 2.0;
		dl.text(
			Layer::Content,
			rect.x + pad_x,
			top,
			px,
			faded(WHITE, fade),
			&text,
		);
		if let Some(kind) = icon {
			draw_icon(
				dl,
				Layer::Icons,
				super::Rect::new(rect.x + pad_x + text_w + chip_gap, top, px, px),
				kind,
				faded(WHITE, fade),
			);
		}
		bottom = rect.y - gap;
	}
}

/// The background bench's tell (SKILLS-DESIGN §13.4): one quiet line above the bar's TOOL end
/// while a job runs with the screen closed — the bench working out of sight has to be visible
/// somewhere, or the first background extraction reads as "my essence changed on its own". It
/// sits over the tool slot rather than the toasts' center lane, so the two never collide.
pub fn bench_tell(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	text: &str,
) {
	let scale = style.scale;
	let px = style.glyph;
	let line = crate::font::face(style.face).line_h(px);
	let pad_x = super::scale::text_pad_x(scale);
	let grid = row_grid(size, scale);
	// Right-aligned to the DRAWN last slot — the tool slot, a split past
	// the lattice — for the same reason the centring test measures it:
	// the lattice's own right edge is not where the row ends.
	let tool = bar_slot_rect(&grid, HOTBAR_SLOTS - 1, scale);
	let w = style.measure(text) + 2.0 * pad_x;
	let rect = super::Rect::new(
		tool.x + tool.w - w,
		grid.origin.1 - super::theme::GAP_ABOVE_BAR * scale - line,
		w,
		line,
	);
	dl.quad(Layer::Backdrop, rect, super::theme::FILL_BG);
	dl.text(
		Layer::Content,
		rect.x + pad_x,
		rect.y + (line - px) / 2.0,
		px,
		super::widgets::DIM,
		text,
	);
}

#[cfg(test)]
mod tests;
