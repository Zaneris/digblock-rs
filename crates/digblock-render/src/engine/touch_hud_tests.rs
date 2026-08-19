
use super::*;
use crate::ui::hud;

const SIZE: (u32, u32) = (1280, 720);

/// An in-game engine with the touch HUD live, sized so normalized
/// touch coordinates and the HUD's pixel rects agree.
fn touch_engine() -> Engine {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	config.menu = true;
	config.touch_hud = true;
	let mut engine = Engine::new(config);
	engine.size = SIZE;
	engine.ui_scale = 1.0;
	// The title menu starts open; enter the world the way play does.
	engine.menu.set_connected(true);
	engine.input(InputEvent::Key {
		key: Key::Escape,
		pressed: true,
	});
	assert!(!engine.menu.is_open(), "in-game to start");
	engine
}

fn touch_at(engine: &mut Engine, id: u64, phase: TouchPhase, px: f32, py: f32) {
	engine.input(InputEvent::Touch {
		id,
		phase,
		x: px / SIZE.0 as f32,
		y: py / SIZE.1 as f32,
	});
}

fn center(r: crate::ui::Rect) -> (f32, f32) {
	(r.x + r.w / 2.0, r.y + r.h / 2.0)
}

#[test]
fn corner_tap_opens_the_menu_and_closes_a_screen() {
	let mut engine = touch_engine();
	let (cx, cy) = center(hud::corner_button_rect(SIZE, 1.0, &engine.safe_area));
	touch_at(&mut engine, 1, TouchPhase::Started, cx, cy);
	assert!(engine.menu.is_open(), "corner = pause");
	// The claimed touch is consumed to its end even though the menu now
	// owns the pointer path.
	touch_at(&mut engine, 1, TouchPhase::Ended, cx, cy);
	assert!(engine.hud_touches.is_empty(), "claim released");

	// Back in-game, with a screen up: the same button is its close.
	engine.input(InputEvent::Key {
		key: Key::Escape,
		pressed: true,
	});
	assert!(!engine.menu.is_open());
	engine.input(InputEvent::Key {
		key: Key::I,
		pressed: true,
	});
	assert!(engine.inv_screen.is_open());
	touch_at(&mut engine, 2, TouchPhase::Started, cx, cy);
	assert!(!engine.inv_screen.is_open(), "corner = close");
	assert!(!engine.menu.is_open(), "close, not pause");
	touch_at(&mut engine, 2, TouchPhase::Ended, cx, cy);
}

#[test]
fn place_tap_is_an_edge_and_never_becomes_look() {
	let mut engine = touch_engine();
	let (px, py) = center(hud::place_button_rect(SIZE, 1.0, &engine.safe_area));
	touch_at(&mut engine, 1, TouchPhase::Started, px, py);
	assert!(engine.click_place, "one place per tap");
	// A drag across the whole screen with the claimed finger is not a
	// camera move.
	touch_at(&mut engine, 1, TouchPhase::Moved, px + 300.0, py);
	assert_eq!(engine.touch.take_look(), glam::Vec2::ZERO);
	touch_at(&mut engine, 1, TouchPhase::Ended, px + 300.0, py);
	assert!(engine.hud_touches.is_empty());
}

#[test]
fn the_jump_button_only_lives_in_button_mode() {
	let mut engine = touch_engine();
	let (jx, jy) = center(hud::jump_button_rect(SIZE, 1.0, &engine.safe_area));
	// Double-tap mode: no button there — the touch is an ordinary
	// look-side candidate, and no jump intent is held.
	touch_at(&mut engine, 1, TouchPhase::Started, jx, jy);
	assert!(!engine.hud_jump_held);
	assert!(engine.hud_touches.is_empty());
	touch_at(&mut engine, 1, TouchPhase::Ended, jx, jy);

	engine.config.touch_jump_button = true;
	touch_at(&mut engine, 2, TouchPhase::Started, jx, jy);
	assert!(engine.hud_jump_held, "held while the finger is down");
	// A second finger still looks while the button is held.
	touch_at(&mut engine, 3, TouchPhase::Started, 900.0, 200.0);
	touch_at(&mut engine, 3, TouchPhase::Moved, 1000.0, 200.0);
	touch_at(&mut engine, 3, TouchPhase::Moved, 1100.0, 200.0);
	assert!(engine.touch.take_look().x > 0.0);
	touch_at(&mut engine, 2, TouchPhase::Ended, jx, jy);
	assert!(!engine.hud_jump_held);
}

#[test]
fn a_bar_slot_tap_selects_it() {
	let mut engine = touch_engine();
	let grid = hud::row_grid(SIZE, 1.0);
	let (sx, sy) = center(hud::bar_slot_rect(&grid, 2, 1.0));
	touch_at(&mut engine, 1, TouchPhase::Started, sx, sy);
	assert_eq!(engine.hotbar.selected_index(), 2);
	touch_at(&mut engine, 1, TouchPhase::Ended, sx, sy);

	// The tool slot mirrors tap-9: selecting it, then tapping it again,
	// cycles the owned roster (a no-op roster keeps the slot selected).
	let tool = digblock_core::hotbar::HOTBAR_SLOTS - 1;
	let (tx, ty) = center(hud::bar_slot_rect(&grid, tool, 1.0));
	touch_at(&mut engine, 2, TouchPhase::Started, tx, ty);
	touch_at(&mut engine, 2, TouchPhase::Ended, tx, ty);
	touch_at(&mut engine, 3, TouchPhase::Started, tx, ty);
	touch_at(&mut engine, 3, TouchPhase::Ended, tx, ty);
	assert_eq!(engine.hotbar.selected_index(), tool);
}
