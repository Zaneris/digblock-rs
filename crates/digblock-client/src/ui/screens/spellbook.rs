//! The spellbook screen (J): Arcana's casting surface. One page — the
//! spell roster with each entry's readiness, and the ladder's next rung
//! as a teaser. Selecting a ready spell RETURNS an action; the engine
//! closes the screen and runs the channel, so the cast is watched, not
//! menued. The screen never mutates game state.

use digblock_core::skills::{Skill, SkillSet};
use digblock_core::unlock;
use digblock_platform::input::{InputEvent, MouseButton};

use crate::ui::draw::Layer;
use crate::ui::widgets::{self, DIM, Style, Ui, WHITE};
use crate::ui::{HitMap, InvId, Rect, hud, theme};

/// What a click on the page asks the engine to do.
pub enum SpellAction {
	/// Begin the Return Home channel.
	CastHome,
}

/// The page's per-frame view, derived once so measure and emit read the
/// same rows — and so `handle_input` answers from the same readiness the
/// pixels showed.
pub struct SpellView {
	rows: Vec<Row>,
	home_ready: bool,
}

impl SpellView {
	/// `cooldown_secs` is the engine's live home-spell countdown,
	/// ceiling-rounded by the caller (a "0s" row that is not yet ready
	/// would be a lie).
	pub fn build(skills: &SkillSet, cooldown_secs: u32) -> Self {
		let level = skills.level(Skill::Arcana);
		let home_ready = cooldown_secs == 0;
		let mut rows = vec![Row::Title, Row::Space];
		rows.push(Row::Spell {
			id: InvId::Spell(0),
			text: "RETURN HOME".to_string(),
			state: if home_ready {
				"READY".to_string()
			} else {
				format!("{cooldown_secs}s")
			},
			ready: home_ready,
		});
		rows.push(Row::Info(
			"a channelled journey back to your spawn — hold still".to_string(),
		));
		rows.push(Row::Space);
		// One teaser tier, the alchemy screen's rule: the next rung and
		// nothing past it. Prose is this screen's own; the level is read
		// from the table that enforces it.
		if let Some((lvl, what)) = next_rung(level) {
			rows.push(Row::Info(format!("next: LV {lvl} — {what}")));
		}
		Self { rows, home_ready }
	}
}

/// The Arcana rungs this page teases, in ladder order. The spellbook
/// speaks of what CASTING gains; the character screen's teasers stay the
/// fanfare surface, and the two share only the unlock tables.
fn next_rung(level: u32) -> Option<(u32, &'static str)> {
	[
		(unlock::RETURN_SWIFT_AT, "home comes back sooner"),
		(unlock::CHANNEL_QUICK_AT, "every channel quickens"),
		(unlock::RETURN_SWIFTER_AT, "home comes back sooner still"),
	]
	.into_iter()
	.find(|&(lvl, _)| lvl > level)
}

enum Row {
	Title,
	Space,
	/// One castable spell: name left, readiness right, clickable when
	/// ready.
	Spell {
		id: InvId,
		text: String,
		state: String,
		ready: bool,
	},
	/// Dim detail line (a spell's description, the teaser).
	Info(String),
}

fn row_text(row: &Row) -> &str {
	match row {
		Row::Space => "",
		Row::Title => "SPELLBOOK",
		Row::Spell { text, .. } => text,
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
			// A spell row carries its right-aligned state text.
			Row::Spell { state, .. } => {
				theme::text_row_w(style, row_text(row))
					+ style.measure(state)
					+ theme::SECTION_GAP * s
			}
			_ => theme::text_row_w(style, row_text(row)),
		};
		width = width.max(w);
		height += row_h(row, s);
	}
	(width, height)
}

#[derive(Default)]
pub struct SpellScreen {
	open: bool,
	cursor: (f32, f32),
}

impl SpellScreen {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		self.open = true;
	}

	pub fn close(&mut self) {
		self.open = false;
	}

	/// Clicks resolve against the hit map the last `build` registered —
	/// the same frame-behind convention every screen shares. Closing
	/// keys are the ENGINE's (they must also be consumed there); this
	/// only answers what a pointer asked of the page.
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
		view: &SpellView,
	) -> Option<SpellAction> {
		match *event {
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => match hits.hit(self.cursor.0, self.cursor.1)? {
				InvId::Spell(0) if view.home_ready => Some(SpellAction::CastHome),
				_ => None,
			},
			_ => None,
		}
	}

	/// Emit the page: the shared dim, the shared panel, the rows. The
	/// character screen's two-pass shape — a handful of rows can't
	/// outgrow any window the game runs in, so no fit ladder.
	pub fn build(&self, ui: &mut Ui<'_, InvId>, size: (u32, u32), view: &SpellView, daylight: f32) {
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
				Row::Spell {
					id, state, ready, ..
				} => {
					let hovered = *ready && rect.contains(self.cursor.0, self.cursor.1);
					let color = if *ready { WHITE } else { DIM };
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text: row_text(row),
							right: Some((state, color)),
							bar: None,
							color,
							highlight: hovered,
							caret: false,
						},
					);
					if *ready {
						ui.hits.push(rect, *id);
					}
				}
			}
		}
	}
}
