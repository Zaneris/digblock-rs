//! Admin chat commands (`--admin-name`): dispatched from the Chat arm
//! for slash-lines from the admin, never broadcast. Every outcome is
//! answered with a private `SERVER_ID` chat reply. The command table is
//! the single source of truth: it drives the dispatcher, `/help`, and
//! the `AdminCommands` advertisement the client autocompletes from.

use super::{client_send, OutMsg, Sim};
use digblock_net::messages::ServerMsg;
use digblock_net::protocol::{validate_name, MAX_CHAT_LEN, SERVER_ID};
use glam::Vec3;

/// `(usage, description)` — usage's first token is the dispatch key and
/// the client's autocomplete stem.
const COMMANDS: &[(&str, &str)] = &[
	("/help", "list admin commands"),
	(
		"/tp <x> <z> [n|s|e|w]",
		"teleport to the column top; ~ or ~<d> = relative, facing optional",
	),
	("/ban <name>", "ban an online player by IP and kick them"),
	("/ban-list", "list bans"),
	("/unban <name-or-ip>", "remove a ban"),
];

/// `/tp` coordinate clamp: far past any playable range, comfortably
/// inside f32 world-position precision.
const TP_LIMIT: i64 = 1_000_000;

/// Compass → look vector. Derived from the engine's conventions, not
/// invented: the sun rotates about world Z (sunrise on the −x horizon),
/// and `Dir::West` is +x — so east = −x, west = +x; with camera yaw 0 =
/// +z and +yaw turning right, north = −z, south = +z.
fn cardinal(dir: &str) -> Option<Vec3> {
	Some(match dir {
		"n" | "north" => Vec3::new(0.0, 0.0, -1.0),
		"s" | "south" => Vec3::new(0.0, 0.0, 1.0),
		"e" | "east" => Vec3::new(-1.0, 0.0, 0.0),
		"w" | "west" => Vec3::new(1.0, 0.0, 0.0),
		_ => return None,
	})
}

/// A private server→player chat line (the client renders SERVER_ID as
/// an unnamed system line).
pub(super) fn server_reply(text: &str) -> ServerMsg {
	ServerMsg::Chat {
		from: SERVER_ID,
		text: text.into(),
	}
}

/// The advertisement sent to the admin at join.
pub(super) fn commands_msg() -> ServerMsg {
	ServerMsg::AdminCommands {
		commands: COMMANDS
			.iter()
			.map(|(u, d)| (u.to_string(), d.to_string()))
			.collect(),
	}
}

/// Absolute integer, or `~`/`~±d` relative to `cur`. Clamped.
fn parse_coord(arg: &str, cur: f32) -> Option<i64> {
	let v = if let Some(rest) = arg.strip_prefix('~') {
		let off: i64 = if rest.is_empty() {
			0
		} else {
			rest.parse().ok()?
		};
		cur.floor() as i64 + off
	} else {
		arg.parse().ok()?
	};
	Some(v.clamp(-TP_LIMIT, TP_LIMIT))
}

impl Sim {
	/// Entry point from the Chat arm: `line` is sanitized and starts
	/// with '/'; the sender is the verified admin.
	pub(super) fn handle_admin_command(&mut self, conn_id: u64, line: &str) {
		let mut parts = line.split_ascii_whitespace();
		let cmd = parts.next().unwrap_or("");
		let args: Vec<&str> = parts.collect();
		match cmd {
			"/help" => {
				for (usage, desc) in COMMANDS {
					self.reply(conn_id, &format!("{usage} - {desc}"));
				}
				self.reply(conn_id, "compass: e = sunrise (-x), n = -z");
			}
			"/tp" => self.cmd_tp(conn_id, &args),
			"/ban" => self.cmd_ban(conn_id, &args),
			"/ban-list" => self.cmd_ban_list(conn_id),
			"/unban" => self.cmd_unban(conn_id, &args),
			other => self.reply(conn_id, &format!("unknown command {other} - try /help")),
		}
	}

	/// Private reply, chunked to the chat length cap so nothing is
	/// silently truncated client-side.
	fn reply(&self, conn_id: u64, text: &str) {
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		let chars: Vec<char> = text.chars().collect();
		for chunk in chars.chunks(MAX_CHAT_LEN) {
			client_send(client, &server_reply(&chunk.iter().collect::<String>()));
		}
	}

	fn cmd_tp(&mut self, conn_id: u64, args: &[&str]) {
		let usage = "usage: /tp <x> <z> [n|s|e|w] (~ or ~<d> = relative)";
		if !(args.len() == 2 || args.len() == 3) {
			return self.reply(conn_id, usage);
		}
		let Some(cur) = self.clients.get(&conn_id).map(|c| c.player.pos) else {
			return;
		};
		let (Some(x), Some(z)) = (parse_coord(args[0], cur.x), parse_coord(args[1], cur.z)) else {
			return self.reply(conn_id, usage);
		};
		let look = match args.get(2) {
			None => Vec3::ZERO,
			Some(d) => match cardinal(&d.to_ascii_lowercase()) {
				Some(v) => v,
				None => return self.reply(conn_id, usage),
			},
		};
		// Land on the topmost block: pristine terrain height, then the
		// spawn rule raises the eye over journaled builds (and, for
		// resident columns, live edits) at the target.
		let h = digblock_core::worldgen::terrain_height(x as i32, z as i32, self.store.seed());
		let eye = Vec3::new(x as f32 + 0.5, h as f32 + 2.5, z as f32 + 0.5);
		let pos = self.clear_landing(eye);
		self.teleport_client(conn_id, pos, look);
		if let Some(client) = self.clients.get(&conn_id) {
			log::info!("admin {} teleported to {x} {z}", client.name);
		}
		self.reply(conn_id, &format!("teleported to {x} {z}"));
	}

	fn cmd_ban(&mut self, conn_id: u64, args: &[&str]) {
		let [name] = args else {
			return self.reply(conn_id, "usage: /ban <name>");
		};
		if validate_name(name).is_err() {
			return self.reply(conn_id, "usage: /ban <name> (a player display name)");
		}
		let matches: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.name.eq_ignore_ascii_case(name))
			.map(|(&id, _)| id)
			.collect();
		let [target] = matches[..] else {
			let msg = if matches.is_empty() {
				format!("{name} is not online (bans need the player's IP)")
			} else {
				format!("{} players named {name} - can't ban by name", matches.len())
			};
			return self.reply(conn_id, &msg);
		};
		if target == conn_id {
			return self.reply(conn_id, "not banning yourself");
		}
		let (ip, stored_name) = {
			let c = &self.clients[&target];
			(c.addr.ip(), c.name.clone())
		};
		if self.clients[&conn_id].addr.ip() == ip {
			// Kicking every connection on the banned IP would include the
			// admin — almost certainly two names from one household/NAT.
			return self.reply(conn_id, &format!("{name} shares your IP - not banning it"));
		}
		self.ban_store.ban(&stored_name, ip);
		// Kick every connection from the banned IP, not just the named one.
		let kicked: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.addr.ip() == ip)
			.map(|(&id, _)| id)
			.collect();
		for id in &kicked {
			let c = &self.clients[id];
			client_send(
				c,
				&ServerMsg::Refused {
					reason: "banned".into(),
				},
			);
			let _ = c.out.try_send(OutMsg::Close);
		}
		for id in kicked {
			self.drop_client(id);
		}
		log::info!("admin banned {stored_name} ({ip})");
		self.reply(conn_id, &format!("banned {stored_name} ({ip})"));
	}

	fn cmd_ban_list(&mut self, conn_id: u64) {
		if self.ban_store.entries().is_empty() {
			return self.reply(conn_id, "no bans");
		}
		let lines: Vec<String> = self
			.ban_store
			.entries()
			.iter()
			.map(|e| format!("{} - {}", e.name, e.ip))
			.collect();
		for line in lines {
			self.reply(conn_id, &line);
		}
	}

	fn cmd_unban(&mut self, conn_id: u64, args: &[&str]) {
		let [arg] = args else {
			return self.reply(conn_id, "usage: /unban <name-or-ip>");
		};
		let removed = self.ban_store.unban_matching(arg);
		let msg = if removed == 0 {
			format!("no ban matches {arg}")
		} else {
			format!("unbanned {arg} ({removed} entry(s))")
		};
		log::info!("admin unban {arg}: {removed} removed");
		self.reply(conn_id, &msg);
	}
}

#[cfg(test)]
mod tests {
	use crate::sim::harness::*;
	use crate::sim::{Sim, SimEvent};
	use digblock_net::messages::{ClientMsg, ServerMsg};
	use digblock_net::protocol::SERVER_ID;
	use glam::Vec3;

	fn chat(conn_id: u64, text: &str) -> SimEvent {
		SimEvent::Msg {
			conn_id,
			msg: ClientMsg::Chat { text: text.into() },
		}
	}

	fn admin_sim(tag: &str) -> Sim {
		let mut sim = flat_sim(tag);
		sim.set_admin_name(Some("boss".into()));
		sim
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
		let admin = connect_named(&mut sim, 1, "BOSS"); // case-insensitive
		let other = connect_named(&mut sim, 2, "guest");
		assert!(admin
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::AdminCommands { .. })));
		assert!(!other
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::AdminCommands { .. })));
	}

	#[test]
	fn slash_lines_never_broadcast_and_non_admin_is_refused() {
		let mut sim = admin_sim("slash");
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		assert!(server_lines(&guest)
			.iter()
			.any(|t| t.contains("admin-only")));
		assert!(!admin
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Chat { text, .. } if text.contains("/tp 0 0"))));
	}

	#[test]
	fn admin_commands_bypass_the_chat_token_bucket() {
		let mut sim = admin_sim("bucket");
		let mut admin = connect_named(&mut sim, 1, "boss");
		for _ in 0..6 {
			sim.tick_once(vec![chat(1, "/ban-list")]);
		}
		admin.drain();
		// Burst is 3; all six commands were answered.
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
		sim.set_admin_name(Some("boss".into()));
		let mut admin = connect_named(&mut sim, 1, "boss");
		let mut victim = connect_named(&mut sim, 2, "griefer");
		sim.tick_once(vec![chat(1, "/ban Griefer")]);
		admin.drain();
		victim.drain();
		assert!(server_lines(&admin)
			.iter()
			.any(|t| t.contains("banned griefer")));
		assert!(victim
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Refused { reason } if reason == "banned")));
		assert!(sim.test_player(2).is_none(), "victim dropped");
		// Same address (conn 2) reconnecting: refused at Hello.
		let again = try_connect(&mut sim, 2, [0; 16]);
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
		let again = try_connect(&mut sim, 2, [0; 16]);
		assert!(matches!(
			again.msgs.first(),
			Some(ServerMsg::Refused { reason }) if reason == "banned"
		));
		// Unbanned (by name) → welcome again.
		sim.set_admin_name(Some("boss".into()));
		let mut admin = connect_named(&mut sim, 1, "boss");
		sim.tick_once(vec![chat(1, "/ban-list")]);
		sim.tick_once(vec![chat(1, "/unban griefer")]);
		admin.drain();
		assert!(server_lines(&admin).iter().any(|t| t.contains("griefer")));
		assert!(server_lines(&admin).iter().any(|t| t.contains("unbanned")));
		let back = try_connect(&mut sim, 2, [0; 16]);
		assert!(matches!(back.msgs.first(), Some(ServerMsg::Welcome { .. })));
	}

	#[test]
	fn ban_edge_cases_reply_without_banning() {
		let mut sim = admin_sim("banedge");
		let mut admin = connect_named(&mut sim, 1, "boss");
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

	#[test]
	fn tp_absolute_relative_and_facing() {
		let mut sim = admin_sim("tp");
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		assert!(server_lines(&admin)
			.iter()
			.any(|t| t.contains("teleported to 40 -20")));

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
		assert!(server_lines(&admin)
			.iter()
			.any(|t| t.contains("usage: /tp")));
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
		sim.set_admin_name(Some("boss".into()));
		let mut admin = connect_named(&mut sim, 1, "boss");
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
}
