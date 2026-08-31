//! The spawn point, the walk that finds a valid one, and the
//! teleports that use it. `lock_region` contains every cell the search
//! inspects, which is why a locked spawn stays valid.

use super::*;

/// Cast attempts one client may make back to back, and the rate the bucket refills at. Sized for
/// the spellbook rather than for the wire: a channel outlasts the whole burst, so no honest client
/// can reach it, while the reliable stream admits casts far faster than a spellbook can offer them
/// — and every one is a sphere derived and scanned against every player before it can be refused.
pub(super) const CAST_BURST: f32 = 6.0;
pub(super) const CAST_RATE_PER_SEC: u32 = 4;

/// Standing kindled lights the world may hold at once. The list is a server-wide backlog handed to
/// every joiner and only expiry takes an entry off it, so without a ceiling a few minutes of
/// casting is an unbounded send at every join for as long as the lights burn. One light per cell (a
/// recast REFRESHES) is what makes the ceiling generous: reaching it takes that many distinct lit
/// cells alive at once.
pub(super) const MAX_KINDLED: usize = 256;

impl Sim {
	/// The spawn point to use right now. Either operator override short-circuits the search: a
	/// hand-picked point stays where it was put, and a locked region cannot have stopped being
	/// valid. Otherwise the anchor is re-validated against the journal and the search relocates
	/// sideways if players have dug it out. Stateless on purpose — a transient hole open at one
	/// join costs that player an offset, and the next join snaps back rather than inherit a detour.
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

	/// Whether `eye` passes every spawn rule as the world stands. Vacuously true for the
	/// synthetic-column test seam, whose worlds real worldgen does not describe, and for a
	/// hand-picked spawn, which this predicate cannot judge and need not: it exists to stop a
	/// spawn the *search* returned only because it gave up being pinned, and an admin's point has
	/// no give-up case and was already checked by [`digblock_core::spawn::stands_clear`].
	pub(super) fn spawn_valid(&self, eye: Vec3) -> bool {
		self.spawn_store.point().is_some()
			|| !self.spawn_search
			|| digblock_core::spawn::valid(eye, self.store.seed(), &|x, y, z| {
				self.journal_cell(x, y, z)
			})
	}

	/// The world as it actually is at one cell: the resident chunk (which already carries its
	/// journal overlay) falling back to the journal for columns that are not loaded, `None` when
	/// neither can answer. Raw cells, not `try_from_id`: a state-carrying cell such as a stair or
	/// a slab must read as its real, solid block rather than decode to nothing.
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
		// Before the interest centre moves: a causeway is retired by the
		// trail that laid it and only a resident column can be written, so
		// a road left standing a world away is a road nothing can ever
		// take up. The SPAN survives — the caster keeps the seconds they
		// paid for, and the road resumes under them where they land.
		self.retire_road(conn_id);
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

	/// Protection is always on: with no stored lock, resolve spawn against the journal and lock
	/// the RESULT — never the raw anchor, so an existing world whose anchor was dug out locks the
	/// relocated spawn rather than the crater. Where even the search gives up, boot proceeds
	/// unlocked and retries next boot instead of pinning a bad answer.
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

	/// Resolve, stamp and record every arrival site: spawn's structure, and one waystone per
	/// laddered biome kind. Idempotent, so boot converges to a no-op; a kind whose walk finds
	/// nothing is skipped and retried next boot, because a port to nowhere must not exist. The
	/// unresolved kinds are derived TOGETHER, since `waystones::sites`' separation rule is mutual,
	/// and only when one is missing, so a settled world pays no ring walks at all.
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
					// pristine site: resolve sideways off the damage
					// the way spawn does, then the answer is FOREVER.
					// The nudge is the one thing that can land a site
					// inside another's exclusion; a crater is worse.
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
		for w in digblock_core::waystones::stamp(eye) {
			let (x, y, z, v) = (w.x, w.y, w.z, w.to);
			if !w.owed(self.journal_cell(x, y, z)) {
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

	/// Whether this client's teleport cooldown has run out. ONE clock behind every teleport —
	/// Return Home and every port — because per-spell timers would let a player ring the roster
	/// and land home with nothing to wait for. The caster's own skills decide the window, since
	/// the server holds them.
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
		{
			// The bucket, spent HERE so no spell can be added outside it, and ahead of every
			// spell's validation so a flood pays for nothing. Silent when it runs dry, like the
			// teleport window and the bench drip: an honest client cannot reach it, and a refusal
			// is a sentence about the caster, not about their client.
			let Some(client) = self.clients.get_mut(&conn_id) else {
				return;
			};
			if !client.live || client.cast_tokens < 1.0 {
				return;
			}
			client.cast_tokens -= 1.0;
		}
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		let idx = match kind {
			CastKind::Waystone(idx) => idx,
			CastKind::Wisp => return self.cast_wisp(conn_id),
			CastKind::KindleHere => return self.cast_kindle(conn_id),
			CastKind::Sphere { slot } => return self.cast_sphere(conn_id, slot),
			CastKind::Causeway => return self.cast_causeway(conn_id),
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
		self.process_cast(conn_id, digblock_core::skills::cast_xp(essence, fare));
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
		let counts = client.inventory.to_wire();
		client_send(client, &ServerMsg::InventoryFull { counts });
		self.light_wisp(conn_id);
		self.process_cast(conn_id, digblock_core::skills::cast_xp(essence, fare));
	}

	/// Stand the caster's wisp for their rung's span from now — a recast refreshes rather than
	/// stacks. The Sphere's seal lights one too, so the lighting is apart from the wisp's debit.
	fn light_wisp(&mut self, conn_id: u64) {
		let tick = self.tick;
		if let Some(client) = self.clients.get_mut(&conn_id) {
			let level = client.skills.level(digblock_core::skills::Skill::Arcana);
			client.wisp_until = Some(tick + digblock_core::unlock::wisp_secs(level) * TICK_RATE);
		}
	}

	/// Conjured light: validate the rung, the host — the cell the caster's head is in, as the
	/// SERVER holds it — and the world's room for another light, then debit, stand the light on
	/// the sim's clock and tell everyone. Not a cell write: a light is never matter and nothing
	/// about it reaches the journal. One light per CELL — a recast refreshes that light's expiry
	/// instead of standing a second, which keeps [`MAX_KINDLED`] a ceiling on PLACES, not casts.
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
		let cell = [at.0, at.1, at.2];
		let standing = self.kindled.iter().position(|k| k.cell == cell);
		// The ceiling, checked BEFORE the debit like every other refusal
		// — and only for a light that would be a NEW place, since a
		// refresh takes no room.
		if standing.is_none() && self.kindled.len() >= MAX_KINDLED {
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
			cell,
			until: self.tick + digblock_core::kindle::SECS * TICK_RATE,
		};
		match standing {
			Some(i) => self.kindled[i] = light,
			None => self.kindled.push(light),
		}
		self.broadcast(&self.kindled_msg(&light));
		self.process_cast(conn_id, digblock_core::skills::cast_xp(essence, fare));
	}

	/// The Sphere, on `Cast(Sphere)` sent as the caster's channel BEGINS: `sphere::plan` derives
	/// the shell from the head as the SERVER holds it and the declared slot's block, scans it whole
	/// for players and refuses it whole — the Arc's rule; what is accepted is a CLIMB over the
	/// channel's ticks. The cheap gates come FIRST and the plan last, or a check behind a cubic scan
	/// is a core a broke client can pin; the DEBIT — blocks and both fares, all or none — trails all.
	fn cast_sphere(&mut self, conn_id: u64, slot: u8) {
		use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem};
		use digblock_core::skills::Skill;
		use digblock_net::messages::CastRefusal;
		let refuse = |client: &Client, reason: CastRefusal| {
			client_send(client, &ServerMsg::CastRefused { reason });
		};
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		let level = client.skills.level(Skill::Arcana);
		let r = digblock_core::unlock::sphere_radius(level);
		if r == 0 {
			return refuse(client, CastRefusal::Locked);
		}
		// The declared slot, mirrored as an edit's is: a cast is evidence
		// of selection that outran the jitter-buffered input path.
		let held = (usize::from(slot) < HOTBAR_SLOTS).then(|| {
			client.hotbar.set_selected(usize::from(slot));
			client.hotbar.slots()[usize::from(slot)]
		});
		let client = self.clients.get(&conn_id).expect("checked above");
		let block = match held {
			Some(Some(HotbarItem::Block(b))) => b,
			_ => return refuse(client, CastRefusal::NoBlock),
		};
		if !digblock_core::sphere::accepts(block) {
			return refuse(client, CastRefusal::BadBlock);
		}
		if self.closings.iter().any(|w| w.conn_id == conn_id) {
			return refuse(client, CastRefusal::BadTarget);
		}
		// A sphere is hundreds of journaled cells; an unhealthy journal
		// stops it where it stops fluid and the sweeps. `BadTarget` is
		// the nearest true answer the wire has — the caster's rung and
		// purse are both fine, there is nowhere to put the shell.
		if !self.journal.healthy() {
			return refuse(client, CastRefusal::BadTarget);
		}
		let (essence, fare) = digblock_core::alchemy::sphere_price(level);
		let wisp = digblock_core::alchemy::wisp_price(level);
		if !client.inventory.can_pay(&[(essence, fare), wisp]) {
			return refuse(client, CastRefusal::CantPay);
		}
		let head = digblock_core::kindle::host_cell(client.player.pos);
		let Ok(writes) =
			digblock_core::sphere::plan(head, r, block, |c| self.store.cell_at(c.0, c.1, c.2))
		else {
			return refuse(client, CastRefusal::BadTarget);
		};
		let debited = digblock_core::sphere::blocks_needed(&writes, block);
		if client.inventory.count(block) < debited {
			return refuse(client, CastRefusal::BlocksShort);
		}
		// The shell closes on nobody — every live player, the caster
		// included, who stands inside by construction.
		let closes_on_someone = self.clients.values().filter(|c| c.live).any(|c| {
			writes.iter().any(|&(cell, val)| {
				digblock_core::player::placement_blocked(c.player.pos, cell, val)
			})
		});
		if closes_on_someone {
			return refuse(client, CastRefusal::BadTarget);
		}
		let span = digblock_core::unlock::channel_ticks(level);
		let plan_len = writes.len();
		let by = client.id;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		if !client.inventory.try_take_fares(&[(essence, fare), wisp])
			|| !client.inventory.try_take(block, debited)
		{
			// Checked above; cannot fail, and would have taken nothing.
			let client = self.clients.get(&conn_id).expect("checked above");
			return refuse(client, CastRefusal::CantPay);
		}
		let counts = client.inventory.to_wire();
		client_send(client, &ServerMsg::InventoryFull { counts });
		self.closings.push(SphereClosing {
			conn_id,
			by,
			head,
			radius: r,
			block,
			essence,
			fare,
			wisp,
			debited,
			placed: 0,
			plan_len,
			span,
			closing: digblock_core::sphere::Closing::new(writes, span),
		});
		// ONE frame for the whole climb. Every client derives the same
		// plan from the same head, radius and block, so the cells
		// themselves are redundant — and a sphere's worth of
		// `EditApplied` is a fan-out that fills an outbound queue, whose
		// refusal kills the peer it was sent to.
		self.broadcast(&ServerMsg::SphereWinding {
			by,
			head: [head.0, head.1, head.2],
			radius: sphere_radius_wire(r),
			span,
			block: block as u16,
		});
	}

	/// One tick of every sphere still climbing. A caster who has left the server has broken the
	/// channel whatever their client said; one merely walking has not — the climb stays anchored
	/// to the head it began at. The tick's cells land, leaving a cell a player has stepped into
	/// unwritten rather than closed over; the completing tick SEALS the sphere, settles the bill
	/// for what the climb declined and lights the wisp. Protection is re-asked per tick, never
	/// filtered into the plan.
	pub(super) fn step_closings(&mut self) {
		// A sphere's cells are journaled edits like any other: an unhealthy
		// journal stops the climb where it stops fluid and the sweeps,
		// rather than mutating a world nothing can persist.
		if self.closings.is_empty() || !self.journal.healthy() {
			return;
		}
		let mut closings = std::mem::take(&mut self.closings);
		let mut i = 0;
		while i < closings.len() {
			let w = &closings[i];
			let present = self.clients.get(&w.conn_id).is_some_and(|c| c.live);
			if !present {
				let w = closings.remove(i);
				self.unwind_sphere(w);
				continue;
			}
			let regions = self.requester_regions(closings[i].conn_id);
			let w = &mut closings[i];
			let by = w.by;
			let wall = w.wall();
			for (cell, val) in w.closing.land() {
				if digblock_core::protect::immutable(cell.0, cell.1, cell.2, &regions) {
					continue;
				}
				let on_someone = self
					.clients
					.values()
					.filter(|c| c.live)
					.any(|c| digblock_core::player::placement_blocked(c.player.pos, cell, val));
				if on_someone {
					continue;
				}
				if let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) {
					w.closing.record(cell, val, prev);
					if val == wall {
						w.placed += 1;
					}
					self.journal_write(cell, val, by);
				}
			}
			if w.closing.done() {
				let w = closings.remove(i);
				self.refund_sphere(&w, w.debited.saturating_sub(w.placed), false);
				self.sphere_ended(&w, true);
				self.light_wisp(w.conn_id);
				let (we, wf) = w.wisp;
				self.process_cast(
					w.conn_id,
					digblock_core::skills::cast_xp(w.essence, w.fare)
						+ digblock_core::skills::cast_xp(we, wf),
				);
				continue;
			}
			i += 1;
		}
		self.closings = closings;
	}

	/// `CastBreak`: the caster's own word that the channel broke.
	pub(super) fn break_sphere(&mut self, conn_id: u64) {
		if let Some(i) = self.closings.iter().position(|w| w.conn_id == conn_id) {
			let w = self.closings.remove(i);
			self.unwind_sphere(w);
		}
	}

	/// Put back what a broken climb had landed, crown-first, through the same write path — but only
	/// a cell that still holds our block, so one somebody dug out mid-climb stays dug and stays
	/// theirs (the dig credited it). The bill goes back to a caster still here as one rebase — both
	/// fares, the blocks never placed and every one just taken down; one who left mid-channel
	/// forfeits it (crediting `player_store` was declined, not impossible: `drop_client` has the record).
	fn unwind_sphere(&mut self, mut w: SphereClosing) {
		let by = w.by;
		let wall = w.wall();
		self.sphere_ended(&w, false);
		let mut restored = 0;
		let closing = std::mem::replace(
			&mut w.closing,
			digblock_core::sphere::Closing::new(Vec::new(), 1),
		);
		for (cell, written, prev) in closing.unwind() {
			// Through the journal behind the store, and journalled even
			// where the store cannot answer: a caster's departure evicts
			// the very columns their sphere stands in before this runs,
			// and a restore skipped there would leave half a sphere in
			// the record forever — the one copy a rejoin reads.
			if self.world_cell(cell.0, cell.1, cell.2) != Some(written) {
				continue;
			}
			if written == wall {
				restored += 1;
			}
			self.store.set_cell(cell.0, cell.1, cell.2, prev);
			self.journal_write(cell, prev, by);
		}
		self.refund_sphere(&w, w.debited.saturating_sub(w.placed) + restored, true);
	}

	/// Hand a caster still here `blocks` of the shell's block back, and the fares too when the
	/// climb came to nothing, as ONE rebase — silent when there is nothing to hand back, so a seal
	/// that placed every cell it billed costs no frame at all.
	fn refund_sphere(&mut self, w: &SphereClosing, blocks: u32, fares: bool) {
		if blocks == 0 && !fares {
			return;
		}
		if let Some(client) = self.clients.get_mut(&w.conn_id) {
			client.inventory.add(w.block, blocks);
			if fares {
				client.inventory.add_fares(&w.fares());
			}
			let counts = client.inventory.to_wire();
			client_send(client, &ServerMsg::InventoryFull { counts });
		}
	}

	/// The one frame a sphere's end costs, whichever way it ended: the
	/// sphere, how far through its plan it got, and whether the shell
	/// stayed. Everything else a client needs it derives.
	fn sphere_ended(&self, w: &SphereClosing, sealed: bool) {
		self.broadcast(&ServerMsg::SphereEnded {
			by: w.by,
			head: [w.head.0, w.head.1, w.head.2],
			radius: sphere_radius_wire(w.radius),
			landed: w.landed(),
			sealed,
			block: w.block as u16,
		});
	}

	/// One light as the wire carries it, on the sim's clock right now.
	pub(super) fn kindled_msg(&self, light: &Kindled) -> ServerMsg {
		kindled_msg_at(self.tick, light)
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

/// A sphere's radius as the wire carries it. `sphere::MAX_RADIUS` is what
/// makes the narrowing total — every castable rank sits inside it, and
/// `sets` clamps anything that does not.
fn sphere_radius_wire(r: i32) -> u8 {
	u8::try_from(r.clamp(0, digblock_core::sphere::MAX_RADIUS)).expect("the cap fits a byte")
}

/// One light as the wire carries it, its remaining time read off the
/// sim's clock at SEND time — so the cast and a join a minute later hand
/// out the same expiry. Free-standing because a joiner's backlog goes out
/// from inside the sync pump, which already holds the client roster.
pub(super) fn kindled_msg_at(tick: u32, light: &Kindled) -> ServerMsg {
	let ticks_left = light.until.saturating_sub(tick);
	ServerMsg::Kindled {
		cell: light.cell,
		secs_left: u16::try_from(ticks_left.div_ceil(TICK_RATE)).unwrap_or(u16::MAX),
	}
}
