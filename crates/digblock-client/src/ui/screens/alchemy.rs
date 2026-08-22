//! The alchemy screen: ONE page under a header that always shows the Alchemy
//! level, its XP bar and the essence holdings. Matter never becomes essence at
//! a bench — the siphon draws nodes in the field, and this screen is where the
//! essence is SPENT.
//!
//! The page is a row of FAMILY tiles whose icons slowly rotate through their
//! members, the selected family's members wrapped into the inventory grid's
//! columns beneath, a detail strip, and the page's ONE `TRANSMUTE` button,
//! clamped to what the holdings afford.
//!
//! **The page shows what you have and one tier of what is coming, and nothing
//! else** ([`member_visible`]): a member is drawn only if its recipe is unlocked
//! or sits at the catalog's NEXT unlock level — every item sharing that level
//! shows together — and a family with nothing on the page hides entirely. The
//! families are DERIVED from §7.2's shape axis rather than mirrored by hand, and
//! `every_craftable_appears_in_exactly_one_family` holds the derivation to the
//! core catalog, so a new craftable cannot silently miss the screen.
//!
//! The button QUEUES that count rather than buying it: crafting drips at
//! `craft_pace_ticks`, and the button itself stands as the batch's CANCEL.
//! Answering instantly made the bench's one immediate action also its fastest
//! XP — a hundred crafts paid in a frame. The bench keeps working with the
//! screen closed; only leaving the world abandons.
//!
//! The screen never mutates game state: `handle_input`/`tick` return actions,
//! and the engine applies them locally offline or sends them to the server
//! online (the `InventoryFull` echo is the rebase).

use digblock_core::alchemy;
use digblock_core::block::{BlockType, Essence, ShapeFamily};
use digblock_core::hotbar::HotbarItem;
use digblock_core::inv_grid::GRID_SLOTS;
use digblock_core::inventory::Inventory;
use digblock_core::material::Material;
use digblock_core::shapes;
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
const BULK_CTRL: u32 = 200;

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

/// The one thing missing before a craft can go ahead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShortOf {
	Feedstock(BlockType),
	Essence(Essence),
}

/// Why the transmute strip says what it says.
///
/// Extracted from the words so a test can ask which case a tile is in
/// without quoting the line: the wording is this screen's own label and
/// may be reworded freely, but WHICH case applies is a rule — and the
/// one that matters most, a full cap reading as a shortfall, sent the
/// player off to gather something that could not help.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StripReason {
	/// Nothing can be added: a tool already made, or a vessel already
	/// charged. `apply_alchemy` refuses on exactly this, so the offer
	/// has to close here too or it is a lie the player pays for.
	AtCap { tool: bool },
	/// The first shortfall, and how much more of it is wanted.
	Short { of: ShortOf, more: u32 },
	/// Affordable: how many batches the stock buys.
	Affords(u32),
	/// Craftable and paid for, but the craft would add nothing.
	NothingToCharge,
}

/// Which case the strip is in for `item`, at this view's level and
/// stock. One answer, shared by the line that is drawn and the tests
/// that check the rule.
pub fn strip_reason(view: &AlchView, item: BlockType) -> StripReason {
	let batches = view.affordable(item);
	if batches > 0 {
		return StripReason::Affords(batches);
	}
	if view.headroom[item as usize] == 0 {
		return StripReason::AtCap {
			tool: item.is_tool(),
		};
	}
	let feedstock = alchemy::craft_feedstock(item);
	if let Some((f, more)) = feedstock.iter().find_map(|&(f, n)| {
		let have = view.counts[f as usize];
		(have < n).then(|| (f, n - have))
	}) {
		return StripReason::Short {
			of: ShortOf::Feedstock(f),
			more,
		};
	}
	let ideal = alchemy::craft_ideal(item).expect("members are craftable");
	if let Some((e, more)) = ideal.iter().find_map(|&(e, n)| {
		let cost = alchemy::craft_cost(n, view.level);
		let have = view.subs[e as usize];
		(have < cost).then(|| (e, cost - have))
	}) {
		return StripReason::Short {
			of: ShortOf::Essence(e),
			more,
		};
	}
	StripReason::NothingToCharge
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
			//
			// Headroom counts ITEMS and this function counts BATCHES, so
			// it has to be divided by the batch before the two can be
			// compared — `apply_alchemy` refuses on `headroom < batch *
			// count`, and offering a count it will refuse leaves the drip
			// queued against a bench that never moves. One number, so the
			// offer and the rule cannot drift.
			.min(self.headroom[item as usize] / shapes::craft_batch(item).max(1))
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
		"Lights",
		&[BlockType::Torch, BlockType::Lantern, BlockType::Aetherlamp],
	),
	(
		"Furniture",
		&[BlockType::Chair, BlockType::Table, BlockType::Bed],
	),
	(
		"Tools",
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

/// The next Alchemy level ABOVE `level` at which anything in the catalog
/// unlocks — the page's one teaser tier.
fn next_unlock_level(level: u32) -> Option<u32> {
	families()
		.iter()
		.flat_map(|f| f.members.iter())
		.filter_map(|&b| unlock::craft(b))
		.filter(|&l| l > level)
		.min()
}

/// Is this member on the page at all? Already unlocked, or part of the
/// NEXT unlock — everything past that stays hidden, so a fresh player
/// reads a short page of what they can do and one tier of what is coming
/// instead of the whole catalog as a wall of locks. Several items sharing
/// the next level all show together, since the tier is a LEVEL and not an
/// item.
fn member_visible(b: BlockType, level: u32, next: Option<u32>) -> bool {
	unlock::craft(b).is_some_and(|l| l <= level || Some(l) == next)
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

		let (width, height) = (self.content_w(&ui.style, view), self.content_h(&ui.style));
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

	/// The essence readout, which is also the widest text the page draws.
	///
	/// One function so the width the panel is MEASURED at and the string
	/// the header EMITS cannot drift — the reason `text_row_w` exists at
	/// all, and the reason a width is never counted in characters.
	fn holdings_text(view: &AlchView) -> String {
		Essence::ALL
			.iter()
			.map(|&e| format!("{} {}", e.name().to_uppercase(), view.subs[e as usize]))
			.collect::<Vec<_>>()
			.join("   ")
	}

	fn content_w(&self, style: &Style, view: &AlchView) -> f32 {
		// The inventory grid's own width, which is also what every other
		// screen measures against — the page keeps its column.
		let spec = Self::grid_spec_at(style, (0.0, 0.0));
		let grid = spec.extent(GRID_SLOTS).0;
		// ...but the holdings row is TEXT, and text does not shrink with
		// the panel: `glyph_px` floors, so the bottom rungs of the fit
		// ladder take a quarter off every box while the readout stays the
		// size it was. Folding it in is what keeps the row inside the
		// panel it is drawn in — it used to be drawn at the grid's width
		// and spill past the edge onto the dimmed world, with nothing
		// clipping it, and the spill grew with the digits.
		grid.max(theme::text_row_w(style, &Self::holdings_text(view)))
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
		// Essence readout, always visible — and the row `content_w` sizes
		// the panel against, through this same function.
		//
		// Elided against the width it is actually drawn at, because the
		// panel cannot always be as wide as the readout wants: `row_w` is
		// clamped to the window, so a player holding near the stack cap
		// on a phone asks for more width than exists. `widgets::row` does
		// not clip, so without this the tail is drawn past the panel edge
		// onto the dimmed world.
		let holdings = elide(
			&ui.style,
			&Self::holdings_text(view),
			row_w - 2.0 * scale::text_pad_x(s),
		);
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
		// one.
		//
		// Only families with something ON the page draw at all — packed
		// left, but keeping their TRUE index in `InvId::Family`, so hiding
		// never shifts selection state or hit identity. A family whose
		// only visible members are next-unlock teasers dims like a locked
		// tile and badges that level.
		let next = next_unlock_level(view.level);
		let fam_spec = Self::grid_spec_at(&ui.style, (row_x + text_pad, y));
		let mut col = 0usize;
		for (i, fam) in families().iter().enumerate() {
			let visible: Vec<BlockType> = fam
				.members
				.iter()
				.copied()
				.filter(|&m| member_visible(m, view.level, next))
				.collect();
			if visible.is_empty() {
				continue;
			}
			let rect = fam_spec.rect(col);
			col += 1;
			let all_locked = visible.iter().all(|&m| !unlock::can_craft(m, view.level));
			// The tile trades one member for the next by shrinking the
			// outgoing one away and growing the incoming one in, rather
			// than cutting between them — rotating over the VISIBLE
			// members only, or the tile would advertise hidden items.
			let (mi, present) = family_preview(self.clock, visible.len());
			let preview = visible[mi];
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
				let earliest = visible
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
			let mut col = 0usize;
			for (j, &item) in family.members.iter().enumerate() {
				if !member_visible(item, view.level, next) {
					continue;
				}
				let rect = mem_spec.rect(col);
				col += 1;
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
			format!("{}   —   LV {need}", item.name().to_uppercase())
		} else {
			let ideal = alchemy::craft_ideal(item).expect("members are craftable");
			let feedstock = alchemy::craft_feedstock(item);
			// Feedstock lines first — matter in hand reads before the
			// currency — then the essence lines at this level's waste.
			let costs = feedstock
				.iter()
				.map(|&(f, n)| format!("{n} {}", f.name().to_uppercase()))
				.chain(ideal.iter().map(|&(e, n)| {
					format!(
						"{} {}",
						alchemy::craft_cost(n, view.level),
						e.name().to_uppercase()
					)
				}))
				.collect::<Vec<_>>()
				.join(" + ");
			// The case is decided once, in `strip_reason`; this only
			// chooses words for it.
			let tail = match strip_reason(view, item) {
				StripReason::AtCap { tool: true } => "ALREADY OWNED".to_string(),
				StripReason::AtCap { tool: false } => "ALREADY CHARGED".to_string(),
				StripReason::Short { of, more } => {
					let what = match of {
						ShortOf::Feedstock(f) => f.name().to_uppercase(),
						ShortOf::Essence(e) => e.name().to_uppercase(),
					};
					format!("NEED {more} MORE {what}")
				}
				StripReason::NothingToCharge => "NOTHING TO CHARGE".to_string(),
				StripReason::Affords(n) => format!("×{n}"),
			};
			// The TAIL is what has to survive: a price is a static fact
			// the player can go and read, but "why can't I" is this
			// line's whole job — and the long prices at the end of the
			// catalog are long enough to push it off the end. Drop the
			// costs before letting that happen.
			let full = format!("{}   —   {costs}   ·   {tail}", item.name().to_uppercase());
			if ui.style.measure(&full) <= avail {
				full
			} else {
				format!("{}   —   {tail}", item.name().to_uppercase())
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
		let queued =
			running.map(|(item, left)| format!("{}   ·   {left} LEFT", item.name().to_uppercase()));
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
						// interval out of this, so a bulk batch is that many
						// paced crafts rather than a frame's worth of XP.
						// The clamp is unchanged — the offer
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

#[cfg(test)]
mod tests;
