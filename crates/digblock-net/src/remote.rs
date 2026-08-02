//! Remote player interpolation: render other players ~100 ms in the
//! past, lerping between 20 Hz snapshot states, with capped velocity
//! extrapolation when snapshots stall.

use crate::protocol::{PlayerId, EXTRAP_CAP_TICKS, INTERP_DELAY_TICKS, TICK_RATE};
use glam::Vec3;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;

const SNAP_CAP: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct RemoteSnap {
	pub server_tick: u32,
	pub pos: Vec3,
	pub vel: Vec3,
	pub look: Vec3,
}

#[derive(Clone, Copy, Debug)]
pub struct RemoteDraw {
	pub id: PlayerId,
	pub pos: Vec3,
	/// Radians around +Y, from the look direction.
	pub yaw: f32,
}

#[derive(Default)]
pub struct RemotePlayers {
	players: FxHashMap<PlayerId, VecDeque<RemoteSnap>>,
	/// Estimated server clock in ticks: lifted by snapshots, advanced by
	/// wall time in between.
	est_server_tick: f64,
	seen_any: bool,
}

impl RemotePlayers {
	pub fn advance(&mut self, dt: f32) {
		if self.seen_any {
			self.est_server_tick += dt as f64 * TICK_RATE as f64;
		}
	}

	pub fn push(&mut self, id: PlayerId, snap: RemoteSnap) {
		self.seen_any = true;
		self.est_server_tick = self.est_server_tick.max(snap.server_tick as f64);
		let ring = self.players.entry(id).or_default();
		// Out-of-order datagrams: ignore anything not newer than the tail.
		if matches!(ring.back(), Some(last) if snap.server_tick <= last.server_tick) {
			return;
		}
		if ring.len() == SNAP_CAP {
			ring.pop_front();
		}
		ring.push_back(snap);
	}

	pub fn remove(&mut self, id: PlayerId) {
		self.players.remove(&id);
	}

	pub fn is_empty(&self) -> bool {
		self.players.is_empty()
	}

	/// Sample every remote player at `est_server_tick - INTERP_DELAY_TICKS`.
	pub fn sample(&self, out: &mut Vec<RemoteDraw>) {
		out.clear();
		let target = self.est_server_tick - INTERP_DELAY_TICKS;
		for (&id, ring) in &self.players {
			if let Some(pos_look) = sample_ring(ring, target) {
				let (pos, look) = pos_look;
				out.push(RemoteDraw {
					id,
					pos,
					yaw: look.x.atan2(look.z),
				});
			}
		}
	}
}

fn sample_ring(ring: &VecDeque<RemoteSnap>, target: f64) -> Option<(Vec3, Vec3)> {
	let newest = *ring.back()?;
	if target >= newest.server_tick as f64 {
		// Past the newest snapshot: extrapolate along velocity, capped.
		let over = (target - newest.server_tick as f64).min(EXTRAP_CAP_TICKS);
		let pos = newest.pos + newest.vel * (over as f32 / TICK_RATE as f32);
		return Some((pos, newest.look));
	}
	let oldest = *ring.front()?;
	if target <= oldest.server_tick as f64 {
		return Some((oldest.pos, oldest.look));
	}
	// Bracket: last snap ≤ target and first snap > target.
	let mut a = oldest;
	for &s in ring.iter() {
		if (s.server_tick as f64) <= target {
			a = s;
		} else {
			let span = (s.server_tick - a.server_tick) as f64;
			let t = ((target - a.server_tick as f64) / span) as f32;
			let pos = a.pos.lerp(s.pos, t);
			let look = a.look.lerp(s.look, t).normalize_or_zero();
			return Some((pos, look));
		}
	}
	Some((newest.pos, newest.look))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn snap(tick: u32, x: f32, vx: f32) -> RemoteSnap {
		RemoteSnap {
			server_tick: tick,
			pos: Vec3::new(x, 0.0, 0.0),
			vel: Vec3::new(vx, 0.0, 0.0),
			look: Vec3::Z,
		}
	}

	#[test]
	fn interp_sampling_is_exact_lerp() {
		let mut r = RemotePlayers::default();
		r.push(1, snap(0, 0.0, 0.0));
		r.push(1, snap(3, 3.0, 0.0));
		r.push(1, snap(6, 12.0, 0.0));
		// est clock = 6; force target = 4.5 by lifting est to 10.5.
		r.est_server_tick = 4.5 + INTERP_DELAY_TICKS;
		let mut out = Vec::new();
		r.sample(&mut out);
		assert_eq!(out.len(), 1);
		// Halfway between tick 3 (x=3) and tick 6 (x=12) → x = 7.5.
		assert!((out[0].pos.x - 7.5).abs() < 1e-5, "x = {}", out[0].pos.x);
	}

	#[test]
	fn extrapolation_capped_then_holds() {
		let mut r = RemotePlayers::default();
		r.push(1, snap(0, 0.0, 6.0)); // 6 blocks/s = 0.1 blocks/tick
		let mut out = Vec::new();

		// 10 ticks past the newest snap → 1.0 block extrapolated.
		r.est_server_tick = 10.0 + INTERP_DELAY_TICKS;
		r.sample(&mut out);
		assert!((out[0].pos.x - 1.0).abs() < 1e-5, "x = {}", out[0].pos.x);

		// 100 ticks past → capped at EXTRAP_CAP_TICKS (15) → 1.5 blocks.
		r.est_server_tick = 100.0 + INTERP_DELAY_TICKS;
		r.sample(&mut out);
		assert!((out[0].pos.x - 1.5).abs() < 1e-5, "x = {}", out[0].pos.x);
	}

	#[test]
	fn out_of_order_snaps_ignored() {
		let mut r = RemotePlayers::default();
		r.push(1, snap(6, 6.0, 0.0));
		r.push(1, snap(3, 3.0, 0.0)); // late datagram
		let ring = &r.players[&1];
		assert_eq!(ring.len(), 1);
		assert_eq!(ring.back().unwrap().server_tick, 6);
	}
}
