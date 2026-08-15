//! The character screen (C): one row per skill — level, XP bar to the
//! next level, the running total. Read-only; it never mutates game
//! state and registers no hit regions. The engine owns opening and
//! closing it like every other screen.

use digblock_core::skills::{self, Skill, SkillSet};

use crate::ui::draw::Layer;
use crate::ui::widgets::{self, Style, Ui, DIM, WHITE};
use crate::ui::{hud, theme, InvId, Rect};

/// The screen's per-frame view, derived once from the engine's
/// `SkillSet` so the measure and emit passes read the same numbers.
pub struct CharView {
	rows: Vec<Row>,
}

impl CharView {
	pub fn build(skills: &SkillSet) -> Self {
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
				None => (1.0, format!("{} XP — mastered", group(xp))),
			};
			rows.push(Row::Skill {
				text: format!("{}   LV {}", skill.name(), level),
				frac,
			});
			rows.push(Row::Info(info));
			if let Some((lvl, what)) = next_unlock(skill, level) {
				rows.push(Row::Info(format!("next: LV {lvl} — {what}")));
			}
			rows.push(Row::Space);
		}
		rows.push(Row::Info(format!("TOTAL LEVEL {total_level}")));
		Self { rows }
	}
}

/// The §8.2 teaser tables: what the next level threshold above `level`
/// opens. Screen-local prose over the CORE's numbers — the pin test
/// holds every named level to the function that enforces it, so the
/// strings can't drift from the tables.
const ALCH_TEASERS: &[(u32, &str)] = &[
	(5, "the stairs shape; dirt and sand"),
	(8, "the torch"),
	(10, "the essence siphon — ore needs it"),
	(15, "planed lumber and its joinery; the bucket charge"),
	(18, "furniture: the table and the chair"),
	(20, "clean stone — block, slab, stairs and door together"),
	(25, "dark lumber; morning dew; a faster bench"),
	(30, "sandstone, snow, grass, mud"),
	(35, "the bed — timber and bedding both"),
	(40, "red lumber; the lantern; trace minerals"),
	(45, "polished stone and sandstone brick"),
	(50, "craft water; METAL — the prestige block"),
	(55, "the resonant pick"),
	(60, "gravel's deep metal"),
	(65, "crystal, grown from aether"),
	(75, "pine resin; the fastest bench"),
	(80, "the aetherlamp — the standing aether sink"),
	(85, "the waste floor"),
	(90, "rubble's aether"),
	(99, "mastery; the master's sigil"),
];

/// Mining's teasers: the strength wall's two rungs, then the four ore
/// reveals — the
/// levels the whole skill is shaped around.
const MINING_TEASERS: &[(u32, &str)] = &[
	(5, "timber gives — trees and planed wood"),
	(10, "rock gives — gravel, sandstone, stone"),
	(15, "emberore reveals — bring a siphon"),
	(30, "duskstone reveals"),
	(40, "metal opens — you can break what you smelt"),
	(55, "glowvein reveals; the resonant pick swings wide"),
	(60, "crystal opens"),
	(80, "aetherock reveals"),
	(85, "the pick's full nine"),
	(90, "prospector's sense — you hear what you can't see"),
	(99, "mastery: every vein gives one more"),
];

fn next_unlock(skill: Skill, level: u32) -> Option<(u32, &'static str)> {
	let table = match skill {
		Skill::Mining => MINING_TEASERS,
		Skill::Alchemy => ALCH_TEASERS,
	};
	table.iter().find(|&&(lvl, _)| lvl > level).copied()
}

/// Thousands-grouped decimal ("1,103,585") — XP totals get big enough
/// that ungrouped digits stop being readable at a glance.
fn group(n: u64) -> String {
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
	/// One skill: name + level on the left, the XP bar right-aligned
	/// (the settings rows' slider geometry, reused as a readout).
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

	/// Emit the screen: the shared dim, the shared panel color, the
	/// rows. Same two-pass shape as the alchemy screen (measure, then
	/// emit over the same rows) minus the fit pass — a handful of rows
	/// can't outgrow any window the game runs in, so the frame's one
	/// glyph size is drawn unconditionally.
	pub fn build(&self, ui: &mut Ui<'_, InvId>, size: (u32, u32), view: &CharView, daylight: f32) {
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
						},
					);
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::{Cmd, DrawList};
	use crate::ui::HitMap;

	const SIZE: (u32, u32) = (1280, 800);

	fn emit(skills: &SkillSet) -> DrawList {
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
			&CharView::build(skills),
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

	#[test]
	fn levels_bars_and_totals_derive_from_xp() {
		let mut skills = SkillSet::default();
		// Dead on a level boundary: nothing banked into the next step.
		skills.set_total(Skill::Mining, skills::total_xp_for(10));
		let dl = emit(&skills);
		let all = texts(&dl).join("\n");
		assert!(all.contains("MINING   LV 10"), "{all}");
		assert!(all.contains("ALCHEMY   LV 1"), "{all}");
		assert!(all.contains("0 / 166 XP to LV 11"), "{all}");
		assert!(all.contains("TOTAL LEVEL 11"), "{all}");

		// The cap: full bar, no "to next".
		skills.set_total(Skill::Mining, skills::total_xp_for(99));
		let all = texts(&emit(&skills)).join("\n");
		assert!(all.contains("MINING   LV 99"), "{all}");
		assert!(all.contains("1,103,585 XP — mastered"), "{all}");
	}

	/// Every level the CORE opens something at has prose here, DERIVED
	/// from `unlock::opens` rather than restated — the thresholds live in
	/// the one module that enforces them, the wording lives here, and
	/// retuning a threshold fails this test until someone writes the
	/// sentence for it. The extra teasers (the pace steps, the waste
	/// floor, mastery) are levels that change a NUMBER rather than open a
	/// recipe, so they are ours alone.
	#[test]
	fn every_unlock_level_has_prose() {
		use digblock_core::{alchemy, unlock};
		for skill in Skill::ALL {
			let table = match skill {
				Skill::Mining => MINING_TEASERS,
				Skill::Alchemy => ALCH_TEASERS,
			};
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
		// promise the game breaks.
		assert_eq!(
			MINING_TEASERS.iter().map(|&(l, _)| l).collect::<Vec<_>>(),
			vec![5, 10, 15, 30, 40, 55, 60, 80, 85, 90, 99]
		);
		// The number-only levels, which `opens` deliberately does not
		// report: the bench's two pace steps and the waste floor.
		assert!(alchemy::base_pace_ticks(25) < alchemy::base_pace_ticks(24));
		assert!(alchemy::base_pace_ticks(75) < alchemy::base_pace_ticks(74));
		assert!(alchemy::waste_permille(85) < alchemy::waste_permille(84));
		for l in [25, 75, 85] {
			assert!(
				ALCH_TEASERS.iter().any(|&(t, _)| t == l),
				"no teaser at {l}"
			);
		}

		// And the rows actually render: a fresh character teases the first
		// thresholds; past the last Mining gate, Mining shows none.
		let fresh = texts(&emit(&SkillSet::default())).join("\n");
		assert!(
			fresh.contains("next: LV 5 — timber gives — trees and planed wood"),
			"{fresh}"
		);
		assert!(fresh.contains("next: LV 5 — the stairs shape"), "{fresh}");
		let mut leveled = SkillSet::default();
		leveled.set_total(Skill::Mining, skills::total_xp_for(10));
		let all = texts(&emit(&leveled)).join("\n");
		assert!(!all.contains("next: LV 5 — timber"), "{all}");
		assert!(all.contains("next: LV 15 — emberore reveals"), "{all}");
	}

	#[test]
	fn grouping_reads_like_a_number() {
		assert_eq!(group(0), "0");
		assert_eq!(group(999), "999");
		assert_eq!(group(1_000), "1,000");
		assert_eq!(group(1_103_585), "1,103,585");
	}
}
