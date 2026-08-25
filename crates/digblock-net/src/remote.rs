//! Remote player interpolation: render other players [`INTERP_DELAY_TICKS`]
//! behind the estimated server clock, lerping between snapshot states, with
//! capped velocity extrapolation when snapshots stall.

use crate::protocol::{
	EXTRAP_CAP_TICKS, INTERP_DELAY_TICKS, PlayerId, REMOTE_EXPIRE_TICKS, TICK_RATE,
};
use glam::Vec3;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;

const SNAP_CAP: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct RemoteSnap {
	pub server_tick: u32,
	pub pos: Vec3,
	pub vel: Vec3,
	/// Facing as the wire's whole-turn fraction, interpolated in that same
	/// space (see [`yaw_lerp`]) so a player crossing north takes the short
	/// way round.
	pub yaw: u16,
	/// Held item in hotbar wire encoding (`WIRE_EMPTY`/`WIRE_TOOL`/block
	/// id). Discrete — never interpolated; the newest bracket wins.
	pub held: u16,
	/// The packed effect byte (`messages::pack_fx`), held's rules: the
	/// newest bracket wins.
	pub fx: u8,
	/// The mastery bits (`Skill::mastery_bits`), held's rules.
	pub mastery: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct RemoteDraw {
	pub id: PlayerId,
	pub pos: Vec3,
	/// Radians around +Y, from the look direction.
	pub yaw: f32,
	/// Held item in hotbar wire encoding, from the newest snapshot.
	pub held: u16,
	/// The packed effect byte, from the newest snapshot.
	pub fx: u8,
	/// The mastery bits, from the newest snapshot.
	pub mastery: u8,
}

/// The shortest between-snapshot displacement read as a teleport.
/// Bursts are presentation: a hop shorter than this reads fine as the
/// interpolation smear, and staying far above the adoption envelope's
/// instant corrections keeps a rubber-band step from flashing one.
const JUMP_MIN_DIST: f32 = 8.0;

/// A jump's implied speed must also clear the fastest honest motion —
/// terminal fall — with a factor of margin, so a walking or falling
/// player behind a stalled snapshot gap never reads as a teleport.
const JUMP_MIN_SPEED: f32 = -2.0 * digblock_core::player::TERM_VELOCITY;

#[derive(Default)]
pub struct RemotePlayers {
	players: FxHashMap<PlayerId, VecDeque<RemoteSnap>>,
	/// Detected teleports `(id, from, to)`, parked for [`Self::take_jumps`].
	jumps: Vec<(PlayerId, Vec3, Vec3)>,
	/// Estimated server clock in ticks: lifted by snapshots, advanced by
	/// wall time in between.
	///
	/// A one-way ratchet (`max`), so it can only ever run fast — a latent
	/// bug rather than a tuning choice, left unbuilt because the fix is a
	/// slewed/PLL estimator and there is no way to test clock skew end to
	/// end yet. Harmless at the skews measured so far.
	est_server_tick: f64,
	seen_any: bool,
}

impl RemotePlayers {
	pub fn advance(&mut self, dt: f32) {
		if self.seen_any {
			self.est_server_tick += dt as f64 * TICK_RATE as f64;
			// Interest management stops snapshots for far-away players —
			// expire them so they vanish instead of freezing in place.
			let cutoff = self.est_server_tick - REMOTE_EXPIRE_TICKS;
			self.players.retain(
				|_, ring| matches!(ring.back(), Some(s) if (s.server_tick as f64) >= cutoff),
			);
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
		// A displacement no honest motion produces is someone else's
		// teleport, detected here so the wire needs no announcement.
		// Dropping the ring pops them to the destination instead of
		// smearing them across the map at the interpolation rate.
		if let Some(last) = ring.back() {
			let gap = (snap.server_tick - last.server_tick) as f32 / TICK_RATE as f32;
			let dist = snap.pos.distance(last.pos);
			if dist >= JUMP_MIN_DIST && dist >= JUMP_MIN_SPEED * gap {
				self.jumps.push((id, last.pos, snap.pos));
				ring.clear();
			}
		}
		if ring.len() == SNAP_CAP {
			ring.pop_front();
		}
		ring.push_back(snap);
	}

	/// Drain the teleports [`Self::push`] detected — `(id, from, to)`
	/// eye positions, for the engine to burst at both ends.
	pub fn take_jumps(&mut self) -> Vec<(PlayerId, Vec3, Vec3)> {
		std::mem::take(&mut self.jumps)
	}

	pub fn remove(&mut self, id: PlayerId) {
		self.players.remove(&id);
	}

	pub fn is_empty(&self) -> bool {
		self.players.is_empty()
	}

	/// Remotes currently being interpolated (post-expiry).
	pub fn len(&self) -> usize {
		self.players.len()
	}

	/// Sample every remote player at `est_server_tick - INTERP_DELAY_TICKS`.
	pub fn sample(&self, out: &mut Vec<RemoteDraw>) {
		out.clear();
		let target = self.est_server_tick - INTERP_DELAY_TICKS;
		for (&id, ring) in &self.players {
			if let Some((pos, yaw)) = sample_ring(ring, target) {
				// The held item and effect byte are discrete state: always
				// render the newest known value rather than a time-shifted one.
				let (held, fx, mastery) = ring
					.back()
					.map_or((u16::MAX, 0, 0), |s| (s.held, s.fx, s.mastery));
				out.push(RemoteDraw {
					id,
					pos,
					yaw: crate::messages::yaw_from_wire(yaw),
					held,
					fx,
					mastery,
				});
			}
		}
	}
}

fn sample_ring(ring: &VecDeque<RemoteSnap>, target: f64) -> Option<(Vec3, u16)> {
	let newest = *ring.back()?;
	if target >= newest.server_tick as f64 {
		// Past the newest snapshot: extrapolate along velocity, capped.
		let over = (target - newest.server_tick as f64).min(EXTRAP_CAP_TICKS);
		let pos = newest.pos + newest.vel * (over as f32 / TICK_RATE as f32);
		return Some((pos, newest.yaw));
	}
	let oldest = *ring.front()?;
	if target <= oldest.server_tick as f64 {
		return Some((oldest.pos, oldest.yaw));
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
			return Some((pos, crate::messages::yaw_lerp(a.yaw, s.yaw, t)));
		}
	}
	Some((newest.pos, newest.yaw))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn snap(tick: u32, x: f32, vx: f32) -> RemoteSnap {
		RemoteSnap {
			server_tick: tick,
			pos: Vec3::new(x, 0.0, 0.0),
			vel: Vec3::new(vx, 0.0, 0.0),
			yaw: 0,
			held: u16::MAX,
			fx: 0,
			mastery: 0,
		}
	}

	#[test]
	fn interp_sampling_is_exact_lerp() {
		// Steps well under the teleport detector's length floor, so this
		// stays a test of the lerp and not of the jump path.
		let step = JUMP_MIN_DIST / 4.0;
		let mut r = RemotePlayers::default();
		r.push(1, snap(0, 0.0, 0.0));
		r.push(1, snap(3, step, 0.0));
		r.push(1, snap(6, 3.0 * step, 0.0));
		// Lift the estimated clock so the interpolation target lands
		// squarely between the two newest snaps.
		r.est_server_tick = 4.5 + INTERP_DELAY_TICKS;
		let mut out = Vec::new();
		r.sample(&mut out);
		assert_eq!(out.len(), 1);
		// Halfway between the bracketing snaps is the midpoint of their x.
		assert!(
			(out[0].pos.x - 2.0 * step).abs() < 1e-5,
			"x = {}",
			out[0].pos.x
		);
	}

	#[test]
	fn extrapolation_capped_then_holds() {
		let vx = 6.0f32;
		let mut r = RemotePlayers::default();
		r.push(1, snap(0, 0.0, vx)); // a steady velocity along +x
		let mut out = Vec::new();

		// A short way past the newest snap → extrapolated along velocity.
		r.est_server_tick = 10.0 + INTERP_DELAY_TICKS;
		r.sample(&mut out);
		let want = vx * 10.0 / TICK_RATE as f32;
		assert!((out[0].pos.x - want).abs() < 1e-5, "x = {}", out[0].pos.x);

		// Far past it → the extrapolation clamps at EXTRAP_CAP_TICKS.
		r.est_server_tick = 100.0 + INTERP_DELAY_TICKS;
		r.sample(&mut out);
		let cap = vx * EXTRAP_CAP_TICKS as f32 / TICK_RATE as f32;
		assert!((out[0].pos.x - cap).abs() < 1e-5, "x = {}", out[0].pos.x);
	}

	#[test]
	fn stale_remote_expires_instead_of_ghosting() {
		let mut r = RemotePlayers::default();
		r.push(1, snap(0, 0.0, 0.0));
		r.push(2, snap(0, 5.0, 0.0));
		// Fresh snaps keep arriving for player 1 only.
		for t in 1..10u32 {
			r.push(1, snap(t * 20, t as f32, 0.0));
			r.advance(20.0 / TICK_RATE as f32);
		}
		let mut out = Vec::new();
		r.sample(&mut out);
		assert_eq!(out.len(), 1, "stale player 2 expired: {out:?}");
		assert_eq!(out[0].id, 1);
	}

	/// A between-snapshot displacement is a teleport only when it is both
	/// long AND faster than honest motion — a terminal fall behind a
	/// stalled snapshot gap covers real distance at a legal speed and
	/// must never burst, while a real jump pops the remote straight to
	/// its destination.
	#[test]
	fn a_snapshot_jump_is_a_teleport_and_honest_speed_never_is() {
		// Honest: terminal fall (half the speed floor) across a long gap.
		let gap_ticks = 60u32;
		let gap_secs = gap_ticks as f32 / TICK_RATE as f32;
		let fell = (JUMP_MIN_SPEED / 2.0) * gap_secs;
		assert!(fell > JUMP_MIN_DIST, "the fall must clear the length arm");
		let mut r = RemotePlayers::default();
		r.push(1, snap(0, 0.0, 0.0));
		r.push(1, snap(gap_ticks, fell, 0.0));
		assert!(r.take_jumps().is_empty(), "a fall is not a teleport");

		// A jump: longer than the floor and far faster than any fall.
		let hop = JUMP_MIN_DIST.max(JUMP_MIN_SPEED * gap_secs) * 2.0;
		let from = fell;
		r.push(1, snap(2 * gap_ticks, from + hop, 0.0));
		let jumps = r.take_jumps();
		assert_eq!(jumps.len(), 1);
		let (id, a, b) = jumps[0];
		assert_eq!(id, 1);
		assert!((a.x - from).abs() < 1e-3 && (b.x - (from + hop)).abs() < 1e-3);
		assert!(r.take_jumps().is_empty(), "drained, not re-reported");
		// The ring was dropped: the remote samples AT the destination
		// rather than smearing across the hop.
		let mut out = Vec::new();
		r.est_server_tick = 2.0 * gap_ticks as f64 + INTERP_DELAY_TICKS;
		r.sample(&mut out);
		assert!(
			(out[0].pos.x - (from + hop)).abs() < 1e-3,
			"x = {}",
			out[0].pos.x
		);
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
