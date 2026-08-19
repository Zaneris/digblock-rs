//! The spawn point, the walk that finds a valid one, and the
//! teleports that use it. `lock_region` is exactly the cell set the
//! search inspects, which is why a locked spawn stays valid.

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

	/// Teleport a live client back to spawn, subject to the cooldown.
	/// Violations are silently ignored — honest clients gate locally.
	pub(super) fn handle_return_to_spawn(&mut self, conn_id: u64) {
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		if let Some(t) = client.last_return_tick
			&& self.tick.wrapping_sub(t) < RETURN_SPAWN_COOLDOWN_SECS * TICK_RATE
		{
			return;
		}
		let pos = self.safe_spawn();
		let look = self.spawn_look();
		let tick = self.tick;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		client.last_return_tick = Some(tick);
		log::info!("player {} ({}) returned to spawn", client.id, client.name);
		self.teleport_client(conn_id, pos, look);
	}
}
