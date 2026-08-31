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
	press(&mut e, INVENTORY_KEY);
	assert!(e.ui.inv_screen.is_open());
	// Straight to the bench, no close in between.
	press(&mut e, ALCHEMY_KEY);
	assert!(!e.ui.inv_screen.is_open());
	assert!(e.ui.alch_screen.is_open());
	// And on around the ring.
	press(&mut e, Key::J);
	assert!(e.ui.spellbook.is_open() && !e.ui.alch_screen.is_open());
	press(&mut e, Key::C);
	assert!(e.ui.char_screen.is_open() && !e.ui.spellbook.is_open());
	press(&mut e, Key::H);
	assert!(e.ui.hiscores.is_open() && !e.ui.char_screen.is_open());
}

#[test]
fn a_pages_own_key_still_toggles_it_closed() {
	let mut e = game_engine();
	press(&mut e, INVENTORY_KEY);
	assert!(e.ui.inv_screen.is_open());
	press(&mut e, INVENTORY_KEY);
	assert!(!e.ui.inv_screen.is_open());
	assert!(!e.ui.menu.is_open(), "a toggle is not a pause");
}

#[test]
fn escape_still_closes_first_and_pauses_second() {
	let mut e = game_engine();
	press(&mut e, INVENTORY_KEY);
	press(&mut e, Key::Escape);
	assert!(!e.ui.inv_screen.is_open());
	assert!(!e.ui.menu.is_open(), "the first press only closes");
	press(&mut e, Key::Escape);
	assert!(e.ui.menu.is_open(), "the second pauses");
}

#[test]
fn the_menu_roots_hotkeys_reach_the_pages() {
	let mut e = game_engine();
	press(&mut e, Key::Escape);
	assert!(e.ui.menu.is_open());
	press(&mut e, Key::H);
	assert!(!e.ui.menu.is_open(), "the root yields to the page");
	assert!(e.ui.hiscores.is_open());
	// The reach-through is the ROOT's: a page-up press from the
	// hiscores hops pages, never reopens the menu.
	press(&mut e, INVENTORY_KEY);
	assert!(e.ui.inv_screen.is_open() && !e.ui.hiscores.is_open());
}
