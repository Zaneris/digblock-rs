//! The alchemy screen (SKILLS-DESIGN §8.1, reshaped by the essence
//! redesign): ONE page under a header that always shows the Alchemy
//! level, its XP bar, and the essence holdings. The EXTRACT tab is gone —
//! matter never becomes essence at a bench any more; the siphon draws
//! nodes in the field, and this screen is where the essence is SPENT.
//!
//! The page is a row of FAMILY tiles whose icons slowly rotate through
//! their members, the selected family's members wrapped into the
//! inventory grid's columns beneath (locked ones dim with an unlock
//! badge, affordable ones count-badged), a detail strip, and the page's
//! ONE `TRANSMUTE` button — CLICK 1 · SHIFT 10 · CTRL 100 clamped to what
//! the holdings afford. The families are DERIVED from §7.2's shape axis
//! rather than mirrored by hand, and
//! `every_craftable_appears_in_exactly_one_family` holds the derivation
//! to the core catalog, so a new craftable cannot silently miss the
//! screen.
//!
//! The button QUEUES that count rather than buying it: crafting drips at
//! `craft_pace_ticks`, with the batch's progress and what is left of it
//! in the row beneath the button, and the button itself standing as the
//! batch's CANCEL. It used to answer instantly, which made the bench's
//! one immediate action also its fastest XP — a hundred crafts paid in a
//! frame. Crafting pays what it always paid; it now takes the time the
//! work takes. The bench keeps working with the screen closed (§13.4's
//! background bench); only leaving the world abandons.
//!
//! The screen never mutates game state: `handle_input`/`tick` return
//! actions; the engine applies them locally offline or sends them to
//! the server online (the `InventoryFull` echo is the rebase).

use digblock_core::alchemy;
use digblock_core::block::{BlockType, Essence, ShapeFamily};
use digblock_core::hotbar::HotbarItem;
use digblock_core::inv_grid::GRID_SLOTS;
use digblock_core::inventory::Inventory;
use digblock_core::material::Material;
use digblock_core::skills::{self, Skill, SkillSet};
use digblock_core::unlock;
use digblock_platform::{InputEvent, Key, MouseButton};

use crate::ui::draw::Layer;
use crate::ui::widgets::{self, DIM, GridSpec, Style, Ui, WHITE};
use crate::ui::{HitMap, InvId, Rect, hud, scale, theme};

/// One requested alchemy operation, in core vocabulary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AlchAction {
	Craft { item: BlockType, count: u32 },
}

/// What the bench is working on, for the HUD's closed-screen tell — the
/// background bench is invisible without one, and an invisible worker is
/// a bug report ("my essence changed on its own").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BenchJob {
	/// Transmuting this item, with this many left on the queue.
	Craft(BlockType, u32),
}

const BULK_SHIFT: u32 = 10;
const BULK_CTRL: u32 = 100;

// This screen's own geometry (theme tokens cover everything shared).
const ROW_ICON: f32 = 36.0;
const BTN_W: f32 = 152.0;
const BTN_H: f32 = 34.0;
/// Gap between an icon/slot and the text that describes it.
const ICON_TEXT_GAP: f32 = 12.0;
/// What a tile the level has not opened yet wears. Locked tiles dim
/// rather than hide, and stay interactive — the screen teaches by letting
/// you ask and answering, which is the whole reason a locked tile is
/// still a click target.
const LOCKED_TINT: u32 = 0xFF58_5858;

/// The screen's per-frame view: level, XP-bar fraction, essence
/// holdings, block counts — derived once so measure and emit agree.
pub struct AlchView {
	pub level: u32,
	/// Progress toward the next level, 0..=1 (full at the cap).
	pub xp_frac: f32,
	pub subs: [u32; Essence::COUNT],
	/// The whole inventory's counts — the feedstock half of affordability.
	pub counts: [u32; BlockType::COUNT],
	/// How many more of each id the inventory can take. Zero for a tool
	/// already owned and for a charged bucket — `apply_alchemy` refuses
	/// a craft on exactly this number, so the button has to grey on it
	/// too or the offer is a lie the player pays for.
	pub headroom: [u32; BlockType::COUNT],
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
		let mut counts = [0u32; BlockType::COUNT];
		let mut headroom = [0u32; BlockType::COUNT];
		for &b in BlockType::ALL.iter() {
			counts[b as usize] = inv.count(b);
			headroom[b as usize] = inv.headroom(b);
		}
		Self {
			level,
			xp_frac,
			subs,
			counts,
			headroom,
		}
	}

	/// Whole batches of `item` the holdings afford at `level` (0 when
	/// locked or unaffordable) — essence lines AND feedstock blocks, the
	/// same two gates `apply_alchemy` runs.
	pub fn affordable(&self, item: BlockType) -> u32 {
		if !unlock::can_craft(item, self.level) {
			return 0;
		}
		let Some(ideal) = alchemy::craft_ideal(item) else {
			return 0;
		};
		let essence = ideal
			.iter()
			.map(|&(e, n)| self.subs[e as usize] / alchemy::craft_cost(n, self.level))
			.min();
		let matter = alchemy::craft_feedstock(item)
			.iter()
			.map(|&(f, n)| self.counts[f as usize] / n)
			.min();
		essence
			.into_iter()
			.chain(matter)
			.min()
			.unwrap_or(0)
			// Capped types buy what they can hold and no more: one
			// bucket charge, one of any tool. Affordability is not the
			// only ceiling.
			.min(self.headroom[item as usize])
	}
}

pub struct AlchScreen {
	open: bool,
	cursor: (f32, f32),
	shift: bool,
	ctrl: bool,
	/// The transmute queue: what the TRANSMUTE button asked for and how
	/// many of it are still owed. A batch is a QUEUE rather than a single
	/// bulk op — what stops the CTRL multiplier being a hundred crafts'
	/// XP in one frame.
	crafting: Option<(BlockType, u32)>,
	/// 0..1 through the current cycle; the in-flight cycle never sends
	/// its op when interrupted (ops are atomic per item).
	progress: f32,
	/// Selected family / member within it. `None` until the player picks
	/// a family — the page opens with the family row and nothing else,
	/// rather than pre-selecting one and naming an item nobody asked
	/// about.
	family: Option<usize>,
	member: usize,
	/// Free-running clock driving the family tiles' member rotation.
	clock: f32,
}

impl Default for AlchScreen {
	fn default() -> Self {
		Self {
			open: false,
			cursor: (-1.0, -1.0),
			shift: false,
			ctrl: false,
			crafting: None,
			progress: 0.0,
			family: None,
			member: 0,
			clock: 0.0,
		}
	}
}

/// One transmute family: a tile that previews its members in slow
/// rotation; selecting it lays the members out under the page's single
/// TRANSMUTE button.
///
/// The grouping is no longer the UI's own — it IS SKILLS-DESIGN §7.2's
/// shape axis (`block::ShapeFamily`), with members in material (i.e.
/// unlock) order. A new material × shape pair therefore joins its family
/// on its own, which is what lets the catalog grow without the screen
/// growing. Only the FUNCTIONAL families are authored here, because §7.3's
/// hand-priced items are not a substance in a form and have no axis to
/// derive from.
pub struct Family {
	pub name: &'static str,
	pub members: Vec<BlockType>,
}

/// Families the grid cannot derive: hand-priced functional items, grouped
/// by what they DO. Drawn after the shape families.
const FUNCTIONAL_FAMILIES: [(&str, &[BlockType]); 3] = [
	(
		"LIGHTS",
		&[BlockType::Torch, BlockType::Lantern, BlockType::Aetherlamp],
	),
	(
		"FURNITURE",
		&[BlockType::Chair, BlockType::Table, BlockType::Bed],
	),
	(
		"TOOLS",
		// The vessels lead: the bucket and the siphon are the catalog's
		// first crafts (feedstock-only, so a fresh player can make both),
		// and members sit in unlock order like everywhere else.
		&[
			BlockType::Bucket,
			BlockType::EssenceSiphon,
			BlockType::ResonantPick,
			BlockType::MastersSigil,
		],
	),
];

/// The families, built once per process: one per shape in
/// `ShapeFamily::ALL` order (members in material order, empty families
/// dropped), then the functional ones.
pub fn families() -> &'static [Family] {
	static FAMILIES: std::sync::OnceLock<Vec<Family>> = std::sync::OnceLock::new();
	FAMILIES.get_or_init(|| {
		let mut out: Vec<Family> = ShapeFamily::ALL
			.into_iter()
			.map(|s| Family {
				name: s.name(),
				members: Material::ALL
					.into_iter()
					.filter_map(|m| BlockType::from_grid(m, s))
					.filter(|&b| unlock::craft(b).is_some())
					.collect(),
			})
			// A shape nobody has authored a material for yet would draw an
			// empty tile with nothing behind it.
			.filter(|f| !f.members.is_empty())
			.collect();
		out.extend(FUNCTIONAL_FAMILIES.iter().map(|&(name, members)| Family {
			name,
			members: members.to_vec(),
		}));
		out
	})
}

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
/// a tile cycling its contents. The handover itself is
/// [`super::super::handover`] — the same shrink-away/grow-in the hotbar's
/// tool slot plays when you cycle tools, and for the same reason.
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
	let (incoming, present) = crate::ui::handover((into - hold) / FAMILY_SWAP_SECS);
	((i + usize::from(incoming)) % members, present)
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
		// The bench keeps working with the screen closed (§13.4's
		// background bench): closing is walking away from a bench that is
		// mid-job, not cancelling it. Only leaving the WORLD abandons
		// ([`Self::abandon`]) — the job names things in this world. The
		// pick resets only when nothing is queued, so reopening mid-batch
		// lands back on the batch's own tile and progress row.
		if self.crafting.is_none() {
			self.family = None;
			self.member = 0;
		}
	}

	/// Drop the running job outright — the WORLD-EXIT arm, kept apart
	/// from [`Self::close`] on purpose: closing the screen keeps the
	/// bench working, but a job points at this world's stocks and this
	/// server's bucket, and neither exists in whatever comes next.
	pub fn abandon(&mut self) {
		self.crafting = None;
		self.progress = 0.0;
		self.family = None;
		self.member = 0;
	}

	/// Is a job on the bench? What keeps the engine ticking this screen
	/// after it closes, and what the HUD's closed-screen tell keys on.
	pub fn busy(&self) -> bool {
		self.crafting.is_some()
	}

	/// The running job, for the HUD tell: what is being worked and how
	/// many remain.
	pub fn job(&self) -> Option<BenchJob> {
		self.crafting
			.map(|(item, left)| BenchJob::Craft(item, left))
	}

	/// Capture hook (`DIG_SCREEN=transmuting`): select `item` in its own
	/// family and put a batch of `count` on the bench, part-way through
	/// its first cycle.
	///
	/// A running batch is a TRANSIENT state — it needs a press, and it is
	/// over in seconds — so it is exactly the kind of thing headless
	/// capture cannot otherwise reach, which is how two look-and-feel
	/// regressions shipped on screens that were only ever driven by hand.
	pub fn debug_batch(&mut self, item: BlockType, count: u32, into_cycle: f32) {
		let Some((f, m)) = families().iter().enumerate().find_map(|(f, fam)| {
			let m = fam.members.iter().position(|&b| b == item)?;
			Some((f, m))
		}) else {
			return;
		};
		self.family = Some(f);
		self.member = m;
		self.crafting = Some((item, count));
		self.progress = into_cycle.clamp(0.0, 1.0);
	}

	/// Advance the craft drip one frame; returns the op a completed cycle
	/// sends. The queue stops (and clears itself) the moment its work
	/// stops qualifying — the holdings can no longer pay — and the
	/// in-flight cycle simply never sends.
	pub fn tick(&mut self, dt: f32, view: &AlchView) -> Option<AlchAction> {
		// The family tiles' rotation runs whenever the screen is open
		// (wrapped far below any f32 precision cliff).
		self.clock = (self.clock + dt) % 3600.0;
		let Some((item, _)) = self.crafting else {
			self.progress = 0.0;
			return None;
		};
		// Re-asked every frame against the SAME number the button greys
		// on, so a queue outliving its price (a rebase landing, or the
		// tool cap filling on the first one) ends where the offer would
		// have been withdrawn.
		if view.affordable(item) == 0 {
			self.crafting = None;
			self.progress = 0.0;
			return None;
		}
		let pace_ticks = alchemy::craft_pace_ticks(item, view.level);
		self.progress += dt / (pace_ticks as f32 / 60.0);
		if self.progress < 1.0 {
			return None;
		}
		self.progress -= 1.0;
		// A finished craft is one off the queue; the last one ends it.
		if let Some((_, left)) = &mut self.crafting {
			*left -= 1;
			if *left == 0 {
				self.crafting = None;
				self.progress = 0.0;
			}
		}
		Some(AlchAction::Craft { item, count: 1 })
	}

	/// Emit the screen. The whole page shares one fitted style:
	/// `theme::fit_scale` steps the full layout down the quarter-rung
	/// ladder when a short window at a big interface scale can't hold it
	/// — never a fractional glyph.
	pub fn build(&self, ui: &mut Ui<'_, InvId>, size: (u32, u32), view: &AlchView, daylight: f32) {
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
		self.emit_transmute(ui, view, row_x, row_w, y);
		ui.style = restore;
	}

	// ---- layout ------------------------------------------------------

	/// Header: title row (level + XP bar), essence readout, and a section
	/// gap below. Must stay in step with the `*y` the header actually
	/// leaves behind in `emit_header` — this is the measure half of the
	/// same layout.
	fn header_h(style: &Style) -> f32 {
		(2.0 * theme::ROW_H + theme::SECTION_GAP) * style.scale
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
		// The inventory grid's own width, which is also what every other
		// screen measures against — the page keeps its column.
		let spec = Self::grid_spec_at(style, (0.0, 0.0));
		spec.extent(GRID_SLOTS).0
	}

	/// The tallest member row any family lays out (wrapped at the grid's
	/// own columns) — the page sizes for it, so selecting a bigger family
	/// never moves the panel.
	fn member_rows_h(style: &Style) -> f32 {
		let spec = Self::grid_spec_at(style, (0.0, 0.0));
		families()
			.iter()
			.map(|f| spec.extent(f.members.len()).1)
			.fold(0.0, f32::max)
	}

	fn content_h(&self, style: &Style) -> f32 {
		let s = style.scale;
		let transmute = hud::SLOT_PX * s
			+ theme::ROW_GAP * s
			+ Self::member_rows_h(style)
			+ theme::SECTION_GAP * s
			+ theme::ACTION_ROW_H * s
			+ theme::ROW_GAP * s
			+ theme::ROW_H * s;
		Self::header_h(style) + transmute
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
		// Essence readout, always visible.
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
		*y += row_h + theme::SECTION_GAP * s;
	}

	/// The page: a row of FAMILY tiles (each slowly rotating through its
	/// members — wood stairs, stone stairs, …), the selected family's
	/// members laid out under it, then a detail strip and the single
	/// TRANSMUTE button. Locked members stay visible (dim, selectable —
	/// the strip names their level); the button is the only act.
	fn emit_transmute(
		&self,
		ui: &mut Ui<'_, InvId>,
		view: &AlchView,
		row_x: f32,
		row_w: f32,
		mut y: f32,
	) {
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
		for (i, fam) in families().iter().enumerate() {
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

		// The selected family's members, wrapped at the grid's own columns
		// so a growing catalog wraps instead of overflowing the panel.
		// Locked ones dim with their unlock badge but stay selectable —
		// the strip explains; affordable ones carry their batch count.
		// Nothing selected yet draws none of it: the row's height is
		// reserved either way (`member_rows_h` is the max over every
		// family), so picking one fills a gap instead of moving the page.
		let picked = self.family.map(|f| f.min(families().len() - 1));
		if let Some(f) = picked {
			let family = &families()[f];
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
		// (post-waste — what it will actually charge, feedstock lines
		// included), and the page's one TRANSMUTE button. Locked reads
		// dim with its level; unlocked but short reads full-color with
		// what's MISSING — "can't yet" and "can't afford" are different
		// answers.
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
		let family = &families()[f];
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
		let btn = Rect::new(
			strip.x + strip.w - text_pad - BTN_W * s,
			strip.y + (strip_h - BTN_H * s) / 2.0,
			BTN_W * s,
			BTN_H * s,
		);
		let text_x = strip.x + text_pad + ROW_ICON * s + ICON_TEXT_GAP * s;
		let avail = btn.x - ICON_TEXT_GAP * s - text_x;
		let text = if locked {
			format!("{}   —   LV {need}", block_name(item))
		} else {
			let ideal = alchemy::craft_ideal(item).expect("members are craftable");
			let feedstock = alchemy::craft_feedstock(item);
			// Feedstock lines first — matter in hand reads before the
			// currency — then the essence lines at this level's waste.
			let costs =
				feedstock
					.iter()
					.map(|&(f, n)| format!("{n} {}", block_name(f)))
					.chain(ideal.iter().map(|&(e, n)| {
						format!("{} {}", alchemy::craft_cost(n, view.level), e.name())
					}))
					.collect::<Vec<_>>()
					.join(" + ");
			let tail = match batches {
				// A full cap is not a shortfall, and naming a cost here
				// would send the player off to gather something that
				// cannot help. Tools are a yes/no; the bucket holds one
				// charge.
				0 if view.headroom[item as usize] == 0 => {
					if item.is_tool() {
						"ALREADY OWNED".to_string()
					} else {
						"ALREADY CHARGED".to_string()
					}
				}
				// Name the first shortfall so the player knows what to go
				// find — a feedstock block or an essence alike.
				0 => feedstock
					.iter()
					.find_map(|&(f, n)| {
						let have = view.counts[f as usize];
						(have < n).then(|| format!("NEED {} MORE {}", n - have, block_name(f)))
					})
					.or_else(|| {
						ideal.iter().find_map(|&(e, n)| {
							let cost = alchemy::craft_cost(n, view.level);
							let have = view.subs[e as usize];
							(have < cost).then(|| format!("NEED {} MORE {}", cost - have, e.name()))
						})
					})
					.unwrap_or_else(|| "NOTHING TO CHARGE".into()),
				n => format!("×{n}"),
			};
			// The TAIL is what has to survive: a price is a static fact
			// the player can go and read, but "why can't I" is this
			// line's whole job — and the long prices at the end of the
			// catalog are long enough to push it off the end. Drop the
			// costs before letting that happen.
			let full = format!("{}   —   {costs}   ·   {tail}", block_name(item));
			if ui.style.measure(&full) <= avail {
				full
			} else {
				format!("{}   —   {tail}", block_name(item))
			}
		};
		widgets::label(
			ui.dl,
			Layer::Content,
			text_x,
			strip.y + (strip_h - glyph) / 2.0,
			glyph,
			if locked { DIM } else { WHITE },
			&elide(&ui.style, &text, avail),
		);
		// While a batch runs the one button is its STOP.
		let running = self.crafting;
		widgets::button(
			ui,
			InvId::CraftGo,
			btn,
			if running.is_some() {
				"CANCEL"
			} else {
				"TRANSMUTE"
			},
			running.is_some() || (!locked && batches > 0),
			btn.contains(cx, cy),
		);
		y += strip_h + theme::ROW_GAP * s;
		// The row under the button is the modifier legend when the bench
		// is idle and the batch's own progress when it is not: a craft is
		// paced, so it needs somewhere to say how far along it is and
		// how many are still owed. It names its own item rather than
		// borrowing the strip's, which follows the SELECTION and may have
		// moved on while the batch runs.
		let queued = running.map(|(item, left)| format!("{}   ·   {left} LEFT", block_name(item)));
		widgets::row(
			ui.dl,
			&ui.style,
			Layer::Content,
			Rect::new(row_x, y, row_w, theme::ROW_H * s),
			widgets::RowContent {
				text: queued
					.as_deref()
					.unwrap_or("CLICK 1   ·   SHIFT 10   ·   CTRL 100"),
				right: None,
				bar: queued.is_some().then(|| self.progress.clamp(0.0, 1.0)),
				color: if queued.is_some() { WHITE } else { DIM },
				highlight: false,
				caret: false,
			},
		);
	}

	// ---- input -------------------------------------------------------

	/// Feed one input event; returns nothing directly (the craft drip
	/// goes through `tick`).
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
		view: &AlchView,
	) -> Option<AlchAction> {
		match *event {
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
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
					InvId::Family(i) => {
						let i = (i as usize).min(families().len() - 1);
						if self.family != Some(i) {
							self.family = Some(i);
							// Land on the first member the level has
							// opened (else the first, whose strip line
							// names its unlock).
							self.member = families()[i]
								.members
								.iter()
								.position(|&m| unlock::can_craft(m, view.level))
								.unwrap_or(0);
						}
					}
					InvId::Craft(j) => {
						let family = &families()[self.family?.min(families().len() - 1)];
						if (j as usize) < family.members.len() {
							self.member = j as usize;
						}
					}
					InvId::CraftGo => {
						// Pressing it again while a batch is running stops
						// it — one control, both ways.
						if self.crafting.is_some() {
							self.crafting = None;
							self.progress = 0.0;
							return None;
						}
						let family = &families()[self.family?.min(families().len() - 1)];
						let item = *family.members.get(self.member)?;
						let want = if self.ctrl {
							BULK_CTRL
						} else if self.shift {
							BULK_SHIFT
						} else {
							1
						};
						// QUEUED, not sent: `tick` drips one `Craft` per
						// interval out of this, so a hundred-item batch is
						// a hundred paced crafts rather than a frame's
						// worth of XP. The clamp is unchanged — the offer
						// is still what the holdings afford right now, and
						// the queue re-asks as it goes.
						let count = want.min(view.affordable(item));
						if count > 0 {
							self.crafting = Some((item, count));
							self.progress = 0.0;
						}
						return None;
					}
					_ => {}
				}
				None
			}
			InputEvent::FocusLost => {
				self.shift = false;
				self.ctrl = false;
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

/// Cut `text` to fit `max_w`, marking the cut — there is no clipping.
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
///
/// A grid item is its substance and its form, in that order — "STONE" +
/// "STAIRS" — so a new material × shape pair names itself and cannot land
/// on the old table's `?` fallback. Only the hand-priced functional items
/// need naming by hand, for the same reason they need pricing by hand.
pub(crate) fn block_name(b: BlockType) -> String {
	if let Some((m, s)) = b.grid_cell() {
		return match s.item_suffix() {
			Some(suffix) => format!("{} {suffix}", m.name()),
			None => m.name().into(),
		};
	}
	match b {
		BlockType::Water => "WATER CHARGE".into(),
		BlockType::Torch => "TORCH".into(),
		BlockType::Table => "TABLE".into(),
		BlockType::Chair => "CHAIR".into(),
		BlockType::Bed => "BED".into(),
		BlockType::Bucket => "BUCKET".into(),
		BlockType::EssenceSiphon => "ESSENCE SIPHON".into(),
		BlockType::ResonantPick => "RESONANT PICK".into(),
		BlockType::Lantern => "LANTERN".into(),
		BlockType::Aetherlamp => "AETHERLAMP".into(),
		BlockType::MastersSigil => "MASTER'S SIGIL".into(),
		_ => "?".into(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::DrawList;
	use digblock_core::block::BlockType as B;

	const SIZE: (u32, u32) = (1280, 800);

	struct Rig {
		screen: AlchScreen,
		hits: HitMap<InvId>,
		view: AlchView,
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
		screen.build(&mut ui, SIZE, &view, 1.0);
		Rig {
			screen,
			hits,
			view,
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
		r.screen.build(&mut ui, SIZE, &r.view, 1.0);
		r.hits = hits;
	}

	fn feed(r: &mut Rig, ev: &InputEvent) -> Option<AlchAction> {
		r.screen.handle_input(ev, &r.hits, &r.view)
	}

	/// Every line of text the screen draws, in emission order — for the
	/// assertions that are about what a player READS.
	fn text_of(r: &Rig) -> String {
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: r.style,
		};
		r.screen.build(&mut ui, SIZE, &r.view, 1.0);
		let (cmds, arena) = dl.cmds(Layer::Content);
		cmds.iter()
			.filter_map(|c| match c {
				crate::ui::draw::Cmd::Text { range, .. } => {
					Some(&arena[range.0 as usize..range.1 as usize])
				}
				_ => None,
			})
			.collect::<Vec<_>>()
			.join(" | ")
	}

	fn center(rect: Rect) -> (f32, f32) {
		(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
	}

	fn press(r: &mut Rig, at: (f32, f32)) -> Option<AlchAction> {
		feed(r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
		feed(
			r,
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
		)
	}

	fn family(r: &Rig, name: &str) -> InvId {
		let _ = r;
		InvId::Family(
			families()
				.iter()
				.position(|f| f.name == name)
				.unwrap_or_else(|| panic!("no {name} family")) as u8,
		)
	}

	/// A batch DRIPS: one paced `Craft { count: 1 }` per interval, with a
	/// progress line under the button counting what is left, until the
	/// queue empties or the price does.
	///
	/// This is the whole point of pacing a craft. The CTRL multiplier used
	/// to pay a hundred crafts' XP in one frame, which made the bench's
	/// one instant action also its fastest XP.
	#[test]
	fn a_transmute_batch_drips_instead_of_paying_at_once() {
		let mut r = rig_at(20, |inv| {
			inv.add(B::Wood, 50);
			inv.add_sub(Essence::Flora, 500);
		});
		let slabs = center(r.hits.rect_of(family(&r, "SLABS")).unwrap());
		press(&mut r, slabs);
		rebuild(&mut r);
		feed(
			&mut r,
			&InputEvent::Key {
				key: Key::Shift,
				pressed: true,
			},
		);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		press(&mut r, go);
		assert_eq!(
			r.screen.crafting,
			Some((B::WoodSlab, 10)),
			"SHIFT queues 10"
		);

		// Half a cycle pays nothing.
		let pace = alchemy::craft_pace_ticks(B::WoodSlab, 20) as f32 / 60.0;
		assert_eq!(r.screen.tick(pace * 0.5, &r.view), None);
		let one = Some(AlchAction::Craft {
			item: B::WoodSlab,
			count: 1,
		});
		assert_eq!(r.screen.tick(pace * 0.6, &r.view), one);
		assert_eq!(
			r.screen.crafting,
			Some((B::WoodSlab, 9)),
			"one off the queue"
		);

		// The row under the button is the batch's own progress while it
		// runs, and the modifier legend again once it stops.
		rebuild(&mut r);
		let text = text_of(&r);
		assert!(
			text.contains("9 LEFT"),
			"the batch says what is left: {text}"
		);
		assert!(!text.contains("CLICK 1"), "the legend steps aside: {text}");

		// The background bench: closing the screen keeps the queue, and
		// the drip keeps sending. Only leaving the world abandons.
		r.screen.close();
		assert!(r.screen.busy(), "the bench works with the screen closed");
		assert_eq!(
			r.screen.job(),
			Some(BenchJob::Craft(B::WoodSlab, 9)),
			"the HUD tell knows the job"
		);
		assert_eq!(r.screen.tick(pace * 1.1, &r.view), one);
		r.screen.open();

		// Spend the essence out from under it: the queue withdraws exactly
		// where the button would have greyed, rather than running on
		// against a rebase that can no longer pay.
		let flora = r.inv.sub_count(Essence::Flora);
		assert!(
			r.inv.try_take_sub(Essence::Flora, flora),
			"drain the holdings"
		);
		rebuild(&mut r);
		assert_eq!(r.screen.tick(pace * 1.1, &r.view), None);
		assert_eq!(r.screen.crafting, None, "a queue nobody can pay for ends");
		rebuild(&mut r);
		assert!(text_of(&r).contains("CLICK 1"), "the legend is back");

		// The world-exit arm clears outright.
		r.screen.debug_batch(B::WoodSlab, 3, 0.5);
		assert!(r.screen.busy());
		r.screen.abandon();
		assert!(!r.screen.busy(), "abandon drops the job");
	}

	/// The transmute flow: pick a family tile, pick a member, and the
	/// page's ONE button queues it — multipliers clamp to affordability,
	/// and a locked selection disables the button outright (no hit).
	#[test]
	fn family_selection_and_the_single_transmute_button() {
		// A level past the mason's unlock and short of dark lumber's, so
		// the rig holds an unlocked-but-unaffordable member (Stone, with
		// no Earth in hand) AND a locked one (DarkLumber) at once.
		let mut r = rig_at(15, |inv| {
			inv.add(B::Wood, 50);
			inv.add_sub(Essence::Flora, 500);
		});

		// It opens on the family row alone: no member is pre-picked, so
		// there is nothing for the one button to act on and it isn't
		// drawn.
		assert!(
			r.hits.rect_of(InvId::Craft(0)).is_none(),
			"no members before a family is picked"
		);
		assert!(
			r.hits.rect_of(InvId::CraftGo).is_none(),
			"no button before a family is picked"
		);

		// SLABS leads (the shape axis first, cheapest form first, the
		// hand-priced ones after). Its first unlocked member is the
		// WoodSlab the Flora affords; Ctrl asks for its whole batch and
		// the holdings clamp it.
		let slabs = center(r.hits.rect_of(family(&r, "SLABS")).unwrap());
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
		// The press QUEUES the batch rather than paying for it.
		assert_eq!(press(&mut r, go), None, "the button starts a batch");
		assert_eq!(r.screen.crafting, Some((B::WoodSlab, batches)));
		// Pressing it again is the stop, so the bench is idle for the
		// selection checks below.
		rebuild(&mut r);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		assert_eq!(press(&mut r, go), None);
		assert_eq!(r.screen.crafting, None, "the second press cancels");
		feed(
			&mut r,
			&InputEvent::Key {
				key: Key::Ctrl,
				pressed: false,
			},
		);

		// BLOCKS: switching family lands on the first unlocked member —
		// Lumber, since the grid puts a material's whole learned set at
		// the material's own level and planed timber is the first
		// substance the bench makes in cube form. (The felled log is not
		// here: the world hands you that one, so it has no recipe.) The
		// one button queues THAT.
		let blocks = center(r.hits.rect_of(family(&r, "BLOCKS")).unwrap());
		press(&mut r, blocks);
		rebuild(&mut r);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		assert_eq!(press(&mut r, go), None);
		assert_eq!(r.screen.crafting, Some((B::Lumber, 1)));
		rebuild(&mut r);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		press(&mut r, go);
		assert_eq!(r.screen.crafting, None, "back to idle");
		// A member locked at this rig's level: the tile still shows (the
		// strip names the level it wants), but the single button registers
		// no hit — honest pixels, not a trap.
		let dark = center(r.hits.rect_of(InvId::Craft(2)).unwrap());
		press(&mut r, dark);
		rebuild(&mut r);
		assert!(
			r.hits.rect_of(InvId::Craft(2)).is_some(),
			"locked member still shows"
		);
		assert!(
			r.hits.rect_of(InvId::CraftGo).is_none(),
			"no button under a locked pick"
		);
		// And picking an unlocked-but-unaffordable member by tile keeps the
		// button away: "can't yet" and "can't afford" are different
		// answers, and neither leaves a hit rect.
		let stone = center(r.hits.rect_of(InvId::Craft(1)).unwrap());
		press(&mut r, stone);
		rebuild(&mut r);
		assert!(r.hits.rect_of(InvId::CraftGo).is_none(), "Stone: no Earth");
	}

	/// The vessels are the catalog's first crafts, and they are paid in
	/// FEEDSTOCK: a fresh player with timber can make the bucket and the
	/// siphon at level 1 with no essence anywhere — the whole reason the
	/// economy can bootstrap — and the strip names the missing timber
	/// when they cannot.
	#[test]
	fn the_vessels_craft_from_timber_at_level_one() {
		let mut r = rig_at(1, |inv| inv.add(B::Wood, 20));
		let tools = center(r.hits.rect_of(family(&r, "TOOLS")).unwrap());
		press(&mut r, tools);
		rebuild(&mut r);
		// Lands on the bucket — the first unlocked member.
		assert!(
			r.view.affordable(B::Bucket) > 0,
			"timber affords the bucket"
		);
		assert!(
			r.view.affordable(B::EssenceSiphon) > 0,
			"and the essence tool"
		);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		press(&mut r, go);
		assert_eq!(r.screen.crafting, Some((B::Bucket, 1)));
		// Cancel (the second press), keeping the family selection.
		rebuild(&mut r);
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		press(&mut r, go);
		assert_eq!(r.screen.crafting, None);

		// Short on timber: the strip names the FEEDSTOCK shortfall, not an
		// essence that cannot help.
		let wood = r.inv.count(B::Wood);
		assert!(r.inv.try_take(B::Wood, wood), "drain the timber");
		rebuild(&mut r);
		let text = text_of(&r);
		assert!(
			text.contains("MORE WOOD"),
			"the strip names the missing timber: {text}"
		);
		assert!(
			r.hits.rect_of(InvId::CraftGo).is_none(),
			"no button with nothing to pay"
		);
	}

	/// The panel bottom-anchors above the hotbar, and a short window at a
	/// big scale fits by stepping the ladder.
	#[test]
	fn panel_sits_above_the_hotbar_and_fits_short_windows() {
		for (size, scale) in [
			((1280u32, 800u32), 1.0f32),
			((1280, 720), 1.75),
			((960, 600), 2.5),
		] {
			let inv = Inventory::default();
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
			screen.build(&mut ui, size, &view, 1.0);
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

	/// The families are derived from the shape axis, so a new material ×
	/// shape pair reaches the screen on its own — but a hand-priced
	/// FUNCTIONAL item still has to be filed by hand, and this is what
	/// catches one that wasn't. Every craftable appears in exactly one
	/// family; nothing else appears at all.
	#[test]
	fn every_craftable_appears_in_exactly_one_family() {
		for &b in &BlockType::ALL {
			let craftable = unlock::craft(b).is_some();
			let appearances: usize = families()
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

	/// Owning a tool is a yes/no (§7.3), so the bench sells exactly one
	/// and then stops offering it. `apply_alchemy` refuses a craft that
	/// would vanish into the cap — an offer the rule refuses is one the
	/// player pays a tool's whole price for and gets nothing back — and
	/// the strip says WHY: a cap is not a shortfall, and naming a cost
	/// here would send them off to gather something that cannot help.
	#[test]
	fn an_owned_tool_is_no_longer_on_offer() {
		let mut r = rig_at(99, |inv| {
			for &e in &Essence::ALL {
				inv.add_sub(e, 5_000);
			}
		});
		let tools = center(r.hits.rect_of(family(&r, "TOOLS")).unwrap());
		press(&mut r, tools);
		rebuild(&mut r);
		let member = families()
			.iter()
			.find(|f| f.name == "TOOLS")
			.unwrap()
			.members
			.iter()
			.position(|&m| m == B::ResonantPick)
			.expect("the pick is a tool");
		let at = center(r.hits.rect_of(InvId::Craft(member as u8)).unwrap());
		press(&mut r, at);
		rebuild(&mut r);

		assert_eq!(r.view.affordable(B::ResonantPick), 1, "one, never a batch");
		let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
		assert_eq!(press(&mut r, go), None, "the press queues the forge");
		// And the forge takes the longest hold in the catalog — the
		// biggest price is the biggest piece of work.
		let pace = alchemy::craft_pace_ticks(B::ResonantPick, 99) as f32 / 60.0;
		assert!(
			pace > alchemy::craft_pace_ticks(B::LumberSlab, 99) as f32 / 60.0,
			"a pick should not forge as fast as a slab"
		);
		assert_eq!(r.screen.tick(pace * 0.9, &r.view), None, "mid-hold");
		assert_eq!(
			r.screen.tick(pace * 0.2, &r.view),
			Some(AlchAction::Craft {
				item: B::ResonantPick,
				count: 1
			})
		);
		assert_eq!(r.screen.crafting, None, "a batch of one ends with it");

		// The server's answer arrives: it's in the bag. The offer closes.
		r.inv.add(B::ResonantPick, 1);
		rebuild(&mut r);
		assert_eq!(r.view.affordable(B::ResonantPick), 0);
		assert!(
			r.hits.rect_of(InvId::CraftGo).is_none(),
			"no button on an item that cannot be crafted"
		);
		let text = text_of(&r);
		assert!(text.contains("ALREADY OWNED"), "the strip explains: {text}");
		assert!(!text.contains("NEED"), "a cap is not a shortfall: {text}");
	}
}
