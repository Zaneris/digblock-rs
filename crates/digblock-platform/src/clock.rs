use std::time::Duration;
use web_time::Instant;

/// Game clock that pauses across suspend. First tick after unpause yields
/// dt = 0; dt is clamped so debugger pauses or missed lifecycle events can
/// never produce a giant simulation step.
pub struct GameClock {
	last_tick: Option<Instant>,
}

const MAX_DT: Duration = Duration::from_millis(100);

impl GameClock {
	pub fn new() -> Self {
		Self { last_tick: None }
	}

	pub fn pause(&mut self) {
		self.last_tick = None;
	}

	pub fn tick(&mut self) -> Duration {
		let now = Instant::now();
		let dt = match self.last_tick {
			Some(last) => (now - last).min(MAX_DT),
			None => Duration::ZERO,
		};
		self.last_tick = Some(now);
		dt
	}
}
