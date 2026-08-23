use std::time::Duration;
use web_time::Instant;

/// Game clock that pauses across suspend. First tick after unpause yields dt = 0; dt is clamped so
/// debugger pauses or missed lifecycle events can never produce a giant simulation step. `fixed`
/// replaces the wall clock with a constant step, which is what makes a capture comparable ACROSS
/// BUILDS: everything animated rides dt, so two builds of different speed reach `--frames N` in
/// different world states and the image difference says nothing about the change under test.
pub(crate) struct GameClock {
	last_tick: Option<Instant>,
	fixed: Option<Duration>,
}

const MAX_DT: Duration = Duration::from_millis(100);

impl GameClock {
	pub(crate) fn new() -> Self {
		Self {
			last_tick: None,
			fixed: None,
		}
	}

	/// Step the world by exactly this much per frame instead of by
	/// measured time (`--fixed-dt`, capture runs only).
	pub(crate) fn with_fixed_step(step: Duration) -> Self {
		Self {
			last_tick: None,
			fixed: Some(step),
		}
	}

	pub(crate) fn pause(&mut self) {
		self.last_tick = None;
	}

	pub(crate) fn tick(&mut self) -> Duration {
		if let Some(step) = self.fixed {
			// Still the zero first tick, so nothing observes a step
			// before the first frame is drawn.
			let dt = if self.last_tick.is_some() {
				step
			} else {
				Duration::ZERO
			};
			self.last_tick = Some(Instant::now());
			return dt;
		}
		let now = Instant::now();
		let dt = match self.last_tick {
			Some(last) => (now - last).min(MAX_DT),
			None => Duration::ZERO,
		};
		self.last_tick = Some(now);
		dt
	}
}
