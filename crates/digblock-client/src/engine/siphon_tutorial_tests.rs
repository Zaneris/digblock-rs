//! The siphon chapter through the engine: the affordability nudge arms
//! it (interactive, server or offline), owning a siphon advances the craft step
//! off the engine's own poll, and the siphon actually in hand finishes
//! the chapter. The cinematic itself is camrig's business; here only
//! the state machine and the wiring around it are on trial.

use super::*;
use digblock_core::skills::{Skill, total_xp_for};

/// A minimal save for `enter_local_world`: `skills` is the variable.
fn save_with_skills(
	seed: digblock_core::worldgen::Seed,
	skills: Vec<(u8, u64)>,
) -> digblock_persist::WorldSave {
	digblock_persist::WorldSave {
		meta: digblock_persist::WorldMeta {
			id: "w".into(),
			name: "w".into(),
			seed,
			created_ms: 0,
			last_played_ms: 0,
			compatible: true,
		},
		rev: digblock_persist::SAVE_REV,
		edits: Vec::new(),
		inventory: Vec::new(),
		hotbar: Vec::new(),
		grid: Vec::new(),
		skills,
		spells: Vec::new(),
		player: None,
	}
}

/// The walkthrough arms for every 0-XP character and for no one else:
/// entering with any XP at all leaves it dark, entering fresh lights it,
/// and the settings row switched off overrules even a fresh character.
#[test]
fn the_walkthrough_arms_only_for_a_fresh_character() {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	let seed = engine.seed;
	engine.enter_local_world(save_with_skills(seed, Vec::new()));
	assert!(
		engine.tutorial.is_running(),
		"fresh character = walkthrough"
	);
	engine.enter_local_world(save_with_skills(seed, vec![(Skill::Mining as u8, 5)]));
	assert!(!engine.tutorial.is_running(), "any XP retires it");
	engine.config.show_tutorial = false;
	engine.enter_local_world(save_with_skills(seed, Vec::new()));
	assert!(!engine.tutorial.is_running(), "the row is a manual off");
}

/// An in-game engine whose pack can afford a siphon.
fn affordable_engine() -> Engine {
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
	engine.skills.set_total(Skill::Alchemy, total_xp_for(99));
	for (b, n) in digblock_core::alchemy::craft_feedstock(BlockType::EssenceSiphon) {
		engine.inventory.add(b, n);
	}
	engine
}

#[test]
fn the_nudge_arms_the_chapter_and_the_pack_walks_it() {
	use crate::tutorial::StepId as S;
	let mut engine = affordable_engine();
	assert!(!engine.tutorial.is_running());
	engine.nudge_siphon();
	let step = |e: &Engine| e.tutorial.view(false).expect("running").step;
	assert_eq!(step(&engine), S::OpenBench);
	// The bench walk, phase by phase off the page's own selection: open
	// it, then land the pick straight on the siphon — the two pick
	// steps complete on consecutive polls.
	engine.ui.alch_screen.open();
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::PickTools);
	let fi = crate::ui::screens::alchemy::families()
		.iter()
		.position(|f| f.members.contains(&BlockType::EssenceSiphon))
		.expect("the siphon has a family");
	let mi = crate::ui::screens::alchemy::families()[fi]
		.members
		.iter()
		.position(|&m| m == BlockType::EssenceSiphon)
		.expect("the siphon is a member");
	engine.ui.alch_screen.force_pick(Some(fi), mi);
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::PickSiphon);
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::CraftSiphon);
	// The siphon lands in the pack (the craft's outcome, however it
	// traveled); the engine's own poll advances the step.
	engine.inventory.add(BlockType::EssenceSiphon, 1);
	engine.update(Duration::from_millis(16));
	assert_eq!(step(&engine), S::SelectSiphon);
	// In hand: tool slot selected, siphon the active tool.
	engine.hotbar.select_digit(9);
	engine
		.hotbar
		.set_active_tool(Some(BlockType::EssenceSiphon));
	engine.update(Duration::from_millis(16));
	assert!(!engine.tutorial.is_running(), "chapter finished");
}

/// A skip is final: a second nudge after one stays quiet.
#[test]
fn the_chapter_respects_a_skip() {
	let mut engine = affordable_engine();
	engine.nudge_siphon();
	assert!(engine.tutorial.is_running());
	if let Some(adv) = engine.tutorial.skip() {
		engine.on_tutorial_advance(adv);
	}
	assert!(!engine.tutorial.is_running());
	// The nudge only ever fires once per session; even called again by
	// hand it must not re-arm a skipped chapter.
	engine.nudge_siphon();
	assert!(!engine.tutorial.is_running(), "skip is final");
}
