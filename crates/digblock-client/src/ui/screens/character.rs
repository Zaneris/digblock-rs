//! The character screen (C): one row per skill — level, XP bar to the
//! next level, the running total. Read-only; it never mutates game
//! state and registers no hit regions. The engine owns opening and
//! closing it like every other screen.

use digblock_core::block::{BlockType, ShapeFamily};
use digblock_core::material::Material;
use digblock_core::skills::{self, Skill, SkillSet};
use digblock_core::unlock;
use digblock_platform::InputEvent;

use crate::hotbar::HotbarView;
use crate::menu::RankView;
use crate::ui::draw::Layer;
use crate::ui::widgets::{self, DIM, Style, Ui, WHITE};
use crate::ui::{InvId, Rect, hud, theme};

/// The screen's per-frame view, derived once from the engine's
/// `SkillSet` so the measure and emit passes read the same numbers.
pub struct CharView {
	rows: Vec<Row>,
}

impl CharView {
	/// `eye_y` is where the player is standing, for Mining's depth-reward
	/// readout. A READOUT, not the rule: the award keys on the cell that
	/// was dug, which is a block or two under the eye, so this says what
	/// this depth is worth and never claims to price a particular swing.
	/// `ranks` is the master's answer for this account, when there is one
	/// — a board the account is not on says nothing rather than "unranked".
	pub fn build(skills: &SkillSet, eye_y: i32, ranks: &[RankView]) -> Self {
		let rank_of = |skill: Option<Skill>| ranks.iter().find(|r| r.skill == skill);
		let mut rows = vec![Row::Title, Row::Space];
		let mut total_level = 0;
		for &skill in &Skill::ALL {
			let xp = skills.xp(skill);
			let level = skills.level(skill);
			total_level += level;
			let (frac, info) = match skills::xp_to_next(level) {
				Some(need) => {
					let into = xp - skills::total_xp_for(level);
					(
						(into as f32 / need as f32).clamp(0.0, 1.0),
						format!("{} / {} XP to LV {}", group(into), group(need), level + 1),
					)
				}
				None => (1.0, format!("{} XP - mastered", group(xp))),
			};
			rows.push(Row::Skill {
				text: format!("{}   LV {}", skill.name().to_uppercase(), level),
				frac,
			});
			rows.push(Row::Info(info));
			if let Some(r) = rank_of(Some(skill)) {
				rows.push(Row::Info(rank_line(r)));
			}
			if let Some((lvl, what)) = next_unlock(skill, level) {
				rows.push(Row::Info(format!("next: LV {lvl} - {what}")));
			}
			// §5.2's depth reward, on the skill it pays into. Stated as a
			// live number rather than a rule, because the ramp is what
			// makes it worth reading: it answers "is it worth going down"
			// with the figure for where you are right now.
			if skill == Skill::Mining {
				rows.push(Row::Info(depth_bonus_line(eye_y)));
			}
			rows.push(Row::Space);
		}
		rows.push(Row::Info(format!("TOTAL LEVEL {total_level}")));
		if let Some(r) = rank_of(None) {
			rows.push(Row::Info(rank_line(r)));
		}
		Self { rows }
	}
}

/// The board line under a skill (or the total): where this account stands. The number on the
/// board is what makes the totals above it mean something (SOCIAL-DESIGN §1).
fn rank_line(r: &RankView) -> String {
	format!("hiscores: rank {}", group(u64::from(r.rank)))
}

/// What `DIG_SCREEN=skills` and the screen sweep show for the board lines, since neither run
/// has a master to ask: a rank on every board, so every line the screen can draw is drawn.
pub fn sample_ranks(skills: &SkillSet) -> Vec<RankView> {
	let total = Skill::ALL.iter().map(|&s| skills.level(s)).sum();
	let mut out: Vec<RankView> = Skill::ALL
		.iter()
		.enumerate()
		.map(|(i, &s)| RankView {
			skill: Some(s),
			rank: 12 + i as u32 * 7,
			level: skills.level(s),
			xp: skills.xp(s),
		})
		.collect();
	out.push(RankView {
		skill: None,
		rank: 9,
		level: total,
		xp: Skill::ALL.iter().map(|&s| skills.xp(s)).sum(),
	});
	out
}

/// The §8.2 teasers: what the next threshold above `level` opens. The prose is the screen's; every
/// LEVEL is READ from `unlock`, never restated, so a retune moves the screen with it and the pin
/// below only has to check the set still matches [`unlock::opens`]. Where several things land on
/// one level, the row asks for the one its sentence leads with.
pub(super) fn teasers(skill: Skill) -> Vec<(u32, &'static str)> {
	match skill {
		Skill::Mining => mining_teasers(),
		Skill::Alchemy => alch_teasers(),
		Skill::Arcana => arcana_teasers(),
	}
}

/// Mining's: the strength wall's two rungs, then the ore reveals and the
/// sense that forewarns them — the levels the whole skill is shaped
/// around. Content tops at mastery; the pick's plane stands alone in the
/// tail on purpose.
fn mining_teasers() -> Vec<(u32, &'static str)> {
	use BlockType as B;
	vec![
		(
			unlock::mine(B::Wood),
			"timber gives: trees and planed wood",
		),
		(
			unlock::mine(B::Stone),
			"rock gives: gravel, sandstone, stone",
		),
		(reveal_at(B::Emberore), "emberore reveals: bring a siphon"),
		(
			unlock::PROSPECT_FAINT_AT,
			"prospector's sense: faint, but you hear what you can't see",
		),
		(reveal_at(B::Duskstone), "duskstone reveals"),
		(
			reveal_at(B::Glowvein),
			"glowvein reveals; the resonant pick swings wide; ferrum's block opens",
		),
		(unlock::mine(B::Crystal), "crystal opens"),
		(unlock::PROSPECT_SHARP_AT, "the prospector's sense sharpens"),
		(reveal_at(B::Aetherock), "aetherock reveals"),
		(unlock::MASTERY_AT, "mastery: every vein gives one more"),
		(unlock::PICK_PLANE_AT, "the pick's full nine"),
	]
}

/// Alchemy's: the shape and material unlocks, the hand-priced items, the
/// surface-node reveals, and the three levels that change a bench NUMBER
/// rather than opening a recipe.
fn alch_teasers() -> Vec<(u32, &'static str)> {
	use BlockType as B;
	use Material as M;
	vec![
		(
			unlock::shape(ShapeFamily::Stairs),
			"the stairs shape; leystone reveals: terra in shallow rock",
		),
		(item_at(B::Torch), "the torch"),
		(
			mat_at(M::Lumber),
			"planed lumber and its joinery; springstone reveals: unda under fresh water",
		),
		(item_at(B::Table), "furniture: the table and the chair"),
		(
			mat_at(M::Stone),
			"clean stone: every learned shape at once",
		),
		(mat_at(M::DarkLumber), "dark lumber and stone brick"),
		(mat_at(M::Sandstone), "sandstone and glass"),
		(item_at(B::Bed), "the bed: timber and bedding both"),
		(unlock::BENCH_FASTER_AT, "a faster bench"),
		(mat_at(M::RedLumber), "red lumber"),
		(item_at(B::Lantern), "the lantern"),
		(
			mat_at(M::PolishedStone),
			"polished stone and sandstone brick",
		),
		(mat_at(M::Metal), "FERRUM's block: the prestige material"),
		(item_at(B::ResonantPick), "the resonant pick"),
		(mat_at(M::Crystal), "crystal, grown from aether"),
		(unlock::BENCH_FASTEST_AT, "the fastest bench"),
		(
			item_at(B::Aetherlamp),
			"the aetherlamp: the standing aether sink",
		),
		(
			unlock::WASTE_DIP_AT,
			"the waste dip: your process tightens early",
		),
		(item_at(B::MastersSigil), "the master's sigil"),
	]
}

/// Arcana's: the waystones humming into reach, with the travel rungs
/// between them — the ports are the spine, the way Mining's reveals are.
fn arcana_teasers() -> Vec<(u32, &'static str)> {
	use digblock_core::worldgen::SurfaceKind as K;
	vec![
		(port_at(K::Plains), "the plains waystone hums into reach"),
		(unlock::KINDLE_AT, "kindle: conjured light in the world"),
		(port_at(K::Forest), "the forest waystone"),
		(
			unlock::SPHERE_AT,
			"the sphere: a shell of whatever you hold rises about you",
		),
		(
			unlock::TELEPORT_SWIFT_AT,
			"swift return: home comes back sooner",
		),
		(
			unlock::CAUSEWAY_AT,
			"the causeway: glass carries your step over nothing",
		),
		(port_at(K::Savanna), "the savanna waystone"),
		(
			unlock::CHANNEL_QUICK_AT,
			"quickened channels: every cast lands sooner",
		),
		(port_at(K::Taiga), "the taiga waystone"),
		(
			unlock::SPHERE_WIDE_AT,
			"the wide sphere: the shell stands further out",
		),
		(
			unlock::WISP_BRIGHT_AT,
			"the wisp burns brighter, and longer",
		),
		(unlock::CAUSEWAY_LONG_AT, "the causeway carries you further"),
		(port_at(K::Beach), "the shore waystone"),
		(port_at(K::Mountain), "the mountain waystone"),
		(
			unlock::SPHERE_VAST_AT,
			"the vast sphere: further out again",
		),
		(unlock::KINDLE_CHEAP_AT, "kindle costs less"),
		(unlock::TELEPORT_SWIFTER_AT, "swifter return"),
		(
			unlock::CAUSEWAY_VAST_AT,
			"the causeway carries you further again",
		),
		(port_at(K::Snow), "the far cold waystone: the last port"),
	]
}

fn port_at(kind: digblock_core::worldgen::SurfaceKind) -> u32 {
	unlock::waystone(kind).expect("a laddered kind has a port level")
}

// The three `Option` levels, with the case that cannot happen stated: an
// ore reveals, a listed material is workable, a hand-priced item has a
// price. A wrong argument is loud at the first character screen rather
// than a row quietly sitting at the first level.
fn reveal_at(b: BlockType) -> u32 {
	unlock::reveal(b).expect("an ore has a reveal level")
}

fn mat_at(m: Material) -> u32 {
	unlock::material(m).expect("a workable material has a level")
}

fn item_at(b: BlockType) -> u32 {
	unlock::functional(b).expect("a hand-priced item has a level")
}

/// The depth-reward readout: the live percentage once there is one to
/// show, and the DIRECTION before that. Rounding decides which — the top of
/// the ramp is a stretch that rounds to nothing, and "+0%" reads as a broken
/// feature to exactly the player who most needs telling the bonus is there.
fn depth_bonus_line(eye_y: i32) -> String {
	match (skills::depth_xp_permille(eye_y) - 1000) / 10 {
		0 => "depth bonus: none yet, deeper pays more".to_string(),
		pct => format!("depth bonus: +{pct}% XP per dig"),
	}
}

fn next_unlock(skill: Skill, level: u32) -> Option<(u32, &'static str)> {
	teasers(skill).into_iter().find(|&(lvl, _)| lvl > level)
}

/// Thousands-grouped decimal ("1,103,585") — XP totals get big enough
/// that ungrouped digits stop being readable at a glance. The hiscores
/// page groups its figures the same way.
pub(super) fn group(n: u64) -> String {
	let digits = n.to_string();
	let mut out = String::with_capacity(digits.len() + digits.len() / 3);
	for (i, c) in digits.chars().enumerate() {
		if i > 0 && (digits.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(c);
	}
	out
}

enum Row {
	Title,
	Space,
	/// One skill: name and level left, the XP bar right — the settings slider reused as a readout.
	Skill {
		text: String,
		frac: f32,
	},
	/// Dim detail line (XP numbers, the total).
	Info(String),
}

fn row_text(row: &Row) -> &str {
	match row {
		Row::Space => "",
		Row::Title => "CHARACTER",
		Row::Skill { text, .. } => text,
		Row::Info(t) => t,
	}
}

fn box_h(row: &Row, scale: f32) -> f32 {
	match row {
		Row::Space => 0.0,
		_ => theme::ROW_H * scale,
	}
}

fn row_h(row: &Row, scale: f32) -> f32 {
	box_h(row, scale)
		+ match row {
			Row::Space => theme::SECTION_GAP * scale,
			_ => 0.0,
		}
}

fn measure_rows(rows: &[Row], style: &Style) -> (f32, f32) {
	let s = style.scale;
	let (mut width, mut height) = (0.0f32, 0.0f32);
	for row in rows {
		let w = match row {
			Row::Space => 0.0,
			// A skill row carries the right-aligned bar: text, a gap,
			// then the shared slider width.
			Row::Skill { .. } => {
				theme::text_row_w(style, row_text(row))
					+ (widgets::SLIDER_BAR_W + theme::SECTION_GAP) * s
			}
			_ => theme::text_row_w(style, row_text(row)),
		};
		width = width.max(w);
		height += row_h(row, s);
	}
	(width, height)
}

#[derive(Default)]
pub struct CharScreen {
	open: bool,
	/// Where the pointer is. The page has no controls of its own; it
	/// tracks this only so it can name the hotbar slot underneath it,
	/// which every screen drawn over the bar owes the player.
	cursor: (f32, f32),
}

impl CharScreen {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		self.open = true;
	}

	pub fn close(&mut self) {
		self.open = false;
	}

	/// Feed one input event. The page is read-only, so the only thing it
	/// takes from one is where the pointer is; every key that means
	/// anything here closes it, and that is the ENGINE's to consume.
	pub fn handle_input(&mut self, event: &InputEvent) {
		if let InputEvent::CursorMoved { x, y } = *event {
			self.cursor = (x, y);
		}
	}

	/// Emit the screen: the shared dim, the shared panel color, the
	/// rows. Same two-pass shape as the alchemy screen (measure, then
	/// emit over the same rows) minus the fit pass — a handful of rows
	/// can't outgrow any window the game runs in, so the frame's one
	/// glyph size is drawn unconditionally.
	pub fn build(
		&self,
		ui: &mut Ui<'_, InvId>,
		size: (u32, u32),
		view: &CharView,
		bar: &HotbarView,
		daylight: f32,
	) {
		let s = ui.style.scale;
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);
		let pad = theme::PAD * s;
		let (width, height) = measure_rows(&view.rows, &ui.style);
		let row_w = width
			.max(theme::MIN_PANEL_W * s)
			.min(size.0 as f32 - 4.0 * pad);
		let row_x = (size.0 as f32 - row_w) / 2.0;
		let bar_top = hud::row_grid(size, s).origin.1;
		let row_y = (bar_top - theme::GAP_ABOVE_BAR * s - (height + 2.0 * pad))
			.max(theme::TOP_MARGIN * s)
			+ pad;
		let panel = theme::panel_of(row_x, row_y, row_w, height, s);
		widgets::panel(ui.dl, Layer::Backdrop, panel, theme::PANEL_BG);

		let mut y = row_y;
		for row in &view.rows {
			let rect = Rect::new(row_x, y, row_w, box_h(row, s));
			y += row_h(row, s);
			match row {
				Row::Space => {}
				Row::Title | Row::Info(_) => {
					let color = if matches!(row, Row::Title) {
						WHITE
					} else {
						DIM
					};
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text: row_text(row),
							right: None,
							bar: None,
							color,
							highlight: false,
							caret: false,
							cells: &[],
						},
					);
				}
				Row::Skill { frac, .. } => {
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text: row_text(row),
							right: None,
							bar: Some(*frac),
							color: WHITE,
							highlight: false,
							caret: false,
							cells: &[],
						},
					);
				}
			}
		}
		// The page owns nothing hoverable of its own, but it is drawn
		// OVER the bar, and the bar is drawn after it at the window's
		// scale — so naming its slots is this page's job, at this frame's
		// glyph. No fit ladder here, so `s` IS the window's scale.
		if let Some((anchor, name)) = hud::hovered_bar_slot(size, s, bar, self.cursor) {
			widgets::tooltip(ui.dl, &ui.style, size, anchor, &name);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::HitMap;
	use crate::ui::draw::{Cmd, DrawList};

	const SIZE: (u32, u32) = (1280, 800);

	/// A bar with nothing on it — these rigs are about the page, and the
	/// cursor never reaches the bar in them anyway.
	fn empty_bar() -> HotbarView {
		HotbarView::build(
			&digblock_core::hotbar::Hotbar::new(),
			&digblock_core::inventory::Inventory::default(),
			&crate::hotbar::ToolSwap::default(),
		)
	}

	fn emit(skills: &SkillSet) -> DrawList {
		emit_at(skills, skills::DEPTH_XP_FROM)
	}

	fn emit_at(skills: &SkillSet, eye_y: i32) -> DrawList {
		let mut dl = DrawList::default();
		let mut hits: HitMap<InvId> = HitMap::default();
		let mut screen = CharScreen::default();
		screen.open();
		screen.build(
			&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(1.0),
			},
			SIZE,
			&CharView::build(skills, eye_y, &[]),
			&empty_bar(),
			1.0,
		);
		assert!(hits.hit(640.0, 400.0).is_none(), "read-only: no hit rects");
		dl
	}

	fn texts(dl: &DrawList) -> Vec<String> {
		let (cmds, arena) = dl.cmds(Layer::Content);
		cmds.iter()
			.filter_map(|c| match c {
				Cmd::Text { range, .. } => {
					Some(arena[range.0 as usize..range.1 as usize].to_string())
				}
				_ => None,
			})
			.collect()
	}

	/// Everything on the row is derived from the XP total rather than remembered: the level
	/// reached, what is banked into it, what the next one costs, and the mastery total. Asserted as
	/// those VALUES reaching the screen, each read from the table that defines it — the words
	/// around them are the screen's own label and may be reworded freely without breaking this.
	#[test]
	fn levels_bars_and_totals_derive_from_xp() {
		let mut skills = SkillSet::default();
		// Dead on a level boundary: nothing banked into the next step.
		const AT: u32 = 10;
		skills.set_total(Skill::Mining, skills::total_xp_for(AT));
		let all = texts(&emit(&skills)).join("\n");
		let need = skills::total_xp_for(AT + 1) - skills::total_xp_for(AT);
		// Summed over the roster, not written as arithmetic — a new skill
		// must move this total without anyone revisiting the test.
		let total_level: u32 = Skill::ALL.iter().map(|&s| skills.level(s)).sum();
		for want in [
			group(0),
			group(need),
			format!("LV {AT}"),
			format!("LV {}", AT + 1),
			format!("LEVEL {total_level}"),
		] {
			assert!(all.contains(&want), "{want:?} missing from:\n{all}");
		}

		// At the cap the readout is the whole curve, and there is no
		// next level to climb to.
		let cap = skills::total_xp_for(skills::MAX_LEVEL);
		skills.set_total(Skill::Mining, cap);
		let all = texts(&emit(&skills)).join("\n");
		assert!(all.contains(&group(cap)), "the mastery total: {all}");
		assert!(
			all.contains(&format!("LV {}", skills::MAX_LEVEL)),
			"the mastery level: {all}"
		);
		assert!(
			!all.contains(&format!("LV {}", skills::MAX_LEVEL + 1)),
			"there is nothing past mastery to climb to: {all}"
		);
	}

	/// §5.2's depth reward is READ OFF THE CORE, at the depth the player is
	/// actually standing at — the screen never carries its own copy of the
	/// ramp. It sits under Mining because that is the skill it pays into,
	/// and it names a direction rather than "+0%" wherever the ramp has not
	/// yet climbed far enough to show a whole percent.
	#[test]
	fn the_depth_bonus_reads_the_ramp_at_the_players_depth() {
		let skills = SkillSet::default();
		let deepest = (skills::DEPTH_XP_MAX_PERMILLE - 1000) / 10;
		let all = texts(&emit_at(&skills, skills::DEPTH_XP_TO)).join("\n");
		assert!(
			all.contains(&format!("depth bonus: +{deepest}% XP per dig")),
			"{all}"
		);
		// The line belongs to Mining, and there is exactly one of it.
		let lines = texts(&emit_at(&skills, skills::DEPTH_XP_TO));
		let at = lines.iter().position(|l| l.starts_with("depth bonus"));
		assert_eq!(
			lines
				.iter()
				.filter(|l| l.starts_with("depth bonus"))
				.count(),
			1,
			"{lines:?}"
		);
		let mining = lines.iter().position(|l| l.starts_with("MINING"));
		let alchemy = lines.iter().position(|l| l.starts_with("ALCHEMY"));
		assert!(mining < at && at < alchemy, "{lines:?}");

		// The waterline and everything above it: the rule, never a zero.
		for y in [skills::DEPTH_XP_FROM, skills::DEPTH_XP_FROM + 60] {
			let all = texts(&emit_at(&skills, y)).join("\n");
			assert!(all.contains("depth bonus: none yet"), "at y {y}: {all}");
		}
		// And no depth anywhere in the world shows a "+0%".
		for y in (skills::DEPTH_XP_TO..=skills::DEPTH_XP_FROM + 60).step_by(7) {
			let all = texts(&emit_at(&skills, y)).join("\n");
			assert!(!all.contains("+0%"), "at y {y}: {all}");
		}
	}

	/// Every level the CORE opens something at has prose here, DERIVED from `unlock::opens` rather
	/// than restated — the thresholds live in the one module that enforces them, the wording lives
	/// here, and retuning a threshold fails this test until someone writes the sentence for it. The
	/// extra teasers (the pace steps, the waste floor, mastery) are levels that change a NUMBER
	/// rather than open a recipe, so they are ours alone.
	#[test]
	fn every_unlock_level_has_prose() {
		use digblock_core::alchemy;
		for skill in Skill::ALL {
			let table = teasers(skill);
			for level in 1..=skills::MAX_LEVEL {
				if unlock::opens(skill, level) {
					assert!(
						table.iter().any(|&(l, _)| l == level),
						"{skill:?} opens something at {level} with nothing to say about it"
					);
				}
			}
			// Prose in level order, so `next_unlock`'s find() is the NEXT
			// one rather than merely a later one.
			assert!(
				table.windows(2).all(|w| w[0].0 < w[1].0),
				"{skill:?} teasers out of order"
			);
		}
		// Mining's teasers are EXACTLY what it opens — the strength wall's
		// two rungs and then the reveals — so an extra row here would be a
		// promise the game breaks. Read off `opens` rather than written
		// down: the levels live in `unlock`, this table owns the wording.
		assert_eq!(
			mining_teasers().iter().map(|&(l, _)| l).collect::<Vec<_>>(),
			(1..=skills::MAX_LEVEL)
				.filter(|&l| unlock::opens(Skill::Mining, l))
				.collect::<Vec<_>>()
		);
		// The number-only levels, which `opens` deliberately does not
		// report: the bench's two pace steps and the waste dip.
		use unlock::{BENCH_FASTER_AT as FASTER, BENCH_FASTEST_AT as FASTEST, WASTE_DIP_AT as DIP};
		assert!(alchemy::base_pace_ticks(FASTER) < alchemy::base_pace_ticks(FASTER - 1));
		assert!(alchemy::base_pace_ticks(FASTEST) < alchemy::base_pace_ticks(FASTEST - 1));
		assert!(alchemy::waste_permille(DIP) < alchemy::waste_permille(DIP - 1));
		for l in [FASTER, FASTEST, DIP] {
			assert!(
				alch_teasers().iter().any(|&(t, _)| t == l),
				"no teaser at {l}"
			);
		}

		// And the rows actually render: a fresh character teases the first
		// thresholds; past the last Mining gate, Mining shows none.
		let fresh = texts(&emit(&SkillSet::default())).join("\n");
		let timber = format!("next: LV {} - timber gives", unlock::mine(BlockType::Wood));
		let stairs = format!(
			"next: LV {} - the stairs shape",
			unlock::shape(ShapeFamily::Stairs)
		);
		let ore = format!(
			"next: LV {} - emberore reveals",
			reveal_at(BlockType::Emberore)
		);
		assert!(fresh.contains(&timber), "{fresh}");
		assert!(fresh.contains(&stairs), "{fresh}");
		let mut leveled = SkillSet::default();
		leveled.set_total(
			Skill::Mining,
			skills::total_xp_for(unlock::mine(BlockType::Stone)),
		);
		let all = texts(&emit(&leveled)).join("\n");
		assert!(!all.contains(&timber), "{all}");
		assert!(all.contains(&ore), "{all}");
	}

	#[test]
	fn grouping_reads_like_a_number() {
		assert_eq!(group(0), "0");
		assert_eq!(group(999), "999");
		assert_eq!(group(1_000), "1,000");
		assert_eq!(group(1_103_585), "1,103,585");
	}
}
