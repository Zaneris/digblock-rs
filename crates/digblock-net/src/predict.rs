//! Client-side movement prediction and server reconciliation.
//!
//! Each predicted tick runs the real `Player::tick` and records (input,
//! post-state); the post-state rides the wire as the adoption envelope claim,
//! frozen at first send. On a snapshot ack, the claim we actually SENT for the
//! acked tick is compared against the server's state — a match costs nothing, a
//! mismatch rewinds and replays the still-unacked inputs.
//!
//! Comparing against the SENT claim rather than the live post-state matters at
//! high ping: a replay rewrites the buffered post-states, but the server
//! (first-wins input dedup) adopted the ORIGINAL in-flight claims, and judging
//! its echo of those against the rewritten values re-replays every snapshot —
//! at rest that ping-pong never decays, vibrating the camera at RTT period.
//!
//! Chosen simplification: replay runs against the LIVE world, not the world as
//! it was at those past ticks. Any error is bounded by one snapshot interval,
//! and the epsilon absorbs cross-platform f32 last-ulp drift.

use crate::messages::{InputBatch, InputTick};
use crate::protocol::INPUT_REDUNDANCY;
use digblock_core::player::{Player, PlayerIntent, PlayerState, TICK};
use glam::Vec3;
use std::collections::VecDeque;

/// How much unacked input the predictor holds, in ticks; overflow drops
/// the oldest (a later snapshot then takes the divergence path and
/// hard-corrects).
const BUF_CAP: usize = 128;

const POS_EPS2: f32 = 1e-6; // = (1e-3 blocks)²
const VEL_EPS2: f32 = 1e-4; // = (1e-2 blocks/s)²

struct BufEntry {
	tick: u32,
	intent: PlayerIntent,
	dir: Vec3,
	post: PlayerState,
	/// Claim actually put on the wire, frozen at first batching. Re-sends
	/// repeat it verbatim (one claim per tick even under datagram loss),
	/// and reconcile compares acks against it: the server's first-wins
	/// input dedup means THIS — not a replay-rewritten `post` — is what
	/// the server adopted.
	sent: Option<PlayerState>,
	/// `clock` when this tick FIRST went on the wire, frozen with `sent`
	/// for the same reason: the ack answers the original transmission, so
	/// timing a re-send would under-report the round trip.
	sent_at: f32,
}

pub struct Predictor {
	next_tick: u32,
	buf: VecDeque<BufEntry>,
	last_acked: u32,
	/// Current hotbar selection, stamped into every outgoing `InputTick`
	/// (feeds the server's held-item broadcast). Not per-tick history —
	/// reconciliation never replays it.
	pub active_slot: u8,
	/// Staging allocation lent out by [`Self::input_batch`] and taken
	/// back by [`Self::recycle_batch`], so the per-frame send path
	/// allocates nothing steady-state.
	spare: Vec<InputTick>,
	/// Seconds since the predictor was created, advanced by
	/// [`Self::advance`]. Sans-IO: time arrives as `dt`, never from a
	/// clock this crate reads itself.
	clock: f32,
	/// Round trip of the most recent ack, in seconds. `None` until one
	/// lands — and again after a teleport drops the buffer.
	last_rtt: Option<f32>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Reconciled {
	/// Server agrees with our prediction at the acked tick.
	InSync,
	/// Diverged: rewound to server state and replayed. `error` is the
	/// position shift (pre-reconcile − post-reconcile) for smoothing.
	Replayed { error: Vec3 },
	/// Ack older than one already processed; ignored.
	StaleAck,
}

impl Predictor {
	/// `start_tick`: first tick to predict (server tick + lead).
	pub fn new(start_tick: u32) -> Self {
		Self {
			next_tick: start_tick,
			buf: VecDeque::new(),
			last_acked: start_tick.saturating_sub(1),
			active_slot: 0,
			spare: Vec::new(),
			clock: 0.0,
			last_rtt: None,
		}
	}

	/// Advance the send-timestamp clock. Called once per `poll`.
	pub fn advance(&mut self, dt: f32) {
		self.clock += dt;
	}

	/// Input→ack round trip of the last reconcile, in seconds.
	pub fn last_rtt(&self) -> Option<f32> {
		self.last_rtt
	}

	/// Ticks predicted beyond the last one the server acknowledged — the
	/// client's view of how deep the server's jitter buffer is running.
	pub fn tick_lead(&self) -> u32 {
		self.next_tick.saturating_sub(self.last_acked)
	}

	pub fn current_tick(&self) -> u32 {
		self.next_tick
	}

	/// Run one predicted tick and record it. Returns the tick number used.
	pub fn predict(
		&mut self,
		player: &mut Player,
		intent: PlayerIntent,
		dir: Vec3,
		cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
	) -> u32 {
		let tick = self.next_tick;
		self.next_tick += 1;
		player.tick(TICK, dir, intent, cell_at);
		if self.buf.len() == BUF_CAP {
			self.buf.pop_front();
		}
		self.buf.push_back(BufEntry {
			tick,
			intent,
			dir,
			post: player.state(),
			sent: None,
			sent_at: 0.0,
		});
		tick
	}

	/// The last ≤ `INPUT_REDUNDANCY` unacked inputs as a wire batch,
	/// oldest-first. The Vec is the predictor's staging allocation on
	/// loan; hand it back with [`Self::recycle_batch`] after sending.
	pub fn input_batch(&mut self) -> InputBatch {
		let start = self.buf.len().saturating_sub(INPUT_REDUNDANCY);
		let active_slot = self.active_slot;
		let mut inputs = std::mem::take(&mut self.spare);
		inputs.clear();
		let now = self.clock;
		inputs.extend(self.buf.iter_mut().skip(start).map(|e| {
			if e.sent.is_none() {
				e.sent_at = now;
			}
			let claim = *e.sent.get_or_insert(e.post);
			InputTick {
				tick: e.tick,
				move_axis: e.intent.move_axis.to_array(),
				jump: e.intent.jump,
				look: e.dir.to_array(),
				claimed_pos: claim.pos.to_array(),
				claimed_vel: claim.vel.to_array(),
				active_slot,
			}
		}));
		InputBatch { inputs }
	}

	/// Take a sent batch's allocation back for the next one. Dropping the
	/// batch instead is correct, just one allocation more.
	pub fn recycle_batch(&mut self, batch: InputBatch) {
		self.spare = batch.inputs;
	}

	/// The server teleported us (return to spawn): every buffered
	/// prediction and any in-flight ack describes the pre-teleport
	/// position — replaying them from the target would yank the player
	/// back. Drop the buffer and refuse acks for ticks predicted before
	/// now; the next predicted inputs (sent from the teleport target)
	/// re-anchor reconciliation within one snapshot interval.
	pub fn on_teleport(&mut self) {
		self.buf.clear();
		self.last_acked = self.next_tick.saturating_sub(1);
		// Every in-flight tick is about to be refused as stale, so the next
		// ack cannot be timed against one — measuring it would report the
		// teleport's own latency as the connection's.
		self.last_rtt = None;
	}

	/// Apply a snapshot's view of the local player.
	pub fn reconcile(
		&mut self,
		player: &mut Player,
		acked_tick: u32,
		server: PlayerState,
		cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
	) -> Reconciled {
		if acked_tick <= self.last_acked {
			return Reconciled::StaleAck;
		}
		self.last_acked = acked_tick;

		// Judge the server against the claim we SENT for that tick (falling
		// back to `post` if it never hit the wire): a server state matching
		// what we told it means it adopted our view, and any difference from
		// our current state is our own later replay — which the server
		// converges to by adopting the newer in-flight claims. Re-replaying
		// here instead is the standing-still RTT-period vibration.
		let acked = self.buf.iter().find(|e| e.tick == acked_tick);
		let predicted = acked.map(|e| e.sent.unwrap_or(e.post));
		// Only a tick that actually went on the wire has a send time; one
		// answered without ever being sent is not a round trip.
		self.last_rtt = acked
			.filter(|e| e.sent.is_some())
			.map(|e| (self.clock - e.sent_at).max(0.0));
		// Drop everything the server has consumed.
		while matches!(self.buf.front(), Some(e) if e.tick <= acked_tick) {
			self.buf.pop_front();
		}

		let in_sync = predicted.is_some_and(|p| {
			p.pos.distance_squared(server.pos) < POS_EPS2
				&& p.vel.distance_squared(server.vel) < VEL_EPS2
		});
		if in_sync {
			return Reconciled::InSync;
		}

		// Rewind to the server's state and replay unacked inputs.
		let before = player.pos;
		player.set_state(server);
		for e in self.buf.iter_mut() {
			player.tick(TICK, e.dir, e.intent, cell_at);
			e.post = player.state();
		}
		Reconciled::Replayed {
			error: before - player.pos,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::block::BlockType;
	use glam::Vec2;
	use std::assert_matches;

	fn flat_world(_x: i32, y: i32, _z: i32) -> Option<u16> {
		Some(if y < 10 {
			BlockType::Dirt as u16
		} else {
			BlockType::Air as u16
		})
	}

	fn forward() -> PlayerIntent {
		PlayerIntent {
			move_axis: Vec2::new(0.0, 1.0),
			jump: false,
		}
	}

	fn landed_player() -> Player {
		let mut p = Player::new(Vec3::new(0.5, 11.5, 0.5));
		for _ in 0..10 {
			p.tick(TICK, Vec3::Z, PlayerIntent::default(), &flat_world);
		}
		p
	}

	/// Reference server: consumes the same inputs with its own Player.
	fn server_run(inputs: &[(PlayerIntent, Vec3)], from: PlayerState) -> PlayerState {
		let mut p = Player::new(Vec3::ZERO);
		p.set_state(from);
		for (intent, dir) in inputs {
			p.tick(TICK, *dir, *intent, &flat_world);
		}
		p.state()
	}

	#[test]
	fn replay_convergence_in_sync() {
		let mut player = landed_player();
		let start = player.state();
		let mut pred = Predictor::new(100);
		let mut inputs = Vec::new();
		for _ in 0..30 {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
			inputs.push((forward(), Vec3::Z));
		}
		// Server consumed the leading slice of ticks and agrees exactly.
		let server = server_run(&inputs[..10], start);
		let r = pred.reconcile(&mut player, 109, server, &flat_world);
		assert_eq!(r, Reconciled::InSync);
	}

	#[test]
	fn divergence_rewinds_and_replays_exactly() {
		let mut player = landed_player();
		let mut pred = Predictor::new(100);
		let mut inputs = Vec::new();
		for _ in 0..30 {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
			inputs.push((forward(), Vec3::Z));
		}
		// Server disagrees at the acked tick (e.g. envelope rejected a claim).
		let mut server = server_run(&inputs[..10], landed_player().state());
		server.pos.x += 2.0;
		let r = pred.reconcile(&mut player, 109, server, &flat_world);
		assert_matches!(r, Reconciled::Replayed { .. });
		// Post-reconcile state must equal the reference: the server state
		// at that ack plus the inputs after it, bit-exact.
		let want = server_run(&inputs[10..], server);
		assert_eq!(player.state(), want);
	}

	#[test]
	fn stale_ack_ignored() {
		let mut player = landed_player();
		let mut pred = Predictor::new(100);
		for _ in 0..10 {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
		}
		let server = player.state();
		assert_ne!(
			pred.reconcile(&mut player, 105, server, &flat_world),
			Reconciled::StaleAck
		);
		assert_eq!(
			pred.reconcile(&mut player, 105, server, &flat_world),
			Reconciled::StaleAck
		);
		assert_eq!(
			pred.reconcile(&mut player, 103, server, &flat_world),
			Reconciled::StaleAck
		);
	}

	#[test]
	fn overflow_forces_correction_path() {
		let mut player = landed_player();
		let mut pred = Predictor::new(0);
		for _ in 0..(BUF_CAP + 40) {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
		}
		// That tick was dropped by overflow: no buffered post to compare,
		// so even a "correct" server state takes the replay path.
		let state = player.state();
		let r = pred.reconcile(&mut player, 10, state, &flat_world);
		assert_matches!(r, Reconciled::Replayed { .. });
	}

	#[test]
	fn teleport_drops_buffer_and_stale_acks() {
		let mut player = landed_player();
		let mut pred = Predictor::new(100);
		for _ in 0..10 {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
		}
		let stale = player.state();
		pred.on_teleport();
		assert!(pred.input_batch().inputs.is_empty(), "buffer dropped");
		// An in-flight ack for a pre-teleport tick must be ignored — it
		// would rewind the player back across the map.
		assert_eq!(
			pred.reconcile(&mut player, 105, stale, &flat_world),
			Reconciled::StaleAck
		);
		// Post-teleport predictions reconcile normally again.
		pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
		let post = player.state();
		assert_eq!(
			pred.reconcile(&mut player, 110, post, &flat_world),
			Reconciled::InSync
		);
	}

	/// The high-ping standing-still bug: after a replay, snapshots echoing
	/// the ORIGINAL in-flight claims (which the server adopted, first-wins)
	/// must read as InSync against the frozen sent claims — re-replaying
	/// them ping-pongs both endpoints at RTT period forever, because rest
	/// is an exact fixed point on both sides.
	#[test]
	fn crossed_adoption_converges_without_ping_pong() {
		let mut player = landed_player();
		let rest = player.state();
		let mut pred = Predictor::new(100);
		// Idle at rest, batching each tick like the per-frame send loop —
		// every claim freezes at `rest`.
		for _ in 0..20 {
			pred.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
			pred.input_batch();
		}
		// Server disagrees by a hair (an underrun/coast seed): replay to S.
		let mut s = rest;
		s.pos.x += 0.005;
		let r = pred.reconcile(&mut player, 109, s, &flat_world);
		assert_matches!(r, Reconciled::Replayed { .. });
		let replayed = player.state();
		// One RTT later the server has adopted our in-flight `rest` claims
		// and echoes them. That matches what we sent for the newer ack —
		// stay put; the server converges to us via the newer claims.
		let r = pred.reconcile(&mut player, 115, rest, &flat_world);
		assert_eq!(r, Reconciled::InSync);
		assert_eq!(player.state(), replayed, "no snap back");
	}

	#[test]
	fn genuine_rejection_still_replays() {
		let mut player = landed_player();
		let rest = player.state();
		let mut pred = Predictor::new(100);
		for _ in 0..20 {
			pred.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
			pred.input_batch();
		}
		// Server state matching neither the sent claims nor any post —
		// e.g. the envelope rejected us. Every ack must keep correcting.
		let mut s = rest;
		s.pos.x += 2.0;
		let r = pred.reconcile(&mut player, 109, s, &flat_world);
		assert_matches!(r, Reconciled::Replayed { .. });
		let mut s2 = rest;
		s2.pos.x += 2.5;
		let r = pred.reconcile(&mut player, 115, s2, &flat_world);
		assert_matches!(r, Reconciled::Replayed { .. });
	}

	/// Every wire copy of a tick must carry the identical claim: under
	/// datagram loss the server may first receive a late redundancy copy,
	/// and a replay-rewritten claim there would dodge the frozen record.
	#[test]
	fn resend_after_replay_repeats_frozen_claim() {
		let mut player = landed_player();
		let rest = player.state();
		let mut pred = Predictor::new(100);
		for _ in 0..10 {
			pred.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
		}
		pred.input_batch(); // freeze ticks 106..=109 at `rest`
		let mut s = rest;
		s.pos.x += 0.5;
		let r = pred.reconcile(&mut player, 105, s, &flat_world);
		assert_matches!(r, Reconciled::Replayed { .. });
		// Re-sent ticks repeat the frozen claim, not the rewritten post.
		for i in pred.input_batch().inputs {
			assert_eq!(i.claimed_pos, rest.pos.to_array());
		}
		// Ticks predicted after the replay claim the new trajectory.
		pred.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
		let post = player.state();
		let batch = pred.input_batch();
		let last = batch.inputs.last().unwrap();
		assert_eq!(last.tick, 110);
		assert_eq!(last.claimed_pos, post.pos.to_array());
	}

	/// The recycled Vec must be cleared, not appended to: a leaked
	/// previous window would resend acked ticks (and blow MAX_DATAGRAM).
	#[test]
	fn recycled_batch_carries_only_the_current_window() {
		let mut player = landed_player();
		let mut pred = Predictor::new(100);
		for _ in 0..10 {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
		}
		let first = pred.input_batch();
		assert_eq!(
			first.inputs.iter().map(|i| i.tick).collect::<Vec<_>>(),
			vec![106, 107, 108, 109]
		);
		pred.recycle_batch(first);
		let server = player.state();
		pred.reconcile(&mut player, 107, server, &flat_world);
		let second = pred.input_batch();
		assert_eq!(
			second.inputs.iter().map(|i| i.tick).collect::<Vec<_>>(),
			vec![108, 109]
		);
	}

	#[test]
	fn input_batch_is_redundancy_window() {
		let mut player = landed_player();
		let mut pred = Predictor::new(100);
		for _ in 0..10 {
			pred.predict(&mut player, forward(), Vec3::Z, &flat_world);
		}
		let batch = pred.input_batch();
		let ticks: Vec<u32> = batch.inputs.iter().map(|i| i.tick).collect();
		assert_eq!(ticks, vec![106, 107, 108, 109]);
		// Ack trims the buffer; the window slides.
		let server = player.state();
		pred.reconcile(&mut player, 107, server, &flat_world);
		let ticks: Vec<u32> = pred.input_batch().inputs.iter().map(|i| i.tick).collect();
		assert_eq!(ticks, vec![108, 109]);
	}
}
