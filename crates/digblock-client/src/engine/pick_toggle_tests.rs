//! The pick-orientation toggle: the cycle walks exactly the shapes the
//! Mining level has opened, the indicator's tap is the bind's touch
//! twin, and the default reproduces the pre-toggle shapes.

use super::*;
use crate::ui::hud;
use digblock_core::pick::DigOrientation as O;
use digblock_core::skills::Skill;

const SIZE: (u32, u32) = (1280, 720);

/// An in-game engine holding the Resonant Pick at `mining` level, with
/// the touch HUD live so the indicator's tap path can be driven.
fn pick_engine(mining: u32) -> Engine {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	config.menu = true;
	config.touch_hud = true;
	let mut engine = Engine::new(config);
	engine.ui.size = SIZE;
	engine.ui.ui_scale = 1.0;
	engine.ui.menu.set_connected(true);
	engine.input(InputEvent::Key {
		key: Key::Escape,
		pressed: true,
	});
	assert!(!engine.ui.menu.is_open(), "in-game to start");
	engine
		.skills
		.set_total(Skill::Mining, digblock_core::skills::total_xp_for(mining));
	// The pick in hand, reached the way a player reaches it: owned,
	// tool slot selected, roster cycled onto it.
	engine.inventory.add(BlockType::ResonantPick, 1);
	engine
		.hotbar
		.select_digit(digblock_core::hotbar::HOTBAR_SLOTS as u8);
	let owned = engine.owned_tools();
	for _ in 0..owned.len() {
		if engine.hotbar.active_tool() == Some(BlockType::ResonantPick) {
			break;
		}
		engine.hotbar.cycle_tool(&owned);
	}
	assert_eq!(engine.hotbar.tool_in_hand(), BlockType::ResonantPick);
	engine
}

#[test]
fn the_cycle_walks_what_the_level_opened() {
	// Row width: two states, and the default (Plane) reads as the
	// shipped horizontal row before the first press.
	let mut e = pick_engine(digblock_core::unlock::PICK_WIDE_AT);
	assert_eq!(e.dig_orientation.clamped(3), O::Horizontal);
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, O::Vertical);
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, O::Horizontal);

	// Plane width: all three states, wrapping through the plane.
	let mut e = pick_engine(digblock_core::unlock::PICK_PLANE_AT);
	assert_eq!(e.dig_orientation, O::Plane, "the default IS the plane here");
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, O::Horizontal);
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, O::Vertical);
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, O::Plane);
}

#[test]
fn the_toggle_noops_where_the_indicator_hides() {
	// Below the first milestone the swing is one cell: nothing to turn.
	let mut e = pick_engine(1);
	assert!(!e.pick_toggle_visible());
	let before = e.dig_orientation;
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, before);

	// The pick stowed: a block slot is an empty hand.
	let mut e = pick_engine(digblock_core::unlock::PICK_WIDE_AT);
	e.hotbar.select_digit(1);
	assert!(!e.pick_toggle_visible());
	e.cycle_dig_orientation();
	assert_eq!(e.dig_orientation, O::Plane, "unchanged from the default");
}

#[test]
fn tapping_the_indicator_cycles_like_the_bind() {
	let mut e = pick_engine(digblock_core::unlock::PICK_WIDE_AT);
	assert!(e.pick_toggle_visible());
	let r = hud::pick_toggle_rect(SIZE, 1.0, &e.input.safe_area, true);
	let (px, py) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
	e.input(InputEvent::Touch {
		id: 9,
		phase: TouchPhase::Started,
		x: px / SIZE.0 as f32,
		y: py / SIZE.1 as f32,
	});
	assert_eq!(e.dig_orientation, O::Vertical, "one tap, one cycle");
	e.input(InputEvent::Touch {
		id: 9,
		phase: TouchPhase::Ended,
		x: px / SIZE.0 as f32,
		y: py / SIZE.1 as f32,
	});

	// With the pick stowed the rect is dead: the same tap is gameplay.
	e.hotbar.select_digit(1);
	e.input(InputEvent::Touch {
		id: 10,
		phase: TouchPhase::Started,
		x: px / SIZE.0 as f32,
		y: py / SIZE.1 as f32,
	});
	assert_eq!(e.dig_orientation, O::Vertical, "a hidden button cannot tap");
}
