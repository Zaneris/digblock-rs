//! Accessors the sim tests and `netbench` reach for. Nothing in the
//! server's own paths calls any of it -- keeping them here is what
//! keeps them out of `impl Sim`'s production surface.

use super::*;

impl Sim {
	/// Write a cell straight into the resident store, bypassing edit
	/// validation — for staging terrain a test needs to exist.
	#[cfg(test)]
	pub(super) fn test_set_cell(&mut self, x: i32, y: i32, z: i32, cell: u16) {
		self.store.set_cell(x, y, z, cell);
	}

	/// Put a client somewhere, bypassing the admin command — for staging a spatial arrangement a
	/// test needs without walking there. The interest centre moves with them, dirty flag and all:
	/// setting the centre alone leaves `update_residency`'s own check with nothing to notice, so
	/// the store would keep serving the columns around wherever the client USED to be.
	#[cfg(test)]
	pub(super) fn test_teleport(&mut self, conn_id: u64, pos: Vec3) {
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.player.set_state(digblock_core::player::PlayerState {
				pos,
				vel: Vec3::ZERO,
			});
			c.residency.center = column_of(pos);
			self.residency_dirty = true;
		}
	}

	/// The hand-picked spawn (`/set-spawn`), if one is stored.
	#[cfg(test)]
	pub(super) fn test_spawn_point(&self) -> Option<Vec3> {
		self.spawn_store.point().map(Vec3::from_array)
	}

	/// The stored spawn facing (`/set-spawn-dir`), if one is set.
	#[cfg(test)]
	pub(super) fn test_spawn_dir(&self) -> Option<Vec3> {
		self.spawn_store.dir().map(Vec3::from_array)
	}

	/// Plant a pre-account character in the store's quarantine, standing in
	/// for what a server that ran before accounts left on disk. Reading a
	/// real v1 file is the STORE's test; this is the sim path above it.
	#[cfg(test)]
	pub(super) fn test_put_legacy(&mut self, legacy: [u8; 16], rec: PlayerRecord) {
		self.player_store.test_put_legacy(legacy, rec);
	}

	#[cfg(test)]
	pub(super) fn test_legacy_len(&self) -> usize {
		self.player_store.legacy_len()
	}

	#[cfg(test)]
	pub(super) fn test_client_inventory(
		&self,
		conn_id: u64,
	) -> &digblock_core::inventory::Inventory {
		&self.clients[&conn_id].inventory
	}

	#[cfg(test)]
	pub(super) fn test_grant(&mut self, conn_id: u64, b: digblock_core::block::BlockType, n: u32) {
		let client = self.clients.get_mut(&conn_id).unwrap();
		client.inventory.add(b, n);
		// Like a real acquisition: the granted type lands on the bar, so
		// places (now slot-enforced) validate the way an honest client's
		// would.
		reconcile_layout(&client.inventory, &mut client.hotbar, &mut client.grid);
	}

	/// Stock a player's essence holdings — the bench's side of
	/// [`Self::test_grant`], for tests about crafting rather than digging.
	#[cfg(test)]
	pub(super) fn test_grant_essence(
		&mut self,
		conn_id: u64,
		e: digblock_core::block::Essence,
		n: u32,
	) {
		self.clients
			.get_mut(&conn_id)
			.unwrap()
			.inventory
			.add_sub(e, n);
	}

	#[cfg(test)]
	pub(super) fn test_player(&self, conn_id: u64) -> Option<digblock_core::player::PlayerState> {
		self.clients.get(&conn_id).map(|c| c.player.state())
	}

	/// The wire id a connection was dealt — what a broadcast names it by.
	#[cfg(test)]
	pub(super) fn test_player_id(&self, conn_id: u64) -> Option<digblock_net::protocol::PlayerId> {
		self.clients.get(&conn_id).map(|c| c.id)
	}

	/// The hotbar slot this client is pointing at — what a real client
	/// declares on every input tick and every edit, and what decides
	/// whether the tool slot's contents are in hand at all.
	#[cfg(test)]
	pub(crate) fn test_selected_slot(&self, conn_id: u64) -> u8 {
		self.clients
			.get(&conn_id)
			.map_or(0, |c| c.hotbar.selected_index() as u8)
	}

	/// The client's Mining level (the base level for an unknown conn).
	#[cfg(test)]
	pub(crate) fn test_mining_level(&self, conn_id: u64) -> u32 {
		self.clients
			.get(&conn_id)
			.map_or(1, |c| c.skills.level(digblock_core::skills::Skill::Mining))
	}

	/// Raise a client's Mining to `level` (total-XP floor — never lowers, like the wire's max-total
	/// rule). Harness/bench support: edit-path digs must clear the strength wall without earning
	/// through it (the gate's own test drives DigStart raw). Not #[cfg(test)] because netbench — a
	/// bin, not a test — drives a real in-process Sim whose flat world is stone.
	pub fn bench_set_mining_level(&mut self, conn_id: u64, level: u32) {
		use digblock_core::skills::{Skill, total_xp_for};
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.skills.set_total(Skill::Mining, total_xp_for(level));
		}
	}

	/// Put a tool in a client's hand. The tool slot's Layout entry is the
	/// second axis (`Hotbar::active_tool`), so granting the item is not
	/// the same as holding it — which is exactly the distinction the
	/// siphon gate is about.
	#[cfg(test)]
	pub(crate) fn bench_set_active_tool(
		&mut self,
		conn_id: u64,
		tool: digblock_core::block::BlockType,
	) {
		if let Some(c) = self.clients.get_mut(&conn_id) {
			// Owning it is a precondition for holding it — `reconcile_layout`
			// takes back a tool the counts don't cover — so the harness has
			// to hand over the item as well as the slot, exactly like the
			// craft that would have earned it.
			c.inventory.add(tool, 1);
			c.hotbar.cycle_tool(&[tool]);
			// And SELECT the slot: since `tool_in_hand` asks whether the
			// hand is on the tool, a granted tool the bar is not pointing
			// at is no more in hand here than it is for a player.
			c.hotbar
				.set_selected(digblock_core::hotbar::HOTBAR_SLOTS - 1);
		}
	}

	/// Lock the spawn region around `eye`, so protection tests can put an
	/// immutable cell wherever they need one.
	#[cfg(test)]
	pub(crate) fn bench_lock_spawn_at(&mut self, eye: Vec3) {
		self.spawn_store
			.set_lock(Some(crate::spawn_store::SpawnLock {
				eye: eye.to_array(),
			}));
	}

	/// A client's XP total in one skill — the two-sided award tests read
	/// it, since a dual award is only interesting as a pair.
	#[cfg(test)]
	pub(crate) fn test_skill_xp(&self, conn_id: u64, skill: digblock_core::skills::Skill) -> u64 {
		self.clients.get(&conn_id).map_or(0, |c| c.skills.xp(skill))
	}

	#[cfg(test)]
	pub(super) fn test_skill_level(
		&self,
		conn_id: u64,
		skill: digblock_core::skills::Skill,
	) -> u32 {
		self.clients
			.get(&conn_id)
			.map_or(1, |c| c.skills.level(skill))
	}

	/// Raise a client's Alchemy to `level` — the craft gate's counterpart
	/// of [`Self::bench_set_mining_level`], for edit-path tests that dig
	/// craft-only blocks (slabs, stairs, doors, torches).
	#[cfg(test)]
	pub(crate) fn bench_set_alchemy_level(&mut self, conn_id: u64, level: u32) {
		use digblock_core::skills::{Skill, total_xp_for};
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.skills.set_total(Skill::Alchemy, total_xp_for(level));
		}
	}

	/// The third twin, for cast-path tests that need a travel rung.
	#[cfg(test)]
	pub(crate) fn bench_set_arcana_level(&mut self, conn_id: u64, level: u32) {
		use digblock_core::skills::{Skill, total_xp_for};
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.skills.set_total(Skill::Arcana, total_xp_for(level));
		}
	}

	/// Seed a resolved waystone, as boot would have stored one — cast
	/// tests need a destination without paying a real anchor walk.
	#[cfg(test)]
	pub(crate) fn bench_add_waystone(&mut self, idx: u8, eye: Vec3) {
		self.waystone_store.set(idx, eye.to_array());
	}

	/// A client's essence balance, for fare assertions.
	#[cfg(test)]
	pub(crate) fn test_essence(
		&self,
		conn_id: u64,
		e: digblock_core::block::Essence,
	) -> Option<u32> {
		self.clients.get(&conn_id).map(|c| c.inventory.sub_count(e))
	}

	/// The cell a client currently holds a dig on, if any.
	#[cfg(test)]
	pub(super) fn test_digging(&self, conn_id: u64) -> Option<(i32, i32, i32)> {
		self.clients
			.get(&conn_id)
			.and_then(|c| c.digging.as_ref())
			.map(|d| d.cell)
	}

	#[cfg(test)]
	pub(super) fn test_is_live(&self, conn_id: u64) -> bool {
		self.clients.get(&conn_id).is_some_and(|c| c.live)
	}

	#[cfg(test)]
	pub(super) fn test_look(&self, conn_id: u64) -> Option<Vec3> {
		self.clients.get(&conn_id).map(|c| c.look)
	}

	#[cfg(test)]
	pub(super) fn test_last_input_tick(&self, conn_id: u64) -> u32 {
		self.clients.get(&conn_id).map_or(0, |c| c.last_input_tick)
	}

	/// Inputs sitting in the jitter buffer right now — how much the drain
	/// has yet to work off.
	#[cfg(test)]
	pub(super) fn test_input_backlog(&self, conn_id: u64) -> usize {
		self.clients.get(&conn_id).map_or(0, |c| c.inputs.len())
	}

	#[cfg(test)]
	pub(super) fn test_draw_dist(&self, conn_id: u64) -> Option<i32> {
		self.clients.get(&conn_id).map(|c| c.residency.draw_dist)
	}

	#[cfg(test)]
	pub(super) fn test_inventory_count(
		&self,
		conn_id: u64,
		b: digblock_core::block::BlockType,
	) -> u32 {
		self.clients
			.get(&conn_id)
			.map_or(0, |c| c.inventory.count(b))
	}

	#[cfg(test)]
	pub(super) fn test_sub_count(&self, conn_id: u64, e: digblock_core::block::Essence) -> u32 {
		self.clients
			.get(&conn_id)
			.map_or(0, |c| c.inventory.sub_count(e))
	}

	/// Standing kindled lights — the world-wide list every joiner is
	/// handed a copy of, and the thing the cap bounds.
	#[cfg(test)]
	pub(crate) fn test_kindled_len(&self) -> usize {
		self.kindled.len()
	}

	// ---- events -------------------------------------------------------

	/// Whether a scheduled sweep is mid-slice.
	#[cfg(test)]
	pub(crate) fn test_world_job_running(&self) -> bool {
		self.world_jobs.sweeping().is_some()
	}
}
