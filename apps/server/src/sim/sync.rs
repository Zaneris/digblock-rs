//! Residency, late journal sync, and snapshot broadcast.

use super::{column_of, in_radius, send_frame, OutMsg, OutTx, Sim, DGRAM_STALL_SECS};
use crate::journal::Journal;
use digblock_net::codec;
use digblock_net::messages::{BlockEdit, Datagram, NetPlayerState, ServerMsg, Snapshot};
use digblock_net::protocol::TICK_RATE;
use tokio::sync::mpsc::error::TrySendError;

/// Columns beyond `draw_dist + slack` fall out of a client's synced set,
/// so re-entry re-syncs (clients evict remote chunks too).
const SYNC_SLACK: i32 = 2;

impl Sim {
	pub(super) fn update_residency(&mut self) {
		for client in self.clients.values_mut() {
			let center = column_of(client.player.pos);
			if center != client.center {
				client.center = center;
				self.residency_dirty = true;
			}
		}
		if self.residency_dirty {
			let centers: Vec<(i32, i32)> = self.clients.values().map(|c| c.center).collect();
			self.store.set_residency(&centers, self.sim_radius);
			self.residency_dirty = false;
		}
		self.store.drain(&self.journal.index);

		// Late journal sync + liveness flips.
		let journal = &self.journal;
		let store = &self.store;
		for client in self.clients.values_mut() {
			if !client.live && store.column_ready(client.center.0, client.center.1) {
				client.live = true;
				log::debug!("player {} live", client.id);
			}
			client
				.synced_columns
				.retain(|&col| in_radius(col, client.center, client.draw_dist + SYNC_SLACK));
			for (cx, cz) in journal.index.edited_columns() {
				if client.dead.get() {
					break;
				}
				if in_radius((cx, cz), client.center, client.draw_dist)
					&& !client.synced_columns.contains(&(cx, cz))
				{
					if !send_journal_column_to(journal, &client.out, cx, cz) {
						client.dead.set(true);
						break;
					}
					client.synced_columns.insert((cx, cz));
				}
			}
		}
	}

	pub(super) fn send_journal_column(&self, out: &OutTx, cx: i32, cz: i32) -> bool {
		send_journal_column_to(&self.journal, out, cx, cz)
	}

	pub(super) fn send_snapshots(&mut self) {
		let players: Vec<NetPlayerState> = self
			.clients
			.values()
			.filter(|c| c.live)
			.map(|c| {
				let s = c.player.state();
				NetPlayerState {
					id: c.id,
					pos: s.pos.to_array(),
					vel: s.vel.to_array(),
					look: c.look.to_array(),
				}
			})
			.collect();
		for client in self.clients.values() {
			if !client.live {
				continue;
			}
			let snap = Snapshot {
				server_tick: self.tick,
				your_last_input_tick: client.last_input_tick,
				players: players.clone(),
			};
			match codec::encode_datagram(&Datagram::Snapshot(snap)) {
				Ok(bytes) => match client.out.try_send(OutMsg::Datagram(bytes)) {
					Ok(()) => client.dgram_full_since.set(None),
					Err(TrySendError::Full(_)) => {
						// Lossy by contract: dropping snapshots is fine for
						// a stall, but a queue full for seconds is a peer
						// that stopped reading.
						match client.dgram_full_since.get() {
							None => client.dgram_full_since.set(Some(self.tick)),
							Some(t0)
								if self.tick.wrapping_sub(t0) > DGRAM_STALL_SECS * TICK_RATE =>
							{
								log::info!(
									"player {}: datagram queue stalled {DGRAM_STALL_SECS}s — dropping",
									client.id
								);
								client.dead.set(true);
							}
							Some(_) => {}
						}
					}
					Err(TrySendError::Closed(_)) => client.dead.set(true),
				},
				Err(e) => {
					// Losing snapshots is a game-breaking condition — never
					// swallow it. Rate-limited to once a second.
					let due = self
						.last_oversize_log
						.is_none_or(|at| self.tick.wrapping_sub(at) >= TICK_RATE);
					if due {
						log::warn!(
							"snapshot encode failed for player {} ({} players in roster): {e}",
							client.id,
							players.len()
						);
						self.last_oversize_log = Some(self.tick);
					}
				}
			}
		}
	}
}

fn send_journal_column_to(journal: &Journal, out: &OutTx, cx: i32, cz: i32) -> bool {
	let Some(cells) = journal.index.column(cx, cz) else {
		return true;
	};
	let cells = cells
		.iter()
		.map(|(&(x, y, z), &block)| BlockEdit { x, y, z, block })
		.collect();
	send_frame(out, &ServerMsg::JournalColumn { cx, cz, cells })
}
