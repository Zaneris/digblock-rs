//! Admin chat commands (`--admin <account-id>`): dispatched from the Chat
//! arm for slash-lines from an admin ACCOUNT, never broadcast. Every
//! outcome is answered with a private typed `AdminReply` — the server
//! names what happened and `voice` chooses the words. The command table is
//! the single source of truth: it drives the dispatcher, `/help`, and
//! the `AdminCommands` advertisement the client autocompletes from.

use super::{OutMsg, Sim, client_send};
use digblock_core::block::Essence;
use digblock_core::compass::{cardinal, compass};
use digblock_core::raycast::{REACH, raycast};
use digblock_net::messages::{AdminReply, AdminUsage, NoPlayer, ServerMsg};
use digblock_net::protocol::validate_name;
use glam::Vec3;

/// `(usage, description)` — usage's first token is the dispatch key and
/// the client's autocomplete stem.
const COMMANDS: &[(&str, &str)] = &[
	("/help", "list admin commands"),
	(
		"/tp <x> <z> [n|s|e|w]",
		"land on the column top; ~ or ~<d> is relative",
	),
	("/goto <name>", "go to a player"),
	(
		"/time [hh:mm]",
		"show or set the clock; 6:00 sunrise, 18:00 sunset",
	),
	("/bring <name>", "pull a player to you"),
	(
		"/give <name> <essence> [n]",
		"credit essence; naming none lists them",
	),
	(
		"/reset <name>",
		"testing: wipe a player's skills and inventory",
	),
	("/ban <name>", "kick a player and ban their IP"),
	("/ban-list", "list bans"),
	("/unban <name-or-ip>", "remove a ban"),
	("/set-spawn", "move spawn to the block you aim at"),
	(
		"/set-spawn-dir [n|s|e|w]",
		"set spawn facing; no argument uses yours",
	),
];

/// `/tp` coordinate clamp: far past any playable range, comfortably
/// inside f32 world-position precision.
const TP_LIMIT: i64 = 1_000_000;

/// Where a spawn eye reads as coordinates: the FLOOR block, not the eye.
/// An admin who aimed at a block and got told a number two higher would
/// reasonably think the command missed, and it is the same y the lock
/// region reports as `y0`.
/// The block a spawn eye stands on, as a cell.
fn at(eye: Vec3) -> [i32; 3] {
	[
		eye.x.floor() as i32,
		eye.y.floor() as i32 - 2,
		eye.z.floor() as i32,
	]
}

/// The lock's own description, as the cell it stands on and the bounds it protects — the region's
/// enclosing box, since the wire field is a flat box and membership questions never ride this
/// reply. `already` distinguishes reporting a standing lock from announcing a new one — the two
/// differ only in wording, which is exactly why the server must not be the one choosing.
fn describe_lock(eye: Vec3, already: bool) -> AdminReply {
	AdminReply::SpawnLocked {
		cell: at(eye),
		region: digblock_core::spawn::lock_region(eye).bounds(),
		already,
	}
}

/// A private server→player answer. Typed rather than composed: the
/// words are `voice`'s, and the server has no business holding a
/// sentence a player reads.
pub(super) fn server_reply(r: AdminReply) -> ServerMsg {
	ServerMsg::Admin(r)
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
/// The inverse of [`tod_of_clock`], as `(hour, minute)`. The colon
/// between them is punctuation, and punctuation is the client's.
fn clock_of_tod(tod: f32) -> (u32, u32) {
	let mins = (((180.0 - tod) / 15.0 * 60.0).round() as i64).rem_euclid(24 * 60);
	((mins / 60) as u32, (mins % 60) as u32)
}

/// An essence by name, case-insensitively.
///
/// Matched against [`Essence::ident`] — the untranslated operator's word — rather than a second
/// list of spellings, so the command and the usage line cannot come to disagree. Not the screens'
/// LABEL: an admin types the same word whatever language the client is showing.
fn parse_essence(arg: &str) -> Option<Essence> {
	Essence::ALL
		.into_iter()
		.find(|e| e.ident().eq_ignore_ascii_case(arg))
}

/// Absolute integer, or `~`/`~±d` relative to `cur`. Clamped.
///
/// The relative add SATURATES: `off` is an unbounded `i64` straight off the wire, so `cur + off`
/// overflows for any offset near `i64::MAX`, and the clamp below runs too late to help. Debug
/// builds take `overflow-checks` and panic the sim thread outright, stopping ticks.
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
					self.reply(
						conn_id,
						AdminReply::CommandHint {
							usage: (*usage).to_string(),
							desc: (*desc).to_string(),
						},
					);
				}
				self.reply(conn_id, AdminReply::CompassHint);
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
			other => self.reply(
				conn_id,
				AdminReply::UnknownCommand {
					typed: other.to_string(),
				},
			),
		}
	}

	/// Private reply. No chunking any more: the sentence is composed at
	/// the client, so how long it runs is a rendering concern there and
	/// not a chat-length cap here.
	fn reply(&self, conn_id: u64, r: AdminReply) {
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		client_send(client, &server_reply(r));
	}

	fn cmd_tp(&mut self, conn_id: u64, args: &[&str]) {
		let usage = AdminReply::Usage(AdminUsage::Tp);
		if !(args.len() == 2 || args.len() == 3) {
			return self.reply(conn_id, usage.clone());
		}
		let Some(cur) = self.clients.get(&conn_id).map(|c| c.player.pos) else {
			return;
		};
		let (Some(x), Some(z)) = (parse_coord(args[0], cur.x), parse_coord(args[1], cur.z)) else {
			return self.reply(conn_id, usage.clone());
		};
		let look = match args.get(2) {
			None => Vec3::ZERO,
			Some(d) => match cardinal(&d.to_ascii_lowercase()) {
				Some(v) => v,
				None => return self.reply(conn_id, usage.clone()),
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
		self.reply(conn_id, AdminReply::Teleported { x, z });
	}

	/// Hand-pick the spawn: the block under the admin's crosshair becomes the ground players
	/// stand on. The TOP face specifically — the cell the ray hits is the floor, so a side or
	/// bottom hit would mean a spawn hanging beside a wall or buried in a ceiling. Validated by
	/// [`digblock_core::spawn::stands_clear`], which reads the world as it IS rather than
	/// pristine worldgen: setting spawn by hand means somewhere terrain would never choose.
	fn cmd_set_spawn(&mut self, conn_id: u64, args: &[&str]) {
		if !args.is_empty() {
			return self.reply(conn_id, AdminReply::Usage(AdminUsage::SetSpawn));
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
			return self.reply(conn_id, AdminReply::NotLookingAtBlock);
		};
		if hit.normal != (0, 1, 0) {
			return self.reply(conn_id, AdminReply::NotLookingAtTopFace);
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
				AdminReply::SpawnNotClear {
					clear: digblock_core::spawn::CLEAR as u32,
					capsule: digblock_core::spawn::CAPSULE as u32,
				},
			);
		}
		self.spawn_store.set_point(eye.to_array());
		log::info!("admin set spawn to {eye}");
		// The structure follows the point: the new pad is stamped before
		// the lock report, and the old site's blocks stay behind as
		// ordinary, now-unprotected edits.
		self.stamp_site(eye);
		self.reply(conn_id, AdminReply::SpawnPointSet { cell: at(eye) });
		// The lock came along — protection is always on — so the old
		// cells are editable again and the new region is announced.
		self.reply(conn_id, describe_lock(eye, false));
	}

	/// Set which way players face when they spawn — the point stays wherever it is. A cardinal
	/// argument, or by default the admin's own facing: stand at spawn, look at the view you want
	/// new players to open on, run the command. Flattened to horizontal and re-normalized, since a
	/// spawn facing is a yaw — the client levels the pitch on arrival either way, so storing one
	/// would only be a value that never comes back out.
	fn cmd_set_spawn_dir(&mut self, conn_id: u64, args: &[&str]) {
		let usage = AdminReply::Usage(AdminUsage::SetSpawnDir);
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
				None => return self.reply(conn_id, usage.clone()),
			},
			_ => return self.reply(conn_id, usage.clone()),
		};
		if dir == Vec3::ZERO {
			// Straight up or straight down: there is no yaw to keep.
			return self.reply(conn_id, AdminReply::NotFacingAnywhere);
		}
		self.spawn_store.set_dir(dir.to_array());
		log::info!("admin set spawn facing to {dir}");
		self.reply(
			conn_id,
			AdminReply::SpawnFacingSet {
				compass: compass(dir).to_string(),
			},
		);
	}

	/// Resolve a display name to exactly one live connection, or a
	/// reply-ready error line. Case-insensitive, same as `/ban`.
	fn find_live_player(&self, name: &str) -> Result<u64, AdminReply> {
		let no = |why| AdminReply::NoSuchPlayer {
			name: name.to_string(),
			why,
		};
		if validate_name(name).is_err() {
			return Err(no(NoPlayer::NotAName));
		}
		let matches: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.name.eq_ignore_ascii_case(name))
			.map(|(&id, _)| id)
			.collect();
		match matches[..] {
			[id] if self.clients[&id].live => Ok(id),
			[_] => Err(no(NoPlayer::StillJoining)),
			[] => Err(no(NoPlayer::NotOnline)),
			_ => Err(no(NoPlayer::Ambiguous {
				count: matches.len() as u32,
			})),
		}
	}

	fn cmd_goto(&mut self, conn_id: u64, args: &[&str]) {
		let [name] = args else {
			return self.reply(conn_id, AdminReply::Usage(AdminUsage::Goto));
		};
		let target = match self.find_live_player(name) {
			Ok(id) => id,
			Err(why) => return self.reply(conn_id, why),
		};
		if target == conn_id {
			return self.reply(conn_id, AdminReply::ThatIsYou);
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
		self.reply(conn_id, AdminReply::TeleportedTo { name: target_name });
	}

	fn cmd_bring(&mut self, conn_id: u64, args: &[&str]) {
		let [name] = args else {
			return self.reply(conn_id, AdminReply::Usage(AdminUsage::Bring));
		};
		let target = match self.find_live_player(name) {
			Ok(id) => id,
			Err(why) => return self.reply(conn_id, why),
		};
		if target == conn_id {
			return self.reply(conn_id, AdminReply::ThatIsYou);
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
		log::info!("admin {admin_name} brought {target_name}");
		self.reply(target, AdminReply::TeleportedYouBy { admin: admin_name });
		self.reply(conn_id, AdminReply::Brought { name: target_name });
	}

	/// `/give` — the one admin command that writes a player's stocks, and ESSENCE rather than
	/// blocks deliberately: a block handed out would be a world edit with no journal entry
	/// behind it and a slot to reconcile, while essence is a pure economy value the standing
	/// `InventoryFull` rebase already carries. It is FOR the reach the economy makes
	/// expensive on purpose — a late recipe in front of a tester, or repaying a server bug.
	fn cmd_give(&mut self, conn_id: u64, args: &[&str]) {
		// The usage line names the essences at the point it is RENDERED, out of `Essence::ALL`, so
		// the advertisement and `parse_essence` read one vocabulary and cannot drift apart.
		let usage = AdminReply::Usage(AdminUsage::Give);
		if !(args.len() == 2 || args.len() == 3) {
			return self.reply(conn_id, usage.clone());
		}
		let Some(essence) = parse_essence(args[1]) else {
			return self.reply(
				conn_id,
				AdminReply::NotAnEssence {
					typed: args[1].to_string(),
				},
			);
		};
		let n: u32 = match args.get(2) {
			None => 1,
			Some(s) => match s.parse() {
				Ok(n) if n > 0 => n,
				_ => return self.reply(conn_id, usage.clone()),
			},
		};
		// Giving to yourself is the ordinary case here, unlike `/goto` and
		// `/bring` where it is a no-op worth refusing.
		let target = match self.find_live_player(args[0]) {
			Ok(id) => id,
			Err(why) => return self.reply(conn_id, why),
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
		let index = essence as u16;
		if target != conn_id {
			self.reply(
				target,
				AdminReply::WereGiven {
					essence: index,
					gained,
				},
			);
		}
		// `asked` rides along beside `gained`: at the cap they differ, and
		// an admin told only the larger number would have no way to find
		// out. Which of the two the line mentions is the client's call.
		self.reply(
			conn_id,
			AdminReply::Gave {
				name: target_name,
				essence: index,
				gained,
				asked: n,
				held,
			},
		);
	}

	fn cmd_time(&mut self, conn_id: u64, args: &[&str]) {
		let usage = AdminReply::Usage(AdminUsage::Time);
		let (h, m) = match args {
			[] => {
				let clock = clock_of_tod(self.current_tod());
				return self.reply(
					conn_id,
					AdminReply::TimeIs {
						hour: clock.0,
						minute: clock.1,
					},
				);
			}
			[arg] => match parse_clock(arg) {
				Some(t) => t,
				None => return self.reply(conn_id, usage.clone()),
			},
			_ => return self.reply(conn_id, usage.clone()),
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
		self.reply(conn_id, AdminReply::TimeSet { hour: h, minute: m });
	}

	/// `/reset` — the testing lever: a named player back to minute one. Zeroes both skills and
	/// empties the inventory, tools included. The wire needs only standing machinery: a full
	/// `Skills` sync, which the client adopts wholesale and re-syncs the node reveal from (a level
	/// DROP un-reveals veins exactly as joining at that level would), and the `InventoryFull`
	/// rebase — both ends then run `reconcile_layout`, so every slot sweeps with no Layout frame.
	fn cmd_reset(&mut self, conn_id: u64, args: &[&str]) {
		let usage = AdminReply::Usage(AdminUsage::Reset);
		let [name] = args else {
			return self.reply(conn_id, usage.clone());
		};
		// Resetting yourself is the ordinary case, as with /give: the
		// tester wiping their own character is what the command is FOR.
		let target = match self.find_live_player(name) {
			Ok(id) => id,
			Err(why) => return self.reply(conn_id, why),
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
			self.reply(target, AdminReply::YouWereReset);
		}
		self.reply(conn_id, AdminReply::ResetPlayer { name: target_name });
	}

	fn cmd_ban(&mut self, conn_id: u64, args: &[&str]) {
		let [name] = args else {
			return self.reply(conn_id, AdminReply::Usage(AdminUsage::Ban));
		};
		if validate_name(name).is_err() {
			return self.reply(conn_id, AdminReply::Usage(AdminUsage::BanName));
		}
		let matches: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.name.eq_ignore_ascii_case(name))
			.map(|(&id, _)| id)
			.collect();
		let [target] = matches[..] else {
			// Names are unique per account at the master now, so two live
			// matches means two connections of the same account — which
			// handle_hello already refuses. Kept as a guard rather than
			// deleted: this branch is the difference between refusing and
			// banning the wrong person.
			let why = if matches.is_empty() {
				NoPlayer::NotOnline
			} else {
				NoPlayer::Ambiguous {
					count: matches.len() as u32,
				}
			};
			return self.reply(
				conn_id,
				AdminReply::NoSuchPlayer {
					name: name.to_string(),
					why,
				},
			);
		};
		if target == conn_id {
			return self.reply(conn_id, AdminReply::NotBanningYourself);
		}
		let (ip, stored_name, account) = {
			let c = &self.clients[&target];
			(c.addr.ip(), c.name.clone(), c.account)
		};
		if self.clients[&conn_id].addr.ip() == ip {
			// Kicking every connection on the banned IP would include the
			// admin — almost certainly two names from one household/NAT.
			return self.reply(
				conn_id,
				AdminReply::SharesYourIp {
					name: name.to_string(),
				},
			);
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
		self.reply(
			conn_id,
			AdminReply::Banned {
				name: stored_name,
				ip: ip.to_string(),
			},
		);
	}

	fn cmd_ban_list(&mut self, conn_id: u64) {
		if self.ban_store.entries().is_empty() {
			return self.reply(conn_id, AdminReply::NoBans);
		}
		let rows: Vec<AdminReply> = self
			.ban_store
			.entries()
			.iter()
			.map(|e| AdminReply::BanEntry {
				name: e.name.clone(),
				ip: e.ip.to_string(),
			})
			.collect();
		for row in rows {
			self.reply(conn_id, row);
		}
	}

	fn cmd_unban(&mut self, conn_id: u64, args: &[&str]) {
		let [arg] = args else {
			return self.reply(conn_id, AdminReply::Usage(AdminUsage::Unban));
		};
		let removed = self.ban_store.unban_matching(arg);
		log::info!("admin unban {arg}: {removed} removed");
		self.reply(
			conn_id,
			if removed == 0 {
				AdminReply::NoBanMatches {
					arg: arg.to_string(),
				}
			} else {
				AdminReply::Unbanned {
					arg: arg.to_string(),
					removed: removed as u32,
				}
			},
		);
	}
}

#[cfg(test)]
mod tests;
