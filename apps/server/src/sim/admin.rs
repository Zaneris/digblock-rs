//! Admin chat commands (`--admin-name`): dispatched from the Chat arm
//! for slash-lines from the admin, never broadcast. Every outcome is
//! answered with a private `SERVER_ID` chat reply. The command table is
//! the single source of truth: it drives the dispatcher, `/help`, and
//! the `AdminCommands` advertisement the client autocompletes from.

use super::{OutMsg, Sim, SpawnLock, client_send};
use digblock_core::block::Essence;
use digblock_core::compass::{cardinal, compass};
use digblock_core::raycast::{REACH, raycast};
use digblock_net::messages::ServerMsg;
use digblock_net::protocol::{MAX_CHAT_LEN, SERVER_ID, validate_name};
use glam::Vec3;

/// `(usage, description)` — usage's first token is the dispatch key and
/// the client's autocomplete stem.
const COMMANDS: &[(&str, &str)] = &[
	("/help", "list admin commands"),
	(
		"/tp <x> <z> [n|s|e|w]",
		"teleport to the column top; ~ or ~<d> = relative, facing optional",
	),
	("/goto <name>", "teleport to an online player"),
	(
		"/time [hh:mm]",
		"show or set the time of day (24h; 6:00 sunrise, 12:00 noon, 18:00 sunset)",
	),
	("/bring <name>", "teleport an online player to you"),
	(
		"/give <name> <essence> [n]",
		"credit essence stocks: flora, earth, aqua, metal or aether; n defaults to one",
	),
	("/ban <name>", "ban an online player by IP and kick them"),
	("/ban-list", "list bans"),
	("/unban <name-or-ip>", "remove a ban"),
	(
		"/set-spawn",
		"move spawn to the top of the block you are looking at",
	),
	(
		"/set-spawn-dir [n|s|e|w]",
		"set which way players face when they spawn; no argument = your facing",
	),
	(
		"/lock-spawn [on|off]",
		"show or set spawn protection (freezes spawn and makes its cells unbreakable)",
	),
];

/// `/tp` coordinate clamp: far past any playable range, comfortably
/// inside f32 world-position precision.
const TP_LIMIT: i64 = 1_000_000;

/// Where a spawn eye reads as coordinates: the FLOOR block, not the eye.
/// An admin who aimed at a block and got told a number two higher would
/// reasonably think the command missed, and it is the same y the lock
/// region reports as `y0`.
fn at(eye: Vec3) -> String {
	format!(
		"{} {} {}",
		eye.x.floor(),
		eye.y.floor() - 2.0,
		eye.z.floor()
	)
}

/// The one wording for "spawn is locked here", shared by `/lock-spawn`
/// and by `/set-spawn` reporting the lock it dragged along.
fn describe_lock(eye: Vec3) -> String {
	let r = digblock_core::spawn::lock_region(eye);
	format!(
		"spawn locked at {} (protecting x {}..{}, y {}..{}, z {}..{})",
		at(eye),
		r.x0,
		r.x1,
		r.y0,
		r.y1,
		r.z0,
		r.z1,
	)
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

/// 24h clock → sun angle: the clock's hours spread evenly over the full
/// turn, anchored to the tod convention in `sun.rs` (midnight at its wrap
/// point, noon at its origin) and falling as the day advances.
fn tod_of_clock(h: u32, m: u32) -> f32 {
	180.0 - (h as f32 + m as f32 / 60.0) * 15.0
}

/// `hh:mm` or a bare hour, 24-hour.
fn parse_clock(arg: &str) -> Option<(u32, u32)> {
	let (h, m) = match arg.split_once(':') {
		Some((h, m)) => (h.parse().ok()?, m.parse().ok()?),
		None => (arg.parse().ok()?, 0),
	};
	(h < 24 && m < 60).then_some((h, m))
}

/// The inverse of `tod_of_clock`, for the no-argument query.
fn clock_of_tod(tod: f32) -> String {
	let mins = (((180.0 - tod) / 15.0 * 60.0).round() as i64).rem_euclid(24 * 60);
	format!("{:02}:{:02}", mins / 60, mins % 60)
}

/// An essence by name, case-insensitively.
///
/// Matched against [`Essence::name`] — the screens' own label — rather
/// than a second list of spellings, so the command cannot come to
/// disagree with the UI about what a currency is called, and a new
/// essence is takeable the day its row exists.
fn parse_essence(arg: &str) -> Option<Essence> {
	Essence::ALL
		.into_iter()
		.find(|e| e.name().eq_ignore_ascii_case(arg))
}

/// Absolute integer, or `~`/`~±d` relative to `cur`. Clamped.
///
/// The relative add SATURATES: `off` is an unbounded `i64` straight off the
/// wire, so `cur + off` overflows for any offset near `i64::MAX`, and the
/// clamp below runs too late to help. Debug builds — which is what `just
/// server` and `just play` produce — take `overflow-checks` and panic the
/// sim thread outright, stopping ticks while the process stays up.
fn parse_coord(arg: &str, cur: f32) -> Option<i64> {
	let v = if let Some(rest) = arg.strip_prefix('~') {
		let off: i64 = if rest.is_empty() {
			0
		} else {
			rest.parse().ok()?
		};
		(cur.floor() as i64).saturating_add(off)
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
			"/goto" => self.cmd_goto(conn_id, &args),
			"/time" => self.cmd_time(conn_id, &args),
			"/bring" => self.cmd_bring(conn_id, &args),
			"/give" => self.cmd_give(conn_id, &args),
			"/ban" => self.cmd_ban(conn_id, &args),
			"/ban-list" => self.cmd_ban_list(conn_id),
			"/unban" => self.cmd_unban(conn_id, &args),
			"/set-spawn" => self.cmd_set_spawn(conn_id, &args),
			"/set-spawn-dir" => self.cmd_set_spawn_dir(conn_id, &args),
			"/lock-spawn" => self.cmd_lock_spawn(conn_id, &args),
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

	/// Hand-pick the spawn: the block under the admin's crosshair becomes
	/// the ground players stand on.
	///
	/// The TOP face specifically, not any face. The cell the ray hits is
	/// the floor, so a side or bottom hit would quietly mean a spawn
	/// hanging in the air beside a wall or buried in a ceiling — and the
	/// admin who aimed at a wall meant the ground, not the cell behind it.
	///
	/// Validated by [`digblock_core::spawn::stands_clear`], which reads the
	/// world as it actually is rather than pristine worldgen: the whole
	/// point of setting spawn by hand is to put it somewhere terrain alone
	/// would never have chosen — a levelled hillside, a built platform, a
	/// hall cut into rock.
	fn cmd_set_spawn(&mut self, conn_id: u64, args: &[&str]) {
		if !args.is_empty() {
			return self.reply(conn_id, "usage: /set-spawn (aim at the block to stand on)");
		}
		let Some((pos, look)) = self
			.clients
			.get(&conn_id)
			.filter(|c| c.live)
			.map(|c| (c.player.pos, c.look))
		else {
			return;
		};
		let Some(hit) = raycast(pos, look, REACH, &|x, y, z| self.world_cell(x, y, z)) else {
			return self.reply(conn_id, "not looking at a block");
		};
		if hit.normal != (0, 1, 0) {
			return self.reply(
				conn_id,
				"not looking at the top of a block - aim at the surface to stand on",
			);
		}
		// The hit cell is the floor; the eye sits centred above it, at
		// exactly the offset `spawn::anchor` uses over a surface block.
		let eye = Vec3::new(
			hit.cell.0 as f32 + 0.5,
			hit.cell.1 as f32 + 2.5,
			hit.cell.2 as f32 + 0.5,
		);
		if !digblock_core::spawn::stands_clear(eye, &|x, y, z| self.world_cell(x, y, z)) {
			return self.reply(
				conn_id,
				&format!(
					"not a valid spawn - needs solid ground {} blocks out, \
					 with {} clear and dry above it",
					digblock_core::spawn::CLEAR,
					digblock_core::spawn::CAPSULE,
				),
			);
		}
		let relocked = self.spawn_store.lock().is_some();
		self.spawn_store.set_point(eye.to_array());
		log::info!("admin set spawn to {eye}");
		self.reply(conn_id, &format!("spawn point set to {}", at(eye)));
		if relocked {
			// The lock came along: the old cells are editable again.
			self.reply(conn_id, &describe_lock(eye));
		}
	}

	/// Set which way players face when they spawn — the point stays
	/// wherever it is. A cardinal argument, or by default the admin's own
	/// facing, which is the point of doing this in-world: stand at spawn,
	/// look at the view you want new players to open on, run the command.
	///
	/// Flattened to horizontal and re-normalized: a spawn facing is a yaw.
	/// The client levels the pitch on arrival either way, so storing one
	/// would only be a value that never comes back out.
	fn cmd_set_spawn_dir(&mut self, conn_id: u64, args: &[&str]) {
		let usage = "usage: /set-spawn-dir [n|s|e|w] (no argument = your facing)";
		let dir = match args {
			[] => {
				let Some(look) = self
					.clients
					.get(&conn_id)
					.filter(|c| c.live)
					.map(|c| c.look)
				else {
					return;
				};
				Vec3::new(look.x, 0.0, look.z).normalize_or_zero()
			}
			[d] => match cardinal(&d.to_ascii_lowercase()) {
				Some(v) => v,
				None => return self.reply(conn_id, usage),
			},
			_ => return self.reply(conn_id, usage),
		};
		if dir == Vec3::ZERO {
			// Straight up or straight down: there is no yaw to keep.
			return self.reply(
				conn_id,
				"not facing anywhere - look along the ground, or name a direction",
			);
		}
		self.spawn_store.set_dir(dir.to_array());
		log::info!("admin set spawn facing to {dir}");
		self.reply(
			conn_id,
			&format!("players will spawn facing {}", compass(dir)),
		);
	}

	/// Show or toggle spawn protection.
	///
	/// Turning it on resolves the spawn FIRST and locks around the
	/// *result*, never around the raw anchor: locking a spawn someone has
	/// already dug out would freeze a broken one forever, and the "locked
	/// ⇒ skip the search" fast path would then hand that broken spawn to
	/// every joiner. If even the search can't find somewhere intact, the
	/// command refuses rather than pinning a bad answer. A hand-picked
	/// spawn resolves to itself and locks as-is — see `spawn_valid`.
	fn cmd_lock_spawn(&mut self, conn_id: u64, args: &[&str]) {
		let usage = "usage: /lock-spawn [on|off]";
		let arg = args.first().map(|s| s.to_ascii_lowercase());
		if args.len() > 1 {
			return self.reply(conn_id, usage);
		}
		match arg.as_deref() {
			None => {
				let line = match (self.spawn_store.lock(), self.spawn_store.point()) {
					(Some(l), _) => describe_lock(Vec3::from_array(l.eye)),
					// Unlocked, but pinned by hand: it stays put, it just
					// isn't protected from being dug out from under itself.
					(None, Some(p)) => format!(
						"spawn is set at {} but unlocked (its cells can be dug)",
						at(Vec3::from_array(p))
					),
					(None, None) => {
						"spawn is unlocked (it moves aside if players dig it out)".into()
					}
				};
				self.reply(conn_id, &line);
			}
			Some("on") => {
				if let Some(l) = self.spawn_store.lock() {
					let line = describe_lock(Vec3::from_array(l.eye));
					return self.reply(conn_id, &format!("already {line}"));
				}
				let eye = self.spawn_point();
				if !self.spawn_valid(eye) {
					return self.reply(
						conn_id,
						"no clear spawn to lock — nowhere within range is intact; \
						 fill in the excavation and try again",
					);
				}
				self.spawn_store.set_lock(Some(SpawnLock {
					eye: eye.to_array(),
				}));
				log::info!("admin locked spawn at {eye}");
				let line = describe_lock(eye);
				self.reply(conn_id, &line);
			}
			Some("off") => {
				if self.spawn_store.lock().is_none() {
					return self.reply(conn_id, "spawn is already unlocked");
				}
				self.spawn_store.set_lock(None);
				log::info!("admin unlocked spawn");
				self.reply(conn_id, "spawn unlocked - its cells are editable again");
			}
			Some(_) => self.reply(conn_id, usage),
		}
	}

	/// Resolve a display name to exactly one live connection, or a
	/// reply-ready error line. Case-insensitive, same as `/ban`.
	fn find_live_player(&self, name: &str) -> Result<u64, String> {
		if validate_name(name).is_err() {
			return Err(format!("{name} is not a player display name"));
		}
		let matches: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.name.eq_ignore_ascii_case(name))
			.map(|(&id, _)| id)
			.collect();
		match matches[..] {
			[id] if self.clients[&id].live => Ok(id),
			[_] => Err(format!("{name} is still joining")),
			[] => Err(format!("{name} is not online")),
			_ => Err(format!("{} players named {name}", matches.len())),
		}
	}

	fn cmd_goto(&mut self, conn_id: u64, args: &[&str]) {
		let [name] = args else {
			return self.reply(conn_id, "usage: /goto <name>");
		};
		let target = match self.find_live_player(name) {
			Ok(id) => id,
			Err(msg) => return self.reply(conn_id, &msg),
		};
		if target == conn_id {
			return self.reply(conn_id, "that's you");
		}
		// The target's position already holds a player capsule, so it
		// lands clear — no ascent needed.
		let (pos, target_name) = {
			let c = &self.clients[&target];
			(c.player.pos, c.name.clone())
		};
		self.teleport_client(conn_id, pos, Vec3::ZERO);
		if let Some(client) = self.clients.get(&conn_id) {
			log::info!("admin {} teleported to {target_name}", client.name);
		}
		self.reply(conn_id, &format!("teleported to {target_name}"));
	}

	fn cmd_bring(&mut self, conn_id: u64, args: &[&str]) {
		let [name] = args else {
			return self.reply(conn_id, "usage: /bring <name>");
		};
		let target = match self.find_live_player(name) {
			Ok(id) => id,
			Err(msg) => return self.reply(conn_id, &msg),
		};
		if target == conn_id {
			return self.reply(conn_id, "that's you");
		}
		let Some((pos, admin_name)) = self
			.clients
			.get(&conn_id)
			.map(|c| (c.player.pos, c.name.clone()))
		else {
			return;
		};
		let target_name = self.clients[&target].name.clone();
		self.teleport_client(target, pos, Vec3::ZERO);
		self.reply(target, &format!("{admin_name} teleported you"));
		log::info!("admin {admin_name} brought {target_name}");
		self.reply(conn_id, &format!("brought {target_name}"));
	}

	/// `/give` — the one admin command that writes a player's stocks.
	///
	/// ESSENCE and not blocks, deliberately. A block handed out is an item
	/// that can be placed, so the command would be a world edit with no
	/// journal entry behind it and a slot assignment to reconcile; essence
	/// is a pure economy value the standing `InventoryFull` rebase already
	/// carries, so no screen and no wire needs to know a gift happened.
	/// What it is FOR is the reach the economy makes expensive on purpose
	/// — putting a late recipe in front of a tester, or repaying a player
	/// what a server bug cost them.
	fn cmd_give(&mut self, conn_id: u64, args: &[&str]) {
		let usage = "usage: /give <name> <flora|earth|aqua|metal|aether> [n]";
		if !(args.len() == 2 || args.len() == 3) {
			return self.reply(conn_id, usage);
		}
		let Some(essence) = parse_essence(args[1]) else {
			return self.reply(conn_id, &format!("{} is not an essence - {usage}", args[1]));
		};
		let n: u32 = match args.get(2) {
			None => 1,
			Some(s) => match s.parse() {
				Ok(n) if n > 0 => n,
				_ => return self.reply(conn_id, usage),
			},
		};
		// Giving to yourself is the ordinary case here, unlike `/goto` and
		// `/bring` where it is a no-op worth refusing.
		let target = match self.find_live_player(args[0]) {
			Ok(id) => id,
			Err(msg) => return self.reply(conn_id, &msg),
		};
		let (gained, held, target_name) = {
			let Some(client) = self.clients.get_mut(&target) else {
				return;
			};
			// Saturating, like every other credit into a stack. So the
			// reply is built from what LANDED rather than what was asked
			// for — at the cap those are different numbers, and an admin
			// told the larger one would have no way to find out.
			let before = client.inventory.sub_count(essence);
			client.inventory.add_sub(essence, n);
			let held = client.inventory.sub_count(essence);
			let counts = client.inventory.to_wire();
			client_send(client, &ServerMsg::InventoryFull { counts });
			(held - before, held, client.name.clone())
		};
		let name = essence.name();
		if let Some(admin) = self.clients.get(&conn_id) {
			log::info!("admin {} gave {target_name} {gained} {name}", admin.name);
		}
		if target != conn_id {
			self.reply(target, &format!("you were given {gained} {name}"));
		}
		let short = if gained < n {
			format!(" ({target_name} is at the {name} cap; {n} asked)")
		} else {
			String::new()
		};
		self.reply(
			conn_id,
			&format!("gave {target_name} {gained} {name} - now {held}{short}"),
		);
	}

	fn cmd_time(&mut self, conn_id: u64, args: &[&str]) {
		let usage = "usage: /time <hh:mm> (24h; 6:00 sunrise, 12:00 noon, 18:00 sunset)";
		let (h, m) = match args {
			[] => {
				let clock = clock_of_tod(self.current_tod());
				return self.reply(conn_id, &format!("time is {clock}"));
			}
			[arg] => match parse_clock(arg) {
				Some(t) => t,
				None => return self.reply(conn_id, usage),
			},
			_ => return self.reply(conn_id, usage),
		};
		// tod falls as time advances, so shifting the clock forward by
		// (current − target) degrees lands on the target; the offset
		// rides the tick clock, so time keeps flowing from the new
		// anchor.
		let delta =
			(f64::from(self.current_tod()) - f64::from(tod_of_clock(h, m))).rem_euclid(360.0);
		self.tod_offset_secs += delta / f64::from(digblock_core::sun::TOD_RATE_DEG_PER_SEC);
		// Re-anchor everyone now instead of waiting out the heartbeat.
		let msg = ServerMsg::TimeOfDay {
			tod: self.current_tod(),
		};
		self.broadcast(&msg);
		log::info!("admin set time to {h:02}:{m:02}");
		self.reply(conn_id, &format!("time set to {h:02}:{m:02}"));
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
	use digblock_core::block::BlockType;
	use digblock_net::messages::{ClientMsg, ServerMsg};
	use digblock_net::protocol::SERVER_ID;
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

	/// `/give` credits the TARGET and rebases them, and every essence is
	/// spellable by the label the screens use — which is the whole reason
	/// `parse_essence` reads `Essence::name` instead of listing spellings.
	#[test]
	fn give_credits_essence_and_rebases_the_player_who_got_it() {
		use digblock_core::block::Essence;
		let mut sim = admin_sim("give");
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		sim.set_admin_name(Some("boss".into()));
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		assert_matches!(
			again.msgs.first(),
			Some(ServerMsg::Refused { reason }) if reason == "banned"
		);
		// Unbanned (by name) → welcome again.
		sim.set_admin_name(Some("boss".into()));
		let mut admin = connect_named(&mut sim, 1, "boss");
		sim.tick_once(vec![chat(1, "/ban-list")]);
		sim.tick_once(vec![chat(1, "/unban griefer")]);
		admin.drain();
		assert!(server_lines(&admin).iter().any(|t| t.contains("griefer")));
		assert!(server_lines(&admin).iter().any(|t| t.contains("unbanned")));
		let back = try_connect(&mut sim, 2, [0; 16]);
		assert_matches!(back.msgs.first(), Some(ServerMsg::Welcome { .. }));
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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

	/// The lock reports, toggles, and persists — and it pins the region
	/// around the *resolved* spawn, so the fast path ("locked means the
	/// region can't have been broken, so skip the search") stays sound.
	#[test]
	fn lock_spawn_toggles_and_reports() {
		let mut sim = admin_sim("lockspawn");
		let mut admin = connect_named(&mut sim, 1, "boss");

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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		let mut admin = connect_named(&mut sim, 1, "boss");
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
		sim.set_admin_name(Some("boss".into()));
		let _admin = connect_named(&mut sim, 1, "boss");
		wait_live(&mut sim, 1);
		sim.tick_once(vec![chat(1, "/set-spawn-dir n")]);

		// A first join under a real identity banks a position...
		let id = [7u8; 16];
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
		let mut admin = connect_named(&mut sim, 1, "boss");
		sim.tick_once(vec![chat(1, "/lock-spawn maybe")]);
		admin.drain();
		assert!(server_lines(&admin).iter().any(|l| l.contains("usage:")));
		assert!(sim.spawn_lock().is_none(), "bad argument changed the lock");
	}
}
