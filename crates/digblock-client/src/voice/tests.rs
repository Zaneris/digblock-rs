use super::*;
use digblock_core::block::BlockType;
use digblock_core::unlock::{self, Kind};

/// One sample of every message. The match below is what keeps the list
/// complete: a new variant fails to COMPILE here until it is sampled,
/// exactly as it fails in `en::line` until it has words.
fn samples() -> Vec<Msg> {
	let req = unlock::requirement(BlockType::Wood, Kind::Strength).expect("timber is gated");
	let all = vec![
		Msg::DigRefused(req),
		Msg::DigProtected,
		Msg::PlaceProtected,
		Msg::NeedsSiphon,
		Msg::DigRefusedByServer,
		Msg::LevelUp {
			skill: Skill::Mining,
			level: 7,
		},
		Msg::RemoteLevelUp {
			who: "ada".into(),
			skill: Skill::Alchemy,
			level: 7,
		},
		Msg::VeinsRevealed,
		Msg::Connected,
		Msg::LeftServer,
		Msg::LeftWorld,
		Msg::Joined { name: "ada".into() },
		Msg::Left { name: "ada".into() },
		Msg::ConnectRefused {
			reason: "full".into(),
		},
		Msg::Disconnected {
			reason: "timeout".into(),
		},
		Msg::Teleported,
		Msg::ReturnedToSpawn,
		Msg::ServerNotice {
			text: "banned ada".into(),
		},
		Msg::Chat {
			name: "ada".into(),
			text: "hello".into(),
		},
		Msg::CommandHint {
			usage: "/ban <name>".into(),
			desc: "ban by IP".into(),
		},
	];
	for m in &all {
		match m {
			Msg::DigRefused(_)
			| Msg::DigProtected
			| Msg::PlaceProtected
			| Msg::NeedsSiphon
			| Msg::DigRefusedByServer
			| Msg::LevelUp { .. }
			| Msg::RemoteLevelUp { .. }
			| Msg::VeinsRevealed
			| Msg::Connected
			| Msg::LeftServer
			| Msg::LeftWorld
			| Msg::Joined { .. }
			| Msg::Left { .. }
			| Msg::ConnectRefused { .. }
			| Msg::Disconnected { .. }
			| Msg::Teleported
			| Msg::ReturnedToSpawn
			| Msg::ServerNotice { .. }
			| Msg::Chat { .. }
			| Msg::CommandHint { .. } => {}
		}
	}
	all
}

/// Every message renders to something a player could read, and no two
/// say the same thing — a duplicate is two situations wearing one
/// sentence, which is the bug `DigProtected`/`PlaceProtected` exist to
/// avoid.
#[test]
fn every_message_has_words_of_its_own() {
	let mut seen: Vec<String> = Vec::new();
	for m in samples() {
		let line = render(&m);
		assert!(!line.trim().is_empty(), "{m:?} renders to nothing");
		assert!(!seen.contains(&line), "{m:?} speaks a line already taken");
		seen.push(line);
	}
}

/// The refusal names the block and the level that opens it, and reads
/// its numbers out of the tables — the wording is pinned here, the LEVEL
/// is `unlock`'s to move. The name arrives in the ONE case core stores
/// it in: a sentence that re-cased it would be transforming text a
/// translation has to own.
#[test]
fn a_refusal_names_the_block_and_the_level() {
	let strength = unlock::requirement(BlockType::Wood, Kind::Strength).expect("timber is gated");
	assert_eq!(
		render(&Msg::DigRefused(strength)),
		format!("Wood needs Mining level {}", unlock::mine(BlockType::Wood))
	);
	let recipe = unlock::requirement(BlockType::StoneStairs, Kind::Recipe).expect("craft-only");
	assert_eq!(
		render(&Msg::DigRefused(recipe)),
		format!(
			"Stone Stairs needs Alchemy level {}",
			unlock::pickup(BlockType::StoneStairs)
		)
	);
}
