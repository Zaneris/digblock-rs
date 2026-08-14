//! The alchemy screen (SKILLS-DESIGN §8.1): two tabs under a header
//! that always shows the Alchemy level, its XP bar, and the essence
//! holdings.
//!
//! **EXTRACT** is the player's inventory plus one drop target: drag a
//! stack (from the grid, the bar, or the bucket tile) onto the
//! extraction slot and it drips, one block per pace interval, until
//! removed or dry. The slot is a POINTER to a block id, never a
//! container — the stack stays in the inventory and visibly drains in
//! place, so the slot touches neither `reconcile_layout` nor the wire.
//! The drip is client-paced ordinary `Convert { count: 1 }` ops; the
//! server enforces the same pace with its extraction token bucket, so
//! pacing here is presentation, not trust.
//!
//! **TRANSMUTE** stays click-based (crafting is a decision, not a
//! grind): a row of FAMILY tiles whose icons slowly rotate through
//! their members, the selected family's members wrapped in a 9-column
//! grid beneath (locked ones dim with an unlock badge, affordable ones
//! count-badged), a detail strip, and the tab's ONE `TRANSMUTE` button
//! — CLICK 1 · SHIFT 10 · CTRL 100 clamped to what the holdings
//! afford. `every_craftable_appears_in_exactly_one_family` holds
//! `FAMILIES` to the core catalog, so a new craftable cannot silently
//! miss the screen.
//!
//! The screen never mutates game state: `handle_input`/`tick` return
//! actions; the engine applies them locally offline or sends them to
//! the server online (the `InventoryFull` echo is the rebase).

use digblock_core::alchemy;
use digblock_core::block::{BlockType, Essence};
use digblock_core::hotbar::HotbarItem;
use digblock_core::inv_grid::{InvGrid, GRID_SLOTS};
use digblock_core::inventory::Inventory;
use digblock_core::skills::{self, Skill, SkillSet};
use digblock_core::unlock;
use digblock_platform::{InputEvent, Key, MouseButton};

use crate::hotbar::{HotbarView, SlotView};
use crate::ui::drag::{DragEvent, DragState, DRAG_THRESHOLD_PX};
use crate::ui::draw::Layer;
use crate::ui::widgets::{self, GridSpec, Style, Ui, DIM, WHITE};
use crate::ui::{hud, scale, theme, HitMap, InvId, Rect};

/// One requested alchemy operation, in core vocabulary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AlchAction {
	Convert { block: BlockType, count: u32 },
	Craft { item: BlockType, count: u32 },
}

const BULK_SHIFT: u32 = 10;
const BULK_CTRL: u32 = 100;

// This screen's own geometry (theme tokens cover everything shared).
const ROW_ICON: f32 = 36.0;
const BTN_W: f32 = 152.0;
const BTN_H: f32 = 34.0;
/// Gap between an icon/slot and the text that describes it.
const ICON_TEXT_GAP: f32 = 12.0;
/// Gap between the tab strip's tabs.
const TAB_GAP: f32 = 12.0;
/// Gap between the tab strip and the first row of whichever tab is
/// open. A SECTION break, not a row gap: crossing from the tab into its
/// contents is the biggest boundary on the page, and at `ROW_GAP` the
/// first slot row read as part of the strip. Lives here rather than in
/// the theme because only this screen has tabs — a constant one screen
/// uses does not belong in the shared tokens.
const TAB_CONTENT_GAP: f32 = theme::SECTION_GAP;
/// The extraction progress bar's height, inset along the slot's bottom.
const PROGRESS_H: f32 = 4.0;
/// How long a rejected drop's explanation holds the preview line.
const HINT_SECS: f32 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlchTab {
	#[default]
	Extract,
	Transmute,
}

/// The screen's per-frame view: level, XP-bar fraction, essence
/// holdings, craft rows — derived once so measure and emit agree.
pub struct AlchView {
	pub level: u32,
	/// Progress toward the next level, 0..=1 (full at the cap).
	pub xp_frac: f32,
	pub subs: [u32; Essence::COUNT],
	pub bucket_full: bool,
}

impl AlchView {
	pub fn build(inv: &Inventory, skills_set: &SkillSet) -> Self {
		let level = skills_set.level(Skill::Alchemy);
		let xp = skills_set.xp(Skill::Alchemy);
		let xp_frac = match skills::xp_to_next(level) {
			Some(need) => ((xp - skills::total_xp_for(level)) as f32 / need as f32).clamp(0.0, 1.0),
			None => 1.0,
		};
		let mut subs = [0u32; Essence::COUNT];
		for (i, &e) in Essence::ALL.iter().enumerate() {
			subs[i] = inv.sub_count(e);
		}
		Self {
			level,
			xp_frac,
			subs,
			bucket_full: inv.bucket_full(),
		}
	}

	/// Whole batches of `item` the holdings afford at `level` (0 when
	/// locked or unaffordable).
	pub fn affordable(&self, item: BlockType) -> u32 {
		if !unlock::can_craft(item, self.level) {
			return 0;
		}
		let Some(ideal) = alchemy::craft_ideal(item) else {
			return 0;
		};
		// The bucket charge crafts one at a time by definition.
		let cap = if item == BlockType::Water {
			if self.bucket_full {
				return 0;
			}
			1
		} else {
			u32::MAX
		};
		ideal
			.iter()
			.map(|&(e, n)| self.subs[e as usize] / alchemy::craft_cost(n, self.level))
			.min()
			.unwrap_or(0)
			.min(cap)
	}
}

pub struct AlchScreen {
	open: bool,
	tab: AlchTab,
	cursor: (f32, f32),
	shift: bool,
	ctrl: bool,
	drag: DragState<InvId, BlockType>,
	/// The extraction slot: which block id is being dripped, if any.
	/// Screen-local UI state, cleared when the screen closes.
	extracting: Option<BlockType>,
	/// 0..1 through the current cycle; the in-flight cycle never sends
	/// its op when interrupted (ops are atomic per block).
	progress: f32,
	/// Transmute tab: selected family / member within it. `None` until
	/// the player picks a family — the tab opens with the family row and
	/// nothing else, rather than pre-selecting one and naming an item
	/// nobody asked about.
	family: Option<usize>,
	member: usize,
	/// Free-running clock driving the family tiles' member rotation.
	clock: f32,
	/// A rejected drop's explanation, shown in the preview line for
	/// [`HINT_SECS`] — silence was the old behavior and read as a bug.
	hint: Option<(String, f32)>,
}

impl Default for AlchScreen {
	fn default() -> Self {
		Self {
			open: false,
			tab: AlchTab::Extract,
			cursor: (-1.0, -1.0),
			shift: false,
			ctrl: false,
			drag: DragState::default(),
			extracting: None,
			progress: 0.0,
			family: None,
			member: 0,
			clock: 0.0,
			hint: None,
		}
	}
}

/// One transmute family: a tile that previews its members in slow
/// rotation; selecting it lays the members out under the tab's single
/// TRANSMUTE button. The grouping is the UI's own — SKILLS-DESIGN
/// §7.2's material × shape grid arrives as more members per family,
/// not more machinery.
pub struct Family {
	pub name: &'static str,
	pub members: &'static [BlockType],
}

pub const FAMILIES: [Family; 6] = [
	Family {
		name: "LIGHTS",
		members: &[BlockType::Torch],
	},
	Family {
		name: "SLABS",
		members: &[
			BlockType::WoodSlab,
			BlockType::StoneSlab,
			BlockType::LumberSlab,
		],
	},
	Family {
		name: "STAIRS",
		members: &[
			BlockType::WoodStairs,
			BlockType::StoneStairs,
			BlockType::LumberStairs,
		],
	},
	Family {
		name: "DOORS",
		members: &[BlockType::LumberDoor],
	},
	Family {
		name: "BLOCKS",
		members: &[
			BlockType::Stone,
			BlockType::Wood,
			BlockType::Lumber,
			BlockType::Dirt,
			BlockType::Sand,
			BlockType::Sandstone,
			BlockType::Snow,
			BlockType::Grass,
			BlockType::Mud,
		],
	},
	Family {
		name: "WATER",
		members: &[BlockType::Water],
	},
];

/// Seconds one cycle of the family tile's member rotation takes. A
/// member is meant to be READ before it goes, so the hold is most of it.
const FAMILY_CYCLE_SECS: f32 = 1.8;
/// How much of that cycle is spent trading one member for the next.
/// Fixed, not a fraction of the cycle: a slower rotation should hold
/// each member longer, not perform the handover in slow motion.
const FAMILY_SWAP_SECS: f32 = 0.30;

/// Which member a family tile shows at `clock`, and how PRESENT it is
/// (0..=1, scaling both the icon and its alpha).
///
/// The rotation used to hard-cut, which read as a glitch rather than as
/// a tile cycling its contents. The outgoing member shrinks away over
/// the first half of the swap window and the incoming one grows in over
/// the second, so the tile never shows two icons at once — cross-fading
/// two overlapping 3D icons is a double exposure of their internal
/// faces, and the effect is a rotation, not a dissolve.
///
/// A one-member family never transitions: there is nothing to trade it
/// for, and a tile pulsing on its own is just noise.
fn family_preview(clock: f32, members: usize) -> (usize, f32) {
	if members <= 1 {
		return (0, 1.0);
	}
	let cycles = clock / FAMILY_CYCLE_SECS;
	let i = cycles.floor().max(0.0) as usize;
	let into = (cycles - cycles.floor()) * FAMILY_CYCLE_SECS;
	let hold = FAMILY_CYCLE_SECS - FAMILY_SWAP_SECS;
	if into < hold {
		return (i % members, 1.0);
	}
	let u = (into - hold) / FAMILY_SWAP_SECS;
	if u < 0.5 {
		(i % members, 1.0 - 2.0 * u)
	} else {
		((i + 1) % members, 2.0 * u - 1.0)
	}
}

/// Scale a 0xAABBGGRR tint's alpha by `f` (the icon's own fade, so the
/// shrink doesn't just look like a small icon).
fn tint_alpha(tint: u32, f: f32) -> u32 {
	let a = ((tint >> 24) as f32 * f.clamp(0.0, 1.0)) as u32;
	a << 24 | (tint & 0x00FF_FFFF)
}

impl AlchScreen {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		self.open = true;
		self.shift = false;
		self.ctrl = false;
	}

	pub fn close(&mut self) {
		self.open = false;
		self.drag.cancel();
		// Extraction runs only while the screen is open — you stand at
		// the bench.
		self.extracting = None;
		self.progress = 0.0;
		// So does the transmute pick: the tab opens unselected every time.
		self.family = None;
		self.member = 0;
	}

	pub fn cancel_drag(&mut self) {
		self.drag.cancel();
	}

	/// Jump straight to a tab (capture harnesses and tests; play
	/// switches through the tab strip).
	pub fn set_tab(&mut self, tab: AlchTab) {
		self.tab = tab;
		if tab != AlchTab::Extract {
			self.extracting = None;
			self.progress = 0.0;
		}
	}

	/// Advance the drip one frame; returns the op a completed cycle
	/// sends. Stops (and clears the slot) when the stack runs dry or
	/// the block stops qualifying — the in-flight cycle simply never
	/// sends.
	pub fn tick(&mut self, dt: f32, level: u32, inv: &Inventory) -> Option<AlchAction> {
		// The family tiles' rotation runs whenever the screen is open
		// (wrapped far below any f32 precision cliff).
		self.clock = (self.clock + dt) % 3600.0;
		if let Some((_, ttl)) = &mut self.hint {
			*ttl -= dt;
			if *ttl <= 0.0 {
				self.hint = None;
			}
		}
		if self.tab != AlchTab::Extract {
			return None;
		}
		let Some(block) = self.extracting else {
			self.progress = 0.0;
			return None;
		};
		if inv.count(block) == 0 || !unlock::can_extract(block, level) {
			self.extracting = None;
			self.progress = 0.0;
			return None;
		}
		// The pace is the BLOCK's, not just the level's: a bench spends
		// its time in proportion to what comes out.
		let pace_secs = alchemy::extract_pace_ticks(block, level) as f32 / 60.0;
		self.progress += dt / pace_secs;
		if self.progress >= 1.0 {
			self.progress -= 1.0;
			return Some(AlchAction::Convert { block, count: 1 });
		}
		None
	}

	/// Emit the screen. The whole page shares one fitted style: this is
	/// the tallest screen in the game, and `theme::fit_scale` steps the
	/// full layout down the quarter-rung ladder when a short window at a
	/// big interface scale can't hold it — never a fractional glyph.
	pub fn build(
		&self,
		ui: &mut Ui<'_, InvId>,
		size: (u32, u32),
		view: &AlchView,
		grid: &InvGrid,
		inv: &Inventory,
		daylight: f32,
	) {
		let window_scale = ui.style.scale;
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);
		let bar_top = hud::row_grid(size, window_scale).origin.1;
		let budget = bar_top - (theme::GAP_ABOVE_BAR + theme::TOP_MARGIN) * window_scale;
		let needed = |s: f32| self.content_h(&Style::new(s)) + 2.0 * theme::PAD * s;
		let restore = ui.style;
		let fitted = theme::fit_scale(window_scale, budget, needed);
		if fitted != window_scale {
			ui.style = Style::new(fitted);
		}
		let s = ui.style.scale;
		let pad = theme::PAD * s;

		let (width, height) = (self.content_w(&ui.style), self.content_h(&ui.style));
		let row_w = width
			.max(theme::MIN_PANEL_W * s)
			.min(size.0 as f32 - 4.0 * pad);
		let row_x = (size.0 as f32 - row_w) / 2.0;
		let row_y = (bar_top - theme::GAP_ABOVE_BAR * window_scale - (height + 2.0 * pad))
			.max(theme::TOP_MARGIN * window_scale)
			+ pad;
		let panel = theme::panel_of(row_x, row_y, row_w, height, s);
		widgets::panel(ui.dl, Layer::Backdrop, panel, theme::PANEL_BG);

		let mut y = row_y;
		self.emit_header(ui, view, row_x, row_w, &mut y);
		match self.tab {
			AlchTab::Extract => self.emit_extract(ui, view, grid, inv, row_x, row_w, y),
			AlchTab::Transmute => self.emit_transmute(ui, view, row_x, row_w, y),
		}

		// The dragged stack rides the cursor above everything.
		if let Some(&block) = self.drag.ghost() {
			let icon_px = hud::ICON_PX * s;
			let (kind, tint) = hud::item_icon(HotbarItem::Block(block), false);
			let (cx, cy) = self.drag.cursor;
			hud::draw_icon(
				ui.dl,
				Layer::Ghost,
				Rect::new(cx - icon_px / 2.0, cy - icon_px / 2.0, icon_px, icon_px),
				kind,
				tint,
			);
		}
		ui.style = restore;
	}

	// ---- layout ------------------------------------------------------

	/// Header: title row (level + XP bar), essence readout, tab strip,
	/// and [`TAB_CONTENT_GAP`] below it. Must stay in step with the
	/// `*y` the header actually leaves behind in `emit_header` — this is
	/// the measure half of the same layout.
	fn header_h(style: &Style) -> f32 {
		(3.0 * theme::ROW_H + TAB_CONTENT_GAP) * style.scale
	}

	fn grid_spec_at(style: &Style, origin: (f32, f32)) -> GridSpec {
		GridSpec {
			origin,
			cols: 9,
			cell: hud::SLOT_PX * style.scale,
			gap: hud::SLOT_GAP * style.scale,
		}
	}

	fn content_w(&self, style: &Style) -> f32 {
		// Both tabs share the inventory grid's width, so switching tabs
		// never makes the panel jump.
		let spec = Self::grid_spec_at(style, (0.0, 0.0));
		spec.extent(GRID_SLOTS).0
	}

	/// The tallest member row any family lays out (wrapped at the grid's
	/// 9 columns) — the transmute arm sizes for it, so selecting a bigger
	/// family never moves the panel either.
	fn member_rows_h(style: &Style) -> f32 {
		let spec = Self::grid_spec_at(style, (0.0, 0.0));
		FAMILIES
			.iter()
			.map(|f| spec.extent(f.members.len()).1)
			.fold(0.0, f32::max)
	}

	/// BOTH tabs' content height — the panel takes the taller arm, so
	/// switching tabs never moves its top edge (bottom-anchored panels
	/// used to jump 90px under the cursor). Pinned by
	/// `panel_height_is_the_same_on_both_tabs`.
	fn content_h(&self, style: &Style) -> f32 {
		let s = style.scale;
		let spec = Self::grid_spec_at(style, (0.0, 0.0));
		let extract = hud::SLOT_PX * s + theme::SECTION_GAP * s + spec.extent(GRID_SLOTS).1;
		let transmute = hud::SLOT_PX * s
			+ theme::ROW_GAP * s
			+ Self::member_rows_h(style)
			+ theme::SECTION_GAP * s
			+ theme::ACTION_ROW_H * s
			+ theme::ROW_GAP * s
			+ theme::ROW_H * s;
		Self::header_h(style) + extract.max(transmute)
	}

	fn emit_header(
		&self,
		ui: &mut Ui<'_, InvId>,
		view: &AlchView,
		row_x: f32,
		row_w: f32,
		y: &mut f32,
	) {
		let s = ui.style.scale;
		let row_h = theme::ROW_H * s;
		// Title + level + XP bar (the settings rows' slider geometry as
		// a readout, exactly like the character screen).
		widgets::row(
			ui.dl,
			&ui.style,
			Layer::Content,
			Rect::new(row_x, *y, row_w, row_h),
			widgets::RowContent {
				text: &format!("ALCHEMY   LV {}", view.level),
				right: None,
				bar: Some(view.xp_frac),
				color: WHITE,
				highlight: false,
				caret: false,
			},
		);
		*y += row_h;
		// Essence readout, always visible on both tabs.
		let holdings = Essence::ALL
			.iter()
			.map(|&e| format!("{} {}", e.name(), view.subs[e as usize]))
			.collect::<Vec<_>>()
			.join("   ");
		widgets::row(
			ui.dl,
			&ui.style,
			Layer::Content,
			Rect::new(row_x, *y, row_w, row_h),
			widgets::RowContent {
				text: &holdings,
				right: None,
				bar: None,
				color: DIM,
				highlight: false,
				caret: false,
			},
		);
		*y += row_h;
		// The tab strip, drawn as TABS rather than a row of buttons: the
		// active tab is a filled block whose base fuses into the content
		// below it — the rule running under the strip breaks beneath it,
		// and instead traces UP the tab's sides and over its top, so the
		// strip reads as one continuous edge with a tab raised out of it
		// rather than as a line with a floating box parked on it — while
		// the inactive tab is dim, flat, pressable text sitting ON the
		// rule.
		let text_pad = scale::text_pad_x(s);
		let rule_h = (2.0 * s).round().max(1.0);
		let mut x = row_x + text_pad;
		let mut active_span = (row_x, row_x);
		for (i, (label, tab)) in [
			("EXTRACT", AlchTab::Extract),
			("TRANSMUTE", AlchTab::Transmute),
		]
		.into_iter()
		.enumerate()
		{
			let w = ui.style.measure(label) + 2.0 * text_pad;
			let rect = Rect::new(x, *y, w, row_h);
			if tab == self.tab {
				ui.dl.quad(Layer::Backdrop, rect, theme::FILL_BG);
				widgets::label(
					ui.dl,
					Layer::Content,
					rect.x + text_pad,
					rect.y + (row_h - ui.style.glyph) / 2.0,
					ui.style.glyph,
					WHITE,
					label,
				);
				active_span = (rect.x, rect.x + rect.w);
			} else {
				if rect.contains(self.cursor.0, self.cursor.1) {
					ui.dl.quad(Layer::Backdrop, rect, widgets::ROW_HILIGHT);
				}
				widgets::label(
					ui.dl,
					Layer::Content,
					rect.x + text_pad,
					rect.y + (row_h - ui.style.glyph) / 2.0,
					ui.style.glyph,
					DIM,
					label,
				);
				ui.hits.push(rect, InvId::Tab(i as u8));
			}
			x += w + TAB_GAP * s;
		}
		let rule_y = *y + row_h - rule_h;
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(row_x, rule_y, (active_span.0 - row_x).max(0.0), rule_h),
			widgets::BAR_TRACK,
		);
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(
				active_span.1,
				rule_y,
				(row_x + row_w - active_span.1).max(0.0),
				rule_h,
			),
			widgets::BAR_TRACK,
		);
		// The break, closed: the rule climbs the active tab's left edge,
		// runs across its top and comes back down the right, meeting both
		// straight runs. Emitted after the fill (same layer, so later wins)
		// and only when a tab actually claimed a span.
		if active_span.1 > active_span.0 {
			for edge in [
				Rect::new(active_span.0, *y, active_span.1 - active_span.0, rule_h),
				Rect::new(active_span.0, *y, rule_h, row_h),
				Rect::new(active_span.1 - rule_h, *y, rule_h, row_h),
			] {
				ui.dl.quad(Layer::Backdrop, edge, widgets::BAR_TRACK);
			}
		}
		// The tab's fill ends at the rule; the content starts a gap below
		// it, so the first slot row doesn't sit flush against the strip.
		*y += row_h + TAB_CONTENT_GAP * s;
	}

	/// The EXTRACT tab: the strip (slot + progress + preview + bucket
	/// tile), then the real inventory grid.
	#[allow(clippy::too_many_arguments)]
	fn emit_extract(
		&self,
		ui: &mut Ui<'_, InvId>,
		view: &AlchView,
		grid: &InvGrid,
		inv: &Inventory,
		row_x: f32,
		row_w: f32,
		mut y: f32,
	) {
		let s = ui.style.scale;
		let slot_px = hud::SLOT_PX * s;
		let icon_px = hud::ICON_PX * s;
		let text_pad = scale::text_pad_x(s);
		let (cx, cy) = self.cursor;

		// The extraction slot: armed = the strong ring (it's the thing
		// the drip acts on), remaining stack as an ordinary count badge.
		let slot_rect = Rect::new(row_x + text_pad, y, slot_px, slot_px);
		let icon = self
			.extracting
			.map(|b| hud::item_icon(HotbarItem::Block(b), false));
		widgets::slot(
			ui,
			InvId::ExtractSlot,
			slot_rect,
			icon,
			icon_px,
			if self.extracting.is_some() {
				widgets::SlotRing::Strong
			} else {
				widgets::SlotRing::None
			},
			slot_rect.contains(cx, cy),
		);
		if let Some(b) = self.extracting {
			widgets::count_badge(
				ui.dl,
				Layer::Content,
				&ui.style,
				slot_rect,
				inv.count(b),
				WHITE,
			);
			// Progress bar along the slot's bottom edge.
			let bar_h = PROGRESS_H * s;
			let track = Rect::new(
				slot_rect.x,
				slot_rect.y + slot_rect.h - bar_h,
				slot_rect.w,
				bar_h,
			);
			ui.dl.quad(Layer::Content, track, widgets::BAR_TRACK);
			ui.dl.quad(
				Layer::Content,
				Rect::new(
					track.x,
					track.y,
					track.w * self.progress.clamp(0.0, 1.0),
					track.h,
				),
				WHITE,
			);
		}
		// The bucket charge as a single water tile, right-aligned in the
		// strip: it can't live in the grid (water is not slottable), and
		// extracting it empties the tool.
		let bucket_tile = (view.bucket_full && unlock::can_extract(BlockType::Water, view.level))
			.then(|| Rect::new(row_x + row_w - text_pad - slot_px, y, slot_px, slot_px));
		if let Some(tile) = bucket_tile {
			widgets::slot(
				ui,
				InvId::BucketTile,
				tile,
				Some(hud::item_icon(HotbarItem::Block(BlockType::Water), false)),
				icon_px,
				widgets::SlotRing::None,
				tile.contains(cx, cy),
			);
		}
		// Yield preview / hint beside the slot; a rejected drop's
		// explanation borrows the line for a moment. Elided before the
		// bucket tile — there is no clipping.
		let preview = if let Some((hint, _)) = &self.hint {
			hint.clone()
		} else {
			match self.extracting {
				Some(b) => {
					let yields = alchemy::extract_yields(b, view.level)
						.iter()
						.map(|&(e, n)| format!("{n} {}", e.name()))
						.collect::<Vec<_>>()
						.join(" + ");
					let pace = alchemy::extract_pace_ticks(b, view.level) as f32 / 60.0;
					format!("→ {yields}   ·   1 block / {pace:.2}s")
				}
				None => "drag a stack here to extract".into(),
			}
		};
		let text_x = slot_rect.x + slot_px + ICON_TEXT_GAP * s;
		let text_end = bucket_tile.map_or(row_x + row_w - text_pad, |t| t.x - ICON_TEXT_GAP * s);
		widgets::label(
			ui.dl,
			Layer::Content,
			text_x,
			y + (slot_px - ui.style.glyph) / 2.0,
			ui.style.glyph,
			if self.hint.is_some() || self.extracting.is_some() {
				WHITE
			} else {
				DIM
			},
			&elide(&ui.style, &preview, text_end - text_x),
		);
		y += slot_px + theme::SECTION_GAP * s;

		// The real inventory grid — same slots, same badges, and its
		// cells double as drag sources. Left-aligned on the same column
		// as everything else (it used to center, sitting 4px off).
		let spec = Self::grid_spec_at(&ui.style, (row_x + text_pad, y));
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
				rect.contains(cx, cy),
			);
			if let Some(b) = grid.get(i) {
				widgets::count_badge(ui.dl, Layer::Content, &ui.style, rect, inv.count(b), WHITE);
			}
		}
	}

	/// The TRANSMUTE tab: a row of FAMILY tiles (each slowly rotating
	/// through its members — wood stairs, stone stairs, …), the selected
	/// family's members laid out under it, then a detail strip and the
	/// tab's single TRANSMUTE button. Locked members stay visible (dim,
	/// selectable — the strip names their level); the button is the only
	/// act.
	fn emit_transmute(
		&self,
		ui: &mut Ui<'_, InvId>,
		view: &AlchView,
		row_x: f32,
		row_w: f32,
		mut y: f32,
	) {
		const LOCKED_TINT: u32 = 0xFF58_5858;
		let s = ui.style.scale;
		let glyph = ui.style.glyph;
		let text_pad = scale::text_pad_x(s);
		let slot_px = hud::SLOT_PX * s;
		let icon_px = hud::ICON_PX * s;
		let (cx, cy) = self.cursor;

		// Family tiles, previews rotating on the shared clock. The
		// selected family wears the WEAK ring — it's context; the chosen
		// member below is what the button acts on and gets the strong
		// one. A family whose every member is locked dims like a locked
		// tile and badges its earliest unlock.
		let fam_spec = Self::grid_spec_at(&ui.style, (row_x + text_pad, y));
		for (i, fam) in FAMILIES.iter().enumerate() {
			let rect = fam_spec.rect(i);
			let all_locked = fam
				.members
				.iter()
				.all(|&m| !unlock::can_craft(m, view.level));
			// The tile trades one member for the next by shrinking the
			// outgoing one away and growing the incoming one in, rather
			// than cutting between them.
			let (mi, present) = family_preview(self.clock, fam.members.len());
			let preview = fam.members[mi];
			let (kind, tint) = hud::item_icon(HotbarItem::Block(preview), false);
			widgets::slot(
				ui,
				InvId::Family(i as u8),
				rect,
				Some((
					kind,
					tint_alpha(if all_locked { LOCKED_TINT } else { tint }, present),
				)),
				icon_px * present,
				if self.family == Some(i) {
					widgets::SlotRing::Weak
				} else {
					widgets::SlotRing::None
				},
				rect.contains(cx, cy),
			);
			if all_locked {
				let earliest = fam
					.members
					.iter()
					.filter_map(|&m| unlock::craft(m))
					.min()
					.unwrap_or(99);
				lock_badge(ui, rect, earliest);
			}
		}
		y += slot_px + theme::ROW_GAP * s;

		// The selected family's members, wrapped at the grid's 9 columns
		// so a growing catalog wraps instead of overflowing the panel.
		// Locked ones dim with their unlock badge but stay selectable —
		// the strip explains; affordable ones carry their batch count.
		// Nothing selected yet draws none of it: the row's height is
		// reserved either way (`member_rows_h` is the max over every
		// family), so picking one fills a gap instead of moving the page.
		let picked = self.family.map(|f| f.min(FAMILIES.len() - 1));
		if let Some(f) = picked {
			let family = &FAMILIES[f];
			let member = self.member.min(family.members.len() - 1);
			let mem_spec = Self::grid_spec_at(&ui.style, (row_x + text_pad, y));
			for (j, &item) in family.members.iter().enumerate() {
				let rect = mem_spec.rect(j);
				let locked = !unlock::can_craft(item, view.level);
				let (kind, tint) = hud::item_icon(HotbarItem::Block(item), false);
				widgets::slot(
					ui,
					InvId::Craft(j as u8),
					rect,
					Some((kind, if locked { LOCKED_TINT } else { tint })),
					icon_px,
					if j == member {
						widgets::SlotRing::Strong
					} else {
						widgets::SlotRing::None
					},
					rect.contains(cx, cy),
				);
				if locked {
					lock_badge(ui, rect, unlock::craft(item).unwrap_or(99));
				} else {
					let batches = view.affordable(item);
					if batches > 0 {
						widgets::count_badge(
							ui.dl,
							Layer::Content,
							&ui.style,
							rect,
							batches,
							WHITE,
						);
					}
				}
			}
		}
		y += Self::member_rows_h(&ui.style) + theme::SECTION_GAP * s;

		// The detail strip (the shared action-row height): the selected
		// member's icon, its name and per-line cost at YOUR level
		// (post-waste — what it will actually charge), and the tab's one
		// TRANSMUTE button. Locked reads dim with its level; unlocked
		// but short reads full-color with what's MISSING — "can't yet"
		// and "can't afford" are different answers.
		let strip_h = theme::ACTION_ROW_H * s;
		let strip = Rect::new(row_x, y, row_w, strip_h);
		// With nothing picked there is no item to name and nothing for the
		// button to do, so the strip says what to do instead of naming an
		// item the player never chose — and the modifier legend below it,
		// which describes that button, waits with it.
		let Some(f) = picked else {
			widgets::label(
				ui.dl,
				Layer::Content,
				strip.x + text_pad,
				strip.y + (strip_h - glyph) / 2.0,
				glyph,
				DIM,
				"SELECT A FAMILY",
			);
			return;
		};
		let family = &FAMILIES[f];
		let member = self.member.min(family.members.len() - 1);
		let item = family.members[member];
		let need = unlock::craft(item).unwrap_or(u32::MAX);
		let locked = !unlock::can_craft(item, view.level);
		let batches = view.affordable(item);
		let (kind, tint) = hud::item_icon(HotbarItem::Block(item), false);
		hud::draw_icon(
			ui.dl,
			Layer::Icons,
			Rect::new(
				strip.x + text_pad,
				strip.y + (strip_h - ROW_ICON * s) / 2.0,
				ROW_ICON * s,
				ROW_ICON * s,
			),
			kind,
			if locked { LOCKED_TINT } else { tint },
		);
		let text = if locked {
			format!("{}   —   LV {need}", block_name(item))
		} else {
			let ideal = alchemy::craft_ideal(item).expect("members are craftable");
			let costs = ideal
				.iter()
				.map(|&(e, n)| format!("{} {}", alchemy::craft_cost(n, view.level), e.name()))
				.collect::<Vec<_>>()
				.join(" + ");
			match batches {
				0 => {
					// Name the first shortfall so the player knows what
					// to go extract.
					let need = ideal
						.iter()
						.find_map(|&(e, n)| {
							let cost = alchemy::craft_cost(n, view.level);
							let have = view.subs[e as usize];
							(have < cost).then(|| format!("NEED {} MORE {}", cost - have, e.name()))
						})
						.unwrap_or_else(|| "NOTHING TO CHARGE".into());
					format!("{}   —   {costs}   ·   {need}", block_name(item))
				}
				n => format!("{}   —   {costs}   ·   ×{n}", block_name(item)),
			}
		};
		let btn = Rect::new(
			strip.x + strip.w - text_pad - BTN_W * s,
			strip.y + (strip_h - BTN_H * s) / 2.0,
			BTN_W * s,
			BTN_H * s,
		);
		let text_x = strip.x + text_pad + ROW_ICON * s + ICON_TEXT_GAP * s;
		widgets::label(
			ui.dl,
			Layer::Content,
			text_x,
			strip.y + (strip_h - glyph) / 2.0,
			glyph,
			if locked { DIM } else { WHITE },
			&elide(&ui.style, &text, btn.x - ICON_TEXT_GAP * s - text_x),
		);
		widgets::button(
			ui,
			InvId::CraftGo,
			btn,
			"TRANSMUTE",
			!locked && batches > 0,
			btn.contains(cx, cy),
		);
		y += strip_h + theme::ROW_GAP * s;
		widgets::row(
			ui.dl,
			&ui.style,
			Layer::Content,
			Rect::new(row_x, y, row_w, theme::ROW_H * s),
			widgets::RowContent {
				text: "CLICK 1   ·   SHIFT 10   ·   CTRL 100",
				right: None,
				bar: None,
				color: DIM,
				highlight: false,
				caret: false,
			},
		);
	}

	// ---- input -------------------------------------------------------

	/// Feed one input event; returns a craft action (extraction goes
	/// through `tick`). `bar` resolves bar-slot drag sources.
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
		style: &Style,
		view: &AlchView,
		grid: &InvGrid,
		bar: &HotbarView,
	) -> Option<AlchAction> {
		match *event {
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
				self.drag.on_move((x, y), DRAG_THRESHOLD_PX * style.scale);
				None
			}
			InputEvent::Key {
				key: Key::Shift,
				pressed,
			} => {
				self.shift = pressed;
				None
			}
			InputEvent::Key {
				key: Key::Ctrl,
				pressed,
			} => {
				self.ctrl = pressed;
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => {
				let id = hits.hit(self.cursor.0, self.cursor.1)?;
				match id {
					InvId::Tab(0) => {
						self.tab = AlchTab::Extract;
						self.drag.cancel();
					}
					InvId::Tab(_) => {
						self.tab = AlchTab::Transmute;
						self.drag.cancel();
						self.extracting = None;
						self.progress = 0.0;
					}
					// Clicking the occupied slot stops the drip (the
					// in-flight cycle never sends).
					InvId::ExtractSlot => {
						self.extracting = None;
						self.progress = 0.0;
					}
					// Drags only arm where a legal target exists — the
					// extraction slot lives on the EXTRACT tab; arming
					// from the hotbar on TRANSMUTE produced a ghost with
					// nowhere to land.
					InvId::Grid(i) => {
						if self.tab == AlchTab::Extract {
							if let Some(b) = grid.get(i as usize) {
								self.drag.on_press(self.cursor, id, b);
							}
						}
					}
					InvId::Bar(i) => {
						if self.tab == AlchTab::Extract {
							if let Some(SlotView::Block { block, .. }) = bar.slots.get(i as usize) {
								self.drag.on_press(self.cursor, id, *block);
							}
						}
					}
					InvId::BucketTile => {
						self.drag.on_press(self.cursor, id, BlockType::Water);
					}
					InvId::Family(i) => {
						let i = (i as usize).min(FAMILIES.len() - 1);
						if self.family != Some(i) {
							self.family = Some(i);
							// Land on the first member the level has
							// opened (else the first, whose strip line
							// names its unlock).
							self.member = FAMILIES[i]
								.members
								.iter()
								.position(|&m| unlock::can_craft(m, view.level))
								.unwrap_or(0);
						}
					}
					InvId::Craft(j) => {
						let family = &FAMILIES[self.family?.min(FAMILIES.len() - 1)];
						if (j as usize) < family.members.len() {
							self.member = j as usize;
						}
					}
					InvId::CraftGo => {
						let family = &FAMILIES[self.family?.min(FAMILIES.len() - 1)];
						let item = *family.members.get(self.member)?;
						let want = if self.ctrl {
							BULK_CTRL
						} else if self.shift {
							BULK_SHIFT
						} else {
							1
						};
						let count = want.min(view.affordable(item));
						return (count > 0).then_some(AlchAction::Craft { item, count });
					}
				}
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: false,
			} => {
				if let DragEvent::Drop {
					payload,
					to: Some(InvId::ExtractSlot),
					..
				} = self.drag.on_release(hits)
				{
					// The gate the drop respects: extractable at all, and
					// past its unlock (the bucket charge waits for 15).
					// A refused drop SAYS why — silence read as a bug.
					if !payload.is_extractable() {
						self.hint = Some(("CANNOT BE EXTRACTED".into(), HINT_SECS));
					} else if !unlock::can_extract(payload, view.level) {
						self.hint = Some((
							format!(
								"{}   —   LV {}",
								block_name(payload),
								unlock::extract(payload)
							),
							HINT_SECS,
						));
					} else {
						self.extracting = Some(payload);
						self.progress = 0.0;
					}
				}
				None
			}
			InputEvent::FocusLost => {
				self.shift = false;
				self.ctrl = false;
				self.drag.cancel();
				None
			}
			_ => None,
		}
	}
}

/// A locked tile's unlock level in its TOP-LEFT corner — the opposite
/// corner from [`widgets::count_badge`], so a locked-but-affordable
/// combination could carry both without collision. Frame glyph size,
/// like every text on the page.
fn lock_badge(ui: &mut Ui<'_, InvId>, slot: Rect, level: u32) {
	const SHADOW: u32 = 0xE0000000;
	let px = ui.style.glyph;
	let inset = 2.0 * ui.style.scale;
	let text = format!("{level}");
	let off = (px / 16.0).round().max(1.0);
	ui.dl.text(
		Layer::Content,
		slot.x + inset + off,
		slot.y + inset + off,
		px,
		SHADOW,
		&text,
	);
	ui.dl.text(
		Layer::Content,
		slot.x + inset,
		slot.y + inset,
		px,
		DIM,
		&text,
	);
}

/// Cut `text` to fit `max_w`, marking the cut — there is no clipping,
/// and a preview line that ran under the bucket tile read as overlap.
fn elide(style: &Style, text: &str, max_w: f32) -> String {
	if style.measure(text) <= max_w {
		return text.into();
	}
	let mut out: String = text.into();
	while !out.is_empty() {
		out.pop();
		let candidate = format!("{}...", out.trim_end());
		if style.measure(&candidate) <= max_w {
			return candidate;
		}
	}
	String::new()
}

/// Display names for the transmute rows (blocks have no Display impl —
/// debug names like "WoodStairs" are not UI text).
fn block_name(b: BlockType) -> &'static str {
	use BlockType as B;
	match b {
		B::WoodSlab => "WOOD SLAB",
		B::StoneSlab => "STONE SLAB",
		B::WoodStairs => "WOOD STAIRS",
		B::StoneStairs => "STONE STAIRS",
		B::LumberDoor => "LUMBER DOOR",
		B::LumberSlab => "LUMBER SLAB",
		B::LumberStairs => "LUMBER STAIRS",
		B::Lumber => "LUMBER",
		B::Stone => "STONE",
		B::Wood => "WOOD",
		B::Dirt => "DIRT",
		B::Sand => "SAND",
		B::Sandstone => "SANDSTONE",
		B::Snow => "SNOW",
		B::Grass => "GRASS",
		B::Mud => "MUD",
		B::Water => "WATER CHARGE",
		B::Torch => "TORCH",
		// Extractables that only show up in the refusal hint.
		B::Leaves => "LEAVES",
		B::PineLeaves => "PINE LEAVES",
		B::DryGrass => "DRY GRASS",
		B::Gravel => "GRAVEL",
		B::Rubble => "RUBBLE",
		_ => "?",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::DrawList;
	use digblock_core::block::BlockType as B;
	use digblock_core::hotbar::Hotbar;

	const SIZE: (u32, u32) = (1280, 800);

	struct Rig {
		screen: AlchScreen,
		hits: HitMap<InvId>,
		view: AlchView,
		grid: InvGrid,
		bar: HotbarView,
		inv: Inventory,
		style: Style,
		level: u32,
	}

	fn skills_at(level: u32) -> SkillSet {
		let mut s = SkillSet::default();
		s.set_total(Skill::Alchemy, skills::total_xp_for(level));
		s
	}

	fn rig_at(level: u32, stock: impl Fn(&mut Inventory)) -> Rig {
		let mut inv = Inventory::default();
		stock(&mut inv);
		let mut hotbar = Hotbar::new();
		let mut grid = InvGrid::default();
		digblock_core::inv_grid::reconcile_layout(&inv, &mut hotbar, &mut grid);
		let bar = HotbarView::build(&hotbar, &inv);
		let style = Style::new(1.0);
		let mut screen = AlchScreen::default();
		screen.open();
		let view = AlchView::build(&inv, &skills_at(level));
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style,
		};
		screen.build(&mut ui, SIZE, &view, &grid, &inv, 1.0);
		crate::ui::hud::hotbar(&mut ui, SIZE, &bar, |i| InvId::Bar(i as u8));
		Rig {
			screen,
			hits,
			view,
			grid,
			bar,
			inv,
			style,
			level,
		}
	}

	fn rebuild(r: &mut Rig) {
		r.view = AlchView::build(&r.inv, &skills_at(r.level));
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: r.style,
		};
		r.screen.build(&mut ui, SIZE, &r.view, &r.grid, &r.inv, 1.0);
		crate::ui::hud::hotbar(&mut ui, SIZE, &r.bar, |i| InvId::Bar(i as u8));
		r.hits = hits;
	}

	fn feed(r: &mut Rig, ev: &InputEvent) -> Option<AlchAction> {
		r.screen
			.handle_input(ev, &r.hits, &r.style, &r.view, &r.grid, &r.bar)
	}

	fn center(rect: Rect) -> (f32, f32) {
		(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
	}

	fn drag_to(r: &mut Rig, from: InvId, to: (f32, f32)) -> Option<AlchAction> {
		let src = center(r.hits.rect_of(from).expect("source exists"));
		feed(r, &InputEvent::CursorMoved { x: src.0, y: src.1 });
		feed(
			r,
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
		);
		feed(r, &InputEvent::CursorMoved { x: to.0, y: to.1 });
		feed(
			r,
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: false,
			},
		)
	}

	fn source_of(r: &Rig, b: B) -> InvId {
		if let Some(i) = r
			.bar
			.slots
			.iter()
			.position(|s| matches!(s, SlotView::Block { block, .. } if *block == b))
		{
			return InvId::Bar(i as u8);
		}
		for i in 0..GRID_SLOTS {
			if r.grid.get(i) == Some(b) {
				return InvId::Grid(i as u16);
			}
		}
		panic!("{b:?} neither on the bar nor in the grid");
	}

	/// §8.1's core loop: drag a stack onto the slot, and the drip sends
	/// one `Convert {{ count: 1 }}` per pace interval until the stack
	/// runs dry — at which point the slot clears itself.
	#[test]
	fn drag_starts_the_drip_and_dry_stops_it() {
		let mut r = rig_at(1, |inv| inv.add(B::Rubble, 2));
		// Rubble reconciles onto the BAR first (bar-first filing), so
		// the drag source is a bar slot — exactly the §8.1 "and hotbar"
		// case.
		let slot = center(r.hits.rect_of(InvId::ExtractSlot).unwrap());
		let src = source_of(&r, B::Rubble);
		assert_eq!(drag_to(&mut r, src, slot), None, "the drop itself is free");

		// Half a cycle: nothing sent.
		let pace = alchemy::extract_pace_ticks(B::Rubble, 1) as f32 / 60.0;
		assert_eq!(r.screen.tick(pace * 0.5, 1, &r.inv), None);
		// Completing the cycle sends exactly one single-block op.
		assert_eq!(
			r.screen.tick(pace * 0.6, 1, &r.inv),
			Some(AlchAction::Convert {
				block: B::Rubble,
				count: 1
			})
		);
		// Engine applies it; one left, drip continues.
		assert!(digblock_core::inventory::apply_alchemy(
			&mut r.inv,
			digblock_core::inventory::AlchemyOp::Convert {
				block: B::Rubble,
				count: 1
			},
			1
		));
		assert_eq!(
			r.screen.tick(pace * 1.1, 1, &r.inv),
			Some(AlchAction::Convert {
				block: B::Rubble,
				count: 1
			})
		);
		assert!(digblock_core::inventory::apply_alchemy(
			&mut r.inv,
			digblock_core::inventory::AlchemyOp::Convert {
				block: B::Rubble,
				count: 1
			},
			1
		));
		// Dry: the slot clears and nothing more is ever sent.
		assert_eq!(r.screen.tick(pace * 2.0, 1, &r.inv), None);
		assert_eq!(r.screen.extracting, None, "slot cleared on dry");
	}

	#[test]
	fn clicking_the_slot_stops_and_closing_clears() {
		let mut r = rig_at(1, |inv| inv.add(B::Wood, 5));
		let slot = center(r.hits.rect_of(InvId::ExtractSlot).unwrap());
		let src = source_of(&r, B::Wood);
		drag_to(&mut r, src, slot);
		assert_eq!(r.screen.extracting, Some(B::Wood));
		// Click the slot: the in-flight cycle never sends.
		feed(
			&mut r,
			&InputEvent::CursorMoved {
				x: slot.0,
				y: slot.1,
			},
		);
		feed(
			&mut r,
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
		);
		assert_eq!(r.screen.extracting, None);

		drag_to(&mut r, src, slot);
		assert_eq!(r.screen.extracting, Some(B::Wood));
		r.screen.close();
		assert_eq!(r.screen.extracting, None, "you stand at the bench");
	}

	/// The bucket charge appears as a single water tile (level 15+ and
	/// charged only), and extracting it is the ordinary Convert whose
	/// debit IS the emptying.
	#[test]
	fn bucket_tile_extracts_the_charge() {
		// Below 15: no tile at all.
		let r = rig_at(1, |inv| inv.add(B::Water, 1));
		assert!(r.hits.rect_of(InvId::BucketTile).is_none(), "locked at 1");

		let mut r = rig_at(20, |inv| inv.add(B::Water, 1));
		let slot = center(r.hits.rect_of(InvId::ExtractSlot).unwrap());
		drag_to(&mut r, InvId::BucketTile, slot);
		assert_eq!(r.screen.extracting, Some(B::Water));
		let pace = alchemy::extract_pace_ticks(B::Water, 20) as f32 / 60.0;
		assert_eq!(
			r.screen.tick(pace * 1.01, 20, &r.inv),
			Some(AlchAction::Convert {
				block: B::Water,
				count: 1
			})
		);
		assert!(digblock_core::inventory::apply_alchemy(
			&mut r.inv,
			digblock_core::inventory::AlchemyOp::Convert {
				block: B::Water,
				count: 1
			},
			20
		));
		assert!(!r.inv.bucket_full(), "the debit is the emptying");
		assert!(r.inv.sub_count(Essence::Aqua) > 0);
		// Tile gone next frame.
		rebuild(&mut r);
		assert!(r.hits.rect_of(InvId::BucketTile).is_none(), "tile gone");
	}

	/// The transmute flow: pick a family tile, pick a member, and the
	/// tab's ONE button crafts it — multipliers clamp to affordability,
	/// and a locked selection disables the button outright (no hit).
	#[test]
	fn family_selection_and_the_single_transmute_button() {
		let mut r = rig_at(20, |inv| {
			inv.add_sub(Essence::Flora, 500);
		});
		let press = |r: &mut Rig, at: (f32, f32)| {
			feed(r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
			feed(
				r,
				&InputEvent::MouseButton {
					button: MouseButton::Left,
					pressed: true,
				},
			)
		};
		// Switch to the transmute tab.
		let tab = center(r.hits.rect_of(InvId::Tab(1)).unwrap());
		press(&mut r, tab);
		rebuild(&mut r);

		// It opens on the family row alone: no member is pre-picked, so
		// there is nothing for the one button to act on and it isn't
		// drawn. The tab used to land on LIGHTS/Torch and name an item
		// nobody had asked for.
		assert!(
			r.hits.rect_of(InvId::Craft(0)).is_none(),
			"no members before a family is picked"
		);
		assert!(
			r.hits.rect_of(InvId::CraftGo).is_none(),
			"no button before a family is picked"
		);

		// SLABS: the WoodSlab the Flora affords. Ctrl asks for 100; the
		// holdings clamp it.
		let slabs = center(r.hits.rect_of(InvId::Family(1)).unwrap());
		press(&mut r, slabs);
		rebuild(&mut r);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		feed(
			&mut r,
			&InputEvent::Key {
				key: Key::Ctrl,
				pressed: true,
			},
		);
		let batches = r.view.affordable(B::WoodSlab);
		assert!(batches > 0 && batches < 100, "clamp is meaningful");
		assert_eq!(
			press(&mut r, go),
			Some(AlchAction::Craft {
				item: B::WoodSlab,
				count: batches
			})
		);
		feed(
			&mut r,
			&InputEvent::Key {
				key: Key::Ctrl,
				pressed: false,
			},
		);

		// The WATER family's one member is locked at 20: the tile still
		// shows (the strip names LV 50), but the single button registers
		// no hit — honest pixels, not a trap.
		let water = center(r.hits.rect_of(InvId::Family(5)).unwrap());
		press(&mut r, water);
		rebuild(&mut r);
		assert!(
			r.hits.rect_of(InvId::Craft(0)).is_some(),
			"locked member still shows"
		);
		assert!(
			r.hits.rect_of(InvId::CraftGo).is_none(),
			"no button under a locked pick"
		);

		// BLOCKS: switching family lands on the first unlocked member
		// (Stone — unaffordable with no Earth, button dark); picking
		// Wood by tile makes the one button craft THAT.
		let blocks = center(r.hits.rect_of(InvId::Family(4)).unwrap());
		press(&mut r, blocks);
		rebuild(&mut r);
		assert!(r.hits.rect_of(InvId::CraftGo).is_none(), "Stone: no Earth");
		let wood = center(r.hits.rect_of(InvId::Craft(1)).unwrap());
		press(&mut r, wood);
		rebuild(&mut r);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		assert_eq!(
			press(&mut r, go),
			Some(AlchAction::Craft {
				item: B::Wood,
				count: 1
			})
		);
	}

	/// The panel bottom-anchors above the hotbar on both tabs, and a
	/// short window at a big scale fits by stepping the ladder.
	#[test]
	fn panel_sits_above_the_hotbar_and_fits_short_windows() {
		for (size, scale) in [
			((1280u32, 800u32), 1.0f32),
			((1280, 720), 1.75),
			((960, 600), 2.5),
		] {
			let mut inv = Inventory::default();
			inv.add(B::Wood, 5);
			let grid = InvGrid::default();
			let view = AlchView::build(&inv, &skills_at(41));
			let mut screen = AlchScreen::default();
			screen.open();
			let mut dl = DrawList::default();
			let mut hits = HitMap::default();
			let mut ui = Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(scale),
			};
			screen.build(&mut ui, size, &view, &grid, &inv, 1.0);
			let bar_top = hud::row_grid(size, scale).origin.1;
			let panel = dl
				.cmds(Layer::Backdrop)
				.0
				.iter()
				.find_map(|c| match c {
					crate::ui::draw::Cmd::Quad { rect, color } if *color == theme::PANEL_BG => {
						Some(*rect)
					}
					_ => None,
				})
				.expect("panel quad");
			assert!(panel.y >= 0.0, "panel top on screen at {size:?}/{scale}");
			assert!(
				panel.y + panel.h <= bar_top + 0.5,
				"panel under the bar at {size:?}/{scale}: {} vs {bar_top}",
				panel.y + panel.h
			);
		}
	}

	/// The panel is bottom-anchored, so a per-tab height means the TOP
	/// edge jumps under the cursor on every tab switch (it did — 90px).
	/// `content_h` takes the taller arm for both tabs.
	#[test]
	fn panel_height_is_the_same_on_both_tabs() {
		let panel_of = |tab: AlchTab| {
			let inv = Inventory::default();
			let grid = InvGrid::default();
			let view = AlchView::build(&inv, &skills_at(10));
			let mut screen = AlchScreen::default();
			screen.open();
			screen.set_tab(tab);
			let mut dl = DrawList::default();
			let mut hits = HitMap::default();
			let mut ui = Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(1.0),
			};
			screen.build(&mut ui, SIZE, &view, &grid, &inv, 1.0);
			dl.cmds(Layer::Backdrop)
				.0
				.iter()
				.find_map(|c| match c {
					crate::ui::draw::Cmd::Quad { rect, color } if *color == theme::PANEL_BG => {
						Some(*rect)
					}
					_ => None,
				})
				.expect("panel quad")
		};
		let (a, b) = (panel_of(AlchTab::Extract), panel_of(AlchTab::Transmute));
		assert_eq!(a, b, "one panel, both tabs");
	}

	/// The family tile rotates rather than cutting: a member holds at
	/// full presence, shrinks away, and the NEXT one grows in — with the
	/// handover happening at zero, so two icons never overlap.
	#[test]
	fn family_tiles_transition_instead_of_cutting() {
		let hold = FAMILY_CYCLE_SECS - FAMILY_SWAP_SECS;
		// The hold: member 0, fully present, start to just before the swap.
		for t in [0.0, hold * 0.5, hold - 0.001] {
			assert_eq!(family_preview(t, 3), (0, 1.0), "t={t}");
		}
		// The swap: presence falls to zero on the outgoing member...
		let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.25, 3);
		assert_eq!(i, 0);
		assert!((p - 0.5).abs() < 1e-4, "half faded: {p}");
		let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.5 - 0.001, 3);
		assert_eq!(i, 0);
		assert!(p < 0.01, "gone at the handover: {p}");
		// ...and the next one grows from zero.
		let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.5 + 0.001, 3);
		assert_eq!(i, 1, "handover at the bottom of the dip");
		assert!(p < 0.01, "arrives from nothing: {p}");
		let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.999, 3);
		assert_eq!(i, 1);
		assert!(p > 0.99, "back to full by the next hold: {p}");
		// Presence is never negative and never over 1, anywhere.
		for n in 1..=8 {
			let mut t = 0.0f32;
			while t < 20.0 {
				let (i, p) = family_preview(t, n);
				assert!(i < n, "index in range at {t} (n={n})");
				assert!((0.0..=1.0).contains(&p), "presence {p} at {t}");
				t += 0.011;
			}
		}
		// One member: nothing to trade for, so no pulsing at all.
		for t in [0.0, 0.5, hold + 0.2, 5.0] {
			assert_eq!(family_preview(t, 1), (0, 1.0), "t={t}");
		}
		// And it really does cycle: over one full lap of a 2-member
		// family both members hold.
		let seen: std::collections::HashSet<usize> = (0..200)
			.map(|k| family_preview(k as f32 * 0.02, 2).0)
			.collect();
		assert_eq!(seen.len(), 2);
	}

	/// `FAMILIES` is a hand-maintained mirror of the core catalog: every
	/// craftable appears in exactly one family, so a new `unlock::craft`
	/// entry cannot silently never reach the screen.
	#[test]
	fn every_craftable_appears_in_exactly_one_family() {
		for &b in &BlockType::ALL {
			let craftable = unlock::craft(b).is_some();
			let appearances: usize = FAMILIES
				.iter()
				.map(|f| f.members.iter().filter(|&&m| m == b).count())
				.sum();
			assert_eq!(
				appearances,
				usize::from(craftable),
				"{b:?}: craftable blocks sit in exactly one family, others in none"
			);
		}
	}
}
