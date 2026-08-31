//! Turning menu state into a frame of rows.
//!
//! `Menu::view` is the whole of it: one pass that lays out every screen into
//! the `Row` stream `overlay.rs` draws, and the hit rects `handle_input`
//! reads back. Split from the state machine because the two change for
//! unrelated reasons — a new page is a `Screen` variant and an arm here,
//! and a layout retune touches only this file.
//!
//! Two rules the rest of the UI shares: every token is a logical size
//! multiplied by `scale` at the point of use, and a frame has exactly ONE
//! text size ([`crate::ui::theme`]). Both are swept by
//! `screens::every_screen_draws_text_at_one_size`.

use super::*;

/// Prefix on a ping the probe derived rather than sampled (`Ping::estimated`).
pub(super) const ESTIMATED_MARK: &str = "~";

/// The mark on a server row whose play counts toward the hiscores board. A glyph, so it is
/// measured and drawn like any other column text — the ONE codepoint the UI face carries beyond
/// the typeable repertoire (assets/fonts/SOURCES.md), pinned by `the_ranked_mark_has_a_glyph`.
pub(crate) const RANKED_MARK: &str = "\u{2605}";

impl Menu {
	/// Build the frame's view. `blink` drives the name-field caret.
	pub(crate) fn view(&self, size: (u32, u32), scale: f32, blink: bool) -> MenuView {
		use crate::ui::theme;
		let row_h = theme::ROW_H * scale;
		// The panel never shrinks below this (short screens would jitter
		// as rows change) and grows to fit its widest row's text.
		let min_panel_w = theme::MIN_PANEL_W * scale;
		let glyph = crate::ui::scale::glyph_px(scale);
		let pad_x = crate::ui::scale::text_pad_x(scale);
		// Minimum gap between a row's text and its right-aligned column.
		let col_gap = 2.0 * glyph;
		let text_w = |s: &str| crate::font::ui().measure(s, glyph);
		let (w, h) = (size.0 as f32, size.1 as f32);
		let y0 = (h * 0.18).max(8.0);
		let mut y = y0;
		// Two passes: rows are collected with their y first, then placed
		// once the widest row has set the panel width.
		let mut specs: Vec<(RowSpec, f32)> = Vec::new();
		let push = |specs: &mut Vec<(RowSpec, f32)>, y: &mut f32, spec: RowSpec| {
			specs.push((spec, *y));
			*y += row_h;
		};
		// The keyboard hint under a page. Dropped entirely on a touch device, which has none of
		// those keys and gets the corner button instead: instructions for hardware that is not
		// there read as a dead end rather than as help. One helper rather than a condition per
		// site, because the next page's author will write another of these without thinking
		// about phones.
		let touch = self.touch;
		let hint = |specs: &mut Vec<(RowSpec, f32)>, y: &mut f32, text: &str| {
			if !touch {
				push(specs, y, RowSpec::dim(text));
			}
		};

		match &self.screen {
			None => {}
			Some(Screen::Root { .. }) => {
				push(&mut specs, &mut y, RowSpec::title("DIGBLOCK"));
				y += row_h * 0.5;
				if self.entered_world {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(0), "Back to Game"),
					);
					// Key chips read the live table, so a rebound page key
					// renames its chip the moment the binding lands.
					use crate::bindings::Action as A;
					let chip = |a| self.settings.bindings.label(a);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(3), "Spellbook", chip(A::OpenSpellbook)),
					);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(6), "Inventory", chip(A::OpenInventory)),
					);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(7), "Alchemy", chip(A::OpenAlchemy)),
					);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(9), "Character", chip(A::OpenCharacter)),
					);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(12), "Hiscores", chip(A::OpenHiscores)),
					);
					if self.connected {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(2), "Leave Server"),
						);
					} else {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(8), "Leave World"),
						);
					}
				} else {
					if self.store_available {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(5), "Singleplayer"),
						);
					} else {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(0), "Play Local"),
						);
					}
					if self.services_available {
						// Signed out, this row IS the sign-in: pressing it
						// walks sign-in → name → server list. Saying so on
						// the row keeps the browser window from arriving
						// as a surprise.
						push(
							&mut specs,
							&mut y,
							match &self.account {
								None => {
									RowSpec::item_hint(RowId::RootItem(1), "Multiplayer", "Sign in")
								}
								Some(_) => RowSpec::item(RowId::RootItem(1), "Multiplayer"),
							},
						);
						if let Some(a) = &self.account {
							push(
								&mut specs,
								&mut y,
								RowSpec::item_hint(
									RowId::RootItem(10),
									"Account",
									a.name.as_deref().unwrap_or("No name yet"),
								),
							);
						}
					} else {
						push(
							&mut specs,
							&mut y,
							RowSpec::dim("Multiplayer (no master server; run with --master-url)"),
						);
					}
				}
				if self.fullscreen_available {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(11), "Fullscreen"),
					);
				} else if self.fullscreen_hint {
					// Not a row: there is nothing here to activate. It
					// stands where the Fullscreen row would, because that
					// is where someone looking for fullscreen looks.
					push(
						&mut specs,
						&mut y,
						RowSpec::dim("Fullscreen: Add to Home Screen (Share menu)"),
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::RootItem(4), "Settings"),
				);
				y += row_h * 0.5;
				hint(
					&mut specs,
					&mut y,
					if self.entered_world {
						"Arrows + Enter, Esc: back to game"
					} else {
						"Arrows + Enter"
					},
				);
			}
			Some(Screen::Settings { .. }) => {
				push(&mut specs, &mut y, RowSpec::title("Settings"));
				y += row_h * 0.5;
				let st = &self.settings;
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(0),
						format!("Draw Distance: {}", st.draw_dist),
						// `.max(0.0)`: a saved value under this platform's
						// slider floor (written where the floor was lower)
						// renders an empty bar rather than a negative one;
						// the first adjustment snaps it legal.
						((st.draw_dist - self.draw_min) as f32 / (DRAW_MAX - self.draw_min) as f32)
							.max(0.0),
					),
				);
				// Row indices are POSITIONAL — they must match
				// SETTINGS_ROWS, which is what `acts()` walks. Inserting a
				// row means renumbering every one below it.
				{
					let n = GI_SCALE_DIVISORS.len();
					let i = GI_SCALE_DIVISORS
						.iter()
						.position(|&d| d == st.gi_scale_div)
						.unwrap_or(1);
					push(
						&mut specs,
						&mut y,
						RowSpec::slider(
							RowId::SettingsRow(1),
							format!("Lighting Detail: {}", gi_scale_label(st.gi_scale_div)),
							// Full bar = the list's first divisor =
							// sharpest, so more fill means more of the
							// thing, like every other slider here.
							(n - 1 - i) as f32 / (n - 1) as f32,
						),
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(2),
						format!("Light Sources: {}", st.max_lights),
						(st.max_lights - LIGHT_CAP_MIN) as f32
							/ (LIGHT_CAP_MAX - LIGHT_CAP_MIN) as f32,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(3),
						format!("Interface Scale: {:.0}%", st.ui_scale * 100.0),
						(st.ui_scale - UI_SCALE_MIN) / (UI_SCALE_MAX - UI_SCALE_MIN),
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(4),
						format!("Music Volume: {:.0}%", st.music_vol * 100.0),
						st.music_vol / VOL_MAX,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(5),
						format!("SFX Volume: {:.0}%", st.sfx_vol * 100.0),
						st.sfx_vol / VOL_MAX,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(
						RowId::SettingsRow(6),
						if st.ssr {
							"Water Reflections: On"
						} else {
							"Water Reflections: Off"
						},
					),
				);
				// Skipped in lockstep with `acts()` — see Setting::TouchJump.
				if self.touch {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(
							RowId::SettingsRow(7),
							if st.touch_jump_button {
								"Jump: On-screen Button"
							} else {
								"Jump: Double-tap"
							},
						),
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(
						RowId::SettingsRow(8),
						if st.show_tutorial {
							"Tutorial: On"
						} else {
							"Tutorial: Off"
						},
					),
				);
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::SettingsRow(SETTINGS_BACK), "Back"),
				);
				hint(&mut specs, &mut y, "Left/right or click adjusts, Esc: back");
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec {
						// The build's version, right-aligned on the credits
						// row. `env!` here is digblock-client's version, which
						// inherits [workspace.package] like every other crate.
						right: Some((format!("v{}", env!("CARGO_PKG_VERSION")), C_DIM)),
						..RowSpec::dim("Created by @Zaneris (GitHub/X)")
					},
				);
			}
			Some(Screen::Browser(b)) => {
				push(&mut specs, &mut y, RowSpec::title("Multiplayer"));
				// Static, not a field: the name belongs to the account now,
				// so this says who you are rather than asking.
				if let Some(name) = self.account.as_ref().and_then(|a| a.name.as_deref()) {
					push(
						&mut specs,
						&mut y,
						RowSpec::dim(format!("Playing as {name}")),
					);
				}
				y += row_h * 0.25;
				if b.fetching && b.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("Fetching server list..."));
				} else if b.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("No servers online"));
				}
				// Four columns — name, players, ranked, ping. The count is right-aligned within
				// its column so the figures stay in step as they climb; the mark stands at one
				// x for every row, past the widest count; the ping keeps the right edge.
				let count = |e: &ServerEntry| format!("{}/{}", e.players, e.max_players);
				let widest = |f: &dyn Fn(&ServerEntry) -> String| {
					b.rows
						.iter()
						.map(|r| text_w(&f(&r.entry)))
						.fold(0.0, f32::max)
				};
				let count_end = widest(&|e| e.name.clone()) + col_gap + widest(&count);
				let mark_x = count_end + col_gap;
				for (i, row) in b.rows.iter().enumerate() {
					// The verdict takes the ping's column and the row's own
					// ink: a latency for a server that cannot be joined is
					// answering a question nobody can ask. Same call `acts`
					// makes, so what is drawn and what is offered agree.
					let refused = compat_label(row.entry.compat());
					let ink = if refused.is_none() { C_WHITE } else { C_DIM };
					let (ping, color) = match row.ping {
						PingState::Pending => ("...".to_string(), C_DIM),
						PingState::Failed => ("n/a".to_string(), C_RED),
						// Bands are round-trip times, and they line up with the
						// netbench's link profiles: green is lan/wifi
						// territory, indistinguishable from local; yellow
						// covers cellular, where delivery still holds up and
						// corrections stay small; red is bad-4g, where
						// delivery falls off even with the input hold. They
						// read as too lenient only if you remember them being
						// fed handshake times, `SESSION_ROUND_TRIPS` of an
						// RTT — the probe reports the transport's own
						// estimate, or that time divided down and marked, so
						// these mean what they say. Don't "retune" them for
						// the old input.
						PingState::Ms(Ping { ms, estimated }) => (
							format!("{}{ms} ms", if estimated { ESTIMATED_MARK } else { "" }),
							if ms < 60 {
								C_GREEN
							} else if ms < 150 {
								C_YELLOW
							} else {
								C_RED
							},
						),
					};
					push(
						&mut specs,
						&mut y,
						RowSpec {
							right: Some(match refused {
								Some(why) => (why.to_string(), C_RED),
								None => (ping, color),
							}),
							cells: std::iter::once((
								count_end - text_w(&count(&row.entry)),
								count(&row.entry),
								ink,
							))
							// Play here counts toward the board: the master's
							// trusted list, worn on the row so nobody joins to
							// find out.
							.chain(row.entry.reports_skills.then(|| {
								let green = if refused.is_none() { C_GREEN } else { C_DIM };
								(mark_x, RANKED_MARK.to_string(), green)
							}))
							.collect(),
							..RowSpec::new(
								refused.is_none().then_some(RowId::Server(i)),
								row.entry.name.clone(),
								ink,
							)
						},
					);
				}
				push(&mut specs, &mut y, RowSpec::item(RowId::Refresh, "Refresh"));
				if let Some(status) = &b.status {
					push(&mut specs, &mut y, RowSpec::status(status.clone()));
				}
				y += row_h * 0.25;
				hint(&mut specs, &mut y, "Type your name, Enter joins, Esc: back");
			}
			Some(Screen::Connecting { label, .. }) => {
				push(&mut specs, &mut y, RowSpec::title("Multiplayer"));
				y += row_h * 0.5;
				push(&mut specs, &mut y, RowSpec::dim(label.clone()));
				hint(&mut specs, &mut y, "Esc: cancel");
			}
			Some(Screen::Worlds(w)) => {
				push(&mut specs, &mut y, RowSpec::title("Singleplayer"));
				y += row_h * 0.5;
				if w.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("No worlds yet"));
				}
				for (i, row) in w.rows.iter().enumerate() {
					push(
						&mut specs,
						&mut y,
						RowSpec {
							right: Some((
								row.detail.clone(),
								if row.compatible { C_DIM } else { C_RED },
							)),
							..RowSpec::new(
								Some(RowId::World(i)),
								row.name.clone(),
								if row.compatible { C_WHITE } else { C_DIM },
							)
						},
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::CreateWorld, "Create New World"),
				);
				if let Some(status) = &w.status {
					push(&mut specs, &mut y, RowSpec::status(status.clone()));
				}
				y += row_h * 0.25;
				hint(&mut specs, &mut y, "Enter opens, Esc: back");
			}
			Some(Screen::CreateWorld { name, .. }) => {
				push(&mut specs, &mut y, RowSpec::title("New World"));
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::field(Some(RowId::CreateWorld), format!("Name: {name}"), blink),
				);
				y += row_h * 0.25;
				hint(&mut specs, &mut y, "Type a name, Enter creates, Esc: back");
			}
			Some(Screen::SignIn(s)) => {
				push(&mut specs, &mut y, RowSpec::title("Sign In"));
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::dim("Finish signing in in your browser"),
				);
				push(
					&mut specs,
					&mut y,
					// Fullscreen hides the browser it just opened, and a
					// player who does not know it is there reads the wait
					// as a hang. Worded for every platform: on a phone the
					// sign-in page is another tab, not another window.
					RowSpec::dim("(switch to it if you can't see it)"),
				);
				if let Some(status) = &s.status {
					push(&mut specs, &mut y, RowSpec::status(status.clone()));
				}
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::SignInCancel, "Cancel"),
				);
			}
			Some(Screen::ChooseName(n)) => {
				push(
					&mut specs,
					&mut y,
					RowSpec::title(if n.rename {
						"Change Name"
					} else {
						"Choose a Name"
					}),
				);
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::field(Some(RowId::NameField), format!("Name: {}", n.name), blink),
				);
				// The availability line, and only for the text on screen:
				// a reply that arrives two keystrokes late must not label
				// what is being typed now.
				let live = n
					.checked
					.as_ref()
					.filter(|(name, _, _)| *name == n.name.trim());
				match (&n.status, live) {
					(Some(status), _) => {
						push(&mut specs, &mut y, RowSpec::status(status.clone()));
					}
					(None, Some((_, true, _))) => push(
						&mut specs,
						&mut y,
						RowSpec {
							color: C_GREEN,
							..RowSpec::dim("Available")
						},
					),
					(None, Some((_, false, reason))) => push(
						&mut specs,
						&mut y,
						RowSpec::status(reason.clone().unwrap_or_else(|| "Taken".into())),
					),
					(None, None) => push(&mut specs, &mut y, RowSpec::dim(" ")),
				}
				y += row_h * 0.25;
				push(
					&mut specs,
					&mut y,
					RowSpec::dim(if n.claiming {
						"Claiming...".to_string()
					} else {
						// The rule, from the function that enforces it, and then the
						// key — separately, because only the second half is a keyboard
						// hint and this page is reachable on a device with no keys to
						// name. A `dim` rather than `hint` for exactly that reason.
						use digblock_net::protocol::{MAX_DISPLAY_NAME, MIN_DISPLAY_NAME};
						let rule =
							format!("{MIN_DISPLAY_NAME}-{MAX_DISPLAY_NAME} chars: a-z A-Z 0-9 - _");
						if touch {
							rule
						} else {
							format!("{rule}, enter confirms")
						}
					}),
				);
			}
			Some(Screen::Account { .. }) => {
				push(&mut specs, &mut y, RowSpec::title("Account"));
				y += row_h * 0.5;
				let (name, id) = match &self.account {
					Some(a) => (
						a.name.clone().unwrap_or_else(|| "(no name yet)".into()),
						a.id.clone(),
					),
					None => ("(signed out)".into(), String::new()),
				};
				push(
					&mut specs,
					&mut y,
					RowSpec::dim(format!("Signed in as {name}")),
				);
				// The id is the thing an operator needs in order to put you
				// in DIGBLOCK_ADMIN_IDS, so it is on screen in full rather
				// than abbreviated — Copy ID is the convenience, not the
				// only way to get it.
				push(&mut specs, &mut y, RowSpec::dim(id));
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::AccountRow(0), "Copy ID"),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::AccountRow(1), "Change Name"),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::AccountRow(2), "Sign Out"),
				);
				y += row_h * 0.25;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::AccountRow(ACCOUNT_BACK), "Back"),
				);
			}
			Some(Screen::WorldActions { prev, idx, .. }) => {
				let (name, compatible) = prev
					.rows
					.get(*idx)
					.map_or(("?".to_string(), true), |r| (r.name.clone(), r.compatible));
				push(&mut specs, &mut y, RowSpec::title(&name));
				y += row_h * 0.5;
				if compatible {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::WorldAction(0), "Enter World"),
					);
				} else {
					push(
						&mut specs,
						&mut y,
						RowSpec::dim("From an old version - can't be entered"),
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(1), "Delete World"),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(2), "Back"),
				);
			}
			Some(Screen::ConfirmDelete { prev, idx, .. }) => {
				let name = prev.rows.get(*idx).map_or("?", |r| r.name.as_str());
				push(&mut specs, &mut y, RowSpec::title("Delete World"));
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(0), &format!("Delete \"{name}\" forever")),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(1), "Cancel"),
				);
			}
		}

		// Width to content: the widest row (text plus any right-aligned column or slider bar)
		// fits with the overlay's own padding, clamped to the window; min_panel_w keeps short
		// screens from looking cramped. Must match what widgets::row actually draws — see
		// `panel_fits_every_row`, which re-derives this independently.
		let content_w = specs
			.iter()
			.map(|(spec, _)| {
				// The body is the text — with its caret reserved whether or not it is lit this
				// frame, so the panel doesn't breathe with the blink — or its cells, whichever
				// reaches further.
				let body = text_w(&spec.text)
					+ if spec.caret {
						crate::ui::widgets::caret_w(glyph)
					} else {
						0.0
					};
				let cells = spec
					.cells
					.iter()
					.map(|(x, text, _)| x + text_w(text))
					.fold(0.0, f32::max);
				pad_x * 2.0
					+ body.max(cells)
					+ spec
						.right
						.as_ref()
						.map_or(0.0, |(right, _)| col_gap + text_w(right))
					+ spec
						.bar
						.map_or(0.0, |_| col_gap + crate::ui::widgets::SLIDER_BAR_W * scale)
			})
			.fold(min_panel_w, f32::max);
		let pw = content_w.min(w - 16.0 * scale);
		let px = ((w - pw) / 2.0).max(8.0 * scale);
		let selected_id = self.acts().get(self.current_sel()).map(|a| a.0);
		let rows = specs
			.into_iter()
			.map(|(spec, ry)| PlacedRow {
				rect: [px, ry, pw, row_h],
				highlight: spec.id.is_some() && spec.id == selected_id,
				id: spec.id,
				text: spec.text,
				right: spec.right,
				bar: spec.bar,
				color: spec.color,
				caret: spec.caret,
				cells: spec.cells,
			})
			.collect();

		let pad = theme::PAD * scale;
		let rows: Vec<PlacedRow> = rows;
		let mut hits = crate::ui::HitMap::default();
		for row in &rows {
			if let Some(id) = row.id {
				hits.push(row.rect.into(), id);
			}
		}
		MenuView {
			panel: [px - pad, y0 - pad, pw + pad * 2.0, (y - y0) + pad * 2.0],
			rows,
			hits,
		}
	}
}
