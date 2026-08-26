use super::craft_gate_tests::block_ahead;
use super::*;
use digblock_core::skills::DIG_DECAY_TICKS;

/// Aimed at a diggable block with the button down, stepped `n` times.
fn hold(engine: &mut Engine, n: u32) {
	engine.input.dig_held = true;
	for _ in 0..n {
		engine.update_target();
		engine.step_dig();
	}
}

/// Button up, stepped `n` times — the decay's only clock.
fn release(engine: &mut Engine, n: u32) {
	engine.input.dig_held = false;
	for _ in 0..n {
		engine.update_target();
		engine.step_dig();
	}
}

/// The crack's own number, as `dig_progress` hands it to the
/// highlight instance.
fn web(engine: &Engine) -> Option<f32> {
	engine.dig_progress().and_then(|t| t.progress)
}

/// Letting go part-way no longer bins the hold: it bleeds back to
/// untouched over a second, the crack retreats with it (same
/// number, one source), and picking the button back up resumes from
/// what is left instead of from zero.
#[test]
fn releasing_a_hold_bleeds_it_out_instead_of_binning_it() {
	let (mut engine, cell) = block_ahead(BlockType::Dirt);
	engine.update_target();
	assert_eq!(engine.target.map(|h| h.cell), Some(cell), "aimed at it");

	// Open the hold (the opening step banks nothing), then bank half
	// of what the block needs.
	hold(&mut engine, 1);
	let need = engine.dig.as_ref().expect("hold opened").required;
	assert!(need >= 8, "dirt should be a real hold, not {need} ticks");
	hold(&mut engine, u32::from(need) / 2);
	let peak = engine.dig.as_ref().unwrap().ticks;
	assert_eq!(peak, need / 2, "banked what was held");
	let full = web(&engine).expect("the web is drawn while digging");

	// Release: the record survives, and both the banked ticks and
	// the web fall together. (Several ticks, not one — the decay
	// truncates down, so a short enough block loses nothing on the
	// very first.)
	release(&mut engine, 10);
	let d = engine.dig.as_ref().expect("the release kept the hold");
	assert_eq!(d.ticks, peak, "the peak is not rewritten");
	assert_eq!(d.idle, 10, "the decay started");
	let after = web(&engine).expect("the web is still drawn");
	assert!(after < full, "the web should retreat: {after} vs {full}");
	assert!(
		(after - f32::from(d.banked()) / f32::from(need)).abs() < 1e-6,
		"the web draws the decayed number, not a second one"
	);

	// A full window of release erases it — record and web together.
	release(&mut engine, DIG_DECAY_TICKS);
	assert!(engine.dig.is_none(), "a spent hold is dropped");
	assert_eq!(web(&engine), None, "and the block outlines unbroken");
}

/// A siphon draw ENDS the press that made it: the cell now holds the tier's spent rock, and
/// quarrying that is a second action — one the same press would roll straight into, destroying
/// the trophy on the hold that earned it. The latch names ONE cell, so a seam next to it still
/// draws without letting go, and only the button coming up re-arms the drawn one.
#[test]
fn a_drawn_cell_needs_a_fresh_press_before_it_breaks() {
	let (mut engine, cell) = block_ahead(BlockType::Dirt);
	engine.update_target();
	assert_eq!(engine.target.map(|h| h.cell), Some(cell), "aimed at it");

	// What `complete_dig` leaves behind after a draw.
	engine.drawn_cell = Some(cell);
	hold(&mut engine, 4);
	assert!(engine.dig.is_none(), "the same press cannot break it");
	assert_eq!(web(&engine), None, "and nothing cracks");

	// Aiming away and back is NOT a release — the latch is about the
	// press, not the aim.
	engine.input.dig_held = false;
	engine.drawn_cell = Some(cell);
	hold(&mut engine, 1);
	assert!(engine.dig.is_none(), "still latched");

	// The button coming up re-arms it, and the next press digs.
	release(&mut engine, 1);
	assert_eq!(engine.drawn_cell, None, "the release re-arms the cell");
	hold(&mut engine, 2);
	assert!(
		engine.dig.is_some(),
		"a fresh press opens a hold on the spent rock"
	);
}

/// The conditional rock rule, client half: a stone standing in a cell somebody WROTE comes
/// back whole to a mason and crumbles for anyone who could not have cut it (SKILLS-DESIGN
/// §7.1/§7.5). Prediction has to agree with the server or a quarry silently drifts the two
/// inventories apart — there is no rebase behind an ordinary dig. `block_ahead` leaves an
/// EDITED cell, the case under test; the pristine half is `only_the_worlds_own_rock_crumbles`.
#[test]
fn the_client_predicts_a_placed_stone_coming_back_whole() {
	use digblock_core::skills::{Skill, total_xp_for};
	let mason = digblock_core::unlock::craft(BlockType::Stone).expect("clean stone is made");
	for (alchemy, want) in [(mason, BlockType::Stone), (mason - 1, BlockType::Rubble)] {
		let (mut engine, cell) = block_ahead(BlockType::Stone);
		engine.skills.set_total(Skill::Mining, total_xp_for(99));
		engine
			.skills
			.set_total(Skill::Alchemy, total_xp_for(alchemy));
		engine.update_target();
		assert_eq!(engine.target.map(|h| h.cell), Some(cell), "aimed at it");
		assert!(
			engine.pipeline.cell_edited(cell.0, cell.1, cell.2),
			"the harness stands the block through an edit"
		);
		hold(&mut engine, 1);
		let need = engine.dig.as_ref().expect("hold opened").required;
		hold(&mut engine, u32::from(need) + 1);
		assert_eq!(
			engine.pipeline.block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Air),
			"the hold completed at Alchemy {alchemy}"
		);
		assert_eq!(engine.inventory.count(want), 1, "Alchemy {alchemy}");
		let other = if want == BlockType::Stone {
			BlockType::Rubble
		} else {
			BlockType::Stone
		};
		assert_eq!(engine.inventory.count(other), 0, "Alchemy {alchemy}");
	}
}

/// A capture ignores the keyboard AND leaves the mouse alone — see the
/// `interactive` field's own doc for why the second half is the one that
/// hurt, and why `bench` alone does not cover it.
#[test]
fn a_capture_takes_neither_the_keyboard_nor_the_pointer() {
	use digblock_platform::{InputEvent, Key};

	let fresh = |interactive: bool| {
		let mut config = EngineConfig::desktop();
		config.interactive = interactive;
		config.workers = 0;
		config.seed = Some(3.0);
		Engine::new(config)
	};
	let any_screen_open = |e: &Engine| {
		e.ui.menu.is_open()
			|| e.ui.inv_screen.is_open()
			|| e.ui.alch_screen.is_open()
			|| e.ui.char_screen.is_open()
			|| e.ui.hiscores.is_open()
			|| e.ui.spellbook.is_open()
	};
	// One key per engine. Pressing all four in sequence cancels
	// itself — `I` opens the inventory, which then swallows `K` and
	// `C` as drag input and closes on `Escape` — so a combined
	// sweep would end with nothing open and "a capture opened
	// nothing" would pass for entirely the wrong reason.
	let opens = |interactive: bool, key: Key| {
		let mut e = fresh(interactive);
		e.input(InputEvent::Key { key, pressed: true });
		e.input(InputEvent::Key {
			key,
			pressed: false,
		});
		any_screen_open(&e)
	};
	// `look_accum`, not the camera: a mouse delta is banked here and
	// spent by `update`, so reading the camera straight after
	// `input` would answer "unmoved" on an interactive run too.
	let turns = |interactive: bool| {
		let mut e = fresh(interactive);
		e.input(InputEvent::MouseDelta {
			dx: 240.0,
			dy: 90.0,
		});
		e.input.look_accum != (0.0, 0.0)
	};

	// The keys that were genuinely reachable. `Escape` is asserted
	// on the capture side too, but it is NOT a control: `Menu::open`
	// checks `enabled`, which a capture never sets, so Escape was
	// already inert there and a control on it would fail for a
	// reason that has nothing to do with this flag.
	const SCREEN_KEYS: [Key; 5] = [Key::I, Key::K, Key::C, Key::J, Key::H];

	// A capture is deaf and never asks for the pointer.
	assert!(
		!fresh(false).wants_cursor_grab(),
		"a capture must never take the operator's cursor"
	);
	for key in SCREEN_KEYS.into_iter().chain([Key::Escape]) {
		assert!(!opens(false, key), "a capture opened a screen on {key:?}");
	}
	assert!(!turns(false), "a capture banked a mouse delta to turn with");

	// The controls: every one of those events DOES land on a real
	// session, or the assertions above prove nothing at all.
	assert!(fresh(true).wants_cursor_grab(), "a session grabs to play");
	for key in SCREEN_KEYS {
		assert!(opens(true, key), "{key:?} does not open anything anyway");
	}
	assert!(turns(true), "MouseDelta does not turn the camera anyway");
}

/// Half a second off the button costs half a BAR, not half of what
/// was banked — which is what makes one second the ceiling for every
/// block rather than a flat wait — and the resumed hold completes in
/// what is still owed.
#[test]
fn a_resumed_hold_keeps_what_the_decay_left() {
	let (mut engine, cell) = block_ahead(BlockType::Dirt);
	engine.update_target();
	hold(&mut engine, 1);
	let need = engine.dig.as_ref().expect("hold opened").required;
	hold(&mut engine, u32::from(need) - 1);
	assert_eq!(
		engine.dig.as_ref().unwrap().ticks,
		need - 1,
		"one tick from breaking"
	);

	let idle = DIG_DECAY_TICKS / 2;
	release(&mut engine, idle);
	let left = engine.dig.as_ref().expect("still held").banked();
	assert_eq!(
		u32::from(left),
		digblock_core::skills::dig_decayed(u32::from(need - 1), u32::from(need), idle),
		"the shared rule, not a second implementation"
	);
	assert!(left > 0 && left < need / 2 + 1, "about half a bar gone");

	// Resuming inherits it: the opening step banks nothing, so the
	// block breaks exactly `need - left` steps later.
	hold(&mut engine, 1);
	assert_eq!(
		engine.dig.as_ref().unwrap().ticks,
		left,
		"picked up at {left}"
	);
	hold(&mut engine, u32::from(need - left) - 1);
	assert_eq!(
		engine.pipeline.block_at(cell.0, cell.1, cell.2),
		Some(BlockType::Dirt),
		"the resumed hold still owed a tick"
	);
	hold(&mut engine, 1);
	assert_eq!(
		engine.pipeline.block_at(cell.0, cell.1, cell.2),
		Some(BlockType::Air),
		"the resumed hold broke it"
	);
}

/// Aiming at a DIFFERENT block is not a release — the old hold is
/// gone outright, because only one cell can be decaying and it must
/// be the one under the crosshair.
#[test]
fn retargeting_drops_the_old_hold_outright() {
	let (mut engine, cell) = block_ahead(BlockType::Dirt);
	engine.update_target();
	hold(&mut engine, 10);
	assert!(engine.dig.as_ref().unwrap().ticks > 0, "banked something");

	// Put a second block in front of the first, so the same ray now
	// stops one cell short: a fresh hold at zero.
	let other = (cell.0, cell.1, cell.2 - 1);
	engine
		.pipeline
		.set_block(other.0, other.1, other.2, BlockType::Dirt)
		.expect("chunk ready");
	hold(&mut engine, 1);
	let d = engine.dig.as_ref().expect("the new hold opened");
	assert_eq!(d.cell, other, "on the other block");
	assert_eq!((d.ticks, d.idle), (0, 0), "from scratch");
}
