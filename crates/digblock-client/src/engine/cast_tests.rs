//! The cast channel's contract: a channel takes exactly the ladder's
//! wind-up, an interrupt resets it outright, and the teleport cooldown
//! the completion starts is the caster's own rung and covers EVERY
//! teleport — every expectation DERIVED from `unlock`, never a tick
//! count written down.

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
	assert_eq!(e.save.teleport_cooldown, 0.0);
	tick(&mut e);
	assert!(e.cast.is_none(), "the channel never completed");
	// The completion started the caster's OWN cooldown rung.
	let want = digblock_core::unlock::teleport_cooldown_secs(e.skills.level(Skill::Arcana));
	assert_eq!(e.save.teleport_cooldown.ceil() as u32, want);
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

/// The teleport's light is engine state on confetti's rules: a completed
/// jump throws its two bursts, and both expire within the shader's own
/// LIFE — an effect the shader has faded to zero must not stay uploaded.
#[test]
fn a_jump_throws_bursts_that_expire_within_their_life() {
	let mut e = fresh();
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	press(&mut e, Key::H);
	for _ in 0..=need {
		tick(&mut e);
	}
	assert!(e.cast.is_none(), "the channel should have completed");
	assert_eq!(e.vfx.casts.len(), 2, "a jump is a departure AND an arrival");
	assert!(e.vfx.flash > 0.0, "the jump flashes");
	let ticks = (crate::teleport::LIFE / digblock_core::player::TICK) as u32 + 2;
	for _ in 0..ticks {
		tick(&mut e);
	}
	assert!(e.vfx.casts.is_empty(), "bursts outlived the shader's fade");
	assert_eq!(e.vfx.flash, 0.0, "the flash outlived the jump");
}

/// The Ward climbs THROUGH its channel and a break unwinds it: halfway
/// through, part of the shell is glass and the fare is gone; one step
/// puts every pane back and the fare with it; a channel left to finish
/// seals the whole shell, washes the overlay and keeps the fare.
#[test]
fn a_broken_ward_channel_unwinds_and_a_finished_one_seals() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::{cell_id, make_cell};
	use digblock_core::skills::total_xp_for;
	let mut e = fresh();
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = e.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
	let level = digblock_core::unlock::WARD_AT;
	e.skills.add_xp(Skill::Arcana, total_xp_for(level));
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	e.inventory.add_sub(essence, fare);
	let r = digblock_core::unlock::ward_radius(level);
	let head = digblock_core::kindle::host_cell(e.player.pos);
	let (shell, _) = digblock_core::ward::sets(head, r);
	let glass = make_cell(BlockType::Wardglass, 0);
	let air_cells: Vec<_> = shell
		.iter()
		.copied()
		.filter(|&(x, y, z)| {
			e.pipeline.cell_at(x, y, z).map(cell_id) == Some(BlockType::Air as u16)
		})
		.collect();
	assert!(
		air_cells.len() > 10,
		"the shell about spawn must be mostly air, or the sweep proves nothing"
	);
	let glassed = |e: &Engine| {
		air_cells
			.iter()
			.filter(|&&(x, y, z)| e.pipeline.cell_at(x, y, z) == Some(glass))
			.count()
	};
	let need = digblock_core::unlock::channel_ticks(level);

	e.start_cast(CastTarget::Ward);
	assert!(e.cast.is_some(), "the channel opened");
	assert_eq!(
		e.inventory.sub_count(essence),
		0,
		"the fare is taken as the channel begins"
	);
	for _ in 0..need / 2 {
		tick(&mut e);
	}
	let half = glassed(&e);
	assert!(
		half > 0 && half < air_cells.len(),
		"halfway through, the glass is climbing"
	);
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: true,
	});
	tick(&mut e);
	assert!(e.cast.is_none(), "the step broke the channel");
	assert_eq!(glassed(&e), 0, "every pane came back down");
	assert_eq!(
		e.inventory.sub_count(essence),
		fare,
		"and the fare came back"
	);
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: false,
	});

	e.start_cast(CastTarget::Ward);
	assert!(e.cast.is_some());
	for _ in 0..=need {
		tick(&mut e);
	}
	assert!(e.cast.is_none(), "the channel completed");
	assert_eq!(glassed(&e), air_cells.len(), "the shell sealed whole");
	assert!(e.vfx.flash > 0.0, "the seal washes the overlay");
	assert_eq!(e.inventory.sub_count(essence), 0, "the fare stays spent");
	assert!(
		e.skills.xp(Skill::Arcana) > total_xp_for(level),
		"the seal paid XP"
	);
}

/// An interrupted channel FADES rather than pops: the aura's envelope is
/// still visible the tick after the break, and then runs monotonically
/// to zero.
#[test]
fn an_interrupted_channel_fades_rather_than_pops() {
	let mut e = fresh();
	press(&mut e, Key::H);
	// Long enough for the envelope to open fully.
	let open = (crate::teleport::FADE_IN_SECS / digblock_core::player::TICK) as u32 + 2;
	for _ in 0..open {
		tick(&mut e);
	}
	assert_eq!(e.vfx.channel_env, 1.0, "the aura never opened");
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: true,
	});
	tick(&mut e);
	assert!(e.cast.is_none(), "the interrupt did not land");
	let mut prev = e.vfx.channel_env;
	assert!(prev > 0.0, "the aura popped instead of fading");
	let mut steps = 0u32;
	while e.vfx.channel_env > 0.0 {
		tick(&mut e);
		assert!(e.vfx.channel_env <= prev, "the fade went back up");
		prev = e.vfx.channel_env;
		steps += 1;
		assert!(steps < 600, "the fade never reaches zero");
	}
	assert!(steps > 1, "the fade was a pop with extra steps");
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
		e.save.teleport_cooldown, 0.0,
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

/// One clock, every teleport: a port cast arms the same cooldown Return
/// Home does, and neither can start while it runs. Swept over both
/// directions, because a per-spell timer would leave exactly one of them
/// open and the bug is invisible from the other side.
#[test]
fn one_cooldown_covers_home_and_every_port() {
	// Both orders: the spell that arms the clock, then the spell that
	// must find it armed.
	for (first, second) in [
		(CastTarget::Home, CastTarget::Waystone(0)),
		(CastTarget::Waystone(0), CastTarget::Home),
	] {
		let mut e = fresh();
		let level = e.skills.level(Skill::Arcana);
		let need = digblock_core::unlock::channel_ticks(level);
		e.start_cast(first);
		for _ in 0..=need {
			tick(&mut e);
		}
		assert!(e.cast.is_none(), "{first:?} never completed");
		assert_eq!(
			e.save.teleport_cooldown.ceil() as u32,
			digblock_core::unlock::teleport_cooldown_secs(level),
			"{first:?} did not arm the shared clock"
		);
		e.start_cast(second);
		assert!(
			e.cast.is_none(),
			"{second:?} started inside {first:?}'s cooldown"
		);
		assert!(
			e.ui.spellbook.is_open(),
			"a cooling {second:?} must show the page"
		);
	}
}

/// The conjuring spells are NOT teleports and never touch the clock —
/// the predicate that decides is asked, not restated here.
#[test]
fn a_light_spell_neither_waits_on_the_clock_nor_arms_it() {
	let mut e = fresh();
	// The Ward validates as its channel BEGINS, so it needs its rung,
	// its fare and a resident sphere to open at all.
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = e.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
	let level = digblock_core::unlock::WARD_AT;
	e.skills
		.add_xp(Skill::Arcana, digblock_core::skills::total_xp_for(level));
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	e.inventory.add_sub(essence, fare);
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	for target in [CastTarget::Wisp, CastTarget::Kindle, CastTarget::Ward] {
		assert!(!target.teleports(), "{target:?} must not ride the clock");
		// Mid-window: long enough that the channel cannot run it out, so
		// "still running" is what the cast happened UNDER.
		let armed = digblock_core::unlock::teleport_cooldown_secs(e.skills.level(Skill::Arcana));
		let before = armed as f32 / 2.0;
		e.save.teleport_cooldown = before;
		e.start_cast(target);
		assert!(e.cast.is_some(), "{target:?} was blocked by the clock");
		for _ in 0..=need {
			tick(&mut e);
		}
		let left = e.save.teleport_cooldown;
		assert!(left > 0.0, "the window ran out — the sweep proved nothing");
		assert!(
			left < before,
			"{target:?} left the clock standing instead of letting it run"
		);
	}
}

/// The offline kindle: the light stands in the cell the caster's head is
/// in, not where they aim; it burns for exactly `kindle::SECS` on its own
/// clock; and moving the caster — a port — leaves it where it was lit.
/// The span, the rung and the fare are all read off the tables.
#[test]
fn a_kindled_light_stands_at_the_head_on_its_own_clock() {
	use digblock_core::skills::total_xp_for;
	let mut e = fresh();
	// Settle the terrain, or the head cell is unloaded and hosts nothing.
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = e.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
	let level = digblock_core::unlock::KINDLE_AT;
	e.skills.add_xp(Skill::Arcana, total_xp_for(level));
	let (essence, fare) = digblock_core::alchemy::kindle_price(level);
	e.inventory.add_sub(essence, fare);
	// Look straight down: an aimed kindle would land on the floor cell.
	e.camera.pitch_deg = -80.0;
	let head = digblock_core::kindle::host_cell(e.player.pos);
	assert!(
		digblock_core::kindle::hosts(e.pipeline.cell_at(head.0, head.1, head.2)),
		"the spawn head cell must host, or the sweep proves nothing"
	);
	e.start_cast(CastTarget::Kindle);
	let need = digblock_core::unlock::channel_ticks(level);
	for _ in 0..=need {
		tick(&mut e);
	}
	assert_eq!(e.vfx.kindled.len(), 1, "one light stands");
	assert_eq!(e.vfx.kindled[0].cell, head, "in the head's own cell");
	assert_eq!(e.inventory.sub_count(essence), 0, "the fare left");
	assert!(
		e.skills.xp(Skill::Arcana) > total_xp_for(level),
		"the cast paid its XP"
	);
	// A port: the light does not follow.
	e.player.pos += Vec3::new(40.0, 0.0, 40.0);
	e.step(std::time::Duration::from_secs_f32(
		digblock_core::kindle::SECS as f32 - 1.0,
	));
	assert_eq!(
		e.vfx.kindled.len(),
		1,
		"still burning a second short of its span"
	);
	assert_eq!(e.vfx.kindled[0].cell, head, "and still where it was lit");
	e.step(std::time::Duration::from_secs_f32(1.5));
	assert!(e.vfx.kindled.is_empty(), "and out once the span has run");
}
