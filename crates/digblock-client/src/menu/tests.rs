use super::*;
use crate::settings::DRAW_FLOOR;
use std::assert_matches;

const SIZE: (u32, u32) = (1280, 800);

/// A signed-in account with this display name. Most tests want one:
/// reaching the server browser at all now means being signed in and
/// named, because that is the route Multiplayer walks.
fn named(name: &str) -> Option<AccountView> {
	Some(AccountView {
		id: "b2379408-3102-8b68-b56b-5774dec269b8".into(),
		name: Some(name.into()),
	})
}

/// Signed in, but no name picked yet — the state a first sign-in
/// leaves behind.
fn unnamed() -> Option<AccountView> {
	Some(AccountView {
		id: "b2379408-3102-8b68-b56b-5774dec269b8".into(),
		name: None,
	})
}

fn entry(id: &str, name: &str) -> ServerEntry {
	ServerEntry {
		id: id.into(),
		name: name.into(),
		url: format!("https://{id}:4433"),
		cert_hash: "aa".repeat(32),
		players: 1,
		max_players: 255,
		protocol_version: digblock_net::protocol::PROTOCOL_VERSION,
		sim_version: digblock_core::SIM_VERSION,
		reports_skills: false,
	}
}

/// The same server, one behaviour version ahead of this build — the shape
/// of a client that has not caught up with the server it is looking at.
fn ahead(id: &str, name: &str) -> ServerEntry {
	ServerEntry {
		sim_version: digblock_core::SIM_VERSION + 1,
		..entry(id, name)
	}
}

fn open_browser(menu: &mut Menu) {
	menu.handle_key(Key::Down); // Multiplayer
	menu.handle_key(Key::Enter);
}

/// The in-game root offers Inventory (hint "I") and the Spellbook
/// (hint "J"); picking Inventory raises the intent and closes.
#[test]
fn inventory_row_raises_intent_and_hotkey_hints_show() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	assert!(
		!menu
			.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text == "Inventory"),
		"no inventory row before entering a world"
	);
	menu.handle_key(Key::Enter); // Play Local
	menu.open();
	let view = menu.view(SIZE, 1.0, false);
	let hint = |name: &str| {
		view.rows
			.iter()
			.find(|r| r.text == name)
			.and_then(|r| r.right.as_ref())
			.map(|(s, _)| s.as_str())
	};
	assert_eq!(hint("Inventory"), Some("I"));
	assert_eq!(hint("Alchemy"), Some("K"));
	assert_eq!(hint("Spellbook"), Some("J"));

	menu.handle_key(Key::Down); // Spellbook
	menu.handle_key(Key::Down); // Inventory
	menu.handle_key(Key::Enter);
	assert!(menu.take_inventory());
	assert!(!menu.take_inventory(), "intent is one-shot");
	assert!(!menu.is_open(), "menu closed for the screen");
}

/// The Alchemy row mirrors Inventory: raises its intent and closes.
#[test]
fn alchemy_row_raises_intent() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.handle_key(Key::Enter); // Play Local
	menu.open();
	menu.handle_key(Key::Down); // Spellbook
	menu.handle_key(Key::Down); // Inventory
	menu.handle_key(Key::Down); // Alchemy
	menu.handle_key(Key::Enter);
	assert!(menu.take_alchemy());
	assert!(!menu.take_alchemy(), "intent is one-shot");
	assert!(!menu.is_open(), "menu closed for the screen");
}

/// Connected to a server, the Multiplayer row disappears entirely —
/// joining another server goes through Leave Server first.
#[test]
fn multiplayer_hidden_while_connected() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	menu.set_connected(true);
	assert!(
		!menu
			.acts()
			.iter()
			.any(|(_, act)| matches!(act, Act::OpenMultiplayer))
	);
	assert!(
		!menu
			.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text.starts_with("Multiplayer"))
	);
	// Leaving brings it back (fresh backdrop world, startup menu).
	menu.on_returned_to_title();
	assert!(
		menu.acts()
			.iter()
			.any(|(_, act)| matches!(act, Act::OpenMultiplayer))
	);
}

/// The view emits through the framework: dim + panel + one text
/// command per row (all Content layer), and the hit map built by the
/// same pass resolves a row's center to its id.
#[test]
fn view_emits_draw_commands_and_hit_map_agrees() {
	use crate::ui::draw::{Cmd, Layer};
	let menu = Menu::new(true, Settings::default(), None, named("ted"));
	let view = menu.view(SIZE, 1.0, false);
	let mut dl = crate::ui::DrawList::default();
	view.emit(&mut dl, &crate::ui::Style::new(1.0), SIZE, 1.0);
	let (cmds, arena) = dl.cmds(Layer::Content);
	let texts: Vec<&str> = cmds
		.iter()
		.filter_map(|c| match c {
			Cmd::Text { range, .. } => Some(&arena[range.0 as usize..range.1 as usize]),
			_ => None,
		})
		.collect();
	assert_eq!(texts.len(), view.rows.len(), "one text per row");
	assert!(texts.contains(&"DIGBLOCK"));
	// Dim + panel quads lead the layer.
	assert!(matches!(cmds[0], Cmd::Quad { .. }) && matches!(cmds[1], Cmd::Quad { .. }));

	// Hit map: a selectable row's center resolves to its id.
	let row = view.rows.iter().find(|r| r.id.is_some()).unwrap();
	let [x, y, w, h] = row.rect;
	assert_eq!(view.hits.hit(x + w / 2.0, y + h / 2.0), row.id);
	assert_eq!(view.hits.hit(1.0, 1.0), None, "outside the panel");
}

/// The panel sizes itself to its widest row: no screen may lay out
/// text (plus any right-aligned column) past the row edge — the
/// browser's hint line used to overflow the old fixed panel width.
#[test]
fn panel_fits_every_row() {
	let check = |menu: &Menu| {
		let view = menu.view(SIZE, 1.0, false);
		let [px, _, pw, _] = view.panel;
		for row in &view.rows {
			let [rx, _, rw, _] = row.rect;
			let glyph = crate::ui::scale::glyph_px(1.0);
			let w = |s: &str| crate::font::ui().measure(s, glyph);
			let mut need = crate::ui::scale::text_pad_x(1.0) * 2.0 + w(&row.text);
			if row.caret {
				need += crate::ui::widgets::caret_w(glyph);
			}
			if let Some((right, _)) = &row.right {
				need += w(right);
			}
			if row.bar.is_some() {
				need += crate::ui::widgets::SLIDER_BAR_W;
			}
			assert!(
				need <= rw,
				"{:?} overflows its row ({need} > {rw})",
				row.text
			);
			assert!(rx >= px && rx + rw <= px + pw, "row outside panel");
		}
	};

	// Root, with the long no-master hint.
	let menu = Menu::new(true, Settings::default(), None, named("ted"));
	check(&menu);
	// Browser: name field, server rows with ping column, hint line.
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	menu.on_response(MenuResponse::ServerList(Ok(vec![entry(
		"s1",
		"Alpha-Beta_0",
	)])));
	menu.on_response(MenuResponse::Ping {
		server_id: "s1".into(),
		ms: Some(1234),
	});
	check(&menu);
	// Settings: slider rows carry text plus a right-aligned bar.
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_settings(&mut menu);
	check(&menu);
	// Worlds screens: list with detail column, create, actions,
	// confirm.
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_worlds(
		&mut menu,
		vec![
			world_row("w1", "A-Long-World-Name", true),
			world_row("w2", "Old", false),
		],
	);
	check(&menu);
	menu.handle_key(Key::Enter); // actions
	check(&menu);
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Enter); // confirm delete
	check(&menu);
	menu.handle_key(Key::Escape); // back to list
	menu.set_sel(2); // create row
	menu.handle_key(Key::Enter);
	check(&menu);
}

#[test]
fn disabled_menu_never_opens() {
	let mut menu = Menu::new(false, Settings::default(), None, named("ted"));
	assert!(!menu.is_open());
	menu.open();
	assert!(!menu.is_open());
}

/// Reopening is `open()` — the Escape that triggers it lives in the
/// engine's input routing, which the menu never sees while closed.
#[test]
fn root_play_local_closes_and_the_menu_reopens() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	assert!(menu.is_open());
	menu.handle_key(Key::Enter); // Play Local
	assert!(!menu.is_open());
	menu.open();
	assert!(menu.is_open());
}

#[test]
fn multiplayer_requires_services() {
	let mut menu = Menu::new(true, Settings::default(), None, None);
	// Startup, without services: no way through to multiplayer at all.
	assert!(!offers(&menu, |a| matches!(a, Act::OpenMultiplayer)));
	menu.set_services_available();
	// Multiplayer joins them. No Account row while signed out — the
	// Multiplayer row IS the way in, and two doors to one place is
	// worse than one.
	assert!(offers(&menu, |a| matches!(a, Act::OpenMultiplayer)));
	assert!(!offers(&menu, |a| matches!(a, Act::OpenAccount)));

	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	assert!(
		offers(&menu, |a| matches!(a, Act::OpenAccount)),
		"signed in, Account joins the list"
	);
}

#[test]
fn startup_menu_locks_until_first_world() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	// No Spellbook row, and Escape is a no-op.
	let view = menu.view(SIZE, 1.0, false);
	assert!(!view.rows.iter().any(|r| r.text.contains("Spellbook")));
	menu.handle_key(Key::Escape);
	assert!(menu.is_open(), "Escape must not dismiss the startup menu");

	// Entering the world (Play Local) unlocks both.
	menu.handle_key(Key::Enter);
	assert!(!menu.take_spellbook(), "Enter was Play Local, not the book");
	menu.open();
	let view = menu.view(SIZE, 1.0, false);
	assert!(view.rows.iter().any(|r| r.text.contains("Spellbook")));
	assert!(view.rows.iter().any(|r| r.text.contains("Back to Game")));
	menu.handle_key(Key::Escape);
	assert!(!menu.is_open());
}

#[test]
fn browser_flow_join_and_fail_returns_with_status() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	assert_matches!(
		menu.take_commands().as_slice(),
		[MenuCommand::RefreshServers]
	);
	menu.on_response(MenuResponse::ServerList(Ok(vec![
		entry("s1", "Alpha"),
		entry("s2", "Beta"),
	])));
	menu.on_response(MenuResponse::Ping {
		server_id: "s2".into(),
		ms: Some(34),
	});
	let view = menu.view(SIZE, 1.0, false);
	assert!(view.rows.iter().any(|r| r.text.contains("Alpha")));
	assert!(
		view.rows
			.iter()
			.any(|r| matches!(&r.right, Some((p, _)) if p == "34 ms"))
	);

	// Join the second server.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Enter);
	match menu.take_commands().as_slice() {
		[MenuCommand::Join { server_id }] => assert_eq!(server_id, "s2"),
		other => panic!("expected Join, got {} commands", other.len()),
	}
	// Failure lands back in the browser with the reason shown.
	menu.on_net_failed("token rejected");
	let view = menu.view(SIZE, 1.0, false);
	assert!(view.rows.iter().any(|r| r.text.contains("token rejected")));
	assert!(
		view.rows.iter().any(|r| r.text.contains("Beta")),
		"rows kept"
	);
}

/// An illegal name is refused on the name screen rather than travelling
/// to the master. The rule itself is `validate_name`; what this pins is
/// that the screen consults it before spending a round trip.
#[test]
fn an_invalid_name_is_refused_without_a_round_trip() {
	for bad in ["", "ab"] {
		let mut menu = Menu::new(true, Settings::default(), None, unnamed());
		menu.set_services_available();
		menu.activate(Act::OpenMultiplayer);
		for ch in bad.chars() {
			menu.handle_text(ch);
		}
		let _ = menu.take_commands(); // availability checks, if any
		menu.handle_key(Key::Enter);
		assert!(
			menu.take_commands().is_empty(),
			"{bad:?} should not reach the master"
		);
		let view = menu.view(SIZE, 1.0, false);
		assert!(
			view.rows.iter().any(|r| r.text.contains("3-10 chars")),
			"{bad:?}: no explanation shown"
		);
	}
}

/// A join carries the server and nothing else. The name it will be
/// played under is the account's, and the master reads it there.
#[test]
fn a_join_carries_only_the_server() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	let _ = menu.take_commands();
	menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
	menu.handle_key(Key::Enter);
	match menu.take_commands().as_slice() {
		[MenuCommand::Join { server_id }] => assert_eq!(server_id, "s1"),
		other => panic!("expected Join, got {} commands", other.len()),
	}
}

/// The name FIELD moved from the browser to the name screen, and it
/// keeps the same three properties: an alphabet, a cap, and backspace.
#[test]
fn text_edits_name_with_charset_cap_and_backspace() {
	let mut menu = Menu::new(true, Settings::default(), None, unnamed());
	menu.set_services_available();
	menu.activate(Act::OpenMultiplayer);
	// Punctuation, spaces, and non-ASCII never enter the buffer.
	for ch in "te d!é_x".chars() {
		menu.handle_text(ch);
	}
	menu.handle_key(Key::Backspace); // drop the trailing x
	assert!(
		menu.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text.contains("name: ted_"))
	);
	for _ in 0..100 {
		menu.handle_text('x');
	}
	let long = menu.view(SIZE, 1.0, true);
	let name_row = long
		.rows
		.iter()
		.find(|r| r.text.starts_with("name:"))
		.unwrap();
	assert!(name_row.text.len() <= "name: ".len() + digblock_net::protocol::MAX_DISPLAY_NAME);
}

/// The bridge's whole-value restatement and the keystroke path are one
/// filter: any divergence would let the DOM buffer smuggle in what
/// typing refuses. Pure deletion — a restatement carrying nothing — must
/// clear the field the way backspacing it out would.
#[test]
fn a_replacement_matches_the_same_text_typed() {
	let line = format!(
		"te d!é_{}",
		"x".repeat(2 * digblock_net::protocol::MAX_DISPLAY_NAME)
	);
	let mut typed = Menu::new(true, Settings::default(), None, unnamed());
	typed.set_services_available();
	typed.activate(Act::OpenMultiplayer);
	for ch in line.chars() {
		typed.handle_text(ch);
	}
	let mut replaced = Menu::new(true, Settings::default(), None, unnamed());
	replaced.set_services_available();
	replaced.activate(Act::OpenMultiplayer);
	replaced.handle_text_replace(&line);
	assert_ne!(typed.text_field_contents(), "");
	assert_eq!(typed.text_field_contents(), replaced.text_field_contents());
	replaced.handle_text_replace("");
	assert_eq!(replaced.text_field_contents(), "");
}

/// Typing a name is one availability check at a time, not one per
/// keystroke. Both fillers serve commands in order and the desktop one
/// blocks a thread doing it, so an unbounded queue of these would sit in
/// front of whatever the player does next.
#[test]
fn name_checks_coalesce_to_one_request_in_flight() {
	let mut menu = Menu::new(true, Settings::default(), None, unnamed());
	menu.set_services_available();
	menu.activate(Act::OpenMultiplayer);
	let _ = menu.take_commands();

	let checks = |menu: &mut Menu| -> Vec<String> {
		menu.take_commands()
			.into_iter()
			.filter_map(|c| match c {
				MenuCommand::CheckName { name } => Some(name),
				_ => None,
			})
			.collect()
	};

	// Six keystrokes. The first two are too short to be worth asking
	// about; the rest land while the "abc" check is still out.
	for ch in "abcdef".chars() {
		menu.handle_text(ch);
	}
	assert_eq!(
		checks(&mut menu),
		vec!["abc".to_string()],
		"one request for the burst, not one per character"
	);

	// The answer re-fires for whatever the field says NOW — the tail of
	// the burst is the answer the player is waiting to see.
	menu.on_response(MenuResponse::NameAvailable {
		name: "abc".into(),
		available: true,
		reason: None,
	});
	assert_eq!(checks(&mut menu), vec!["abcdef".to_string()]);

	// And once the answer matches the field, nothing re-fires.
	menu.on_response(MenuResponse::NameAvailable {
		name: "abcdef".into(),
		available: true,
		reason: None,
	});
	assert!(checks(&mut menu).is_empty(), "settled, so no further asks");
}

#[test]
fn welcome_closes_menu_and_cancel_restores_browser() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
	menu.handle_key(Key::Enter);
	menu.on_welcome();
	assert!(!menu.is_open());

	// Second run: cancel instead. From a fresh (pre-entry) menu —
	// Multiplayer lives on the title menu, not the in-game one.
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	menu.handle_key(Key::Down); // Multiplayer (after Play Local)
	menu.handle_key(Key::Enter);
	menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
	menu.handle_key(Key::Enter);
	let _ = menu.take_commands();
	menu.handle_key(Key::Escape); // cancel connect
	assert!(menu.take_disconnect());
	assert!(
		menu.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text.contains("cancelled"))
	);
}

#[test]
fn click_hits_rows() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	// Click "Multiplayer" (second selectable row).
	let view = menu.view(SIZE, 1.0, false);
	let mp_row = view
		.rows
		.iter()
		.find(|r| r.id == Some(RowId::RootItem(1)))
		.unwrap();
	let (cx, cy) = (mp_row.rect[0] + 5.0, mp_row.rect[1] + 5.0);
	menu.handle_input(&InputEvent::CursorMoved { x: cx, y: cy }, SIZE, 1.0);
	menu.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		SIZE,
		1.0,
	);
	assert_matches!(
		menu.take_commands().as_slice(),
		[MenuCommand::RefreshServers]
	);
}

#[test]
fn leave_server_appears_when_connected() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_connected(true);
	// Connected, so leaving the SERVER is what the row offers — never
	// leaving a local world.
	assert!(offers(&menu, |a| matches!(a, Act::LeaveServer)));
	assert!(!offers(&menu, |a| matches!(a, Act::LeaveWorld)));
	select_act(&mut menu, |a| matches!(a, Act::LeaveServer));
	menu.handle_key(Key::Enter);
	assert!(menu.take_disconnect());
	assert!(!menu.is_open());
}

/// The two in-game menus used to diverge: a local world offered
/// Singleplayer and Multiplayer where a server offered Leave Server,
/// so the same Escape key produced a different page depending on
/// where you were. Both now end in exactly one exit row.
#[test]
fn leave_world_is_the_local_counterpart_of_leave_server() {
	let rows_of = |menu: &Menu| -> Vec<String> {
		menu.view(SIZE, 1.0, false)
			.rows
			.iter()
			.map(|r| r.text.to_string())
			.collect()
	};
	let mut local = Menu::new(true, Settings::default(), None, named("ted"));
	local.set_services_available();
	local.set_store_available();
	local.on_world_entered();
	local.open();
	let mut server = Menu::new(true, Settings::default(), None, named("ted"));
	server.set_services_available();
	server.set_store_available();
	server.set_connected(true);

	let (a, b) = (rows_of(&local), rows_of(&server));
	assert_eq!(
		a.iter().position(|r| r == "Leave World"),
		b.iter().position(|r| r == "Leave Server"),
		"the exit row sits in the same place: {a:?} vs {b:?}"
	);
	for rows in [&a, &b] {
		assert!(!rows.iter().any(|r| r == "Singleplayer"), "{rows:?}");
		assert!(
			!rows.iter().any(|r| r.starts_with("Multiplayer")),
			"{rows:?}"
		);
	}
	assert_eq!(a.len(), b.len(), "same page either side: {a:?} vs {b:?}");

	// Activating it raises the intent and closes the menu, exactly
	// as Leave Server does.
	let leave = local
		.acts()
		.iter()
		.position(|(id, _)| *id == RowId::RootItem(8))
		.expect("the Leave World row is an act");
	local.set_sel(leave);
	local.handle_key(Key::Enter);
	assert!(local.take_leave_world());
	assert!(!local.take_disconnect(), "a local world has no transport");
	assert!(!local.is_open());
}

#[test]
fn spellbook_row_raises_intent_and_closes() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.handle_key(Key::Enter); // Play Local: the row now exists
	menu.open();
	// Directly after Back to Game (no services); activating raises the
	// intent and closes the menu — the cooldown lives on the PAGE now,
	// so the row itself is never greyed.
	assert!(
		offers(&menu, |a| matches!(a, Act::OpenSpellbook)),
		"the row is always an act"
	);
	menu.handle_key(Key::Down); // Spellbook
	menu.handle_key(Key::Enter);
	assert!(menu.take_spellbook());
	assert!(!menu.take_spellbook(), "intent is one-shot");
	assert!(!menu.is_open());
}

/// Does the menu offer an act matching `f`?
///
/// Rows are found by what they DO, never by counting them: a count says
/// nothing about which row appeared, and every one of these tests broke
/// whenever an unrelated row was inserted.
fn offers(menu: &Menu, f: impl Fn(&Act) -> bool) -> bool {
	menu.acts().iter().any(|(_, a)| f(a))
}

/// Select the act matching `f`, so a test never counts `Key::Down`s.
fn select_act(menu: &mut Menu, f: impl Fn(&Act) -> bool) {
	let at = menu
		.acts()
		.iter()
		.position(|(_, a)| f(a))
		.expect("the act is not on offer");
	menu.set_sel(at);
}

fn open_settings(menu: &mut Menu) {
	select_act(menu, |a| matches!(a, Act::OpenSettings));
	menu.handle_key(Key::Enter);
}

#[test]
fn settings_adjust_clamp_and_dirty_once() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_settings(&mut menu);
	assert!(
		menu.take_settings_change().is_none(),
		"opening isn't a change"
	);

	// Draw distance: sel 0, one step per press, clamped at DRAW_MAX.
	menu.handle_key(Key::Right);
	let s = menu.take_settings_change().expect("dirty after adjust");
	assert_eq!(s.draw_dist, Settings::default().draw_dist + DRAW_STEP);
	assert!(menu.take_settings_change().is_none(), "drained once");
	for _ in 0..40 {
		menu.handle_key(Key::Right);
	}
	assert_eq!(menu.take_settings_change().unwrap().draw_dist, DRAW_MAX);
	// At the max, Right is a no-op — not a dirty change.
	menu.handle_key(Key::Right);
	assert!(menu.take_settings_change().is_none());

	// Lighting detail: sel 1. Render scale has no row — it is locked
	// at native for players (settings::RENDER_SCALE_MIN says why), so
	// nothing here can move it. Walks the DIVISOR list, and Left is
	// coarser — so it ends on the largest divisor, not the smallest.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Left);
	// One notch coarser than wherever the default sits, read off the
	// divisor list rather than written down: the default moves with the
	// display's density, so a literal here would be a second answer to a
	// question `density_defaults` already answers.
	let coarser = {
		let at = GI_SCALE_DIVISORS
			.iter()
			.position(|d| *d == Settings::default().gi_scale_div)
			.expect("the default is a listed divisor");
		GI_SCALE_DIVISORS[at + 1]
	};
	assert_eq!(menu.take_settings_change().unwrap().gi_scale_div, coarser);
	for _ in 0..10 {
		menu.handle_key(Key::Left);
	}
	assert_eq!(
		menu.take_settings_change().unwrap().gi_scale_div,
		*GI_SCALE_DIVISORS.last().unwrap()
	);
	menu.handle_key(Key::Left);
	assert!(menu.take_settings_change().is_none(), "already coarsest");
	// And back up to the sharpest rung the slider offers.
	for _ in 0..10 {
		menu.handle_key(Key::Right);
	}
	assert_eq!(
		menu.take_settings_change().unwrap().gi_scale_div,
		GI_SCALE_DIVISORS[0]
	);

	// Light sources: sel 2, one step per press, floored at the slider's
	// own minimum — every expectation derived from the cap constants.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Left);
	assert_eq!(
		menu.take_settings_change().unwrap().max_lights,
		Settings::default().max_lights - LIGHT_CAP_STEP
	);
	for _ in 0..40 {
		menu.handle_key(Key::Left);
	}
	assert_eq!(
		menu.take_settings_change().unwrap().max_lights,
		LIGHT_CAP_MIN
	);
	menu.handle_key(Key::Left);
	assert!(
		menu.take_settings_change().is_none(),
		"already at the floor"
	);
	for _ in 0..40 {
		menu.handle_key(Key::Right);
	}
	assert_eq!(
		menu.take_settings_change().unwrap().max_lights,
		LIGHT_CAP_MAX
	);

	// Interface scale: sel 3, one notch down from neutral, then on
	// down to the min.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Left);
	let s = menu.take_settings_change().unwrap();
	// One Left is one notch DOWN THE GRID, which is not the same as
	// "the default minus a step": the default need not sit on the grid,
	// and when it does not, the first press lands on the notch below it.
	let before = Settings::default().ui_scale;
	assert!(
		s.ui_scale < before,
		"Left must lower the scale: {}",
		s.ui_scale
	);
	assert!(
		before - s.ui_scale <= UI_SCALE_STEP + 1e-4,
		"Left moved more than one notch: {} -> {}",
		before,
		s.ui_scale
	);
	let notches = (s.ui_scale - UI_SCALE_MIN) / UI_SCALE_STEP;
	assert!(
		(notches - notches.round()).abs() < 1e-3,
		"{} is off the notch grid",
		s.ui_scale
	);
	for _ in 0..40 {
		menu.handle_key(Key::Left);
	}
	assert!((menu.take_settings_change().unwrap().ui_scale - UI_SCALE_MIN).abs() < 1e-4);
	menu.handle_key(Key::Left);
	assert!(menu.take_settings_change().is_none(), "already at the min");

	// Music volume: sel 4, one notch below the default, floor at silence.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Left);
	let s = menu.take_settings_change().unwrap();
	assert!((s.music_vol - (Settings::default().music_vol - VOL_STEP)).abs() < 1e-4);
	for _ in 0..40 {
		menu.handle_key(Key::Left);
	}
	assert_eq!(menu.take_settings_change().unwrap().music_vol, 0.0);

	// SSR: sel 6, Enter toggles, Left also toggles.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Enter);
	assert!(!menu.take_settings_change().unwrap().ssr);
	menu.handle_key(Key::Left);
	assert!(menu.take_settings_change().unwrap().ssr);

	// Escape returns to Root; the values held.
	menu.handle_key(Key::Escape);
	open_settings(&mut menu);
	let view = menu.view(SIZE, 1.0, false);
	assert!(
		view.rows
			.iter()
			.any(|r| r.text.contains(&format!("Draw Distance: {DRAW_MAX}")))
	);
	assert!(
		view.rows
			.iter()
			.any(|r| r.text.contains("Music Volume: 0%"))
	);
	assert!(view.rows.iter().any(|r| {
		r.text
			.contains(&format!("Interface Scale: {:.0}%", UI_SCALE_MIN * 100.0))
	}));
	// No render-scale row exists at all.
	assert!(!view.rows.iter().any(|r| r.text.contains("Render Scale")));
	assert!(
		view.rows
			.iter()
			.any(|r| r.text.contains("Lighting Detail: High"))
	);
}

/// Every notch of the interface-scale slider is reachable and lands on
/// its exact grid value. With a fine step the old
/// add-to-the-live-value path drifts, and the label rounds to whole
/// percent — so a run to one end and back is the test that would
/// catch it.
#[test]
fn ui_scale_slider_walks_its_whole_range_without_drift() {
	let notches = ((UI_SCALE_MAX - UI_SCALE_MIN) / UI_SCALE_STEP).round() as i32;
	let mut v = 1.0f32;
	let mut seen = Vec::new();
	for _ in 0..notches * 2 {
		v = snap_range(v, -1, UI_SCALE_MIN, UI_SCALE_MAX, UI_SCALE_STEP);
	}
	assert_eq!(v, UI_SCALE_MIN, "bottoms out exactly");
	for _ in 0..notches {
		seen.push(v);
		v = snap_range(v, 1, UI_SCALE_MIN, UI_SCALE_MAX, UI_SCALE_STEP);
	}
	assert_eq!(v, UI_SCALE_MAX, "tops out exactly");
	seen.push(v);
	assert_eq!(seen.len() as i32, notches + 1);
	for (i, got) in seen.iter().enumerate() {
		let want = UI_SCALE_MIN + i as f32 * UI_SCALE_STEP;
		assert!((got - want).abs() < 1e-6, "notch {i}: {got} vs {want}");
	}
	// The label rounds to whole percent, and no two notches may print the
	// same number — which is the drift this exists to catch. Asserted as
	// that property rather than as the percentages themselves, so a finer
	// step is a retune of one constant and not of this list.
	let printed: Vec<i32> = seen
		.iter()
		.map(|v| format!("{:.0}", v * 100.0).parse::<i32>().unwrap())
		.collect();
	let distinct = {
		let mut d = printed.clone();
		d.dedup();
		d.len()
	};
	assert_eq!(
		distinct,
		printed.len(),
		"two notches print alike: {printed:?}"
	);
	let mut sorted = printed.clone();
	sorted.sort_unstable();
	assert_eq!(sorted, printed, "the labels do not climb with the notches");
}

#[test]
fn settings_bar_click_sets_snapped_value() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_settings(&mut menu);
	let view = menu.view(SIZE, 1.0, false);
	let row = view
		.rows
		.iter()
		.find(|r| r.id == Some(RowId::SettingsRow(0)))
		.expect("draw slider row");
	let [bx, by, bw, bh] = bar_rect(row.rect, 1.0);

	// Click at the middle of the bar: snapped to the step grid around
	// the range midpoint, so it lands on a notch either side of it.
	let (cx, cy) = (bx + bw * 0.5, by + bh * 0.5);
	menu.handle_input(&InputEvent::CursorMoved { x: cx, y: cy }, SIZE, 1.0);
	menu.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		SIZE,
		1.0,
	);
	let s = menu
		.take_settings_change()
		.expect("bar click changes value");
	assert!(s.draw_dist == 20 || s.draw_dist == 22, "{}", s.draw_dist);
	assert_eq!((s.draw_dist - DRAW_MIN) % DRAW_STEP, 0);

	// Click at the far left end: the minimum.
	menu.handle_input(&InputEvent::CursorMoved { x: bx, y: cy }, SIZE, 1.0);
	menu.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		SIZE,
		1.0,
	);
	assert_eq!(menu.take_settings_change().unwrap().draw_dist, DRAW_MIN);

	// A click on the row but OUTSIDE the bar leaves the value alone
	// (sliders don't "activate").
	let (lx, ly) = (row.rect[0] + 5.0, row.rect[1] + 5.0);
	menu.handle_input(&InputEvent::CursorMoved { x: lx, y: ly }, SIZE, 1.0);
	menu.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		SIZE,
		1.0,
	);
	assert!(menu.take_settings_change().is_none());
}

/// A platform that declared the lower floor (`Menu::set_draw_min` — mobile
/// web) opens the slider's lower rungs: the far-left click lands on the
/// floor, still on the step grid, and keyboard steps walk down to it.
#[test]
fn the_declared_draw_floor_opens_the_sliders_lower_rungs() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_draw_min(DRAW_FLOOR);
	open_settings(&mut menu);
	for _ in 0..40 {
		menu.handle_key(Key::Left);
	}
	assert_eq!(menu.take_settings_change().unwrap().draw_dist, DRAW_FLOOR);

	let view = menu.view(SIZE, 1.0, false);
	let row = view
		.rows
		.iter()
		.find(|r| r.id == Some(RowId::SettingsRow(0)))
		.expect("draw slider row");
	let [bx, by, _, bh] = bar_rect(row.rect, 1.0);
	// Walk off the floor first so the far-left click is a CHANGE.
	menu.handle_key(Key::Right);
	menu.take_settings_change().expect("stepped off the floor");
	menu.handle_input(
		&InputEvent::CursorMoved {
			x: bx,
			y: by + bh * 0.5,
		},
		SIZE,
		1.0,
	);
	menu.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		SIZE,
		1.0,
	);
	let s = menu
		.take_settings_change()
		.expect("bar click changes value");
	assert_eq!(s.draw_dist, DRAW_FLOOR);
	assert_eq!((s.draw_dist - DRAW_FLOOR) % DRAW_STEP, 0);
}

/// Pressing Multiplayer while signed out walks the whole route:
/// sign in, pick a name, then the server list. Each stage advances when
/// its answer arrives, so a first-time player presses one button.
#[test]
fn multiplayer_walks_sign_in_then_naming_then_the_list() {
	let mut menu = Menu::new(true, Settings::default(), None, None);
	menu.set_services_available();
	menu.activate(Act::OpenMultiplayer);
	assert_matches!(
		menu.take_commands().as_slice(),
		[MenuCommand::SignIn],
		"Multiplayer while signed out must start a sign-in"
	);

	// Signed in, but unnamed: the name screen, not the server list.
	menu.on_response(MenuResponse::Account(unnamed()));
	assert!(
		menu.take_commands().is_empty(),
		"no list fetch before a name"
	);
	for ch in "Miner42".chars() {
		menu.handle_text(ch);
	}
	let checks = menu.take_commands();
	assert!(
		checks
			.iter()
			.any(|c| matches!(c, MenuCommand::CheckName { .. })),
		"typing a legal name asks whether it is free"
	);
	menu.handle_key(Key::Enter);
	assert_matches!(
		menu.take_commands().as_slice(),
		[MenuCommand::SetName { name }] if name == "Miner42",
		"enter claims the name"
	);

	// The claim lands: now the server list.
	menu.on_response(MenuResponse::NameSet(Ok("Miner42".into())));
	assert_matches!(
		menu.take_commands().as_slice(),
		[MenuCommand::RefreshServers],
		"a claimed name goes straight on to the browser"
	);
	assert_eq!(
		menu.account().and_then(|a| a.name.as_deref()),
		Some("Miner42")
	);
}

/// Copy ID raises an intent rather than doing I/O — this module has
/// none — and hands over exactly the account id an operator needs.
#[test]
fn copy_id_offers_the_account_id() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	menu.activate(Act::OpenAccount);
	assert_eq!(menu.take_clipboard(), None, "nothing copied unprompted");
	menu.activate(Act::CopyAccountId);
	assert_eq!(
		menu.take_clipboard().as_deref(),
		Some("b2379408-3102-8b68-b56b-5774dec269b8")
	);
	assert_eq!(menu.take_clipboard(), None, "drained once");
}

/// Signing out drops the account immediately, without waiting for the
/// launcher to confirm: the player asked, and a menu that still says
/// "signed in as" while the request is in flight is a lie.
#[test]
fn signing_out_clears_the_account_at_once() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	menu.activate(Act::OpenAccount);
	menu.activate(Act::SignOut);
	assert!(menu.account().is_none());
	assert_matches!(menu.take_commands().as_slice(), [MenuCommand::SignOut]);
}

#[test]
fn the_browser_shows_the_account_name() {
	let mut menu = Menu::new(true, Settings::default(), None, named("Miner42"));
	menu.set_services_available();
	open_browser(&mut menu);
	let view = menu.view(SIZE, 1.0, false);
	assert!(
		view.rows
			.iter()
			.any(|r| r.text.contains("playing as Miner42")),
		"{:?}",
		view.rows.iter().map(|r| &r.text).collect::<Vec<_>>()
	);
}

fn world_row(id: &str, name: &str, compatible: bool) -> WorldRow {
	WorldRow {
		id: id.into(),
		name: name.into(),
		detail: if compatible {
			"3d ago".into()
		} else {
			"old version".into()
		},
		compatible,
	}
}

fn open_worlds(menu: &mut Menu, rows: Vec<WorldRow>) {
	menu.set_store_available();
	menu.set_sel(0);
	menu.handle_key(Key::Enter); // Singleplayer (row 0 pre-entry)
	assert_matches!(menu.take_world_intents().as_slice(), [WorldIntent::Refresh]);
	menu.set_world_list(rows);
}

#[test]
fn store_replaces_play_local_until_entered() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_store_available();
	let view = menu.view(SIZE, 1.0, false);
	assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
	assert!(!view.rows.iter().any(|r| r.text == "Play Local"));
	// Enter opens the world list, not the game.
	menu.handle_key(Key::Enter);
	assert!(menu.is_open());
	assert_matches!(menu.take_world_intents().as_slice(), [WorldIntent::Refresh]);
	// Entering a world unlocks Back to Game and swaps Singleplayer
	// for Leave World: switching saves goes back through the title.
	menu.on_world_entered();
	assert!(!menu.is_open());
	menu.open();
	let view = menu.view(SIZE, 1.0, false);
	assert!(view.rows.iter().any(|r| r.text == "Back to Game"));
	assert!(view.rows.iter().any(|r| r.text == "Leave World"));
	assert!(!view.rows.iter().any(|r| r.text == "Singleplayer"));
}

#[test]
fn world_enter_flow_emits_intent() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_worlds(&mut menu, vec![world_row("w1", "Home", true)]);
	menu.handle_key(Key::Enter); // world row -> actions
	menu.handle_key(Key::Enter); // Enter World
	assert_matches!(
		menu.take_world_intents().as_slice(),
		[WorldIntent::Enter { id }] if id == "w1"
	);
	// Engine opens it and reports success.
	menu.on_world_entered();
	assert!(!menu.is_open());
}

#[test]
fn create_world_text_entry_and_intent() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_worlds(&mut menu, vec![]);
	menu.handle_key(Key::Enter); // "Create New World" (only act)
	// Empty name: Enter is a no-op.
	menu.handle_key(Key::Enter);
	assert!(menu.take_world_intents().is_empty());
	// Charset + cap; backspace edits.
	for ch in "My World!".chars() {
		menu.handle_text(ch);
	}
	for ch in "abcdefghijklmnop".chars() {
		menu.handle_text(ch); // runs into the name-length cap
	}
	menu.handle_key(Key::Backspace);
	let Some(Screen::CreateWorld { name, .. }) = &menu.screen else {
		panic!("on create screen");
	};
	// World names take the full text alphabet (space and punctuation
	// included — they never leave this machine), filled to the cap,
	// minus the backspace.
	// Filled to the cap, minus the backspace — derived, so widening the
	// cap is one constant and not this literal too.
	let typed: String = "My World!abcdefghijklmnop"
		.chars()
		.take(WORLD_NAME_MAX)
		.collect();
	let expect: String = typed.chars().take(WORLD_NAME_MAX - 1).collect();
	assert_eq!(name, &expect);
	menu.handle_key(Key::Enter);
	assert_matches!(
		menu.take_world_intents().as_slice(),
		[WorldIntent::Create { name }] if name == "My World!abcdef"
	);
	assert!(
		matches!(&menu.screen, Some(Screen::Worlds(w)) if w.status.is_some()),
		"back on the list with a status while the engine works"
	);
}

#[test]
fn delete_requires_confirm() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_worlds(&mut menu, vec![world_row("w1", "Home", true)]);
	menu.handle_key(Key::Enter); // actions
	menu.handle_key(Key::Down); // Delete World
	menu.handle_key(Key::Enter);
	assert!(menu.take_world_intents().is_empty(), "no delete yet");
	assert!(matches!(&menu.screen, Some(Screen::ConfirmDelete { .. })));
	// Cancel first.
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Enter);
	assert!(menu.take_world_intents().is_empty());
	assert!(matches!(&menu.screen, Some(Screen::Worlds(_))));
	// Now delete for real.
	menu.handle_key(Key::Enter); // actions
	menu.handle_key(Key::Down);
	menu.handle_key(Key::Enter); // confirm screen
	menu.handle_key(Key::Enter); // "Delete forever" is row 0
	assert_matches!(
		menu.take_world_intents().as_slice(),
		[WorldIntent::Delete { id }] if id == "w1"
	);
}

#[test]
fn incompatible_world_offers_delete_only() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	open_worlds(&mut menu, vec![world_row("w1", "Old", false)]);
	menu.handle_key(Key::Enter); // actions
	let acts = menu.acts();
	assert!(
		acts.iter().any(|(_, a)| matches!(a, Act::DeleteWorld)),
		"an incompatible save may still be deleted"
	);
	assert!(
		!acts.iter().any(|(_, a)| matches!(a, Act::EnterWorld)),
		"but never entered"
	);
	assert!(!acts.iter().any(|(id, _)| *id == RowId::WorldAction(0)));
	// Enter on row 0 must be Delete (→ confirm), never EnterWorld.
	menu.handle_key(Key::Enter);
	assert!(matches!(&menu.screen, Some(Screen::ConfirmDelete { .. })));
	assert!(menu.take_world_intents().is_empty());
}

#[test]
fn left_server_relocks_startup_menu() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_store_available();
	menu.set_connected(true);
	assert!(menu.entered_world);
	menu.on_returned_to_title();
	assert!(menu.is_open());
	// Locked again: Escape can't dismiss, no Back to Game.
	menu.handle_key(Key::Escape);
	assert!(menu.is_open());
	let view = menu.view(SIZE, 1.0, false);
	assert!(!view.rows.iter().any(|r| r.text == "Back to Game"));
	assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
}

/// The `Screen` a page landed on, as a stable name. Exhaustive on
/// purpose: a new page cannot be added without being named here, and
/// `every_touch_page` then refuses to pass until the sweep reaches it.
/// That pair is what makes the sweep a sweep — the list of pages below
/// is hand-written, so on its own it is a spot-check wearing a loop.
fn screen_name(s: &Screen) -> &'static str {
	match s {
		Screen::Root { .. } => "root",
		Screen::Browser(_) => "browser",
		Screen::Connecting { .. } => "connecting",
		Screen::Settings { .. } => "settings",
		Screen::Worlds(_) => "worlds",
		Screen::CreateWorld { .. } => "create world",
		Screen::WorldActions { .. } => "world actions",
		Screen::ConfirmDelete { .. } => "confirm delete",
		Screen::SignIn(_) => "sign in",
		Screen::ChooseName(_) => "choose name",
		Screen::Account { .. } => "account",
	}
}

/// Every page a finger can reach, for the two touch rules that have to hold on all of them: it can
/// be backed out of, and it never names a key. Swept rather than spot-checked because both rules
/// are about the pages nobody thought of. The sweep proves its own completeness at the end: every
/// `Screen` variant `screen_name` knows about has to have been visited, so a page added later
/// fails to compile there and fails the coverage assert here.
fn every_touch_page(mut visit: impl FnMut(&mut Menu, &str)) {
	use std::collections::BTreeSet;
	let mut seen: BTreeSet<&'static str> = BTreeSet::new();
	// Reached through the menu's own actions rather than by building
	// its state by hand, so a page that changes how it is entered is
	// still the page this sweeps. All but the last hang off the TITLE
	// menu, which is where a phone meets most of them.
	let pages: [(&str, &[Act]); 11] = [
		("title root", &[]),
		("multiplayer", &[Act::OpenMultiplayer]),
		("settings", &[Act::OpenSettings]),
		("worlds", &[Act::OpenWorlds]),
		("create world", &[Act::OpenWorlds, Act::OpenCreate]),
		("world actions", &[Act::OpenWorlds, Act::WorldRow(0)]),
		(
			"confirm delete",
			&[Act::OpenWorlds, Act::WorldRow(0), Act::DeleteWorld],
		),
		("account", &[Act::OpenAccount]),
		(
			"sign in",
			&[Act::StartSignIn {
				then_multiplayer: true,
			}],
		),
		("connecting", &[Act::OpenMultiplayer, Act::JoinRow(0)]),
		// The rename is the reachable half of the name picker: a first
		// pick needs a sign-in round trip, and both open the same page
		// through the same call.
		("choose name", &[Act::OpenAccount, Act::OpenRename]),
	];
	for (name, acts) in pages {
		let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
		menu.set_touch();
		menu.set_store_available();
		menu.set_services_available();
		for &act in acts {
			menu.activate(act);
			// Two lists arrive from outside rather than from a press,
			// and the pages behind them are unreachable without one.
			match act {
				Act::OpenWorlds => {
					menu.take_world_intents();
					menu.set_world_list(vec![world_row("w1", "Home", true)]);
				}
				Act::OpenMultiplayer => {
					menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
				}
				_ => {}
			}
		}
		// A sweep that silently stayed on Root would pass everything.
		assert_eq!(
			matches!(menu.screen, Some(Screen::Root { .. })),
			acts.is_empty(),
			"{name} was not reached"
		);
		if let Some(s) = &menu.screen {
			seen.insert(screen_name(s));
		}
		visit(&mut menu, name);
	}
	// The in-world root is a different page from the title one: it has
	// a world behind it, which is exactly what makes it leavable.
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_touch();
	menu.handle_key(Key::Enter); // Play Local — the only row without a store
	menu.open();
	assert!(matches!(menu.screen, Some(Screen::Root { .. })));
	visit(&mut menu, "in-world root");

	// The claim the doc above makes, checked rather than asserted: a
	// page `screen_name` knows and this sweep never stood on is a page
	// neither touch rule was ever applied to.
	let missing: Vec<_> = ALL_SCREEN_NAMES
		.iter()
		.filter(|n| !seen.contains(*n))
		.collect();
	assert!(
		missing.is_empty(),
		"never swept: {missing:?} — add the page above, or this is a spot-check"
	);
}

/// Every name [`screen_name`] can return. Hand-written, and held to
/// the match by `every_touch_page`'s own coverage assert: a variant
/// added to one and not the other is caught the first time the sweep
/// runs.
const ALL_SCREEN_NAMES: &[&str] = &[
	"root",
	"browser",
	"connecting",
	"settings",
	"worlds",
	"create world",
	"world actions",
	"confirm delete",
	"sign in",
	"choose name",
	"account",
];

#[test]
fn a_touch_page_never_names_a_key() {
	every_touch_page(|menu, page| {
		for row in menu.view(SIZE, 1.0, false).rows {
			// Only the rows that are PROSE. A selectable row is a thing
			// a finger presses and its label is a verb — the world
			// actions page offers "Enter World", a place to go rather
			// than a key to press, and matching on the word alone reads
			// it as the second. An instruction to press something can
			// only live in a row you cannot press.
			if row.id.is_some() {
				continue;
			}
			let t = row.text.to_lowercase();
			for word in ["esc", "arrows", "enter "] {
				assert!(
					!t.contains(word),
					"{page} tells a phone about {word:?}: {:?}",
					row.text
				);
			}
		}
	});
}

#[test]
fn a_touch_page_can_always_be_backed_out_of() {
	// The corner button is the only way off a page on a phone, and it
	// is drawn on exactly this answer. The one page that says no is the
	// locked title root, which has nothing behind it to go back TO.
	every_touch_page(|menu, page| {
		assert_eq!(
			menu.can_go_back(),
			page != "title root",
			"{page}: wrong answer for the back button"
		);
	});
}

/// A server the handshake would refuse stays on the list — a self-hoster's
/// players have to be able to see it is there — and stops being an offer:
/// its verdict takes the ping column, selection skips it, and Enter lands
/// on the next row that can actually be joined.
#[test]
fn an_incompatible_server_is_listed_but_never_offered() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	let _ = menu.take_commands();
	menu.on_response(MenuResponse::ServerList(Ok(vec![
		ahead("s1", "Stale"),
		entry("s2", "Current"),
	])));
	let view = menu.view(SIZE, 1.0, false);
	let row = view
		.rows
		.iter()
		.find(|r| r.text.contains("Stale"))
		.expect("the row is still listed");
	let why = compat_label(Compat::PeerNewer).expect("a refused row has a word for it");
	assert_matches!(&row.right, Some((label, _)) if label == why);
	assert!(row.id.is_none(), "and cannot be clicked");

	// Enter without arrowing: the first SELECTABLE row is the second one.
	menu.handle_key(Key::Enter);
	match menu.take_commands().as_slice() {
		[MenuCommand::Join { server_id }] => assert_eq!(server_id, "s2"),
		other => panic!("expected a join of the compatible row, got {other:?}"),
	}
}

/// The scripted path (`--join`) reaches `JoinRow` without going through
/// `acts`, so the refusal lives in the action too — and says so, rather
/// than spending a one-time token and a QUIC connect to be told.
#[test]
fn a_scripted_join_of_an_incompatible_server_spends_nothing() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	let _ = menu.take_commands();
	menu.on_response(MenuResponse::ServerList(Ok(vec![ahead("s1", "Stale")])));
	menu.activate(Act::JoinRow(0));
	assert!(
		menu.take_commands().is_empty(),
		"no token is minted for a server the handshake would refuse"
	);
	let view = menu.view(SIZE, 1.0, false);
	let why = compat_label(Compat::PeerNewer).expect("a refused row has a word for it");
	assert!(
		view.rows
			.iter()
			.any(|r| r.text.contains("Stale") && r.text.contains(why)),
		"and the status line names the server and the reason"
	);
}

/// The verdict reads the row's OWN two fields, and each one independently
/// closes the gate: a `compat` that passed either field twice would still
/// answer correctly on the version pairs where both agree, and this is the
/// table where they do not. The window itself is pinned in `digblock-net`;
/// what is pinned here is the wiring.
#[test]
fn each_version_field_can_refuse_a_row_on_its_own() {
	use digblock_net::protocol::{MIN_PROTOCOL, PROTOCOL_VERSION};
	let sim = digblock_core::SIM_VERSION;
	let cases = [
		(PROTOCOL_VERSION, sim, true),
		(MIN_PROTOCOL, sim, true),
		(MIN_PROTOCOL.saturating_sub(1), sim, false),
		(PROTOCOL_VERSION + 1, sim, false),
		(PROTOCOL_VERSION, sim.saturating_sub(1), false),
		(PROTOCOL_VERSION, sim + 1, false),
	];
	let mut refused = 0;
	for (protocol, sim_version, joinable) in cases {
		let e = ServerEntry {
			protocol_version: protocol,
			sim_version,
			..entry("s1", "Alpha")
		};
		assert_eq!(
			e.compat() == Compat::Joinable,
			joinable,
			"protocol {protocol}, sim {sim_version}"
		);
		refused += u32::from(!joinable);
	}
	// Two per gate, or a saturating edge quietly turned a case into a
	// duplicate of one that passes.
	assert_eq!(refused, 4);
}

/// A server whose play counts toward the board says so on its row, and
/// one whose play does not says nothing — the master's trusted list is
/// the only thing that decides, and the row is where a player reads it
/// before spending a join.
#[test]
fn a_ranked_server_says_so_on_its_row() {
	let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
	menu.set_services_available();
	open_browser(&mut menu);
	menu.on_response(MenuResponse::ServerList(Ok(vec![
		entry("s1", "Alpha"),
		ServerEntry {
			reports_skills: true,
			..entry("s2", "Beta")
		},
	])));
	let view = menu.view(SIZE, 1.0, false);
	let mark = |name: &str| {
		view.rows
			.iter()
			.find(|r| r.text.starts_with(name))
			.unwrap_or_else(|| panic!("a row for {name}"))
			.mark
	};
	assert_eq!(mark("Beta"), Some(super::view::RANKED_MARK));
	assert_eq!(mark("Alpha"), None);
	// A picture, never a word: the row's text is the same either way.
	let text = |name: &str| {
		view.rows
			.iter()
			.find(|r| r.text.starts_with(name))
			.unwrap()
			.text
			.clone()
	};
	assert_eq!(
		text("Beta").trim_start_matches("Beta"),
		text("Alpha").trim_start_matches("Alpha")
	);
}

/// Choosing a server remembers the master's word on whether play there
/// counts, for the engine to say at the door — and it is the listing's
/// word, never the server's.
#[test]
fn joining_remembers_whether_the_server_is_ranked() {
	for ranked in [false, true] {
		let mut menu = Menu::new(true, Settings::default(), None, named("ted"));
		menu.set_services_available();
		open_browser(&mut menu);
		assert_eq!(menu.join_ranked(), None);
		menu.on_response(MenuResponse::ServerList(Ok(vec![ServerEntry {
			reports_skills: ranked,
			..entry("s1", "Alpha")
		}])));
		menu.handle_key(Key::Enter);
		assert_eq!(menu.join_ranked(), Some(ranked));
		assert!(
			menu.take_commands()
				.iter()
				.any(|c| matches!(c, MenuCommand::Join { .. }))
		);
	}
}
