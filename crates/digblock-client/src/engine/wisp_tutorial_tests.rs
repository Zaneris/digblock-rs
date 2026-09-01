//! The wisp chapter through the engine: the affordability nudge arms it
//! only inside the night window (21:00-03:00 on the painted sun), the
//! book, the bar and the burning wisp advance it off the engine's own
//! poll and screen events, and a skip is final for the session.

use super::*;

/// An in-game engine holding a wisp's fare, its sky set to `(h, m)`.
fn engine_at(h: u32, m: u32) -> Engine {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	config.menu = true;
	let mut engine = Engine::new(config);
	engine.ui.menu.set_connected(true);
	engine.input(InputEvent::Key {
		key: Key::Escape,
		pressed: true,
	});
	assert!(!engine.ui.menu.is_open(), "in-game to start");
	let (essence, fare) = digblock_core::alchemy::wisp_price(1);
	engine.inventory.add_sub(essence, fare);
	engine.sun.tod = digblock_core::sun::tod_of_clock(h, m);
	engine
}

/// The nudge fires only inside the night window, and not for a player
/// whose bar already holds the wisp.
#[test]
fn the_nudge_respects_the_night_window_and_a_known_bar() {
	for (h, want) in [(12, false), (20, false), (22, true), (2, true), (3, false)] {
		let mut engine = engine_at(h, 0);
		engine.update(Duration::from_millis(16));
		assert_eq!(engine.tutorial.is_running(), want, "{h}:00");
	}
	// The wisp already slotted: the player knows the bar; no chapter.
	let mut engine = engine_at(22, 0);
	engine.spell_bar.assign(0, crate::spellbar::SLOT_WISP);
	engine.update(Duration::from_millis(16));
	assert!(!engine.tutorial.is_running(), "a slotted wisp arms nothing");
}

#[test]
fn the_nudge_arms_the_chapter_and_the_night_walks_it() {
	use crate::tutorial::StepId as S;
	let mut engine = engine_at(22, 0);
	engine.update(Duration::from_millis(16));
	let step = |e: &Engine| e.tutorial.view(false).expect("running").step;
	assert_eq!(step(&engine), S::OpenArcana);
	engine.ui.spellbook.open();
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::SlotWisp);
	// The wisp lands on the bar (drag, save adoption, however): the
	// engine's own poll advances the step.
	engine.spell_bar.assign(1, crate::spellbar::SLOT_WISP);
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::CloseArcana);
	// The close arrives as the frame's screen-delta event.
	engine.ui.spellbook.close();
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::CastWisp);
	// The cast completes: the wisp burns, the chapter ends.
	engine.light_wisp();
	engine.update(Duration::from_millis(16));
	assert!(!engine.tutorial.is_running(), "chapter finished");
}

/// A skip is final: the nudge never fires twice in one session, so the
/// next night frame must not re-arm a skipped chapter.
#[test]
fn a_skip_stays_skipped_for_the_session() {
	let mut engine = engine_at(22, 0);
	engine.update(Duration::from_millis(16));
	assert!(engine.tutorial.is_running());
	if let Some(adv) = engine.tutorial.skip() {
		engine.on_tutorial_advance(adv);
	}
	engine.update(Duration::from_millis(16));
	assert!(!engine.tutorial.is_running(), "skip is final");
}
