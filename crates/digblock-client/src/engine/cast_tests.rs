//! The cast channel's contract: a channel takes exactly the ladder's
//! wind-up, an interrupt resets it outright, and the home cooldown the
//! completion starts is the caster's own rung — every expectation
//! DERIVED from `unlock`, never a tick count written down.

use super::*;
use digblock_core::skills::Skill;
use digblock_platform::{InputEvent, Key};

fn fresh() -> Engine {
	let mut config = EngineConfig::desktop();
	config.interactive = true;
	config.workers = 0;
	config.seed = Some(3.0);
	Engine::new(config)
}

fn press(e: &mut Engine, key: Key) {
	e.input(InputEvent::Key { key, pressed: true });
	e.input(InputEvent::Key {
		key,
		pressed: false,
	});
}

/// One engine step of exactly one sim tick, so the fixed-rate cast
/// stepping advances once per call.
fn tick(e: &mut Engine) {
	e.step(std::time::Duration::from_secs_f32(
		digblock_core::player::TICK,
	));
}

#[test]
fn a_completed_channel_takes_channel_ticks_and_starts_the_rung_cooldown() {
	let mut e = fresh();
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	press(&mut e, Key::H);
	tick(&mut e); // drains the request into a live channel
	assert!(e.cast.is_some(), "H did not open a channel");
	// One tick shy: still winding up, nothing fired.
	for _ in 0..need - 2 {
		tick(&mut e);
	}
	assert!(e.cast.is_some(), "the channel completed early");
	assert_eq!(e.save.spawn_cooldown, 0.0);
	tick(&mut e);
	assert!(e.cast.is_none(), "the channel never completed");
	// The completion started the caster's OWN cooldown rung.
	let want = digblock_core::unlock::return_cooldown_secs(e.skills.level(Skill::Arcana));
	assert_eq!(e.save.spawn_cooldown.ceil() as u32, want);
	// And a fresh request during the cooldown opens the PAGE (where the
	// countdown lives) instead of silently doing nothing.
	press(&mut e, Key::H);
	tick(&mut e);
	assert!(e.cast.is_none());
	assert!(
		e.ui.spellbook.is_open(),
		"a cooling cast must show the page"
	);
}

#[test]
fn an_interrupt_resets_the_channel() {
	let mut e = fresh();
	press(&mut e, Key::H);
	tick(&mut e);
	assert!(e.cast.is_some());
	// Movement is the interrupt: press-and-hold W for one tick.
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: true,
	});
	tick(&mut e);
	assert!(e.cast.is_none(), "a moving caster kept the channel");
	assert_eq!(
		e.save.spawn_cooldown, 0.0,
		"an interrupted channel must not charge the cooldown"
	);
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: false,
	});
	// A reset is a reset: the next channel starts from nothing and runs
	// its full length again.
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	press(&mut e, Key::H);
	tick(&mut e);
	for _ in 0..need - 2 {
		tick(&mut e);
	}
	assert!(e.cast.is_some(), "the second channel inherited progress");
}
