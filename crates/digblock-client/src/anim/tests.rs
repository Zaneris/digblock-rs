use super::*;

/// The one-shot contract: the crossing frame fires once, and only once —
/// ticking past zero twice must not fire twice.
#[test]
fn a_timer_fires_exactly_once() {
	let mut t = Timer::default();
	assert!(!t.tick(1.0), "an idle timer fired");
	t.start(0.5);
	assert!(t.running());
	assert!(!t.tick(0.3));
	assert!(t.tick(0.3), "the crossing frame did not fire");
	assert!(!t.running());
	assert!(!t.tick(0.3), "a spent timer fired again");
	// A stopped timer never fires at all.
	t.start(0.5);
	t.stop();
	assert!(!t.tick(10.0));
}

#[test]
fn a_cooldown_gates_until_its_window_runs_out() {
	let mut c = Cooldown::default();
	assert!(c.ready(), "a fresh cooldown must not gate");
	c.start(2.0);
	assert!(!c.ready());
	c.tick(1.5);
	assert!(!c.ready());
	assert_eq!(c.left(), 0.5);
	c.tick(1.0);
	assert!(c.ready());
	// Clamped at zero: `left` is a display value, never negative.
	assert_eq!(c.left(), 0.0);
}

/// The rails are exact — an open aura is 1.0 bitwise, a closed one 0.0 —
/// and the two speeds are independent.
#[test]
fn an_envelope_rides_its_rails_exactly() {
	let mut e = Envelope::default();
	assert_eq!(e.level(), 0.0);
	// Rise over ~10 frames of a 0.1s attack; land exactly on 1.0.
	for _ in 0..20 {
		e.tick(0.016, true, 0.1, 0.5);
	}
	assert_eq!(e.level(), 1.0);
	// Release is slower and monotonic, and lands exactly on 0.0.
	let mut prev = e.level();
	let mut steps = 0;
	while e.level() > 0.0 {
		e.tick(0.016, false, 0.1, 0.5);
		assert!(e.level() <= prev, "the fade went back up");
		prev = e.level();
		steps += 1;
		assert!(steps < 100, "the fade never reached zero");
	}
	assert!(steps > 5, "the release ignored its slower speed");
	assert_eq!(e.level(), 0.0);
	e.tick(0.016, true, 0.1, 0.5);
	assert!(e.level() > 0.0);
	e.reset();
	assert_eq!(e.level(), 0.0);
}
