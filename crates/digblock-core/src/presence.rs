//! How many players a world has HAD lately — the rolling high-water mark
//! a rule can scale by when "how busy is this world" matters more than
//! "who is here this tick".
//!
//! One tracker, fed by whichever host owns the roster (the server counts
//! live clients, a local world counts its one player), answering the
//! peak concurrent count over a trailing window of the GAME's clock. The
//! window is the game day by default, for the reason every world job
//! keeps that clock: wall time means nothing to a player elsewhere.

use std::collections::VecDeque;

use crate::sun::DAY_SECS;

/// Peak of a count over a trailing window: samples are kept only where
/// the count CHANGED, so a quiet server holds a handful of entries
/// however long it runs, and the peak is a walk over what the window
/// still holds plus the sample that was in force when the window opened.
#[derive(Clone, Debug)]
pub struct RollingPeak {
	window_secs: f64,
	/// `(secs, count)`, secs non-decreasing. The front entry may predate
	/// the window: it is the count that was standing when the window
	/// opened, and a window that began mid-sample must still see it.
	samples: VecDeque<(f64, u32)>,
}

impl RollingPeak {
	pub fn new(window_secs: f64) -> Self {
		Self {
			window_secs,
			samples: VecDeque::new(),
		}
	}

	/// The game-day window — what the world jobs read.
	pub fn day() -> Self {
		Self::new(f64::from(DAY_SECS))
	}

	/// Record the count standing at `secs`. A clock that runs backwards
	/// (a fresh local world's seconds restart at zero) starts the record
	/// over rather than filing a sample behind a later one.
	pub fn observe(&mut self, secs: f64, count: u32) {
		if self.samples.back().is_some_and(|&(t, _)| secs < t) {
			self.samples.clear();
		}
		if self.samples.back().is_none_or(|&(_, c)| c != count) {
			self.samples.push_back((secs, count));
		}
		self.prune(secs);
	}

	/// The highest count observed within the window ending at `secs`,
	/// including the sample in force when the window opened. Zero before
	/// the first observation.
	pub fn peak(&self, secs: f64) -> u32 {
		let from = secs - self.window_secs;
		let mut best = 0;
		for (i, &(t, c)) in self.samples.iter().enumerate() {
			// A sample older than the window still counts if the next
			// sample lands inside it — or there is no next sample — since
			// that count was standing at the window's open.
			let in_force = t >= from || self.samples.get(i + 1).is_none_or(|&(n, _)| n > from);
			if in_force && t <= secs {
				best = best.max(c);
			}
		}
		best
	}

	/// Forget everything — for a host whose world is replaced.
	pub fn reset(&mut self) {
		self.samples.clear();
	}

	/// Drop samples that can never be in force again: those with a later
	/// sample also older than the window's open.
	fn prune(&mut self, secs: f64) {
		let from = secs - self.window_secs;
		while self.samples.len() >= 2 && self.samples[1].0 <= from {
			self.samples.pop_front();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const WINDOW: f64 = 100.0;

	#[test]
	fn a_fresh_tracker_has_no_peak() {
		assert_eq!(RollingPeak::new(WINDOW).peak(0.0), 0);
	}

	#[test]
	fn the_peak_is_the_high_water_mark_inside_the_window() {
		let mut p = RollingPeak::new(WINDOW);
		p.observe(0.0, 1);
		p.observe(10.0, 3);
		p.observe(20.0, 2);
		assert_eq!(p.peak(20.0), 3);
		// The 3 stood until the 2 replaced it, so it counts while THAT
		// instant is inside the window, and not once it is out.
		assert_eq!(p.peak(20.0 + WINDOW - 1.0), 3);
		assert_eq!(p.peak(20.0 + WINDOW + 1.0), 2);
	}

	#[test]
	fn the_sample_standing_when_the_window_opened_still_counts() {
		let mut p = RollingPeak::new(WINDOW);
		p.observe(0.0, 4);
		// Nothing observed for several windows: the count never changed,
		// so the old sample is the one in force throughout.
		assert_eq!(p.peak(WINDOW * 5.0), 4);
		p.observe(WINDOW * 5.0, 1);
		// Now the 4 was in force until the 1 arrived, inside this window.
		assert_eq!(p.peak(WINDOW * 5.0 + 1.0), 4);
		// ...and once the 1 predates the window's open, the 4 is gone.
		assert_eq!(p.peak(WINDOW * 6.0 + 1.0), 1);
	}

	#[test]
	fn pruning_keeps_exactly_what_can_still_be_in_force() {
		let mut p = RollingPeak::new(WINDOW);
		for i in 0..1000 {
			p.observe(f64::from(i), (i % 7) as u32);
		}
		// Every retained sample is inside the window, except at most the
		// one standing at its open.
		let from = 999.0 - WINDOW;
		let stale = p.samples.iter().filter(|&&(t, _)| t < from).count();
		assert!(stale <= 1, "{stale} samples older than the window survive");
		assert_eq!(p.peak(999.0), 6);
	}

	#[test]
	fn an_unchanged_count_files_no_sample() {
		let mut p = RollingPeak::new(WINDOW);
		for t in 0..50 {
			p.observe(f64::from(t), 2);
		}
		assert_eq!(p.samples.len(), 1);
	}

	#[test]
	fn a_rewound_clock_starts_over() {
		let mut p = RollingPeak::new(WINDOW);
		p.observe(50.0, 9);
		p.observe(0.0, 1);
		assert_eq!(p.peak(0.0), 1);
	}

	#[test]
	fn the_default_window_is_the_game_day() {
		let mut p = RollingPeak::day();
		p.observe(0.0, 5);
		p.observe(1.0, 1);
		assert_eq!(p.peak(f64::from(DAY_SECS)), 5);
		assert_eq!(p.peak(f64::from(DAY_SECS) + 1.5), 1);
	}
}
