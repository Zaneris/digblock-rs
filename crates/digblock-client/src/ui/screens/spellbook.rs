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

use crate::hotbar::HotbarView;
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
	/// Begin conjured light's channel (it stands where the head is at completion).
	CastKindle,
	/// Begin the Ward's channel (the sphere closes about the head at completion).
	CastWard,
	/// Begin the Causeway's channel, or — with a road standing — take it up: one row for both.
	ToggleCauseway,
	/// Flip the Mason's Arc stance.
	ToggleArc,
}

/// The non-port spell slots in the `InvId::Spell` space — ports use
/// `1 + kind index`, which tops out well below these.
const SLOT_WISP: u8 = 100;
const SLOT_KINDLE: u8 = 101;
const SLOT_ARC: u8 = 102;
const SLOT_WARD: u8 = 103;
const SLOT_CAUSEWAY: u8 = 104;

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
	ward_ready: bool,
	arc_shown: bool,
	causeway_ready: bool,
	/// The ports a click may cast right now, by `InvId::Spell` slot.
	castable: Vec<(u8, u8)>,
}

impl SpellView {
	/// The rows priced in essence, by slot — what a capture or a sweep
	/// parks its cursor on ([`crate::ui::InvId::SpellFare`]) to
	/// photograph a fare's name, paired with the essence that name is.
	/// Derived from the rows the page actually built, so a roster that
	/// grows a priced spell grows this with it.
	pub fn fares(&self) -> Vec<(u8, digblock_core::block::Essence)> {
		self.rows
			.iter()
			.filter_map(|row| match row {
				Row::Spell {
					id: InvId::Spell(slot),
					state: State::Fare { essence, .. },
					..
				} => Some((*slot, *essence)),
				_ => None,
			})
			.collect()
	}

	/// `cooldown_secs` is the engine's live TELEPORT countdown — one clock over Return Home and
	/// every port — ceiling-rounded by the caller (a "0s" row that is not yet ready would be a
	/// lie). `ports` is what THIS world offers; a port greys on the same two things the cast is
	/// refused for, its fare and that clock, so the offer and the rule cannot drift.
	pub fn build(
		skills: &SkillSet,
		cooldown_secs: u32,
		ports: &[Port],
		inventory: &Inventory,
		arc_on: bool,
		causeway_secs_left: u32,
	) -> Self {
		let level = skills.level(Skill::Arcana);
		let teleport_ready = cooldown_secs == 0;
		let mut castable = Vec::new();
		let mut rows = vec![Row::Title, Row::Space];
		rows.push(Row::Spell {
			id: InvId::Spell(0),
			text: "RETURN HOME".to_string(),
			state: State::Text(if teleport_ready {
				"READY".to_string()
			} else {
				format!("{cooldown_secs}s")
			}),
			ready: teleport_ready,
		});
		rows.push(Row::Info(
			"a channelled journey back to your spawn — hold still".to_string(),
		));
		// The illumination family: the wisp from the first level (the
		// ladder's essence-priced bootstrap), kindle at its rung.
		let (wisp_essence, wisp_fare) = digblock_core::alchemy::wisp_price(level);
		let wisp_ready = inventory.sub_count(wisp_essence) >= wisp_fare;
		rows.push(Row::Spell {
			id: InvId::Spell(SLOT_WISP),
			text: "WISP".to_string(),
			state: State::fare(wisp_essence, wisp_fare, wisp_ready),
			ready: wisp_ready,
		});
		let mut kindle_ready = false;
		if level >= unlock::KINDLE_AT {
			let (k_essence, k_fare) = digblock_core::alchemy::kindle_price(level);
			kindle_ready = inventory.sub_count(k_essence) >= k_fare;
			rows.push(Row::Spell {
				id: InvId::Spell(SLOT_KINDLE),
				text: "KINDLE".to_string(),
				state: State::fare(k_essence, k_fare, kindle_ready),
				ready: kindle_ready,
			});
		}
		let mut ward_ready = false;
		if unlock::ward_radius(level) > 0 {
			let (w_essence, w_fare) = digblock_core::alchemy::ward_price(level);
			ward_ready = inventory.sub_count(w_essence) >= w_fare;
			rows.push(Row::Spell {
				id: InvId::Spell(SLOT_WARD),
				text: "WARD".to_string(),
				state: State::fare(w_essence, w_fare, ward_ready),
				ready: ward_ready,
			});
		}
		// The second glass spell. A road already standing shows what is
		// left of it and clicks to take it up; otherwise the row is an
		// ordinary fare.
		let mut causeway_ready = false;
		if unlock::causeway_secs(level) > 0 {
			let (c_essence, c_fare) = digblock_core::alchemy::causeway_price(level);
			let standing = causeway_secs_left > 0;
			causeway_ready = standing || inventory.sub_count(c_essence) >= c_fare;
			rows.push(Row::Spell {
				id: InvId::Spell(SLOT_CAUSEWAY),
				text: "CAUSEWAY".to_string(),
				state: if standing {
					State::Text(format!("{causeway_secs_left}s"))
				} else {
					State::fare(c_essence, c_fare, causeway_ready)
				},
				ready: causeway_ready,
			});
		}
		// The arc is a STANCE, not a channel: the row flips it, and the
		// price is the blocks each line lays.
		let arc_shown = unlock::arc_cells(level) > 1;
		if arc_shown {
			rows.push(Row::Spell {
				id: InvId::Spell(SLOT_ARC),
				text: "MASON'S ARC".to_string(),
				state: State::Text(if arc_on {
					"ON".to_string()
				} else {
					"OFF".to_string()
				}),
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
						State::fare(port.essence, port.fare, affordable)
					} else {
						State::Text(format!("{cooldown_secs}s"))
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
			ward_ready,
			arc_shown,
			causeway_ready,
			castable,
		}
	}
}

/// The Arcana rungs this page teases, in ladder order. The spellbook
/// speaks of what CASTING gains; the character screen's teasers stay the
/// fanfare surface, and the two share only the unlock tables — pinned by
/// `screens::the_two_arcana_teaser_surfaces_share_no_prose`, since a rung
/// worded the same on both makes one reword a two-file edit.
pub(super) fn next_rung(level: u32) -> Option<(u32, &'static str)> {
	[
		(unlock::WARD_AT, "a ward of glass closes about you"),
		(unlock::TELEPORT_SWIFT_AT, "home comes back sooner"),
		(unlock::CAUSEWAY_AT, "glass takes your step over nothing"),
		(unlock::CHANNEL_QUICK_AT, "every channel quickens"),
		(unlock::WARD_WIDE_AT, "the ward widens"),
		(unlock::CAUSEWAY_LONG_AT, "the road runs longer"),
		(unlock::WARD_VAST_AT, "the ward widens again"),
		(unlock::TELEPORT_SWIFTER_AT, "home comes back sooner still"),
		(unlock::CAUSEWAY_VAST_AT, "the road runs longer still"),
	]
	.into_iter()
	.find(|&(lvl, _)| lvl > level)
}

enum Row {
	Title,
	Space,
	/// One castable spell: name left, readiness right, clickable when ready.
	Spell {
		id: InvId,
		text: String,
		state: State,
		ready: bool,
	},
	/// Dim detail line (a spell's description, the teaser).
	Info(String),
}

/// What a spell row's right column says: a word (ready, a countdown,
/// on/off) or a fare — an amount of essence, which is a chip and never
/// a word.
enum State {
	Text(String),
	Fare {
		essence: digblock_core::block::Essence,
		n: u32,
		affordable: bool,
	},
}

impl State {
	fn fare(essence: digblock_core::block::Essence, n: u32, affordable: bool) -> Self {
		State::Fare {
			essence,
			n,
			affordable,
		}
	}

	/// The fare as the chip the row draws — `None` for a word.
	fn chip(&self, color: u32) -> Option<widgets::Chip> {
		match self {
			State::Text(_) => None,
			State::Fare {
				essence,
				n,
				affordable,
			} => Some(widgets::Chip {
				kind: hud::IconKind::Essence(*essence),
				tint: 0xFFFF_FFFF,
				n: *n,
				ink: if *affordable { color } else { theme::SHORT_INK },
			}),
		}
	}
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
				let right = match state {
					State::Text(t) => style.measure(t),
					fare => fare
						.chip(WHITE)
						.map_or(0.0, |c| widgets::chips_w(style, &[c])),
				};
				theme::text_row_w(style, row_text(row)) + right + theme::SECTION_GAP * s
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
			} => {
				// A fare chip belongs to its row: the separate id exists
				// so the page can NAME the essence from the geometry that
				// drew it, not so the price can be clicked at on its own.
				let slot = match hits.hit(self.cursor.0, self.cursor.1)? {
					InvId::Spell(slot) | InvId::SpellFare(slot) => slot,
					_ => return None,
				};
				match slot {
					0 if view.teleport_ready => Some(SpellAction::CastHome),
					SLOT_WISP if view.wisp_ready => Some(SpellAction::CastWisp),
					SLOT_KINDLE if view.kindle_ready => Some(SpellAction::CastKindle),
					SLOT_WARD if view.ward_ready => Some(SpellAction::CastWard),
					SLOT_CAUSEWAY if view.causeway_ready => Some(SpellAction::ToggleCauseway),
					SLOT_ARC if view.arc_shown => Some(SpellAction::ToggleArc),
					slot => view
						.castable
						.iter()
						.find(|&&(s, _)| s == slot)
						.map(|&(_, idx)| SpellAction::CastWaystone(idx)),
				}
			}
			_ => None,
		}
	}

	/// Emit the page: the shared dim, the shared panel, the rows. The
	/// character screen's two-pass shape — a handful of rows can't
	/// outgrow any window the game runs in, so no fit ladder.
	pub fn build(
		&self,
		ui: &mut Ui<'_, InvId>,
		size: (u32, u32),
		view: &SpellView,
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
		let mut hover: Option<(Rect, String)> = None;
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
							mark: None,
						},
					);
				}
				Row::Spell {
					id, state, ready, ..
				} => {
					let hovered = *ready && rect.contains(self.cursor.0, self.cursor.1);
					let color = if *ready { WHITE } else { DIM };
					let mut fare_rect = None;
					let right = match state {
						State::Text(t) => widgets::RowRight::Text(t, color),
						fare => {
							let chip = fare.chip(color).expect("a fare");
							// The chip's own rect, as `widgets::row` places
							// it, so hovering the fare names its essence.
							let run = [chip];
							let w = widgets::chips_w(&ui.style, &run);
							let pad_x = crate::ui::scale::text_pad_x(s);
							let chip_rect = Rect::new(
								rect.x + rect.w - pad_x - w,
								rect.y + (rect.h - theme::ROW_H * s) / 2.0,
								w,
								theme::ROW_H * s,
							);
							fare_rect = Some(chip_rect);
							if let (hud::IconKind::Essence(e), true) =
								(chip.kind, chip_rect.contains(self.cursor.0, self.cursor.1))
							{
								hover = Some((chip_rect, e.name().to_string()));
							}
							widgets::RowRight::Chip(chip)
						}
					};
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text: row_text(row),
							right: Some(right),
							bar: None,
							color,
							highlight: hovered,
							caret: false,
							mark: None,
						},
					);
					if *ready {
						ui.hits.push(rect, *id);
					}
					// After the row, so the chip wins the hit test — and
					// whether or not the row is castable, because the name
					// it shows is worth having either way.
					if let (Some(chip), InvId::Spell(slot)) = (fare_rect, *id) {
						ui.hits.push(chip, InvId::SpellFare(slot));
					}
				}
			}
		}
		// The bar is drawn after us and this page has no fit ladder, so
		// `s` IS the window's scale — but the naming still belongs here,
		// so every page over the bar answers a hover the same way.
		if hover.is_none() {
			hover = hud::hovered_bar_slot(size, s, bar, self.cursor);
		}
		if let Some((anchor, name)) = hover {
			widgets::tooltip(ui.dl, &ui.style, size, anchor, &name);
		}
	}
}

#[cfg(test)]
mod tests;
