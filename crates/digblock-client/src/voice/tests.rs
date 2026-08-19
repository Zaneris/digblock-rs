use super::*;
use digblock_core::block::{BlockType, Essence};
use digblock_core::unlock::{self, Kind};
use digblock_net::messages::{AdminReply, AdminUsage, NoPlayer};

/// One sample of every admin reply. The match is what keeps the list
/// complete: a new variant fails to COMPILE here until it is sampled,
/// exactly as it fails in `en::admin` until it has words.
fn admin_samples() -> Vec<AdminReply> {
	use AdminReply as A;
	let all = vec![
		A::Usage(AdminUsage::Tp),
		A::CommandHint {
			usage: "/tp <x> <z>".into(),
			desc: "teleport".into(),
		},
		A::UnknownCommand {
			typed: "/nope".into(),
		},
		A::CompassHint,
		A::NoSuchPlayer {
			name: "ada".into(),
			why: NoPlayer::NotOnline,
		},
		A::ThatIsYou,
		A::Teleported { x: 40, z: -20 },
		A::TeleportedTo { name: "ada".into() },
		A::Brought { name: "ada".into() },
		A::TeleportedYouBy {
			admin: "boss".into(),
		},
		A::NotLookingAtBlock,
		A::NotLookingAtTopFace,
		A::SpawnNotClear {
			clear: 2,
			capsule: 3,
		},
		A::SpawnPointSet { cell: [1, 2, 3] },
		A::NotFacingAnywhere,
		A::SpawnFacingSet {
			compass: "north".into(),
		},
		A::SpawnLocked {
			cell: [1, 2, 3],
			region: [0, 4, 1, 5, 2, 6],
			already: false,
		},
		A::SpawnSetButUnlocked { cell: [1, 2, 3] },
		A::NoClearSpawnToLock,
		A::SpawnAlreadyUnlocked,
		A::SpawnUnlocked,
		A::NotAnEssence {
			typed: "gold".into(),
		},
		A::Gave {
			name: "ada".into(),
			essence: Essence::Vitae as u16,
			gained: 3,
			asked: 3,
			held: 9,
		},
		A::WereGiven {
			essence: Essence::Vitae as u16,
			gained: 3,
		},
		A::TimeIs {
			hour: 12,
			minute: 0,
		},
		A::TimeSet { hour: 6, minute: 0 },
		A::ResetPlayer { name: "ada".into() },
		A::YouWereReset,
		A::NotBanningYourself,
		A::SharesYourIp { name: "ada".into() },
		A::Banned {
			name: "ada".into(),
			ip: "10.0.0.1".into(),
		},
		A::NoBans,
		A::BanEntry {
			name: "ada".into(),
			ip: "10.0.0.1".into(),
		},
		A::Unbanned {
			arg: "ada".into(),
			removed: 1,
		},
		A::NoBanMatches { arg: "ada".into() },
		A::LegacyCharacterClaimed,
		A::CommandsAreAdminOnly,
	];
	for a in &all {
		match a {
			A::Usage(_)
			| A::CommandHint { .. }
			| A::UnknownCommand { .. }
			| A::CompassHint
			| A::NoSuchPlayer { .. }
			| A::ThatIsYou
			| A::Teleported { .. }
			| A::TeleportedTo { .. }
			| A::Brought { .. }
			| A::TeleportedYouBy { .. }
			| A::NotLookingAtBlock
			| A::NotLookingAtTopFace
			| A::SpawnNotClear { .. }
			| A::SpawnPointSet { .. }
			| A::NotFacingAnywhere
			| A::SpawnFacingSet { .. }
			| A::SpawnLocked { .. }
			| A::SpawnSetButUnlocked { .. }
			| A::NoClearSpawnToLock
			| A::SpawnAlreadyUnlocked
			| A::SpawnUnlocked
			| A::NotAnEssence { .. }
			| A::Gave { .. }
			| A::WereGiven { .. }
			| A::TimeIs { .. }
			| A::TimeSet { .. }
			| A::ResetPlayer { .. }
			| A::YouWereReset
			| A::NotBanningYourself
			| A::SharesYourIp { .. }
			| A::Banned { .. }
			| A::NoBans
			| A::BanEntry { .. }
			| A::Unbanned { .. }
			| A::NoBanMatches { .. }
			| A::LegacyCharacterClaimed
			| A::CommandsAreAdminOnly => {}
		}
	}
	all
}

/// One sample of every message. The match below is what keeps the list
/// complete: a new variant fails to COMPILE here until it is sampled,
/// exactly as it fails in `en::line` until it has words.
fn samples() -> Vec<Msg> {
	let req = unlock::requirement(BlockType::Wood, Kind::Strength).expect("timber is gated");
	let mut all = vec![
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
			| Msg::CommandHint { .. }
			| Msg::Admin(_) => {}
		}
	}
	all.extend(admin_samples().into_iter().map(Msg::Admin));
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

/// A message's arguments must actually reach its words. Asserted by
/// VARYING each one and demanding the line change — which proves the
/// template consumes the argument without this test knowing a single
/// word of it, so any rewording is free and a dropped `{}` is not.
///
/// A template that ignored an argument would otherwise pass everything
/// above: it renders, and it is distinct from the others.
#[test]
fn every_argument_reaches_the_words() {
	let pairs: Vec<(Msg, Msg)> = vec![
		(
			Msg::LevelUp {
				skill: Skill::Mining,
				level: 7,
			},
			Msg::LevelUp {
				skill: Skill::Mining,
				level: 8,
			},
		),
		(
			Msg::Joined { name: "ada".into() },
			Msg::Joined { name: "bo".into() },
		),
		(
			Msg::Admin(AdminReply::Teleported { x: 1, z: 2 }),
			Msg::Admin(AdminReply::Teleported { x: 3, z: 4 }),
		),
		(
			Msg::Admin(AdminReply::TimeSet { hour: 6, minute: 0 }),
			Msg::Admin(AdminReply::TimeSet {
				hour: 18,
				minute: 30,
			}),
		),
		(
			Msg::Admin(AdminReply::Banned {
				name: "ada".into(),
				ip: "10.0.0.1".into(),
			}),
			Msg::Admin(AdminReply::Banned {
				name: "bo".into(),
				ip: "10.0.0.2".into(),
			}),
		),
		(
			Msg::Admin(AdminReply::SpawnLocked {
				cell: [1, 2, 3],
				region: [0, 4, 1, 5, 2, 6],
				already: false,
			}),
			Msg::Admin(AdminReply::SpawnLocked {
				cell: [7, 8, 9],
				region: [6, 10, 7, 11, 8, 12],
				already: true,
			}),
		),
	];
	for (a, b) in pairs {
		assert_ne!(
			render(&a),
			render(&b),
			"{a:?} and {b:?} render alike, so an argument never reaches the words"
		);
	}
}

/// The refusal carries the block, the skill and the level that opens it
/// — asserted as those three VALUES appearing, each read from the table
/// that owns it. Nothing is asserted about the words between them, so a
/// rewording costs nothing here and a template that drops the level
/// still fails.
///
/// The name arrives in the ONE case core stores it in: a sentence that
/// re-cased it would be transforming text a translation has to own.
#[test]
fn a_refusal_names_the_block_and_the_level() {
	for (block, kind, level) in [
		(
			BlockType::Wood,
			Kind::Strength,
			unlock::mine(BlockType::Wood),
		),
		(
			BlockType::StoneStairs,
			Kind::Recipe,
			unlock::pickup(BlockType::StoneStairs),
		),
	] {
		let req = unlock::requirement(block, kind).expect("gated");
		let line = render(&Msg::DigRefused(req));
		for want in [
			block.name().to_string(),
			req.skill.name().to_string(),
			level.to_string(),
		] {
			assert!(line.contains(&want), "{want:?} missing from {line:?}");
		}
	}
}

/// `/give`'s usage line names every essence, in the identifier the
/// parser takes — the two are built from `Essence::ALL` at opposite
/// ends of the wire, and this is the end that draws it.
///
/// Its counterpart, that `parse_essence` accepts every identifier, is
/// `admin::give_only_advertises_essences_it_accepts`. Together they
/// close the gap that once let a refusal repeat the word it had just
/// rejected.
#[test]
fn the_give_usage_names_every_essence() {
	let line = render(&Msg::Admin(AdminReply::Usage(AdminUsage::Give)));
	for e in Essence::ALL {
		assert!(
			line.contains(e.ident()),
			"{:?} is takeable but not offered: {line}",
			e
		);
	}
}
