//! Page-to-page navigation: while one screen is up, another page's key
//! swaps to it in one press; the open page's own key still toggles it
//! closed; Escape closes the page first and only a second press pauses;
//! and the in-world menu root lets the same hotkeys reach through to
//! the pages its rows name.

use super::*;

/// An in-game engine on the keyboard profile.
fn game_engine() -> Engine {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	config.menu = true;
	let mut engine = Engine::new(config);
	// The title menu starts open; enter the world the way play does.
	engine.ui.menu.set_connected(true);
	engine.input(InputEvent::Key {
		key: Key::Escape,
		pressed: true,
	});
	assert!(!engine.ui.menu.is_open(), "in-game to start");
	engine
}

use crate::bindings::Action;

/// The default key an action ships bound to.
fn key_of(a: Action) -> Key {
	crate::bindings::Bindings::default().key(a).unwrap()
}

fn press(engine: &mut Engine, key: Key) {
	engine.input(InputEvent::Key { key, pressed: true });
	engine.input(InputEvent::Key {
		key,
		pressed: false,
	});
}

#[test]
fn one_press_hops_page_to_page() {
	let mut e = game_engine();
	press(&mut e, key_of(Action::OpenInventory));
	assert!(e.ui.inv_screen.is_open());
	// Straight to the bench, no close in between.
	press(&mut e, key_of(Action::OpenAlchemy));
	assert!(!e.ui.inv_screen.is_open());
	assert!(e.ui.alch_screen.is_open());
	// And on around the ring.
	press(&mut e, key_of(Action::OpenSpellbook));
	assert!(e.ui.spellbook.is_open() && !e.ui.alch_screen.is_open());
	press(&mut e, key_of(Action::OpenCharacter));
	assert!(e.ui.char_screen.is_open() && !e.ui.spellbook.is_open());
	press(&mut e, key_of(Action::OpenHiscores));
	assert!(e.ui.hiscores.is_open() && !e.ui.char_screen.is_open());
}

#[test]
fn a_pages_own_key_still_toggles_it_closed() {
	let mut e = game_engine();
	press(&mut e, key_of(Action::OpenInventory));
	assert!(e.ui.inv_screen.is_open());
	press(&mut e, key_of(Action::OpenInventory));
	assert!(!e.ui.inv_screen.is_open());
	assert!(!e.ui.menu.is_open(), "a toggle is not a pause");
}

#[test]
fn escape_still_closes_first_and_pauses_second() {
	let mut e = game_engine();
	press(&mut e, key_of(Action::OpenInventory));
	press(&mut e, Key::Escape);
	assert!(!e.ui.inv_screen.is_open());
	assert!(!e.ui.menu.is_open(), "the first press only closes");
	press(&mut e, Key::Escape);
	assert!(e.ui.menu.is_open(), "the second pauses");
}

#[test]
fn a_rebound_page_key_moves_the_door() {
	let mut e = game_engine();
	let mut s = crate::settings::Settings::default();
	s.bindings.bind(Action::OpenInventory, Key::L);
	e.apply_settings(s);
	press(&mut e, Key::I);
	assert!(!e.ui.inv_screen.is_open(), "the old key is dead");
	press(&mut e, Key::L);
	assert!(e.ui.inv_screen.is_open(), "the new key opens");
	// The rebound key still hops page-to-page and toggles closed.
	press(&mut e, key_of(Action::OpenAlchemy));
	assert!(e.ui.alch_screen.is_open() && !e.ui.inv_screen.is_open());
	press(&mut e, Key::L);
	assert!(e.ui.inv_screen.is_open() && !e.ui.alch_screen.is_open());
	press(&mut e, Key::L);
	assert!(!e.ui.inv_screen.is_open());
}

#[test]
fn binding_an_arrow_shadows_its_movement_alias() {
	let mut e = game_engine();
	// Stock: Up is a walk-forward alias.
	e.input(InputEvent::Key {
		key: Key::Up,
		pressed: true,
	});
	assert!(e.input.keys.w);
	e.input(InputEvent::Key {
		key: Key::Up,
		pressed: false,
	});
	assert!(!e.input.keys.w);
	// Bound: the table answers first and the alias goes quiet.
	let mut s = crate::settings::Settings::default();
	s.bindings.bind(Action::OpenHiscores, Key::Up);
	e.apply_settings(s);
	e.input(InputEvent::Key {
		key: Key::Up,
		pressed: true,
	});
	assert!(!e.input.keys.w, "the binding shadows the alias");
	assert!(e.ui.hiscores.is_open());
}

#[test]
fn the_menu_roots_hotkeys_reach_the_pages() {
	let mut e = game_engine();
	press(&mut e, Key::Escape);
	assert!(e.ui.menu.is_open());
	press(&mut e, key_of(Action::OpenHiscores));
	assert!(!e.ui.menu.is_open(), "the root yields to the page");
	assert!(e.ui.hiscores.is_open());
	// The reach-through is the ROOT's: a page-up press from the
	// hiscores hops pages, never reopens the menu.
	press(&mut e, key_of(Action::OpenInventory));
	assert!(e.ui.inv_screen.is_open() && !e.ui.hiscores.is_open());
}
