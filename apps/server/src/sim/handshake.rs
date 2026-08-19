//! Join: token verification against the master, the pre-account
//! legacy claim, and the sync every new client is sent. Verification
//! fails CLOSED; the client never asserts who it is.

use super::*;

impl Sim {
	/// One-time transfer of this server's pre-account character onto the
	/// account that asked for it (`ClientMsg::ClaimLegacy`, protocol v24).
	///
	/// **Silence is the normal answer.** Every client that still has an old
	/// identity file sends this on every join, to every server, because it
	/// cannot know which ones it has already migrated on — so "nothing held
	/// under that key" is the overwhelmingly common case and is not an
	/// error. Only a claim that actually moves something says anything.
	///
	/// The one-time-ness lives in the store: a successful claim deletes the
	/// quarantined record, so a repeat is indistinguishable from a key that
	/// was never here. Nothing tracks who has claimed what.
	pub(super) fn claim_legacy(&mut self, conn_id: u64, legacy: [u8; 16]) {
		// An unverified connection has no account to migrate ONTO, so there
		// is nothing this could mean. That is the offline harness and the
		// tests; a real join always carries one.
		let Some(account) = self.clients.get(&conn_id).and_then(|c| c.identity) else {
			return;
		};
		let Some(record) = self.player_store.claim_legacy(legacy, account) else {
			return;
		};
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		client.inventory = record.inventory;
		client.skills = record.skills;
		// The merged record's layout was authored against ONE of the two
		// piles, so it has no slot for whatever the other one brought.
		// `reconcile_layout` is what files those — the same call every
		// count change already runs, run here for the same reason.
		if let Some(layout) = &record.layout {
			client.hotbar = Hotbar::from_wire(&layout.hotbar);
			client.grid = InvGrid::from_wire(&layout.grid);
		}
		reconcile_layout(&client.inventory, &mut client.hotbar, &mut client.grid);
		// Re-send exactly the three frames join sync sent, in the same
		// order: the client's decode path for each is the standing rebase,
		// so a second copy is a correction rather than a special case.
		let out = client.out.clone();
		send_frame(
			&out,
			&ServerMsg::InventoryFull {
				counts: client.inventory.to_wire(),
			},
		);
		send_frame(
			&out,
			&ServerMsg::Layout {
				hotbar: client.hotbar.to_wire().to_vec(),
				grid: client.grid.to_wire().to_vec(),
			},
		);
		send_frame(
			&out,
			&ServerMsg::Skills {
				xp: client.skills.to_wire(),
			},
		);
		send_frame(
			&out,
			&admin::server_reply(
				"your pre-account character on this server has been added to your account",
			),
		);
		let name = client.name.clone();
		// No forced flush, and it needs none: the store is a whole-file
		// snapshot, so the removal and the merge reach disk together or
		// not at all. A crash inside the flush window leaves the file with
		// the quarantined record still in it and the account record
		// un-merged — the player simply claims again on their next join,
		// which is the state they were already in.
		log::info!("player {name} claimed a pre-account character on this server");
	}

	pub(super) fn handle_hello(
		&mut self,
		conn_id: u64,
		protocol: u16,
		name: String,
		draw_dist: i32,
		sim_version: u16,
		build: String,
	) {
		let Some((out, _, addr, account)) = self.lobby.remove(&conn_id) else {
			return;
		};
		let refuse = |out: &OutTx, reason: String| {
			send_frame(&out.clone(), &ServerMsg::Refused { reason });
			let _ = out.try_send(OutMsg::Close);
		};
		// Ban gate first — a banned peer learns nothing else. Both keys are
		// checked here: the account is the durable one (a new IP no longer
		// buys a way back in), the IP still catches an unauthenticated peer
		// and everyone sharing a griefer's connection.
		if self.ban_store.is_banned(addr.ip()) || self.ban_store.is_account_banned(account) {
			refuse(&out, "banned".into());
			return;
		}
		// Redundant with the net-layer preamble gate, but the sim is also
		// driven directly (tests, future transports) — keep it authoritative.
		if !(MIN_PROTOCOL..=PROTOCOL_VERSION).contains(&protocol) {
			refuse(
				&out,
				format!("protocol {protocol}, server supports {MIN_PROTOCOL}..={PROTOCOL_VERSION}"),
			);
			return;
		}
		if sim_version != digblock_core::SIM_VERSION {
			refuse(
				&out,
				format!(
					"client sim {sim_version} (build {build}), server sim {} — update the older side",
					digblock_core::SIM_VERSION
				),
			);
			return;
		}
		let Some(id) = self.free_id() else {
			refuse(&out, "server full".into());
			return;
		};
		// AccountId::NONE = no master vouched for this connection, so there
		// is nothing to persist against: session-only inventory, no dup
		// check. That is the offline harness and the tests, never a real
		// join — net.rs refuses a connection it could not verify.
		//
		// Two live connections sharing one persisted inventory would stomp
		// each other's copy, so the second is refused. The check is the same
		// shape it always was; what changed is that the key is now something
		// the master said rather than something the client chose, so
		// "already connected" can no longer be dodged by picking a different
		// secret.
		let identity = (!account.is_none()).then_some(account);
		if let Some(account) = identity
			&& self.clients.values().any(|c| c.identity == Some(account))
		{
			refuse(&out, "this account is already connected".into());
			return;
		}
		let record = identity
			.map(|account| self.player_store.get(&account))
			.unwrap_or_default();
		let inventory = record.inventory;
		// The item arrangement: the stored layout when one exists, else
		// the deterministic default — the SAME derivation the client runs
		// from the `Layout` echo / its `InventoryFull`, so both sides
		// agree without a round-trip.
		let (mut hotbar, mut grid) = match &record.layout {
			Some(l) => (Hotbar::from_wire(&l.hotbar), InvGrid::from_wire(&l.grid)),
			None => (Hotbar::new(), InvGrid::default()),
		};
		reconcile_layout(&inventory, &mut hotbar, &mut grid);
		let name = sanitize_name(&name);
		let draw_dist = draw_dist.clamp(DRAW_DIST_MIN, DRAW_DIST_MAX);
		// Resume where they left off. Rejoining used to be a free trip to spawn
		// that sidestepped the return-to-spawn cooldown; stored positions close
		// that, and `last_return_tick` stops a rejoin resetting the timer too.
		//
		// The landing rule still applies: the world may have changed while they
		// were away, so a resumed position is stepped clear the same way a spawn
		// is. Anonymous joins and pre-v3 records start at spawn. The spawn
		// facing rides along only when this join really is a spawn — a player
		// resuming in their own house has already chosen a view.
		let (spawn, spawn_look) = match record.pos {
			Some(p) => (self.clear_landing(Vec3::from_array(p)), Vec3::ZERO),
			None => (self.safe_spawn(), self.spawn_look()),
		};
		let mut ok = send_frame(
			&out,
			&ServerMsg::Welcome {
				protocol: PROTOCOL_VERSION,
				seed: self.store.seed(),
				player_id: id,
				spawn: spawn.to_array(),
				server_tick: self.tick,
				snapshot_divisor: SNAPSHOT_DIVISOR,
				// Issued now so resume can be added without a wire change;
				// server-side session storage arrives with that feature.
				session_token: rand::random(),
				sim_version: digblock_core::SIM_VERSION,
				build: env!("CARGO_PKG_VERSION").into(),
				tod: self.current_tod(),
				spawn_look: spawn_look.to_array(),
			},
		);

		// Join sync: final cell state for every edited column in range,
		// QUEUED rather than sent. Encoding them all here meant thousands
		// of frames and megabytes inside one tick's budget on a mature
		// world — a hitch for everyone else on the server, every time anyone
		// joined — and past OUT_CAP the joiner simply lost the race and got
		// dropped. `pump_join_sync` drains the queue a few per tick against
		// the room the outbound queue actually has.
		//
		// Region-indexed, so this is O(columns near the spawn) rather than
		// O(every edited column in the world); the radius test still does
		// the real filtering.
		let center = column_of(spawn);
		let mut cols: Vec<(i32, i32)> = self
			.journal
			.index
			.columns_near(center, draw_dist)
			.filter(|&col| in_radius(col, center, draw_dist))
			.collect();
		// Nearest first. The order used to be whatever the region index
		// yielded, which on a mature world can leave the spawn column
		// itself until late in a stream of thousands — so the ground the
		// player is actually standing on was among the last to arrive.
		// That is what the arrival hold waits on, and sorting is what
		// keeps the wait proportional to the radius rather than to how
		// edited the whole draw distance happens to be.
		cols.sort_by_key(|&col| radius_key(col, center));
		let sync_queue: std::collections::VecDeque<(i32, i32)> = cols.into();
		let sync_queue_set: FxHashSet<(i32, i32)> = sync_queue.iter().copied().collect();
		let queued = sync_queue.len();
		// `JournalDone` carries no ordering obligation the client acts on
		// (it ignores the message), and the inventory and layout below do
		// not depend on the columns — so the handshake completes now and
		// the terrain fills in over the next few ticks.
		ok &= send_frame(
			&out,
			&ServerMsg::JournalDone {
				through_seq: self.journal.index.next_seq,
			},
		);
		// The authoritative inventory base — explicit even when empty
		// (anonymous joins), so the client never guesses.
		ok &= send_frame(
			&out,
			&ServerMsg::InventoryFull {
				counts: inventory.to_wire(),
			},
		);
		// The item arrangement (stored or derived — always sent, so the
		// client adopts exactly what enforcement will validate against).
		ok &= send_frame(
			&out,
			&ServerMsg::Layout {
				hotbar: hotbar.to_wire().to_vec(),
				grid: grid.to_wire().to_vec(),
			},
		);
		// Per-skill XP totals — always sent, even empty (a fresh
		// identity sits at the base level everywhere), so the client never
		// guesses.
		ok &= send_frame(
			&out,
			&ServerMsg::Skills {
				xp: record.skills.to_wire(),
			},
		);
		if !ok {
			// The handshake itself no longer competes with the column
			// stream for queue space, so this is now a genuinely broken
			// peer rather than a big world.
			log::warn!(
				"conn {conn_id}: handshake overflowed the outbound queue \
				 (cap {OUT_CAP}) — dropping"
			);
			return;
		}
		if queued > 0 {
			log::debug!("conn {conn_id}: {queued} journal columns queued for sync");
		}

		// Roster for the joiner (who's already online), then the join
		// announcement to everyone else — same message both directions.
		for c in self.clients.values() {
			send_frame(
				&out,
				&ServerMsg::PlayerJoined {
					id: c.id,
					name: c.name.clone(),
				},
			);
			client_send(
				c,
				&ServerMsg::PlayerJoined {
					id,
					name: name.clone(),
				},
			);
		}
		log::info!("player {id} ({name}) joined at tick {}", self.tick);
		// The server's command set, so the chat box can autocomplete —
		// admin only (nobody else can use them, or needs to see them).
		if self.is_admin(account) {
			send_frame(&out, &admin::commands_msg());
		}
		self.clients.insert(
			conn_id,
			Client {
				id,
				name,
				out,
				addr,
				dead: Cell::new(false),
				dgram_full_since: Cell::new(None),
				no_datagrams: Cell::new(false),
				player: Player::new(spawn),
				look: Vec3::Z,
				last_intent: PlayerIntent::default(),
				inputs: BTreeMap::new(),
				last_input_tick: 0,
				admit_hi: 0,
				admit_seen: 0,
				hold_ticks: 0,
				stats: InputStats::default(),
				edits: EditStats::default(),
				drift: Vec3::ZERO,
				edit_tokens: EDIT_BURST,
				chat_tokens: CHAT_BURST,
				bench_tokens: digblock_core::alchemy::bench_burst_ticks(
					record.skills.level(digblock_core::skills::Skill::Alchemy),
				),
				draw_dist,
				synced_columns: FxHashSet::default(),
				sync_queue,
				sync_queue_set,
				resync_queue: std::collections::VecDeque::new(),
				resync_countdown: 0,
				needs_sync_scan: false,
				live: false,
				center,
				// A join is the first arrival. The queue above is already
				// sorted nearest-first and `live` gates the release, so
				// this holds until the spawn's own columns have landed —
				// or releases on the first pump if the spawn is untouched.
				arrival_hold: true,
				inventory,
				hotbar,
				grid,
				identity,
				account,
				digging: None,
				// Restored, not reset: otherwise reconnecting clears the
				// cooldown and the timer is bypassed by rejoining.
				last_return_tick: record.last_return_tick,
				skills: record.skills,
			},
		);
		self.residency_dirty = true;
	}
}
