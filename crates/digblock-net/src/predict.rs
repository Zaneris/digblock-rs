//! Client-side movement prediction and server reconciliation.
//!
//! Each predicted tick runs the real `Player::tick` and records
//! (input, post-state); the post-state rides the wire as the adoption
//! envelope claim. On a snapshot ack, the buffered post-state at the
//! acked tick is compared against the server's — a match (the common
//! case: the server adopted our claims) costs nothing, a mismatch rewinds
//! to the server state and replays the still-unacked inputs.
//!
//! Chosen simplification: replay runs against the LIVE world (which
//! already contains predicted-but-unconfirmed block edits), not the world
//! as it was at those past ticks. Any resulting error is bounded by one
//! snapshot interval — the next reconcile re-converges — and the epsilon
//! absorbs cross-platform f32 last-ulp drift.

use crate::messages::{InputBatch, InputTick};
use crate::protocol::INPUT_REDUNDANCY;
use digblock_core::block::BlockType;
use digblock_core::player::{Player, PlayerIntent, PlayerState, TICK};
use glam::Vec3;
use std::collections::VecDeque;

/// ~2 s of unacked input; overflow drops oldest (a later snapshot then
/// takes the divergence path and hard-corrects).
const BUF_CAP: usize = 128;

const POS_EPS2: f32 = 1e-6; // |Δpos| < 1e-3 blocks
const VEL_EPS2: f32 = 1e-4; // |Δvel| < 1e-2 blocks/s

struct BufEntry {
	tick: u32,
	intent: PlayerIntent,
	dir: Vec3,
	post: PlayerState,
}

pub struct Predictor {
	next_tick: u32,
	buf: VecDeque<BufEntry>,
	last_acked: u32,
}

#[derive(Debug, PartialEq)]
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
		}
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
		block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
	) -> u32 {
		let tick = self.next_tick;
		self.next_tick += 1;
		player.tick(TICK, dir, intent, block_at);
		if self.buf.len() == BUF_CAP {
			self.buf.pop_front();
		}
		self.buf.push_back(BufEntry {
			tick,
			intent,
			dir,
			post: player.state(),
		});
		tick
	}

	/// The last ≤ `INPUT_REDUNDANCY` unacked inputs as a wire batch,
	/// oldest-first.
	pub fn input_batch(&self) -> InputBatch {
		let start = self.buf.len().saturating_sub(INPUT_REDUNDANCY);
		let inputs = self
			.buf
			.iter()
			.skip(start)
			.map(|e| InputTick {
				tick: e.tick,
				move_axis: e.intent.move_axis.to_array(),
				jump: e.intent.jump,
				look: e.dir.to_array(),
				claimed_pos: e.post.pos.to_array(),
				claimed_vel: e.post.vel.to_array(),
			})
			.collect();
		InputBatch { inputs }
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
	}

	/// Apply a snapshot's view of the local player.
	pub fn reconcile(
		&mut self,
		player: &mut Player,
		acked_tick: u32,
		server: PlayerState,
		block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
	) -> Reconciled {
		if acked_tick <= self.last_acked {
			return Reconciled::StaleAck;
		}
		self.last_acked = acked_tick;

		let predicted = self
			.buf
			.iter()
			.find(|e| e.tick == acked_tick)
			.map(|e| e.post);
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
			player.tick(TICK, e.dir, e.intent, block_at);
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
	use glam::Vec2;

	fn flat_world(_x: i32, y: i32, _z: i32) -> Option<BlockType> {
		Some(if y < 10 {
			BlockType::Dirt
		} else {
			BlockType::Air
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
		// Server consumed the first 10 ticks and agrees exactly.
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
		// Server disagrees at tick 109 (e.g. envelope rejected a claim).
		let mut server = server_run(&inputs[..10], landed_player().state());
		server.pos.x += 2.0;
		let r = pred.reconcile(&mut player, 109, server, &flat_world);
		assert!(matches!(r, Reconciled::Replayed { .. }));
		// Post-reconcile state must equal the reference: server state at
		// 109 + the remaining 20 inputs, bit-exact.
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
		// Tick 10 was dropped by overflow: no buffered post to compare,
		// so even a "correct" server state takes the replay path.
		let state = player.state();
		let r = pred.reconcile(&mut player, 10, state, &flat_world);
		assert!(matches!(r, Reconciled::Replayed { .. }));
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
