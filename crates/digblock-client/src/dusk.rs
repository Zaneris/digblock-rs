//! The lightless character's held dusk: anyone entering a world with no
//! light of their own (a torch, the siphon, a burning wisp) gets a
//! personal clock that starts at 15:00, runs to 17:00 and stands there
//! until they first own one — except a server join in its daytime
//! (07:00–17:00), which shadows the server clock as-is and only begins
//! the stand at 17:00. The engine paints [`digblock_core::sun::Sun`]
//! from [`DuskHold::tick`] while this is active. Offline the world clock
//! IS this clock, so a light simply lets it run on; on a server the real
//! clock kept moving underneath (tracked here, re-anchored by
//! heartbeats), so the freed clock chases it at 4x while the server sits
//! inside the night window — no more than 14 hours ahead, 17:00 through
//! 07:00 — and otherwise keeps standing at 17:00 until the server's own
//! clock comes round to it, then hands over in sync. Pure and headless,
//! `tutorial`'s way.

use digblock_core::sun::{DEG_PER_HOUR, Sun, TOD_RATE_DEG_PER_SEC, tod_of_hours};

/// The personal clock's entry and stand-still readings.
const ENTRY_HOURS: f32 = 15.0;
const HOLD_HOURS: f32 = 17.0;
/// A server at most this many hours ahead is chased; further is waited
/// out at the stand-still (its clock is somewhere in the day).
const CHASE_MAX_HOURS: f32 = 14.0;
/// The chase's speed-up over the world clock's own rate.
const CHASE_RATE: f32 = 4.0;

/// The held dusk's whole life. `Off` is both "never armed" and "handed
/// over": the world clock is in charge.
#[derive(Default, Debug, PartialEq)]
pub(crate) enum DuskHold {
	#[default]
	Off,
	/// Joined in the server's daytime: showing its clock unchanged, and
	/// the stand begins only if 17:00 arrives with still no light.
	Shadowing { server: f32 },
	/// Running 15:00 → 17:00, then standing, light unseen. `server`
	/// tracks the real clock when one exists; None offline.
	Waiting { tod: f32, server: Option<f32> },
	/// Light owned, online: closing on the tracked server clock.
	Chasing { tod: f32, server: f32 },
}

/// Hours `server` sits ahead of `tod`, measured forward (tod falls as
/// time advances).
fn hours_ahead(tod: f32, server: f32) -> f32 {
	(tod - server).rem_euclid(360.0) / DEG_PER_HOUR
}

impl DuskHold {
	/// Enter the held evening; `server` is the live world clock when a
	/// server owns one. A server sitting in its daytime — past the night
	/// window — is shadowed instead: no personal clock until its dusk.
	pub(crate) fn start(&mut self, server: Option<f32>) {
		if let Some(s) = server
			&& hours_ahead(tod_of_hours(HOLD_HOURS), s) > CHASE_MAX_HOURS
		{
			*self = DuskHold::Shadowing { server: s };
			return;
		}
		*self = DuskHold::Waiting {
			tod: tod_of_hours(ENTRY_HOURS),
			server,
		};
	}

	/// Still short of a light? (The frame's poll for one runs only then.)
	pub(crate) fn waiting_for_light(&self) -> bool {
		matches!(self, DuskHold::Waiting { .. } | DuskHold::Shadowing { .. })
	}

	/// A heartbeat's authoritative clock. True = consumed (the hold is
	/// tracking the server itself); false = not ours, paint the sun.
	pub(crate) fn on_server_tod(&mut self, tod: f32) -> bool {
		match self {
			DuskHold::Shadowing { server: s }
			| DuskHold::Waiting {
				server: Some(s), ..
			}
			| DuskHold::Chasing { server: s, .. } => {
				*s = tod;
				true
			}
			_ => false,
		}
	}

	/// The player owns a light: offline the clock is simply released;
	/// online it starts closing on the server's. A shadow was never
	/// anything but the server's clock, so it just hands over.
	pub(crate) fn light_obtained(&mut self) {
		match self {
			DuskHold::Shadowing { .. } => *self = DuskHold::Off,
			DuskHold::Waiting { tod, server } => {
				*self = match server {
					None => DuskHold::Off,
					Some(s) => DuskHold::Chasing {
						tod: *tod,
						server: *s,
					},
				};
			}
			_ => {}
		}
	}

	/// One frame. Some(tod) = the sun to show; None = inactive, the
	/// world clock is in charge (the last Some was the handover value).
	pub(crate) fn tick(&mut self, dt: f32) -> Option<f32> {
		let fall = TOD_RATE_DEG_PER_SEC * dt;
		match self {
			DuskHold::Off => None,
			DuskHold::Shadowing { server } => {
				*server = Sun::normalize(*server - fall);
				let pin = tod_of_hours(HOLD_HOURS);
				// The window test rather than a crossing test, so a
				// heartbeat that leapt the evening (a backgrounded tab)
				// still lands on the stand instead of shadowing all night.
				if hours_ahead(pin, *server) <= CHASE_MAX_HOURS {
					let s = *server;
					*self = DuskHold::Waiting {
						tod: pin,
						server: Some(s),
					};
					return Some(pin);
				}
				Some(*server)
			}
			DuskHold::Waiting { tod, server } => {
				if let Some(s) = server {
					*s = Sun::normalize(*s - fall);
				}
				// World rate, landing exactly on the stand-still: a frame
				// that would reach it snaps, so float drift can never slip
				// past the pin and read as a lap still to run.
				let pin = tod_of_hours(HOLD_HOURS);
				let left = (*tod - pin).rem_euclid(360.0);
				*tod = if left <= fall {
					pin
				} else {
					Sun::normalize(*tod - fall)
				};
				Some(*tod)
			}
			DuskHold::Chasing { tod, server } => {
				*server = Sun::normalize(*server - fall);
				// A server past the window is somewhere in its day, and
				// standing still until it comes round beats racing the
				// long way through a whole night and morning.
				if hours_ahead(*tod, *server) > CHASE_MAX_HOURS {
					return Some(*tod);
				}
				let ahead = (*tod - *server).rem_euclid(360.0);
				let step = fall * CHASE_RATE;
				if step >= ahead {
					let caught = *server;
					*self = DuskHold::Off;
					return Some(caught);
				}
				*tod = Sun::normalize(*tod - step);
				Some(*tod)
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::sun::{DAY_SECS, tod_of_clock};

	/// Seconds of world time per clock hour.
	const HOUR_SECS: f32 = DAY_SECS / 24.0;

	/// Drive the hold in frame-sized steps for `secs`, returning the
	/// last painted tod (which must exist throughout).
	fn run(hold: &mut DuskHold, secs: f32) -> f32 {
		let steps = (secs / 0.05).round() as u32;
		let mut last = f32::NAN;
		for _ in 0..steps {
			last = hold.tick(0.05).expect("hold active");
		}
		last
	}

	#[test]
	fn the_evening_runs_to_1700_and_stands() {
		let mut hold = DuskHold::default();
		hold.start(None);
		let mid = run(&mut hold, HOUR_SECS);
		assert!(
			(mid - tod_of_clock(16, 0)).abs() < 0.1,
			"one hour in: {mid}"
		);
		let pinned = run(&mut hold, 3.0 * HOUR_SECS);
		assert_eq!(pinned, tod_of_clock(17, 0), "standing at the hold");
	}

	#[test]
	fn a_light_releases_the_offline_clock() {
		let mut hold = DuskHold::default();
		hold.start(None);
		run(&mut hold, 3.0 * HOUR_SECS);
		hold.light_obtained();
		assert_eq!(hold, DuskHold::Off);
		assert_eq!(hold.tick(0.05), None);
	}

	#[test]
	fn a_night_server_is_chased_at_4x_until_caught() {
		let mut hold = DuskHold::default();
		// Server two hours ahead at entry; by the stand-still it reads
		// 19:00, four hours short of the window's edge.
		hold.start(Some(tod_of_clock(17, 0)));
		run(&mut hold, 2.0 * HOUR_SECS);
		hold.light_obtained();
		// The gap is 2h and closes at 3x world rate; watch the chase run
		// 4x and land exactly on the tracked clock, then hand over.
		let before = hold.tick(0.05).unwrap();
		let after = hold.tick(0.05).unwrap();
		let rate = (before - after) / 0.05;
		assert!((rate - 4.0 * TOD_RATE_DEG_PER_SEC).abs() < 0.01, "{rate}");
		let mut last = after;
		let mut caught_after = 0.1f32;
		while let Some(t) = hold.tick(0.05) {
			last = t;
			caught_after += 0.05;
			assert!(caught_after < 2.0 * HOUR_SECS, "chase never caught up");
		}
		// Gap/3 seconds to close, and the handover value is the server's
		// clock at that moment: 19:00 plus the chase's own duration.
		let expect_secs = 2.0 * HOUR_SECS / 3.0;
		assert!((caught_after - expect_secs).abs() < 1.0, "{caught_after}");
		let server_now = tod_of_clock(19, 0) - TOD_RATE_DEG_PER_SEC * caught_after;
		assert!((last - server_now).abs() < 0.1, "{last} vs {server_now}");
	}

	#[test]
	fn a_day_server_is_waited_out_at_the_stand_still() {
		let mut hold = DuskHold::default();
		// A night join (04:00, inside the window), standing at 17:00 by
		// two hours in; the server then runs on into its day.
		hold.start(Some(tod_of_clock(4, 0)));
		assert!(hold.waiting_for_light());
		run(&mut hold, 6.0 * HOUR_SECS);
		hold.light_obtained();
		// 10:00 on the server: 17 hours ahead, outside the window. The
		// clock stands until the server itself reaches 17:00 (7 hours),
		// then hands over in sync.
		let held = run(&mut hold, 6.9 * HOUR_SECS);
		assert_eq!(held, tod_of_clock(17, 0), "still standing");
		for _ in 0..((0.2 * HOUR_SECS / 0.05) as u32) {
			if hold.tick(0.05).is_none() {
				return;
			}
		}
		panic!("never handed over after the server came round");
	}

	#[test]
	fn a_day_join_shadows_the_server_clock_until_its_dusk() {
		let mut hold = DuskHold::default();
		hold.start(Some(tod_of_clock(12, 0)));
		assert!(hold.waiting_for_light());
		// The shown clock is the server's, running at world rate.
		let noonish = run(&mut hold, HOUR_SECS);
		assert!(
			(noonish - tod_of_clock(13, 0)).abs() < 0.1,
			"an hour after noon: {noonish}"
		);
		// A heartbeat re-anchors the shadow directly.
		assert!(hold.on_server_tod(tod_of_clock(16, 0)));
		// One server hour later its dusk arrives, and the stand begins:
		// the clock pins while the server runs on into the night.
		let pinned = run(&mut hold, 3.0 * HOUR_SECS);
		assert_eq!(pinned, tod_of_clock(17, 0), "standing at the hold");
		assert!(hold.waiting_for_light());
	}

	#[test]
	fn a_light_while_shadowing_just_hands_back() {
		let mut hold = DuskHold::default();
		hold.start(Some(tod_of_clock(12, 0)));
		run(&mut hold, HOUR_SECS);
		hold.light_obtained();
		assert_eq!(hold, DuskHold::Off);
		assert_eq!(hold.tick(0.05), None);
	}

	#[test]
	fn heartbeats_reanchor_only_an_active_hold() {
		let mut hold = DuskHold::default();
		assert!(!hold.on_server_tod(0.0));
		hold.start(Some(tod_of_clock(3, 0)));
		assert!(hold.on_server_tod(tod_of_clock(4, 0)));
		hold.light_obtained();
		assert!(hold.on_server_tod(tod_of_clock(5, 0)));
	}
}
