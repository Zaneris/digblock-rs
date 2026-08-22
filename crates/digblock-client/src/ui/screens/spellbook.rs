//! The spellbook screen (J): Arcana's casting surface. One page — the
//! spell roster with each entry's readiness, and the ladder's next rung
//! as a teaser. Selecting a ready spell RETURNS an action; the engine
//! closes the screen and runs the channel, so the cast is watched, not
//! menued. The screen never mutates game state.

use digblock_core::inventory::Inventory;
use digblock_core::skills::{Skill, SkillSet};
use digblock_core::unlock;
use digblock_core::worldgen::SurfaceKind;
use digblock_platform::input::{InputEvent, MouseButton};

use crate::ui::draw::Layer;
use crate::ui::widgets::{self, DIM, Style, Ui, WHITE};
use crate::ui::{HitMap, InvId, Rect, hud, theme};

/// What a click on the page asks the engine to do.
pub enum SpellAction {
	/// Begin the Return Home channel.
	CastHome,
	/// Begin the channel for the port at this `SurfaceKind` index.
	CastWaystone(u8),
	/// Begin the self-light's channel.
	CastWisp,
	/// Begin conjured light's channel (aimed at completion).
	CastKindle,
	/// Flip the Mason's Arc stance.
	ToggleArc,
}

/// The two non-port spell slots in the `InvId::Spell` space — ports use
/// `1 + kind index`, which tops out well below these.
const SLOT_WISP: u8 = 100;
const SLOT_KINDLE: u8 = 101;
const SLOT_ARC: u8 = 102;

/// One port as the page lists it — everything derived from the core
/// tables by [`all_ports`], so the roster and the rules cannot drift.
#[derive(Clone, Copy)]
pub struct Port {
	/// `SurfaceKind` declaration-order index.
	pub idx: u8,
	pub level: u32,
	pub essence: digblock_core::block::Essence,
	pub fare: u32,
}

/// Every laddered port, ascending by level: the whole roster offline,
/// and the universe the online caller filters to its world's RESOLVED
/// sites. Reads the three twinned tables and nothing else.
pub fn all_ports() -> Vec<Port> {
	let mut out: Vec<Port> = SurfaceKind::ALL
		.iter()
		.enumerate()
		.filter_map(|(i, &k)| {
			let level = unlock::waystone(k)?;
			let (essence, fare) = digblock_core::alchemy::cast_price(k)?;
			Some(Port {
				idx: i as u8,
				level,
				essence,
				fare,
			})
		})
		.collect();
	out.sort_by_key(|p| p.level);
	out
}

/// The page's own name for a port — a label, not a `voice` sentence;
/// screens own their labels.
fn port_label(idx: u8) -> &'static str {
	match SurfaceKind::ALL.get(idx as usize) {
		Some(SurfaceKind::Plains) => "PLAINS WAYSTONE",
		Some(SurfaceKind::Forest) => "FOREST WAYSTONE",
		Some(SurfaceKind::Savanna) => "SAVANNA WAYSTONE",
		Some(SurfaceKind::Taiga) => "TAIGA WAYSTONE",
		Some(SurfaceKind::Beach) => "SHORE WAYSTONE",
		Some(SurfaceKind::Mountain) => "MOUNTAIN WAYSTONE",
		Some(SurfaceKind::Snow) => "FAR COLD WAYSTONE",
		_ => "WAYSTONE",
	}
}

/// The page's per-frame view, derived once so measure and emit read the
/// same rows — and so `handle_input` answers from the same readiness the
/// pixels showed.
pub struct SpellView {
	rows: Vec<Row>,
	/// The shared teleport cooldown has run out — the readiness of Return
	/// Home AND of every port, since one clock covers them all.
	teleport_ready: bool,
	wisp_ready: bool,
	kindle_ready: bool,
	arc_shown: bool,
	/// The ports a click may cast right now, by `InvId::Spell` slot.
	castable: Vec<(u8, u8)>,
}

impl SpellView {
	/// `cooldown_secs` is the engine's live TELEPORT countdown — one
	/// clock over Return Home and every port — ceiling-rounded by the
	/// caller (a "0s" row that is not yet ready would be a lie).
	/// `ports` is what THIS world offers (the engine filters online to
	/// the sites its server resolved); a port greys on the same two
	/// things the cast is refused for, its fare and that clock, so the
	/// offer and the rule cannot drift.
	pub fn build(
		skills: &SkillSet,
		cooldown_secs: u32,
		ports: &[Port],
		inventory: &Inventory,
		arc_on: bool,
	) -> Self {
		let level = skills.level(Skill::Arcana);
		let teleport_ready = cooldown_secs == 0;
		let mut castable = Vec::new();
		let mut rows = vec![Row::Title, Row::Space];
		rows.push(Row::Spell {
			id: InvId::Spell(0),
			text: "RETURN HOME".to_string(),
			state: if teleport_ready {
				"READY".to_string()
			} else {
				format!("{cooldown_secs}s")
			},
			ready: teleport_ready,
		});
		rows.push(Row::Info(
			"a channelled journey back to your spawn — hold still".to_string(),
		));
		// The illumination family: the wisp from the first level (the
		// ladder's essence-priced bootstrap), kindle at its rung.
		let (wisp_essence, wisp_fare) = digblock_core::alchemy::wisp_price();
		let wisp_ready = inventory.sub_count(wisp_essence) >= wisp_fare;
		rows.push(Row::Spell {
			id: InvId::Spell(SLOT_WISP),
			text: "WISP".to_string(),
			state: format!("{wisp_fare} {}", wisp_essence.name().to_uppercase()),
			ready: wisp_ready,
		});
		let mut kindle_ready = false;
		if level >= unlock::KINDLE_AT {
			let (k_essence, k_fare) = digblock_core::alchemy::kindle_price(level);
			kindle_ready = inventory.sub_count(k_essence) >= k_fare;
			rows.push(Row::Spell {
				id: InvId::Spell(SLOT_KINDLE),
				text: "KINDLE".to_string(),
				state: format!("{k_fare} {}", k_essence.name().to_uppercase()),
				ready: kindle_ready,
			});
		}
		// The arc is a STANCE, not a channel: the row flips it, and the
		// price is the blocks each line lays.
		let arc_shown = unlock::arc_cells(level) > 1;
		if arc_shown {
			rows.push(Row::Spell {
				id: InvId::Spell(SLOT_ARC),
				text: "MASON'S ARC".to_string(),
				state: if arc_on {
					"ON".to_string()
				} else {
					"OFF".to_string()
				},
				ready: true,
			});
		}
		rows.push(Row::Space);
		// The ports: every unlocked one, plus ONE teaser tier — the
		// alchemy screen's "what you have and one tier of what is
		// coming".
		let mut teased = false;
		for port in ports {
			if port.level <= level {
				let affordable = inventory.sub_count(port.essence) >= port.fare;
				let castable_now = affordable && teleport_ready;
				let slot = 1 + port.idx;
				if castable_now {
					castable.push((slot, port.idx));
				}
				rows.push(Row::Spell {
					id: InvId::Spell(slot),
					text: port_label(port.idx).to_string(),
					// The clock is what the row SAYS while it runs: an
					// affordable port that still cannot be cast has to
					// name the reason, and the fare is not it.
					state: if teleport_ready {
						format!("{} {}", port.fare, port.essence.name().to_uppercase())
					} else {
						format!("{cooldown_secs}s")
					},
					ready: castable_now,
				});
			} else if !teased {
				teased = true;
				rows.push(Row::Info(format!(
					"next port: LV {} — {}",
					port.level,
					port_label(port.idx).to_lowercase()
				)));
			}
		}
		rows.push(Row::Space);
		// The rung teaser rides beneath the roster.
		if let Some((lvl, what)) = next_rung(level) {
			rows.push(Row::Info(format!("next: LV {lvl} — {what}")));
		}
		Self {
			rows,
			teleport_ready,
			wisp_ready,
			kindle_ready,
			arc_shown,
			castable,
		}
	}
}

/// The Arcana rungs this page teases, in ladder order. The spellbook
/// speaks of what CASTING gains; the character screen's teasers stay the
/// fanfare surface, and the two share only the unlock tables.
fn next_rung(level: u32) -> Option<(u32, &'static str)> {
	[
		(unlock::TELEPORT_SWIFT_AT, "home comes back sooner"),
		(unlock::CHANNEL_QUICK_AT, "every channel quickens"),
		(unlock::TELEPORT_SWIFTER_AT, "home comes back sooner still"),
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
				InvId::Spell(0) if view.teleport_ready => Some(SpellAction::CastHome),
				InvId::Spell(SLOT_WISP) if view.wisp_ready => Some(SpellAction::CastWisp),
				InvId::Spell(SLOT_KINDLE) if view.kindle_ready => Some(SpellAction::CastKindle),
				InvId::Spell(SLOT_ARC) if view.arc_shown => Some(SpellAction::ToggleArc),
				InvId::Spell(slot) => view
					.castable
					.iter()
					.find(|&&(s, _)| s == slot)
					.map(|&(_, idx)| SpellAction::CastWaystone(idx)),
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
							right: Some(widgets::RowRight::Text(state, color)),
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
