//! Adaptive per-frame streaming budget for the wasm client.
//!
//! It governs the whole main-thread streaming deadline — pulling a job
//! pool's replies, the result drain, evictions and GPU uploads — and NOT
//! only the jobs, which is why it is not conditioned on where jobs run.
//! Moving jobs to a pool moves the jobs; absorbing their results stays
//! pinned to the thread that owns the device.
//!
//! Feedback controller over measured frame headroom: grows the budget
//! slowly while the worst recent frame still has slack against the
//! protected frame interval, collapses to the floor on any near-miss.
//! The floor is the fixed budget the engine ships with, so "no headroom
//! proven" is exactly today's behavior.
//!
//! Headroom says how much the frame CAN afford; [`urgency`] says how
//! much the pending work DESERVES. Steady-state walking only churns
//! chunks at the draw-distance rim, and burning the full ceiling on
//! those trades visible stutter for fill speed nobody perceives — so
//! the ceiling is scaled by how near the closest pending job is, and
//! rim-only churn streams at the fixed floor exactly like the
//! pre-adaptive build.

/// The frame interval this controller protects — deliberately a fixed
/// target rather than the display's, so high-refresh displays are allowed
/// to eat back down to it while streaming.
const INTERVAL_MS: f32 = 16.7;
/// Budget ceiling: leaves room for update + encode inside the interval
/// even with the drain/upload overshoot on top of the job allowance.
const CEIL_MS: f32 = 10.0;
/// Safety band under the interval: covers the growth taken during the
/// 2-frame metrics feedback latency plus one job's prediction error, so
/// the frame that triggers backoff is a near-miss, not a dropped frame.
const MARGIN_MS: f32 = 2.0;
/// Additive growth per clean frame: the climb from floor to ceiling is a
/// ramp over many frames, never a jump.
const GROW_MS: f32 = 0.25;
/// Worst-of window, in frames. A single near-miss sample blocks growth
/// until it ages out; an EMA would hide exactly that frame.
const WINDOW_N: usize = 30;
/// Growth holdoff after a hard miss, in frames.
const COOLDOWN: u32 = 30;
/// Samples at/above this are not frames: the platform dt clamp, rAF
/// throttling in background tabs, and resume gaps all land here.
const STALE_MS: f32 = 100.0;
/// A frame past interval × this overran its vsync slot outright.
const MISS_FACTOR: f32 = 1.25;
/// Work closer than this fraction of the draw distance gets the full
/// ceiling; from here the ceiling fades linearly to the floor at the
/// draw distance itself.
const URGENT_FRAC: f32 = 0.5;

/// Ceiling scale (0..=1) from the nearest pending job's flat distance
/// to the player, both in chunk units: 1 inside `URGENT_FRAC` of the
/// draw distance (initial fill, teleports, near edits), fading to 0 at
/// the rim (the load/unload churn of ordinary walking).
pub fn urgency(nearest_dist: f32, draw_dist: f32) -> f32 {
	let start = draw_dist * URGENT_FRAC;
	let span = draw_dist - start;
	if span <= 0.0 {
		return if nearest_dist < draw_dist { 1.0 } else { 0.0 };
	}
	(1.0 - (nearest_dist - start) / span).clamp(0.0, 1.0)
}

pub struct StreamBudget {
	floor_ms: f32,
	budget_ms: f32,
	/// Measured busy-time ring, newest last.
	busy: [f32; WINDOW_N],
	len: usize,
	head: usize,
	cooldown: u32,
}

impl StreamBudget {
	pub fn new(floor_ms: f32) -> Self {
		Self {
			floor_ms,
			budget_ms: floor_ms,
			busy: [0.0; WINDOW_N],
			len: 0,
			head: 0,
			cooldown: 0,
		}
	}

	pub fn budget_ms(&self) -> f32 {
		self.budget_ms
	}

	/// Drop all evidence and return to the floor (suspend, hard miss).
	pub fn reset(&mut self) {
		self.budget_ms = self.floor_ms;
		self.len = 0;
		self.head = 0;
	}

	/// Feed the previous frame's wall delta and its MEASURED main-thread
	/// occupancy; returns the budget for this frame. Growth requires a full
	/// window of valid samples, so cold starts and throttled tabs sit at
	/// the floor. `urgency` (see [`urgency`]) scales the ceiling between
	/// the floor and `CEIL_MS`; a drop clamps the stored budget
	/// immediately, and recovery re-grows additively — near work appearing
	/// never slams the full ceiling onto an unproven frame.
	///
	/// `busy_ms` must be measured (platform `frame_busy` minus the acquire
	/// wait), NEVER derived as `frame_ms − acquire_ms`. That subtraction is
	/// right only where vsync blocks inside the swapchain acquire: on the
	/// web `getCurrentTexture` cannot block, pacing lives in rAF between
	/// frames, and the derived value reads as a full interval every frame —
	/// headroom is then permanently negative and this controller pins to
	/// its floor forever. See `web_signal_shape_still_grows`.
	pub fn on_frame(&mut self, frame_ms: f32, busy_ms: f32, urgency: f32) -> f32 {
		if frame_ms <= 0.0 || frame_ms >= STALE_MS {
			self.reset();
			return self.floor_ms;
		}
		if frame_ms > INTERVAL_MS * MISS_FACTOR {
			self.reset();
			self.cooldown = COOLDOWN;
			return self.floor_ms;
		}
		self.push(busy_ms.max(0.0));
		let ceil = self.floor_ms + (CEIL_MS - self.floor_ms).max(0.0) * urgency.clamp(0.0, 1.0);
		self.budget_ms = self.budget_ms.min(ceil);
		let headroom = INTERVAL_MS - MARGIN_MS - self.worst();
		if headroom < 0.0 {
			// Full-deficit backoff: collapses to the floor within a few
			// frames and stays there until the bad sample ages out.
			self.budget_ms = (self.budget_ms + headroom).max(self.floor_ms);
		} else if self.cooldown > 0 {
			self.cooldown -= 1;
		} else if self.len == WINDOW_N {
			self.budget_ms = (self.budget_ms + GROW_MS.min(headroom)).min(ceil);
		}
		self.budget_ms
	}

	fn push(&mut self, busy_ms: f32) {
		self.busy[self.head] = busy_ms;
		self.head = (self.head + 1) % WINDOW_N;
		self.len = (self.len + 1).min(WINDOW_N);
	}

	fn worst(&self) -> f32 {
		self.busy[..self.len].iter().fold(0.0f32, |a, &b| a.max(b))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const FLOOR: f32 = 3.0;

	fn feed(b: &mut StreamBudget, n: usize, frame: f32, busy: f32) -> f32 {
		let mut out = b.budget_ms();
		for _ in 0..n {
			out = b.on_frame(frame, busy, 1.0);
		}
		out
	}

	#[test]
	fn holds_floor_until_window_fills() {
		let mut b = StreamBudget::new(FLOOR);
		let out = feed(&mut b, WINDOW_N - 1, 16.0, 5.0);
		assert_eq!(out, FLOOR, "no growth before a full window");
		assert!(
			b.on_frame(16.0, 5.0, 1.0) > FLOOR,
			"grows once the window fills"
		);
	}

	#[test]
	fn grows_additively_then_caps() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, WINDOW_N, 16.0, 5.0); // light frames, plenty of headroom
		let before = b.budget_ms();
		let after = b.on_frame(16.0, 5.0, 1.0);
		assert!((after - before - GROW_MS).abs() < 1e-4, "additive step");
		let capped = feed(&mut b, 200, 16.0, 5.0);
		assert_eq!(capped, CEIL_MS, "stops at the ceiling");
	}

	#[test]
	fn growth_clamped_by_headroom() {
		let mut b = StreamBudget::new(FLOOR);
		// Busy enough that the headroom left is under GROW_MS: steps
		// shrink to the slack.
		let out = feed(&mut b, 400, 16.0, 14.5);
		assert!(
			out <= INTERVAL_MS - MARGIN_MS,
			"never past interval − margin"
		);
		assert!(out > FLOOR, "still grows within the slack");
	}

	#[test]
	fn near_miss_collapses_to_floor() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 5.0);
		assert_eq!(b.budget_ms(), CEIL_MS);
		// One sample busy past the interval: headroom stays negative for
		// every frame it lingers in the window.
		b.on_frame(16.4, 16.0, 1.0);
		let mut prev = b.budget_ms();
		assert!(prev < CEIL_MS, "backoff starts immediately");
		for _ in 0..WINDOW_N - 1 {
			let cur = b.on_frame(16.0, 5.0, 1.0);
			assert!(cur <= prev, "monotone while the sample lingers");
			prev = cur;
		}
		assert_eq!(prev, FLOOR, "collapsed to floor within the window");
		assert!(
			b.on_frame(16.0, 5.0, 1.0) > FLOOR,
			"re-grows once it ages out"
		);
	}

	#[test]
	fn hard_miss_resets_with_cooldown() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 5.0);
		assert_eq!(
			b.on_frame(30.0, 30.0, 1.0),
			FLOOR,
			"hard miss resets instantly"
		);
		// Cooldown + window refill must both elapse before growth.
		let held = feed(&mut b, (COOLDOWN as usize).max(WINDOW_N), 16.0, 5.0);
		assert_eq!(held, FLOOR, "no growth during cooldown/refill");
		assert!(b.on_frame(16.0, 5.0, 1.0) > FLOOR, "then growth resumes");
	}

	#[test]
	fn background_tab_pins_floor() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 5.0);
		let out = feed(&mut b, 20, 1000.0, 10.0);
		assert_eq!(out, FLOOR, "throttled frames reset and pin the floor");
		let out = feed(&mut b, WINDOW_N + 1, 16.0, 5.0);
		assert!(out > FLOOR, "recovers after real frames resume");
	}

	/// REGRESSION: the web frame shape must still prove headroom.
	///
	/// On the web the swapchain acquire never blocks and rAF paces the loop
	/// between frames, so every frame's wall delta is a full display
	/// interval no matter how little work it did. The controller previously
	/// derived busy as `frame − acquire`, which on this shape reads as
	/// "always full" and pinned the budget to its floor forever — the
	/// adaptive path was inert on the one platform it exists for.
	#[test]
	fn web_signal_shape_still_grows() {
		for &busy in &[1.0f32, 2.0, 5.0, 8.0] {
			let mut b = StreamBudget::new(FLOOR);
			// rAF-paced: the delta is a full display interval, acquire is
			// ~0, and the real work is `busy`.
			let out = feed(&mut b, 200, 16.7, busy);
			assert_eq!(
				out, CEIL_MS,
				"web frame with {busy}ms of work must reach the ceiling"
			);
		}
		// And a genuinely busy web frame still refuses to grow.
		let mut b = StreamBudget::new(FLOOR);
		assert_eq!(
			feed(&mut b, 200, 16.7, 15.5),
			FLOOR,
			"no headroom left means no growth"
		);
	}

	/// A high-refresh web display quantizes the wall delta to whole display
	/// slots while the work itself varies continuously; the budget must
	/// track the work, not the quantized delta (which used to bistably slam
	/// floor/ceiling).
	#[test]
	fn high_refresh_web_tracks_work_not_quantized_delta() {
		let mut b = StreamBudget::new(FLOOR);
		// Work spilled past one display slot, so the delta doubled — but
		// that much work still leaves real headroom against the protected
		// interval.
		assert_eq!(feed(&mut b, 200, 16.7, 9.0), CEIL_MS);
	}

	#[test]
	fn bounds_fuzz() {
		let mut b = StreamBudget::new(FLOOR);
		// Deterministic pseudo-random walk over the input space.
		let mut x = 0x2545f4914f6cdd1du64;
		for _ in 0..5000 {
			x ^= x << 13;
			x ^= x >> 7;
			x ^= x << 17;
			let frame = (x % 4000) as f32 / 100.0; // tiny frames through stale ones
			let busy = ((x >> 16) % 4000) as f32 / 100.0; // the same span of work
			let urgency = ((x >> 32) % 100) as f32 / 100.0; // the whole urgency range
			let out = b.on_frame(frame, busy, urgency);
			assert!(
				(FLOOR..=CEIL_MS).contains(&out),
				"budget {out} out of bounds"
			);
		}
	}

	#[test]
	fn zero_urgency_pins_floor() {
		let mut b = StreamBudget::new(FLOOR);
		for _ in 0..200 {
			assert_eq!(
				b.on_frame(16.0, 5.0, 0.0),
				FLOOR,
				"rim-only work never grows past the floor"
			);
		}
	}

	#[test]
	fn urgency_scales_ceiling() {
		let mut b = StreamBudget::new(FLOOR);
		let mut out = FLOOR;
		for _ in 0..400 {
			out = b.on_frame(16.0, 5.0, 0.5);
		}
		let want = FLOOR + (CEIL_MS - FLOOR) * 0.5;
		assert!((out - want).abs() < 1e-4, "caps at the scaled ceiling");
	}

	#[test]
	fn urgency_drop_clamps_then_regrows_additively() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 5.0);
		assert_eq!(b.budget_ms(), CEIL_MS);
		assert_eq!(
			b.on_frame(16.0, 5.0, 0.0),
			FLOOR,
			"urgency drop clamps immediately"
		);
		// Near work reappears: no jump back to the ceiling — one
		// additive step from the floor.
		let out = b.on_frame(16.0, 5.0, 1.0);
		assert!(
			(out - FLOOR - GROW_MS).abs() < 1e-4,
			"regrowth is additive, got {out}"
		);
	}

	#[test]
	fn urgency_fade_shape() {
		assert_eq!(urgency(0.0, 20.0), 1.0, "at the player");
		assert_eq!(urgency(10.0, 20.0), 1.0, "full inside the urgent band");
		assert!((urgency(15.0, 20.0) - 0.5).abs() < 1e-4, "midpoint");
		assert_eq!(urgency(20.0, 20.0), 0.0, "zero at the rim");
		assert_eq!(urgency(25.0, 20.0), 0.0, "clamped beyond");
		let mut prev = 1.0;
		for d in 0..40 {
			let u = urgency(d as f32 / 2.0 + 0.25, 10.0);
			assert!(u <= prev, "monotone non-increasing");
			prev = u;
		}
	}
}
