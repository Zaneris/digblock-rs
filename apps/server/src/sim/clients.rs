//! The connection roster: reaping dead links, broadcasting, and the
//! player-id pool. One account holds one live connection.

use super::*;

impl Sim {
	/// Clients flagged dead by a refused queue push get a real disconnect;
	/// lobby members that never completed Hello time out.
	pub(super) fn reap_dead(&mut self) {
		let dead: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.dead.get())
			.map(|(&id, _)| id)
			.collect();
		for conn_id in dead {
			// Dropping the Client drops the last Sender: the net layer's
			// recv ends and the connection closes.
			self.drop_client(conn_id);
		}
		let tick = self.tick;
		self.lobby.retain(|conn_id, (out, since, _addr, _account)| {
			let keep = tick.wrapping_sub(*since) < HELLO_TIMEOUT_TICKS;
			if !keep {
				log::info!("conn {conn_id}: no Hello within the deadline — dropping");
				// Close ends the net task; dropping the Sender is the
				// backstop if the queue is already full.
				let _ = out.try_send(OutMsg::Close);
			}
			keep
		});
	}

	/// Encode once, fan out to every client. Use this for any message going
	/// to more than one recipient — `client_send` re-encodes per call, and
	/// the fan-out paths (edits, chat, time-of-day) are the hot ones.
	pub(super) fn broadcast(&self, msg: &ServerMsg) {
		self.broadcast_where(msg, |_| true);
	}

	/// Tell everyone BUT the author that they crossed a level — the cue
	/// for the confetti over their head. The author is excluded here
	/// rather than filtered client-side: they already have the `XpGain`
	/// total that implies it, and celebrate off that.
	pub(super) fn announce_level_up(
		&self,
		by: PlayerId,
		skill: digblock_core::skills::Skill,
		level: u32,
	) {
		self.broadcast_where(
			&ServerMsg::LevelUp {
				player: by,
				skill: skill as u8,
				level,
			},
			|c| c.id != by,
		);
	}

	/// [`Self::broadcast`] restricted to clients matching `want`.
	pub(super) fn broadcast_where(&self, msg: &ServerMsg, want: impl Fn(&Client) -> bool) {
		let Some(frame) = encode_frame(msg) else {
			return;
		};
		for c in self.clients.values() {
			if want(c) {
				client_send_frame(c, &frame);
			}
		}
	}

	pub(super) fn drop_client(&mut self, conn_id: u64) {
		// Before the record leaves: a road is retired by the trail that
		// laid it, so one whose holder is gone would stand forever.
		self.lift_causeway(conn_id);
		if let Some(client) = self.clients.remove(&conn_id) {
			log::info!("player {} ({}) left", client.id, client.name);
			if let Some(hash) = client.identity {
				// The record lands in memory immediately, which is what a
				// reconnect reads — `get` never touches the file. Durability
				// rides the ordinary coalesced flush: forcing one here meant
				// a whole-file rebuild + fsync on the sim thread PER
				// disconnect, so a server emptying out paid several inside
				// one tick.
				self.player_store.put(hash, client.record());
				self.player_store.sync_soon();
			}
			self.broadcast(&ServerMsg::PlayerLeft { id: client.id });
			self.residency_dirty = true;
		}
		self.lobby.remove(&conn_id);
	}

	pub(super) fn free_id(&self) -> Option<PlayerId> {
		let used: FxHashSet<PlayerId> = self.clients.values().map(|c| c.id).collect();
		(0..SERVER_ID).find(|id| !used.contains(id))
	}
}
