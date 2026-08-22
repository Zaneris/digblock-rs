//! The spawn point, the walk that finds a valid one, and the
//! teleports that use it. `lock_region` contains every cell the search
//! inspects, which is why a locked spawn stays valid.

use super::*;

impl Sim {
	/// The spawn point to use right now.
	///
	/// Either operator override short-circuits the search. A hand-picked
	/// point (`/set-spawn`) is an explicit decision and stays exactly where
	/// it was put; a locked region is unmodifiable, so it cannot have
	/// stopped being valid and there is nothing to search for. Otherwise
	/// the anchor is re-validated against the journal and the search
	/// relocates sideways if players have dug it out. Stateless on purpose
	/// — a transient hole that happens to be open when someone joins costs
	/// that one player an offset, and the next join snaps back to the
	/// anchor rather than inheriting a stored detour.
	pub(super) fn spawn_point(&self) -> Vec3 {
		if let Some(eye) = self.spawn_store.pinned() {
			return Vec3::from_array(eye);
		}
		if !self.spawn_search {
			return self.spawn_anchor;
		}
		digblock_core::spawn::resolve(self.spawn_anchor, self.store.seed(), &|x, y, z| {
			self.journal_cell(x, y, z)
		})
	}

	/// Whether `eye` passes every spawn rule as the world stands.
	///
	/// Vacuously true for the synthetic-column test seam: those worlds are not
	/// described by real worldgen, so the pristine half would reject a spawn that
	/// is perfectly good in the world the sim actually has.
	///
	/// Also vacuously true for a hand-picked spawn, which this predicate cannot
	/// judge and does not need to. It guards `/lock-spawn` against pinning a
	/// spawn the *search* only returned because it gave up; a point an admin
	/// aimed at has no give-up case and was already checked against the real
	/// world by [`digblock_core::spawn::stands_clear`].
	pub(super) fn spawn_valid(&self, eye: Vec3) -> bool {
		self.spawn_store.point().is_some()
			|| !self.spawn_search
			|| digblock_core::spawn::valid(eye, self.store.seed(), &|x, y, z| {
				self.journal_cell(x, y, z)
			})
	}

	/// The world as it actually is at one cell: the resident chunk (which
	/// already carries its journal overlay) falling back to the journal for
	/// columns that are not loaded. `None` = neither can answer.
	///
	/// Raw cells, not `try_from_id`: a state-carrying cell (a stair, a
	/// slab) must read as its real, solid block, not decode to nothing.
	pub(super) fn world_cell(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.store
			.cell_at(x, y, z)
			.or_else(|| self.journal_cell(x, y, z))
	}

	/// The journal's final value for a cell, or `None` if no player has
	/// ever edited it. Journal ONLY — never the chunk store, which answers
	/// `None` for anything non-resident and would make unloaded columns
	/// read as void. The journal is the complete record: every landed
	/// write, fluid included, is appended to it.
	pub(super) fn journal_cell(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.journal
			.index
			.column(x.div_euclid(16), z.div_euclid(16))
			.and_then(|cells| cells.get(&(x, y, z)).copied())
	}

	/// The facing a spawning player adopts (`/set-spawn-dir`), or
	/// `Vec3::ZERO` for "keep whatever view you have" — the same
	/// convention `Teleported.look` and `Welcome.spawn_look` carry.
	pub(super) fn spawn_look(&self) -> Vec3 {
		self.spawn_store
			.dir()
			.map(Vec3::from_array)
			.unwrap_or(Vec3::ZERO)
	}

	/// The resolved spawn with any residual obstruction stepped over.
	/// The search already guarantees a clear capsule, so this only bites
	/// on the give-up path where nowhere within range was intact.
	pub(super) fn safe_spawn(&self) -> Vec3 {
		self.clear_landing(self.spawn_point())
	}

	/// Raise `eye` until the capsule clears both live terrain and
	/// journaled edits — the shared landing rule for spawn and `/tp`.
	pub(super) fn clear_landing(&self, eye: Vec3) -> Vec3 {
		player::ascend_clear(eye, &|x, y, z| self.world_cell(x, y, z))
	}

	/// Hard-move a client: set state, shed pre-teleport residue (the
	/// drift EWMA and any dig hold describe the old position, and
	/// coasting on the old intent would walk the player off the target),
	/// tell the client, and re-derive residency. `look` zero = keep the
	/// current view.
	pub(super) fn teleport_client(&mut self, conn_id: u64, pos: Vec3, look: Vec3) {
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		client.player.set_state(PlayerState {
			pos,
			vel: Vec3::ZERO,
		});
		client.drift = Vec3::ZERO;
		client.digging = None;
		client.last_intent = PlayerIntent::default();
		// A teleport is an arrival: the destination's journaled columns are
		// no more synced than a join's, and the client would otherwise land
		// on pristine worldgen and be corrected off it. `needs_sync_scan` is
		// set here rather than left to the centre-change check, so a hop
		// within one column still resolves (its scan finds nothing owed and
		// the hold releases the same tick).
		client.arrival_hold = true;
		client.residency.needs_sync_scan = true;
		if look != Vec3::ZERO {
			// The client adopts this facing; mirror it so the server's
			// aim raycasts agree until the next input overwrites it.
			client.look = look;
		}
		client_send(
			client,
			&ServerMsg::Teleported {
				pos: pos.to_array(),
				look: look.to_array(),
			},
		);
		// The interest center jumped; re-derive residency + sync scope.
		self.residency_dirty = true;
	}

	/// Protection is always on: with no stored lock, resolve spawn against
	/// the journal and lock the RESULT — never the raw anchor, so an
	/// existing world whose anchor was dug out locks the relocated spawn
	/// rather than the crater. On the pathological seed where even the
	/// search gives up, boot proceeds unlocked and retries next boot
	/// instead of pinning a bad answer.
	pub(super) fn ensure_spawn_lock(&mut self) {
		if self.spawn_store.lock().is_some() {
			return;
		}
		let eye = self.spawn_point();
		if !self.spawn_valid(eye) {
			log::warn!("spawn protection deferred: no clear spawn to lock");
			return;
		}
		self.spawn_store.set_lock(Some(SpawnLock {
			eye: eye.to_array(),
		}));
		log::info!("spawn locked at {eye}");
	}

	/// Resolve, stamp and record every arrival site: spawn's structure,
	/// and one waystone per laddered biome kind. Idempotent — a stored
	/// site is never re-derived (its region cannot change), and a
	/// re-stamp skips cells the journal already holds — so boot converges
	/// to a no-op after the first run. A kind whose walk finds nothing is
	/// skipped and retried next boot; a port to nowhere must not exist.
	///
	/// The unresolved kinds are derived TOGETHER, because the separation
	/// rule is mutual (`waystones::sites`) — and only when at least one
	/// is missing, so a settled world pays no ring walks at all. Damage
	/// resolution still nudges a pad sideways off whatever players built
	/// through it, which is the one thing that can put a stored site
	/// inside another's exclusion; it moves a pad by a few blocks, and
	/// the alternative is a port standing in a crater.
	pub(super) fn ensure_arrival_sites(&mut self) {
		if let Some(l) = self.spawn_store.lock() {
			self.stamp_site(Vec3::from_array(l.eye));
		}
		let seed = self.store.seed();
		let ladder: Vec<(u8, digblock_core::worldgen::SurfaceKind)> =
			digblock_core::worldgen::SurfaceKind::ALL
				.iter()
				.enumerate()
				.filter(|(_, k)| digblock_core::unlock::waystone(**k).is_some())
				.map(|(i, &k)| (i as u8, k))
				.collect();
		let derived = if ladder
			.iter()
			.any(|&(idx, _)| self.waystone_store.get(idx).is_none())
		{
			digblock_core::waystones::sites(seed)
		} else {
			Vec::new()
		};
		for (idx, kind) in ladder {
			let eye = match self.waystone_store.get(idx) {
				Some(e) => Vec3::from_array(e),
				None => {
					let Some(&(_, a)) = derived.iter().find(|&&(i, _)| i == idx) else {
						log::warn!("no {kind:?} anywhere the walk reaches — port deferred");
						continue;
					};
					// An existing world may have built through the
					// pristine site: resolve sideways off the damage the
					// way spawn does, then the answer is FOREVER.
					let eye = if digblock_core::spawn::valid(a, seed, &|x, y, z| {
						self.journal_cell(x, y, z)
					}) {
						a
					} else {
						digblock_core::spawn::resolve(a, seed, &|x, y, z| {
							self.journal_cell(x, y, z)
						})
					};
					log::info!("waystone resolved: {kind:?} at {eye}");
					self.waystone_store.set(idx, eye.to_array());
					eye
				}
			};
			self.stamp_site(eye);
		}
	}

	/// Apply one site's structure as ordinary journaled edits — the
	/// worldjob write discipline. A resident cell takes the full landed
	/// path; a non-resident one is journal-only, which join sync and the
	/// standing resync repair deliver exactly like any other edit.
	pub(super) fn stamp_site(&mut self, eye: Vec3) {
		for (x, y, z, v) in digblock_core::waystones::stamp(eye) {
			if self.journal_cell(x, y, z) == Some(v) {
				continue;
			}
			if let Some(prev) = self.store.set_cell(x, y, z, v) {
				self.apply_write((x, y, z), v, prev, SERVER_ID, NO_CLIENT_SEQ, true);
			} else {
				let edit = digblock_net::messages::BlockEdit { x, y, z, block: v };
				let (_, new_column) = self.journal.append(edit, SERVER_ID);
				if new_column {
					self.enroll_new_column(edit);
				}
			}
		}
	}

	/// Whether this client's teleport cooldown has run out.
	///
	/// ONE clock behind every teleport — Return Home and every port —
	/// because per-spell timers would let a player ring the roster and
	/// land home with nothing to wait for. The caster's own skills
	/// decide the window, since the server holds them.
	pub(super) fn teleport_ready(&self, client: &Client) -> bool {
		use digblock_core::skills::Skill;
		let cooldown =
			digblock_core::unlock::teleport_cooldown_secs(client.skills.level(Skill::Arcana));
		match client.last_teleport_tick {
			Some(t) => self.tick.wrapping_sub(t) >= cooldown * TICK_RATE,
			None => true,
		}
	}

	/// Teleport a live client back to spawn, subject to the shared
	/// teleport cooldown. Violations are silently ignored — honest
	/// clients gate locally.
	pub(super) fn handle_return_to_spawn(&mut self, conn_id: u64) {
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		if !client.live || !self.teleport_ready(client) {
			return;
		}
		let from = client.player.pos;
		let pos = self.safe_spawn();
		let look = self.spawn_look();
		let tick = self.tick;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		client.last_teleport_tick = Some(tick);
		log::info!("player {} ({}) returned to spawn", client.id, client.name);
		self.teleport_client(conn_id, pos, look);
		// The jump's Arcana pay, derived from the distance it covered —
		// a recast standing at spawn covers nothing and pays nothing.
		self.process_cast(conn_id, digblock_core::skills::home_xp(from.distance(pos)));
	}

	/// A completed cast asking for its spell. The channel was client
	/// pacing; the two REAL gates are validated here — the unlock against
	/// the caster's own Arcana, and the fare, refused BEFORE any debit
	/// (the headroom rule). Wisp and Kindle decode and refuse until their
	/// spells land.
	pub(super) fn handle_cast(&mut self, conn_id: u64, kind: digblock_net::messages::CastKind) {
		use digblock_core::skills::Skill;
		use digblock_net::messages::{CastKind, CastRefusal};
		let refuse = |client: &Client, reason: CastRefusal| {
			client_send(client, &ServerMsg::CastRefused { reason });
		};
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		let idx = match kind {
			CastKind::Waystone(idx) => idx,
			CastKind::Wisp => return self.cast_wisp(conn_id),
			CastKind::KindleHere => return self.cast_kindle(conn_id),
			// The retired aimed form: a cell the client named is exactly
			// what the spell no longer trusts.
			CastKind::Kindle { .. } => return refuse(client, CastRefusal::BadTarget),
		};
		let Some(&skind) = digblock_core::worldgen::SurfaceKind::ALL.get(idx as usize) else {
			return refuse(client, CastRefusal::Locked);
		};
		let (level, eye) = match (
			digblock_core::unlock::waystone(skind),
			self.waystone_store.get(idx),
		) {
			(Some(l), Some(eye)) => (l, Vec3::from_array(eye)),
			// No port on this kind, or a site this world never managed
			// to resolve — a port to nowhere must not exist.
			_ => return refuse(client, CastRefusal::Locked),
		};
		if client.skills.level(Skill::Arcana) < level {
			return refuse(client, CastRefusal::Locked);
		}
		// The same clock Return Home rests on, checked BEFORE the fare so
		// a refusal never follows a debit. Silent, the way a return
		// inside its window is: honest clients gate locally.
		if !self.teleport_ready(client) {
			return;
		}
		let (essence, fare) =
			digblock_core::alchemy::cast_price(skind).expect("a laddered kind has a fare");
		let pos = self.clear_landing(eye);
		let tick = self.tick;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		if !client.inventory.try_take_sub(essence, fare) {
			let client = self.clients.get(&conn_id).expect("checked above");
			return refuse(client, CastRefusal::CantPay);
		}
		// The debit's rebase, the standing answer to every alchemy-side
		// mutation — the offer greyed on the same headroom, so an honest
		// client only ever sees this move the number it predicted.
		let counts = client.inventory.to_wire();
		client_send(client, &ServerMsg::InventoryFull { counts });
		client.last_teleport_tick = Some(tick);
		let id = client.id;
		let name = client.name.clone();
		log::info!("player {id} ({name}) ported to the {skind:?} waystone");
		self.teleport_client(conn_id, pos, Vec3::ZERO);
		self.process_cast(conn_id, digblock_core::skills::cast_xp(fare));
	}

	/// The self-light. The server's whole share is the debit and the pay:
	/// the buff's LIGHT is per-viewer GI, applied client-side (each
	/// viewer computes lighting locally), and its expiry is the caster's
	/// own clock. A refusal answers so a predicted buff can cancel.
	fn cast_wisp(&mut self, conn_id: u64) {
		use digblock_net::messages::CastRefusal;
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		let level = client.skills.level(digblock_core::skills::Skill::Arcana);
		let (essence, fare) = digblock_core::alchemy::wisp_price(level);
		if !client.inventory.try_take_sub(essence, fare) {
			client_send(
				client,
				&ServerMsg::CastRefused {
					reason: CastRefusal::CantPay,
				},
			);
			return;
		}
		client.wisp_until = Some(self.tick + digblock_core::unlock::wisp_secs(level) * TICK_RATE);
		let counts = client.inventory.to_wire();
		client_send(client, &ServerMsg::InventoryFull { counts });
		self.process_cast(conn_id, digblock_core::skills::cast_xp(fare));
	}

	/// Conjured light: validate the rung and the host — the cell the
	/// caster's head is in, as the SERVER holds it — then debit, stand
	/// the light on the sim's clock and tell everyone. Not a cell write:
	/// a light is never matter, and nothing about it reaches the journal.
	fn cast_kindle(&mut self, conn_id: u64) {
		use digblock_core::skills::Skill;
		use digblock_net::messages::CastRefusal;
		let refuse = |client: &Client, reason: CastRefusal| {
			client_send(client, &ServerMsg::CastRefused { reason });
		};
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		let level = client.skills.level(Skill::Arcana);
		if level < digblock_core::unlock::KINDLE_AT {
			return refuse(client, CastRefusal::Locked);
		}
		let at = digblock_core::kindle::host_cell(client.player.pos);
		if !digblock_core::kindle::hosts(self.store.cell_at(at.0, at.1, at.2)) {
			return refuse(client, CastRefusal::BadTarget);
		}
		let (essence, fare) = digblock_core::alchemy::kindle_price(level);
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		if !client.inventory.try_take_sub(essence, fare) {
			let client = self.clients.get(&conn_id).expect("checked above");
			return refuse(client, CastRefusal::CantPay);
		}
		let counts = client.inventory.to_wire();
		client_send(client, &ServerMsg::InventoryFull { counts });
		let light = Kindled {
			cell: [at.0, at.1, at.2],
			until: self.tick + digblock_core::kindle::SECS * TICK_RATE,
		};
		self.broadcast(&self.kindled_msg(&light));
		self.kindled.push(light);
		self.process_cast(conn_id, digblock_core::skills::cast_xp(fare));
	}

	/// One light as the wire carries it, its remaining time read off the
	/// sim's clock at send time — so the cast and a join a minute later
	/// hand out the same expiry.
	pub(super) fn kindled_msg(&self, light: &Kindled) -> ServerMsg {
		let ticks_left = light.until.saturating_sub(self.tick);
		ServerMsg::Kindled {
			cell: light.cell,
			secs_left: u16::try_from(ticks_left.div_ceil(TICK_RATE)).unwrap_or(u16::MAX),
		}
	}

	/// The standing lights, for a joiner's backlog.
	pub(super) fn kindled_msgs(&self) -> Vec<ServerMsg> {
		self.kindled.iter().map(|k| self.kindled_msg(k)).collect()
	}

	/// The cast chokepoint — Arcana's twin of `process_alchemy`'s award
	/// tail: the one place a landed cast becomes XP, the wire total, the
	/// persisted record and the level-up announcement. Offline play
	/// awards through the same core tables in the engine's offline arm.
	pub(super) fn process_cast(&mut self, conn_id: u64, xp: u64) {
		use digblock_core::skills::Skill;
		if xp == 0 {
			return;
		}
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		let before = client.skills.level(Skill::Arcana);
		let total = client.skills.add_xp(Skill::Arcana, xp);
		client_send(
			client,
			&ServerMsg::XpGain {
				skill: Skill::Arcana as u8,
				total,
			},
		);
		let after = client.skills.level(Skill::Arcana);
		let id = client.id;
		if let Some(hash) = client.identity {
			self.player_store.put(hash, client.record());
		}
		if after > before {
			self.announce_level_up(id, Skill::Arcana, after);
		}
	}
}
