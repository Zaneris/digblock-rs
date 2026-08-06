//! Adaptive per-frame streaming budget for the inline (wasm) executor.
//! Feedback controller over measured frame headroom: grows the budget
//! slowly while the worst recent frame still has slack against a 60fps
//! frame interval, collapses to the floor on any near-miss. The floor is
//! the fixed budget the engine ships with, so "no headroom proven" is
//! exactly today's behavior.

/// 60fps frame interval — the protection target. High-refresh displays
/// are deliberately allowed to eat down to 60fps while streaming.
const INTERVAL_MS: f32 = 16.7;
/// Budget ceiling: leaves >4ms for update + encode inside the interval
/// even with the drain/upload overshoot on top of the job allowance.
const CEIL_MS: f32 = 10.0;
/// Safety band under the interval: covers the growth taken during the
/// 2-frame metrics feedback latency plus one job's prediction error, so
/// the frame that triggers backoff is a near-miss, not a dropped frame.
const MARGIN_MS: f32 = 2.0;
/// Additive growth per clean frame — floor to ceiling in ~0.5s.
const GROW_MS: f32 = 0.25;
/// Worst-of window (~0.5s at 60Hz). A single near-miss sample blocks
/// growth until it ages out; an EMA would hide exactly that frame.
const WINDOW_N: usize = 30;
/// Growth holdoff after a hard miss, in frames.
const COOLDOWN: u32 = 30;
/// Samples at/above this are not frames: the platform dt clamp, rAF
/// throttling in background tabs, and resume gaps all land here.
const STALE_MS: f32 = 100.0;
/// A frame past interval × this overran its vsync slot outright.
const MISS_FACTOR: f32 = 1.25;

pub struct StreamBudget {
	floor_ms: f32,
	budget_ms: f32,
	/// Busy-time ring (total − acquire), newest last.
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

	/// Feed the previous frame's wall delta and vsync wait; returns the
	/// budget for this frame. Growth requires a full window of valid
	/// samples, so cold starts and throttled tabs sit at the floor.
	pub fn on_frame(&mut self, total_ms: f32, acquire_ms: f32) -> f32 {
		if total_ms <= 0.0 || total_ms >= STALE_MS {
			self.reset();
			return self.floor_ms;
		}
		if total_ms > INTERVAL_MS * MISS_FACTOR {
			self.reset();
			self.cooldown = COOLDOWN;
			return self.floor_ms;
		}
		self.push((total_ms - acquire_ms).max(0.0));
		let headroom = INTERVAL_MS - MARGIN_MS - self.worst();
		if headroom < 0.0 {
			// Full-deficit backoff: collapses to the floor within a few
			// frames and stays there until the bad sample ages out.
			self.budget_ms = (self.budget_ms + headroom).max(self.floor_ms);
		} else if self.cooldown > 0 {
			self.cooldown -= 1;
		} else if self.len == WINDOW_N {
			self.budget_ms = (self.budget_ms + GROW_MS.min(headroom)).min(CEIL_MS);
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

	fn feed(b: &mut StreamBudget, n: usize, total: f32, acquire: f32) -> f32 {
		let mut out = b.budget_ms();
		for _ in 0..n {
			out = b.on_frame(total, acquire);
		}
		out
	}

	#[test]
	fn holds_floor_until_window_fills() {
		let mut b = StreamBudget::new(FLOOR);
		let out = feed(&mut b, WINDOW_N - 1, 16.0, 11.0);
		assert_eq!(out, FLOOR, "no growth before a full window");
		assert!(
			b.on_frame(16.0, 11.0) > FLOOR,
			"grows once the window fills"
		);
	}

	#[test]
	fn grows_additively_then_caps() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, WINDOW_N, 16.0, 11.0); // busy 5.0, headroom 9.7
		let before = b.budget_ms();
		let after = b.on_frame(16.0, 11.0);
		assert!((after - before - GROW_MS).abs() < 1e-4, "additive step");
		let capped = feed(&mut b, 200, 16.0, 11.0);
		assert_eq!(capped, CEIL_MS, "stops at the ceiling");
	}

	#[test]
	fn growth_clamped_by_headroom() {
		let mut b = StreamBudget::new(FLOOR);
		// busy 14.5 → headroom 0.2 < GROW_MS: steps shrink to the slack.
		let out = feed(&mut b, 400, 16.0, 1.5);
		assert!(
			out <= INTERVAL_MS - MARGIN_MS,
			"never past interval − margin"
		);
		assert!(out > FLOOR, "still grows within the slack");
	}

	#[test]
	fn near_miss_collapses_to_floor() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 11.0);
		assert_eq!(b.budget_ms(), CEIL_MS);
		// One busy = 16.0 sample: headroom −1.3 every frame it lingers.
		b.on_frame(16.4, 0.4);
		let mut prev = b.budget_ms();
		assert!(prev < CEIL_MS, "backoff starts immediately");
		for _ in 0..WINDOW_N - 1 {
			let cur = b.on_frame(16.0, 11.0);
			assert!(cur <= prev, "monotone while the sample lingers");
			prev = cur;
		}
		assert_eq!(prev, FLOOR, "collapsed to floor within the window");
		assert!(b.on_frame(16.0, 11.0) > FLOOR, "re-grows once it ages out");
	}

	#[test]
	fn hard_miss_resets_with_cooldown() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 11.0);
		assert_eq!(b.on_frame(30.0, 0.0), FLOOR, "hard miss resets instantly");
		// Cooldown + window refill must both elapse before growth.
		let held = feed(&mut b, (COOLDOWN as usize).max(WINDOW_N), 16.0, 11.0);
		assert_eq!(held, FLOOR, "no growth during cooldown/refill");
		assert!(b.on_frame(16.0, 11.0) > FLOOR, "then growth resumes");
	}

	#[test]
	fn background_tab_pins_floor() {
		let mut b = StreamBudget::new(FLOOR);
		feed(&mut b, 200, 16.0, 11.0);
		let out = feed(&mut b, 20, 1000.0, 990.0);
		assert_eq!(out, FLOOR, "throttled frames reset and pin the floor");
		let out = feed(&mut b, WINDOW_N + 1, 16.0, 11.0);
		assert!(out > FLOOR, "recovers after real frames resume");
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
			let total = (x % 4000) as f32 / 100.0; // 0..40ms
			let acquire = ((x >> 16) % 1000) as f32 / 100.0; // 0..10ms
			let out = b.on_frame(total, acquire);
			assert!(
				(FLOOR..=CEIL_MS).contains(&out),
				"budget {out} out of bounds"
			);
		}
	}
}
