//! Admin chat commands (`--admin <account-id>`): dispatched from the Chat
//! arm for slash-lines from an admin ACCOUNT, never broadcast. Every
//! outcome is
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
		"credit essence stocks; the reagent names are the ones the screens \
		 print, and /give with no essence lists them. n defaults to one",
	),
	(
		"/reset <name>",
		"testing: zero an online player's skills and wipe their inventory, tools included",
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
/// Matched against [`Essence::ident`] — the untranslated operator's word
/// — rather than a second list of spellings, so the command and the
/// usage line cannot come to disagree, and a new essence is takeable the
/// day its row exists. Not the screens' LABEL: an admin types the same
/// word whatever language the client is showing.
fn parse_essence(arg: &str) -> Option<Essence> {
	Essence::ALL
		.into_iter()
		.find(|e| e.ident().eq_ignore_ascii_case(arg))
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
			"/reset" => self.cmd_reset(conn_id, &args),
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

	/// Hand-pick the spawn: the block under the admin's crosshair becomes the
	/// ground players stand on.
	///
	/// The TOP face specifically, not any face. The cell the ray hits is the
	/// floor, so a side or bottom hit would quietly mean a spawn hanging in the
	/// air beside a wall or buried in a ceiling.
	///
	/// Validated by [`digblock_core::spawn::stands_clear`], which reads the world
	/// as it actually is rather than pristine worldgen: the whole point of
	/// setting spawn by hand is to put it somewhere terrain alone would never
	/// have chosen.
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
		// Spelled from `Essence::ident` like `parse_essence` is, rather
		// than typed out again: the usage line and the parser were two
		// copies of one vocabulary, and the rename that gave the essences
		// their reagent names moved only the parser — leaving `/help` and
		// this line advertising four words the command now refuses, in a
		// refusal that repeated the word it had just rejected.
		let names = Essence::ALL.map(|e| e.ident()).join("|");
		let usage = format!("usage: /give <name> <{names}> [n]");
		let usage = usage.as_str();
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

	/// `/reset` — the testing lever: a named player back to minute one. Zeroes
	/// both skills and empties the inventory, tools included, so the
	/// fresh-player opening can be walked on a live server without minting a new
	/// account.
	///
	/// The wire needs only the standing machinery: a full `Skills` sync, which
	/// the client adopts wholesale and re-syncs the node reveal from (a level
	/// DROP un-reveals veins exactly as joining at that level would), and the
	/// `InventoryFull` rebase — both ends run the same `reconcile_layout` over
	/// the zeroed counts, so every slot sweeps with no Layout frame needed.
	fn cmd_reset(&mut self, conn_id: u64, args: &[&str]) {
		let usage = "usage: /reset <name>";
		let [name] = args else {
			return self.reply(conn_id, usage);
		};
		// Resetting yourself is the ordinary case, as with /give: the
		// tester wiping their own character is what the command is FOR.
		let target = match self.find_live_player(name) {
			Ok(id) => id,
			Err(msg) => return self.reply(conn_id, &msg),
		};
		let target_name = {
			let Some(client) = self.clients.get_mut(&target) else {
				return;
			};
			client.skills = digblock_core::skills::SkillSet::default();
			client.inventory = digblock_core::inventory::Inventory::default();
			// A wiped hold has nothing left to complete against.
			client.digging = None;
			digblock_core::inv_grid::reconcile_layout(
				&client.inventory,
				&mut client.hotbar,
				&mut client.grid,
			);
			client_send(
				client,
				&ServerMsg::Skills {
					xp: client.skills.to_wire(),
				},
			);
			let counts = client.inventory.to_wire();
			client_send(client, &ServerMsg::InventoryFull { counts });
			client.name.clone()
		};
		if let Some(admin) = self.clients.get(&conn_id) {
			log::info!("admin {} reset {target_name}", admin.name);
		}
		if target != conn_id {
			self.reply(target, "your skills and inventory were reset");
		}
		self.reply(
			conn_id,
			&format!("reset {target_name} - skills zeroed, inventory wiped"),
		);
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
				format!("{name} is not online (bans need the player's address)")
			} else {
				// Names are unique per account at the master now, so two
				// live matches means two connections of the same account —
				// which handle_hello already refuses. Kept as a guard rather
				// than deleted: this branch is the difference between
				// refusing and banning the wrong person.
				format!("{} players named {name} - can't ban by name", matches.len())
			};
			return self.reply(conn_id, &msg);
		};
		if target == conn_id {
			return self.reply(conn_id, "not banning yourself");
		}
		let (ip, stored_name, account) = {
			let c = &self.clients[&target];
			(c.addr.ip(), c.name.clone(), c.account)
		};
		if self.clients[&conn_id].addr.ip() == ip {
			// Kicking every connection on the banned IP would include the
			// admin — almost certainly two names from one household/NAT.
			return self.reply(conn_id, &format!("{name} shares your IP - not banning it"));
		}
		self.ban_store.ban(&stored_name, ip, account);
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
mod tests;
