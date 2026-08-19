use crate::sim::harness::*;
use crate::sim::{Sim, SimEvent};
use digblock_core::block::BlockType;
use digblock_net::messages::{ClientMsg, ServerMsg};
use digblock_net::protocol::{AccountId, SERVER_ID};
use glam::Vec3;
use std::assert_matches;

fn chat(conn_id: u64, text: &str) -> SimEvent {
	SimEvent::Msg {
		conn_id,
		msg: ClientMsg::Chat { text: text.into() },
	}
}

fn admin_sim(tag: &str) -> Sim {
	let mut sim = flat_sim(tag);
	sim.set_admin_accounts([admin_account()]);
	sim
}

/// Admin is an ACCOUNT decision. Two things follow, and both used to be
/// false: sharing the admin's display name grants nothing, and the
/// admin keeps their commands after a rename.
///
/// The name compare this replaced made `--admin-name admin` plus a join
/// under the name "admin" a complete server takeover. The
/// impersonation half of that is what the second assertion
/// here pins.
#[test]
fn admin_keys_on_the_account_not_the_name() {
	let mut sim = admin_sim("adminacct");

	// The real admin, joined under any name at all.
	let mut admin = connect_admin(&mut sim, 1, "boss");
	sim.tick_once(vec![chat(1, "/help")]);
	admin.drain();
	assert!(
		admin.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::Chat { text, .. } if text.starts_with("/help")
		)),
		"the admin's account gets the commands: {:?}",
		admin.msgs
	);

	// Somebody else, wearing the same name. Under the old rule this was
	// a takeover; now it is just a player with a confusing name.
	let mut impostor = connect_named_as(&mut sim, 2, "boss", test_account(0x99));
	sim.tick_once(vec![chat(2, "/help")]);
	impostor.drain();
	assert!(
		!impostor
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Chat { text, .. } if text.starts_with("/help"))),
		"a matching NAME must grant nothing: {:?}",
		impostor.msgs
	);
}

/// An unverified connection carries `AccountId::NONE`. Admitting that to
/// the admin set would hand admin to every offline and test connection
/// at once, so it is dropped on the way in.
#[test]
fn the_empty_account_can_never_be_an_admin() {
	let mut sim = flat_sim("adminnone");
	sim.set_admin_accounts([AccountId::NONE]);
	let mut anon = connect(&mut sim, 1);
	sim.tick_once(vec![chat(1, "/help")]);
	anon.drain();
	assert!(
		!anon
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Chat { text, .. } if text.starts_with("/help"))),
		"AccountId::NONE must not be grantable: {:?}",
		anon.msgs
	);
}

/// Server-authored chat lines received by a client.
fn server_lines(c: &TestClient) -> Vec<String> {
	c.msgs
		.iter()
		.filter_map(|m| match m {
			ServerMsg::Chat { from, text } if *from == SERVER_ID => Some(text.clone()),
			_ => None,
		})
		.collect()
}

#[test]
fn admin_gets_command_advertisement_others_do_not() {
	let mut sim = admin_sim("advert");
	// The name is irrelevant now — the account is what is granted, so
	// the admin can join as anything.
	let admin = connect_admin(&mut sim, 1, "BOSS");
	let other = connect_named(&mut sim, 2, "guest");
	assert!(
		admin
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::AdminCommands { .. }))
	);
	assert!(
		!other
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::AdminCommands { .. }))
	);
}

/// `/reset` walks a levelled, stocked player back to minute one:
/// skills zeroed and re-synced wholesale (the client re-reveals ore
/// from a `Skills` frame, so a level drop un-reveals), every count
/// wiped — the crafted bucket included, which is the point: the
/// fresh-player opening can be walked without minting an account —
/// and the layout swept by the same reconcile both ends run.
#[test]
fn reset_returns_a_player_to_minute_one() {
	use digblock_core::block::{BlockType, Essence};
	use digblock_core::skills::Skill;
	let mut sim = admin_sim("reset");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	let mut guest = connect_named(&mut sim, 2, "guest");
	wait_live(&mut sim, 1);
	wait_live(&mut sim, 2);
	// A character worth resetting: a level, stocks, blocks, a tool.
	sim.bench_set_mining_level(2, 33);
	sim.test_grant(2, BlockType::Dirt, 5);
	sim.bench_set_active_tool(2, BlockType::Bucket);
	sim.tick_once(vec![chat(1, "/give guest aether 7")]);
	assert!(sim.test_mining_level(2) > 1 && sim.test_sub_count(2, Essence::Aether) == 7);
	admin.drain();
	guest.drain();

	sim.tick_once(vec![chat(1, "/reset guest")]);
	assert_eq!(sim.test_mining_level(2), 1, "skills zeroed");
	assert_eq!(sim.test_skill_xp(2, Skill::Mining), 0);
	assert_eq!(sim.test_inventory_count(2, BlockType::Dirt), 0);
	assert_eq!(sim.test_sub_count(2, Essence::Aether), 0);
	assert_eq!(
		sim.test_inventory_count(2, BlockType::Bucket),
		0,
		"the vessel is wiped with everything else"
	);
	guest.drain();
	assert!(
		guest
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Skills { xp } if xp.is_empty())),
		"the target's client is re-synced to the bottom of the curve"
	);
	assert!(
		guest
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::InventoryFull { counts } if counts.is_empty())),
		"the standing rebase carries the wipe"
	);
	assert!(
		server_lines(&guest).iter().any(|t| t.contains("reset")),
		"the target was told"
	);

	// The admin can reset themselves — the ordinary testing loop —
	// and nonsense is refused with the usage line.
	sim.test_grant(1, BlockType::Stone, 3);
	sim.tick_once(vec![chat(1, "/reset boss")]);
	assert_eq!(sim.test_inventory_count(1, BlockType::Stone), 0);
	admin.drain();
	sim.tick_once(vec![chat(1, "/reset")]);
	sim.tick_once(vec![chat(1, "/reset nobody")]);
	admin.drain();
	let lines = server_lines(&admin);
	assert!(lines.iter().any(|t| t.contains("usage:")), "{lines:?}");
	assert!(lines.iter().any(|t| t.contains("not online")), "{lines:?}");
}

/// Every word `/give`'s refusal ADVERTISES is a word it accepts.
///
/// The usage line used to be a third copy of the vocabulary beside
/// `parse_essence` and `Essence::name`, and when the essences took
/// their reagent names only the parser moved: `/give bob earth 100`
/// answered "earth is not an essence" and then offered `earth`. A
/// refusal that repeats the word it just refused is the failure this
/// pins — so the assertion reads the advertised list back through the
/// parser rather than comparing it to a spelling written here, which
/// would be a fourth copy.
#[test]
fn give_only_advertises_essences_it_accepts() {
	use super::parse_essence;
	use digblock_core::block::Essence;
	let mut sim = admin_sim("giveusage");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	sim.tick_once(vec![chat(1, "/give")]);
	admin.drain();
	let usage = admin
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Chat { text, .. } if text.contains("usage:") => Some(text.clone()),
			_ => None,
		})
		.expect("a bare /give answers with its usage line");
	// The second angle-bracket group: `/give <name> <essences> [n]`.
	let listed: Vec<&str> = usage
		.split('<')
		.nth(2)
		.and_then(|r| r.split_once('>'))
		.map(|(w, _)| w)
		.expect("the usage line names an essence alternation")
		.split('|')
		.collect();
	assert_eq!(
		listed.len(),
		Essence::COUNT,
		"every essence is offered: {usage}"
	);
	for word in listed {
		assert!(
			parse_essence(word).is_some(),
			"{word:?} is advertised but refused: {usage}"
		);
	}
}

/// `/give` credits the TARGET and rebases them, and every essence is
/// spellable by the label the screens use — which is the whole reason
/// `parse_essence` reads `Essence::name` instead of listing spellings.
#[test]
fn give_credits_essence_and_rebases_the_player_who_got_it() {
	use digblock_core::block::Essence;
	let mut sim = admin_sim("give");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	let mut guest = connect_named(&mut sim, 2, "guest");
	wait_live(&mut sim, 1);
	wait_live(&mut sim, 2);
	admin.drain();
	guest.drain();

	// Every essence, named as its own screen label, cased freely.
	for (i, &e) in Essence::ALL.iter().enumerate() {
		let n = i as u32 + 1;
		sim.tick_once(vec![chat(
			1,
			&format!("/give guest {} {n}", e.name().to_ascii_lowercase()),
		)]);
		assert_eq!(sim.test_sub_count(2, e), n, "{e:?} landed in the stocks");
	}
	guest.drain();
	// The standing rebase, not a bespoke message: the guest's screens
	// learn about a gift exactly as they learn about a field draw.
	assert!(
		guest
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::InventoryFull { .. })),
		"the target was never rebased"
	);
	assert!(
		server_lines(&guest).iter().any(|t| t.contains("given")),
		"the target was never told: {:?}",
		server_lines(&guest)
	);

	// An admin gives themselves, which `/goto` and `/bring` refuse.
	sim.tick_once(vec![chat(1, "/give boss aether 7")]);
	assert_eq!(sim.test_sub_count(1, Essence::Aether), 7);

	// Nonsense is refused before anything is credited, and says how.
	admin.drain();
	sim.tick_once(vec![chat(1, "/give guest sunshine 5")]);
	sim.tick_once(vec![chat(1, "/give guest aether 0")]);
	sim.tick_once(vec![chat(1, "/give nobody aether 5")]);
	admin.drain();
	let lines = server_lines(&admin);
	assert!(
		lines.iter().any(|t| t.contains("not an essence")),
		"{lines:?}"
	);
	assert!(
		lines.iter().filter(|t| t.contains("usage:")).count() >= 1,
		"{lines:?}"
	);
	assert!(lines.iter().any(|t| t.contains("not online")), "{lines:?}");
	assert_eq!(
		sim.test_sub_count(2, Essence::Aether),
		Essence::ALL.len() as u32,
		"a refused give credited something"
	);
}

#[test]
fn slash_lines_never_broadcast_and_non_admin_is_refused() {
	let mut sim = admin_sim("slash");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	let mut guest = connect_named(&mut sim, 2, "guest");
	sim.tick_once(vec![chat(1, "/help"), chat(2, "/tp 0 0")]);
	admin.drain();
	guest.drain();
	// The admin got usage lines privately; the guest saw none of it.
	assert!(server_lines(&admin).iter().any(|t| t.contains("/ban")));
	assert!(!guest.msgs.iter().any(
		|m| matches!(m, ServerMsg::Chat { text, .. } if text.contains("/help") || text.contains("/ban"))
	));
	// The guest's own slash line: private refusal, nothing broadcast
	// (the admin's /help reply mentions /tp usage, so key on the
	// guest's exact line).
	assert!(
		server_lines(&guest)
			.iter()
			.any(|t| t.contains("admin-only"))
	);
	assert!(
		!admin
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Chat { text, .. } if text.contains("/tp 0 0")))
	);
}

#[test]
fn admin_commands_bypass_the_chat_token_bucket() {
	let mut sim = admin_sim("bucket");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	for _ in 0..6 {
		sim.tick_once(vec![chat(1, "/ban-list")]);
	}
	admin.drain();
	// More lines than the chat burst allows; all were answered.
	let n = server_lines(&admin)
		.iter()
		.filter(|t| t.contains("no bans"))
		.count();
	assert_eq!(n, 6);
}

#[test]
fn ban_kicks_persists_and_refuses_reconnect() {
	let dir = temp_dir("banflow");
	let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	sim.set_admin_accounts([admin_account()]);
	let mut admin = connect_admin(&mut sim, 1, "boss");
	let mut victim = connect_named(&mut sim, 2, "griefer");
	sim.tick_once(vec![chat(1, "/ban Griefer")]);
	admin.drain();
	victim.drain();
	assert!(
		server_lines(&admin)
			.iter()
			.any(|t| t.contains("banned griefer"))
	);
	assert!(
		victim
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Refused { reason } if reason == "banned"))
	);
	assert!(sim.test_player(2).is_none(), "victim dropped");
	// Same address (conn 2) reconnecting: refused at Hello.
	let again = try_connect(&mut sim, 2, AccountId::NONE);
	assert!(
		matches!(
			again.msgs.first(),
			Some(ServerMsg::Refused { reason }) if reason == "banned"
		),
		"got {:?}",
		again.msgs.first()
	);
	// Survives a server restart (bans.json).
	drop(sim);
	let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let again = try_connect(&mut sim, 2, AccountId::NONE);
	assert_matches!(
		again.msgs.first(),
		Some(ServerMsg::Refused { reason }) if reason == "banned"
	);
	// Unbanned (by name) → welcome again.
	sim.set_admin_accounts([admin_account()]);
	let mut admin = connect_admin(&mut sim, 1, "boss");
	sim.tick_once(vec![chat(1, "/ban-list")]);
	sim.tick_once(vec![chat(1, "/unban griefer")]);
	admin.drain();
	assert!(server_lines(&admin).iter().any(|t| t.contains("griefer")));
	assert!(server_lines(&admin).iter().any(|t| t.contains("unbanned")));
	let back = try_connect(&mut sim, 2, AccountId::NONE);
	assert_matches!(back.msgs.first(), Some(ServerMsg::Welcome { .. }));
}

#[test]
fn ban_edge_cases_reply_without_banning() {
	let mut sim = admin_sim("banedge");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	sim.tick_once(vec![chat(1, "/ban nosuch")]);
	sim.tick_once(vec![chat(1, "/ban boss")]);
	sim.tick_once(vec![chat(1, "/ban !!!")]);
	admin.drain();
	let lines = server_lines(&admin);
	assert!(lines.iter().any(|t| t.contains("not online")));
	assert!(lines.iter().any(|t| t.contains("not banning yourself")));
	assert!(lines.iter().any(|t| t.contains("usage: /ban")));
	assert!(sim.test_player(1).is_some(), "admin still connected");
}

/// `~<offset>` is an unbounded i64 off the wire added to the player's
/// own coordinate. The add has to saturate: overflow panics the sim
/// thread under `overflow-checks` (which is what `just server` and
/// `just play` build), stopping ticks while the process stays up — and
/// the clamp runs after the add, so it cannot rescue it.
#[test]
fn extreme_relative_tp_offsets_saturate_instead_of_overflowing() {
	for arg in [
		format!("~{}", i64::MAX),
		format!("~{}", i64::MIN),
		format!("~{}", i64::MAX - 1),
	] {
		let got = super::parse_coord(&arg, 1.0).expect("parses");
		assert!(
			(-super::TP_LIMIT..=super::TP_LIMIT).contains(&got),
			"{arg} from x=1 landed at {got}, outside the clamp"
		);
	}
	// Out-of-range absolutes still fail to parse rather than saturate.
	assert_eq!(super::parse_coord("99999999999999999999", 1.0), None);
	// And the ordinary cases are untouched.
	assert_eq!(super::parse_coord("~10", 5.9), Some(15));
	assert_eq!(super::parse_coord("~", 5.9), Some(5));
	assert_eq!(super::parse_coord("-40", 5.9), Some(-40));
}

#[test]
fn tp_absolute_relative_and_facing() {
	let mut sim = admin_sim("tp");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	sim.tick_once(vec![chat(1, "/tp 40 -20 e")]);
	admin.drain();
	let (pos, look) = admin
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Teleported { pos, look } => Some((*pos, *look)),
			_ => None,
		})
		.expect("Teleported");
	assert_eq!((pos[0], pos[2]), (40.5, -19.5));
	// The landing rides the pristine column top (the target column is
	// not resident and unedited).
	let h = digblock_core::worldgen::terrain_height(40, -20, SEED);
	assert_eq!(pos[1], h as f32 + 2.5);
	// East = sunrise = -x.
	assert_eq!(look, [-1.0, 0.0, 0.0]);
	assert_eq!(sim.test_look(1), Some(Vec3::new(-1.0, 0.0, 0.0)));
	assert_eq!(sim.test_player(1).unwrap().pos.to_array(), pos);
	assert!(
		server_lines(&admin)
			.iter()
			.any(|t| t.contains("teleported to 40 -20"))
	);

	// Relative: ~ keeps the axis, ~<d> offsets from the floor.
	sim.tick_once(vec![chat(1, "/tp ~-10 ~ n")]);
	admin.drain();
	let (pos, look) = admin
		.msgs
		.iter()
		.rev()
		.find_map(|m| match m {
			ServerMsg::Teleported { pos, look } => Some((*pos, *look)),
			_ => None,
		})
		.expect("second Teleported");
	assert_eq!((pos[0], pos[2]), (30.5, -19.5));
	assert_eq!(look, [0.0, 0.0, -1.0]);

	// Bad args: usage reply, no teleport (gravity may still settle the
	// player vertically between ticks — compare the column).
	let before = sim.test_player(1).unwrap().pos;
	let teleports = admin
		.msgs
		.iter()
		.filter(|m| matches!(m, ServerMsg::Teleported { .. }))
		.count();
	sim.tick_once(vec![chat(1, "/tp up down")]);
	admin.drain();
	assert!(
		server_lines(&admin)
			.iter()
			.any(|t| t.contains("usage: /tp"))
	);
	let after = sim.test_player(1).unwrap().pos;
	assert_eq!((after.x, after.z), (before.x, before.z));
	let teleports_after = admin
		.msgs
		.iter()
		.filter(|m| matches!(m, ServerMsg::Teleported { .. }))
		.count();
	assert_eq!(teleports_after, teleports, "no new Teleported");
}

#[test]
fn goto_and_bring_move_the_right_player() {
	let mut sim = admin_sim("gotobring");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	let mut guest = connect_named(&mut sim, 2, "guest");
	wait_live(&mut sim, 1);
	wait_live(&mut sim, 2);
	// Separate them, then jump to the guest (case-insensitive).
	sim.tick_once(vec![chat(1, "/tp 40 -20")]);
	let gpos = sim.test_player(2).unwrap().pos;
	sim.tick_once(vec![chat(1, "/goto GUEST")]);
	admin.drain();
	let apos = sim.test_player(1).unwrap().pos;
	assert_eq!((apos.x, apos.z), (gpos.x, gpos.z));
	assert!(
		server_lines(&admin)
			.iter()
			.any(|t| t.contains("teleported to guest"))
	);

	// Separate again, then bring the guest to the admin.
	sim.tick_once(vec![chat(1, "/tp 40 -20")]);
	let apos = sim.test_player(1).unwrap().pos;
	sim.tick_once(vec![chat(1, "/bring guest")]);
	admin.drain();
	guest.drain();
	let gpos = sim.test_player(2).unwrap().pos;
	assert_eq!((gpos.x, gpos.z), (apos.x, apos.z));
	assert!(
		guest
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Teleported { .. }))
	);
	assert!(
		server_lines(&guest)
			.iter()
			.any(|t| t.contains("boss teleported you"))
	);
	assert!(
		server_lines(&admin)
			.iter()
			.any(|t| t.contains("brought guest"))
	);

	// Edge cases: unknown player, self, missing arg — reply, no move.
	let before = sim.test_player(1).unwrap().pos;
	sim.tick_once(vec![chat(1, "/goto nosuch")]);
	sim.tick_once(vec![chat(1, "/goto boss")]);
	sim.tick_once(vec![chat(1, "/bring")]);
	admin.drain();
	let lines = server_lines(&admin);
	assert!(lines.iter().any(|t| t.contains("not online")));
	assert!(lines.iter().any(|t| t.contains("that's you")));
	assert!(lines.iter().any(|t| t.contains("usage: /bring")));
	let after = sim.test_player(1).unwrap().pos;
	assert_eq!((after.x, after.z), (before.x, before.z));
}

#[test]
fn time_sets_the_clock_and_reanchors_everyone() {
	let mut sim = admin_sim("time");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	let mut guest = connect_named(&mut sim, 2, "guest");
	sim.tick_once(vec![chat(1, "/time 18:30")]);
	admin.drain();
	guest.drain();
	// The evening hour maps through `tod_of_clock` and is pushed to both
	// immediately.
	for c in [&admin, &guest] {
		let tod = c
			.msgs
			.iter()
			.find_map(|m| match m {
				ServerMsg::TimeOfDay { tod } => Some(*tod),
				_ => None,
			})
			.expect("immediate TimeOfDay");
		assert!((tod + 97.5).abs() < 0.01, "tod {tod}");
	}
	assert!(
		server_lines(&admin)
			.iter()
			.any(|t| t.contains("time set to 18:30"))
	);

	// Time keeps flowing from the new anchor: the next heartbeat has
	// fallen by the elapsed ticks at the sun's rate, not reverted to
	// the tick clock.
	for _ in 0..digblock_net::protocol::TOD_SYNC_TICKS {
		sim.tick_once(vec![]);
	}
	guest.drain();
	let beat = guest
		.msgs
		.iter()
		.rev()
		.find_map(|m| match m {
			ServerMsg::TimeOfDay { tod } => Some(*tod),
			_ => None,
		})
		.expect("heartbeat");
	assert!((-100.6..=-97.5).contains(&beat), "beat {beat}");
}

#[test]
fn time_query_bare_hour_and_bad_input() {
	let mut sim = admin_sim("timeedge");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	sim.tick_once(vec![chat(1, "/time 12:00")]);
	sim.tick_once(vec![chat(1, "/time")]);
	sim.tick_once(vec![chat(1, "/time 6")]);
	sim.tick_once(vec![chat(1, "/time 24:00")]);
	sim.tick_once(vec![chat(1, "/time noon")]);
	admin.drain();
	let lines = server_lines(&admin);
	// The query lands one tick after the set — still rounds to noon.
	assert!(lines.iter().any(|t| t == "time is 12:00"), "{lines:?}");
	assert!(lines.iter().any(|t| t.contains("time set to 06:00")));
	assert_eq!(
		lines.iter().filter(|t| t.contains("usage: /time")).count(),
		2,
		"{lines:?}"
	);
}

#[test]
fn tp_landing_rises_over_journaled_builds() {
	use digblock_net::messages::BlockEdit;
	let dir = temp_dir("tptower");
	let h = digblock_core::worldgen::terrain_height(200, 200, SEED);
	{
		let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
		// A tower through the landing capsule at the /tp target.
		for y in h + 1..=h + 4 {
			j.append(
				BlockEdit {
					x: 200,
					y,
					z: 200,
					block: digblock_core::block::BlockType::Stone as u16,
				},
				0,
			);
		}
		j.sync();
	}
	let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	sim.set_admin_accounts([admin_account()]);
	let mut admin = connect_admin(&mut sim, 1, "boss");
	sim.tick_once(vec![chat(1, "/tp 200 200")]);
	admin.drain();
	let pos = admin
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Teleported { pos, .. } => Some(*pos),
			_ => None,
		})
		.expect("Teleported");
	assert!(
		pos[1] >= (h + 4) as f32 + 2.0,
		"landed on the tower: y {} vs tower top {}",
		pos[1],
		h + 4
	);
}

/// The lock reports, toggles, and persists — and it pins the region
/// around the *resolved* spawn, so the fast path ("locked means the
/// region can't have been broken, so skip the search") stays sound.
#[test]
fn lock_spawn_toggles_and_reports() {
	let mut sim = admin_sim("lockspawn");
	let mut admin = connect_admin(&mut sim, 1, "boss");

	sim.tick_once(vec![chat(1, "/lock-spawn")]);
	admin.drain();
	assert!(
		server_lines(&admin).iter().any(|l| l.contains("unlocked")),
		"expected an unlocked report: {:?}",
		server_lines(&admin)
	);
	assert!(sim.spawn_lock().is_none());

	admin.msgs.clear();
	sim.tick_once(vec![chat(1, "/lock-spawn on")]);
	admin.drain();
	assert!(
		server_lines(&admin).iter().any(|l| l.contains("locked at")),
		"expected a locked report: {:?}",
		server_lines(&admin)
	);
	let region = sim.spawn_lock().expect("locked");
	// A box around the spawn floor: `spawn::lock_region`'s extents.
	let spawn = FLAT_SPAWN;
	let (sx, sz) = (spawn.x.floor() as i32, spawn.z.floor() as i32);
	let h = spawn.y.floor() as i32 - 2;
	assert_eq!(
		(
			region.x0, region.x1, region.y0, region.y1, region.z0, region.z1
		),
		(sx - 2, sx + 2, h, h + 3, sz - 2, sz + 2)
	);

	// Idempotent: locking again reports rather than re-pinning.
	admin.msgs.clear();
	sim.tick_once(vec![chat(1, "/lock-spawn on")]);
	admin.drain();
	assert!(server_lines(&admin).iter().any(|l| l.contains("already")));

	admin.msgs.clear();
	sim.tick_once(vec![chat(1, "/lock-spawn off")]);
	admin.drain();
	assert!(server_lines(&admin).iter().any(|l| l.contains("unlocked")));
	assert!(sim.spawn_lock().is_none());
}

/// Aim the client and wait for the server to have adopted it: inputs
/// are jitter-buffered, so a command issued in the same tick as the
/// look that motivates it reads the OLD facing.
fn face(sim: &mut Sim, conn_id: u64, dir: Vec3, t: &mut u32) {
	for _ in 0..120 {
		if sim
			.test_look(conn_id)
			.is_some_and(|l| (l - dir).length() < 1e-4)
		{
			return;
		}
		*t += 1;
		sim.tick_once(vec![batch(vec![aim(*t, dir)], conn_id)]);
	}
	panic!("server never adopted the look {dir}");
}

/// The three outcomes, in order: aimed at nothing, aimed at a side,
/// aimed at the top of a good block.
#[test]
fn set_spawn_needs_the_top_face_of_a_block() {
	let mut sim = admin_sim("setspawn");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	let mut t = 0;
	let eye = sim.test_player(1).unwrap().pos;
	let (ex, ey, ez) = (
		eye.x.floor() as i32,
		eye.y.floor() as i32,
		eye.z.floor() as i32,
	);

	// Looking at the sky: nothing under the ray.
	face(&mut sim, 1, Vec3::Y, &mut t);
	sim.tick_once(vec![chat(1, "/set-spawn")]);
	admin.drain();
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l == "not looking at a block"),
		"{:?}",
		server_lines(&admin)
	);
	assert!(sim.test_spawn_point().is_none(), "the sky set a spawn");

	// A wall at eye height: a side face is a hit, but not a floor.
	admin.msgs.clear();
	sim.test_set_cell(ex + 1, ey, ez, BlockType::Stone as u16);
	face(&mut sim, 1, Vec3::X, &mut t);
	sim.tick_once(vec![chat(1, "/set-spawn")]);
	admin.drain();
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l.contains("not looking at the top")),
		"{:?}",
		server_lines(&admin)
	);
	assert!(sim.test_spawn_point().is_none(), "a wall set a spawn");

	// Straight down at the block underfoot: the happy path. (The wall
	// stood inside the spawn cylinder, so it has to come down first.)
	admin.msgs.clear();
	sim.test_set_cell(ex + 1, ey, ez, BlockType::Air as u16);
	face(&mut sim, 1, -Vec3::Y, &mut t);
	sim.tick_once(vec![chat(1, "/set-spawn")]);
	admin.drain();
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l.starts_with("spawn point set to")),
		"{:?}",
		server_lines(&admin)
	);
	// The eye sits over the block the ray hit — the same offset
	// `spawn::anchor` puts one at over a surface block.
	assert_eq!(
		sim.test_spawn_point(),
		Some(Vec3::new(
			ex as f32 + 0.5,
			FLAT_GROUND_TOP as f32 + 2.5,
			ez as f32 + 0.5
		))
	);
}

/// The block picked is fine; the ground around it is not. The reply
/// says so and nothing is stored.
#[test]
fn set_spawn_refuses_ground_a_player_cannot_stand_on() {
	let mut sim = admin_sim("setspawnbad");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	let mut t = 0;
	// Dig a hole at the rim of the disc the admin is standing in the
	// middle of: the cell underfoot is solid, the footing is not.
	let eye = sim.test_player(1).unwrap().pos;
	sim.test_set_cell(
		eye.x.floor() as i32 + digblock_core::spawn::CLEAR,
		FLAT_GROUND_TOP as i32,
		eye.z.floor() as i32,
		BlockType::Air as u16,
	);
	face(&mut sim, 1, -Vec3::Y, &mut t);
	sim.tick_once(vec![chat(1, "/set-spawn")]);
	admin.drain();
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l.starts_with("not a valid spawn")),
		"{:?}",
		server_lines(&admin)
	);
	assert!(sim.test_spawn_point().is_none());
}

/// The point overrides the search for joins, and an active lock
/// travels with it: the old cells become editable, the new ones
/// protected.
#[test]
fn a_set_spawn_moves_the_lock_and_becomes_the_spawn() {
	let mut sim = admin_sim("setspawnlock");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	sim.tick_once(vec![chat(1, "/lock-spawn on")]);
	let before = sim.spawn_lock().expect("locked");

	// Aim down and out: within reach, but a different column's top.
	let mut t = 0;
	face(&mut sim, 1, Vec3::new(1.0, -0.8, 0.0).normalize(), &mut t);
	admin.msgs.clear();
	sim.tick_once(vec![chat(1, "/set-spawn")]);
	admin.drain();
	let set = sim.test_spawn_point().expect("spawn set");
	let eye = sim.test_player(1).unwrap().pos;
	assert_ne!(set.x, eye.x, "picked the column it was standing in");

	// The lock moved with it — reported, and enforced somewhere new.
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l.contains("spawn locked at")),
		"{:?}",
		server_lines(&admin)
	);
	let after = sim.spawn_lock().expect("still locked");
	assert_ne!(before.x0, after.x0, "lock stayed on the old cells");
	assert_eq!(after, digblock_core::spawn::lock_region(set));

	// And it is the spawn a joiner gets.
	let guest = connect_named(&mut sim, 2, "guest");
	let welcome = guest
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Welcome { spawn, .. } => Some(*spawn),
			_ => None,
		})
		.expect("Welcome");
	assert_eq!((welcome[0], welcome[2]), (set.x, set.z));

	// Unlocking releases the cells but leaves the spawn where it was
	// put — it is not the search's to move any more.
	admin.msgs.clear();
	sim.tick_once(vec![chat(1, "/lock-spawn off")]);
	sim.tick_once(vec![chat(1, "/lock-spawn")]);
	admin.drain();
	assert!(sim.spawn_lock().is_none());
	assert_eq!(sim.test_spawn_point(), Some(set));
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l.contains("set at") && l.contains("unlocked")),
		"{:?}",
		server_lines(&admin)
	);
}

/// The facing is the admin's own look by default, a cardinal when one
/// is named, and it reaches players through the two paths that are
/// actually spawns.
#[test]
fn set_spawn_dir_takes_your_facing_and_rides_both_spawn_paths() {
	let mut sim = admin_sim("spawndir");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);

	// No argument: the admin's own look, flattened to a yaw.
	let mut t = 0;
	let looking = Vec3::new(0.0, -0.5, -1.0).normalize();
	face(&mut sim, 1, looking, &mut t);
	sim.tick_once(vec![chat(1, "/set-spawn-dir")]);
	admin.drain();
	let dir = sim.test_spawn_dir().expect("dir set");
	assert_eq!(dir, Vec3::new(0.0, 0.0, -1.0), "pitch was not flattened");
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l == "players will spawn facing north"),
		"{:?}",
		server_lines(&admin)
	);

	// A named cardinal overrides where the admin happens to be looking.
	admin.msgs.clear();
	sim.tick_once(vec![chat(1, "/set-spawn-dir e")]);
	admin.drain();
	// East = sunrise = −x.
	assert_eq!(sim.test_spawn_dir(), Some(Vec3::new(-1.0, 0.0, 0.0)));
	assert!(
		server_lines(&admin)
			.iter()
			.any(|l| l.contains("facing east"))
	);

	// A joiner arrives facing it...
	let guest = connect_named(&mut sim, 2, "guest");
	let look = guest
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Welcome { spawn_look, .. } => Some(*spawn_look),
			_ => None,
		})
		.expect("Welcome");
	assert_eq!(look, [-1.0, 0.0, 0.0]);

	// ...and so does a return to spawn.
	wait_live(&mut sim, 2);
	let mut guest = guest;
	guest.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 2,
		msg: ClientMsg::ReturnToSpawn,
	}]);
	guest.drain();
	let look = guest
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Teleported { look, .. } => Some(*look),
			_ => None,
		})
		.expect("Teleported");
	assert_eq!(look, [-1.0, 0.0, 0.0]);
	assert_eq!(sim.test_look(2), Some(Vec3::new(-1.0, 0.0, 0.0)));
}

/// Straight up has no yaw in it, and a garbled cardinal is not a
/// silent no-op.
#[test]
fn set_spawn_dir_rejects_a_facing_it_cannot_use() {
	let mut sim = admin_sim("spawndirbad");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	let mut t = 0;
	face(&mut sim, 1, Vec3::Y, &mut t);
	sim.tick_once(vec![chat(1, "/set-spawn-dir")]);
	sim.tick_once(vec![chat(1, "/set-spawn-dir sideways")]);
	admin.drain();
	let lines = server_lines(&admin);
	assert!(lines.iter().any(|l| l.contains("not facing anywhere")));
	assert!(lines.iter().any(|l| l.contains("usage: /set-spawn-dir")));
	assert!(sim.test_spawn_dir().is_none(), "a bad facing was stored");
}

/// A player resuming in place keeps their own view: the spawn facing
/// describes spawn, not wherever they logged out.
#[test]
fn a_resumed_join_does_not_get_the_spawn_facing() {
	let dir = temp_dir("spawndirresume");
	let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	sim.set_admin_accounts([admin_account()]);
	let _admin = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	sim.tick_once(vec![chat(1, "/set-spawn-dir n")]);

	// A first join under a real account banks a position...
	let id = test_account(7);
	let first = crate::sim::harness::try_connect_as(&mut sim, 2, id, "guest");
	assert_matches!(
		first.msgs.first(),
		Some(ServerMsg::Welcome { spawn_look, .. }) if *spawn_look == [0.0, 0.0, -1.0]
	);
	wait_live(&mut sim, 2);
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);

	// ...so the next one resumes it, with no facing attached.
	let again = crate::sim::harness::try_connect_as(&mut sim, 3, id, "guest");
	let look = again
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Welcome { spawn_look, .. } => Some(*spawn_look),
			_ => None,
		})
		.expect("Welcome");
	assert_eq!(look, [0.0; 3], "a resume adopted the spawn facing");
}

/// A bad argument explains itself instead of silently toggling.
#[test]
fn lock_spawn_rejects_a_bad_argument() {
	let mut sim = admin_sim("lockspawnbad");
	let mut admin = connect_admin(&mut sim, 1, "boss");
	sim.tick_once(vec![chat(1, "/lock-spawn maybe")]);
	admin.drain();
	assert!(server_lines(&admin).iter().any(|l| l.contains("usage:")));
	assert!(sim.spawn_lock().is_none(), "bad argument changed the lock");
}
