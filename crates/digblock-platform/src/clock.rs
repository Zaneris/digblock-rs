use std::time::Duration;
use web_time::Instant;

/// Game clock that pauses across suspend. First tick after unpause yields
/// dt = 0; dt is clamped so debugger pauses or missed lifecycle events can never
/// produce a giant simulation step.
///
/// `fixed` replaces the wall clock with a constant step, which is what makes a
/// capture comparable ACROSS BUILDS. Everything animated in this engine rides dt
/// — the water surface's ripple phase and uv advection, the fluid sim, the spawn
/// settle — so two builds of different speed reach `--frames N` in different
/// world states, and the resulting image difference has nothing to do with the
/// change under test. At DIG_RIVER a same-build repeat diffs an order of
/// magnitude below ANY cross-build pair, entirely in the ripple bands; a fixed
/// step makes both zero (GI-DESIGN.md R19).
pub struct GameClock {
	last_tick: Option<Instant>,
	fixed: Option<Duration>,
}

const MAX_DT: Duration = Duration::from_millis(100);

impl GameClock {
	pub fn new() -> Self {
		Self {
			last_tick: None,
			fixed: None,
		}
	}

	/// Step the world by exactly this much per frame instead of by
	/// measured time (`--fixed-dt`, capture runs only).
	pub fn with_fixed_step(step: Duration) -> Self {
		Self {
			last_tick: None,
			fixed: Some(step),
		}
	}

	pub fn pause(&mut self) {
		self.last_tick = None;
	}

	pub fn tick(&mut self) -> Duration {
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
