//! Render-time sampling of the fixed-timestep player sim (pure helper).

use digblock_core::player::TICK;
use glam::Vec3;

/// Interpolation anchor: the player position from *before* the most recent sim tick. The sim runs
/// on a fixed-`TICK` accumulator while frames arrive at whatever rate the display allows, so
/// drawing the raw post-tick position aliases — the camera alternates between not moving and moving
/// double. Draw between the two states by how far the accumulator has refilled; extrapolating
/// forward instead overshoots into walls. Presentation only: gameplay reads `Player::pos`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TickLerp {
	prev: Vec3,
}

impl TickLerp {
	pub(crate) fn new(pos: Vec3) -> Self {
		Self { prev: pos }
	}

	/// Re-anchor with no interpolation pending: teleports, world swaps,
	/// and any frame that SKIPS the sim (an unloaded feet column drops
	/// the accumulator, which would otherwise leave alpha at 0 and draw a
	/// tick-sized step backwards).
	pub(crate) fn reset(&mut self, pos: Vec3) {
		self.prev = pos;
	}

	/// Record the pre-tick position. Called immediately before every
	/// `Player::tick`, so after a multi-tick frame the anchor is the
	/// state before the LAST tick — the one being interpolated toward.
	pub(crate) fn before_tick(&mut self, pos: Vec3) {
		self.prev = pos;
	}

	/// A reconcile rewound the player by `error` (pre − post). Carry the
	/// anchor with it: otherwise the lerp interpolates from the
	/// pre-rewind position and re-injects the very jump the camera's
	/// correction offset exists to hide.
	pub(crate) fn rewind(&mut self, error: Vec3) {
		self.prev -= error;
	}

	/// Where to draw, given the post-tick position and the leftover accumulator; `accum` is
	/// always < `TICK` after the sim loop, so the clamp is a backstop. Written out rather than
	/// `Vec3::lerp`: glam's `a*(1 - t) + b*t` is not exact when `a == b`, while `a + (b - a)*t`
	/// returns `a` bit-exactly. `GiDraw::flush_lights` compares a carried light's position for
	/// exact equality to decide staleness, so a still torch-bearer would rebuild for nothing.
	pub(crate) fn render_pos(&self, curr: Vec3, accum: f32) -> Vec3 {
		let t = (accum / TICK).clamp(0.0, 1.0);
		self.prev + (curr - self.prev) * t
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A player who is not moving must draw at EXACTLY the same place every frame, in every bit
	/// — not merely close. The accumulator sweeps the whole tick between frames, so the lerp
	/// parameter differs every time even at rest. `GiDraw::flush_lights` compares a carried
	/// light's position for exact equality to decide whether the GPU light list is stale.
	/// Coordinates include awkward fractions and a negative; powers of two pass either way.
	#[test]
	fn a_resting_camera_does_not_drift() {
		for p in [
			Vec3::new(64.0, 70.5, 64.0),
			Vec3::new(12.25, 137.3, -3.75),
			Vec3::new(-1023.7, 200.05, 511.9),
		] {
			let lerp = TickLerp::new(p);
			for i in 0..20_000 {
				let accum = TICK * (i as f32 / 20_000.0);
				assert_eq!(lerp.render_pos(p, accum), p, "{p:?} at accum {accum}");
			}
		}
	}

	/// The bug this module exists for: a fixed-rate sim sampled by jittery
	/// frames of nearly the same period draws zero or two ticks of motion
	/// per frame. Interpolating must hold apparent speed near constant.
	#[test]
	fn jittery_frames_draw_constant_speed() {
		const SPEED: f32 = 4.0;
		let deltas = [0.0155f32, 0.0180, 0.0150, 0.0185, 0.0160, 0.0170];
		let run = |interp: bool| -> (f32, f32) {
			let mut lerp = TickLerp::new(Vec3::ZERO);
			let (mut pos, mut accum, mut cam) = (Vec3::ZERO, 0.0f32, Vec3::ZERO);
			let (mut lo, mut hi) = (f32::INFINITY, 0.0f32);
			for i in 0..120 {
				let dt = deltas[i % deltas.len()];
				accum += dt;
				while accum >= TICK {
					accum -= TICK;
					lerp.before_tick(pos);
					pos.z += SPEED * TICK;
				}
				let drawn = if interp {
					lerp.render_pos(pos, accum)
				} else {
					pos
				};
				// Steady state only: for the first few frames the anchor is
				// still priming (it starts equal to `curr`, so the lerp has
				// nothing to span and the drawn position lags the formula).
				if i > 10 {
					let v = (drawn - cam).length() / dt / SPEED;
					lo = lo.min(v);
					hi = hi.max(v);
				}
				cam = drawn;
			}
			(lo, hi)
		};
		let (raw_lo, raw_hi) = run(false);
		assert!(
			raw_lo < 0.1 && raw_hi > 1.5,
			"raw sampling should stall then double: {raw_lo}..{raw_hi}"
		);
		let (lo, hi) = run(true);
		assert!(
			lo > 0.95 && hi < 1.05,
			"interpolated speed should be near constant: {lo}..{hi}"
		);
	}

	/// A frame that runs no tick still advances toward the current state
	/// as the accumulator refills — that is what removes the stall.
	#[test]
	fn tickless_frame_still_advances() {
		let mut lerp = TickLerp::new(Vec3::ZERO);
		lerp.before_tick(Vec3::ZERO);
		let curr = Vec3::new(0.0, 0.0, 1.0);
		let a = lerp.render_pos(curr, TICK * 0.25);
		let b = lerp.render_pos(curr, TICK * 0.75);
		assert!(a.z > 0.2 && a.z < 0.3, "quarter-way: {}", a.z);
		assert!(b.z > 0.7 && b.z < 0.8, "three-quarters: {}", b.z);
	}

	/// After a rewind the drawn position must not jump: the camera adds
	/// `error` back as its correction offset, so `prev + error` has to
	/// land exactly where the pre-rewind interpolation was.
	#[test]
	fn rewind_carries_the_anchor() {
		let mut lerp = TickLerp::new(Vec3::ZERO);
		lerp.before_tick(Vec3::ZERO);
		let curr = Vec3::new(0.0, 0.0, 1.0);
		let accum = TICK * 0.5;
		let before = lerp.render_pos(curr, accum);

		// Reconcile: the server put us back, so error = pre - post.
		let error = Vec3::new(0.0, 0.0, 0.3);
		let rewound = curr - error;
		lerp.rewind(error);
		let after = lerp.render_pos(rewound, accum) + error;
		assert!(
			(after - before).length() < 1e-6,
			"camera moved across the rewind: {before:?} -> {after:?}"
		);
	}

	/// Without the rewind the same reconcile visibly yanks the camera —
	/// the regression the test above pins.
	#[test]
	fn rewind_is_load_bearing() {
		let mut lerp = TickLerp::new(Vec3::ZERO);
		lerp.before_tick(Vec3::ZERO);
		let curr = Vec3::new(0.0, 0.0, 1.0);
		let accum = TICK * 0.5;
		let before = lerp.render_pos(curr, accum);
		let error = Vec3::new(0.0, 0.0, 0.3);
		let after = lerp.render_pos(curr - error, accum) + error;
		assert!((after - before).length() > 0.1, "expected a visible jump");
	}

	/// A skipped sim frame zeroes the accumulator; without the re-anchor
	/// that draws a whole tick BACKWARDS.
	#[test]
	fn reset_prevents_backward_step_on_skipped_sim() {
		let mut lerp = TickLerp::new(Vec3::ZERO);
		lerp.before_tick(Vec3::ZERO);
		let curr = Vec3::new(0.0, 0.0, 1.0);
		assert_eq!(lerp.render_pos(curr, 0.0), Vec3::ZERO, "the backward step");
		lerp.reset(curr);
		assert_eq!(lerp.render_pos(curr, 0.0), curr, "held in place instead");
	}

	#[test]
	fn accum_at_or_past_a_full_tick_lands_on_curr() {
		let mut lerp = TickLerp::new(Vec3::ZERO);
		lerp.before_tick(Vec3::ZERO);
		let curr = Vec3::new(0.0, 0.0, 1.0);
		assert_eq!(lerp.render_pos(curr, TICK), curr);
		assert_eq!(lerp.render_pos(curr, TICK * 4.0), curr, "clamped");
	}
}
