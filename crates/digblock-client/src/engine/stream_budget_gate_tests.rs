use super::*;

/// The trap this gate's split exists to close. The adaptive budget is ONE deadline over jobs,
/// drain, eviction and uploads, and only the jobs can move to a worker pool — so gating it on
/// the executor sent the drain and the uploads back to the fixed floor the moment web workers
/// were switched on. Jobs faster, absorption slower, on exactly the machines with headroom.
#[test]
fn a_worker_backed_web_client_keeps_its_adaptive_budget() {
	let mut web = EngineConfig::web();
	assert!(
		web.adaptive_effective(),
		"the inline web client is the baseline"
	);
	web.workers = 2;
	assert!(
		web.adaptive_effective(),
		"moving jobs to a pool must not drop drain and uploads to the floor"
	);
}

/// Captures stay hermetic: bench pins the fixed floor whatever else is
/// configured, so no baseline drifts with the display's headroom.
#[test]
fn bench_still_pins_the_fixed_budget() {
	let mut web = EngineConfig::web();
	web.bench = true;
	assert!(!web.adaptive_effective());
	web.workers = 2;
	assert!(!web.adaptive_effective());
}

/// Desktop never opted in, and a worker count must not opt it in.
#[test]
fn desktop_stays_on_the_fixed_budget() {
	assert!(!EngineConfig::desktop().adaptive_effective());
}

/// Where the release queue stops being a rim crescent and becomes an arrival.
///
/// Asserted as the two things the line has to separate rather than as its value: one boundary
/// crossing must stay under it — bursting on every crossing would put thousands of buffer
/// destroys into a walk — and a whole disc must land over it, or a teleport keeps draining at
/// `EVICT_MIN` and the two worlds stay live together. Both sides are derived from the same
/// geometry `visibility_diff` walks, so a retune of either moves with it.
#[test]
fn a_crossing_stays_under_the_arrival_line_and_a_disc_lands_over_it() {
	let stack = digblock_core::chunk::WORLD_VCHUNK as usize;
	let mut checked = 0u32;
	// The whole span the slider can reach, ends included.
	for r in [
		crate::settings::DRAW_FLOOR,
		8,
		12,
		16,
		20,
		26,
		crate::settings::DRAW_MAX,
	] {
		// A crossing shifts the disc by one column, retiring the far column of every row.
		let crossing = (2 * r as usize + 1) * stack;
		// An arrival retires the whole disc.
		let disc = (std::f64::consts::PI * f64::from(r) * f64::from(r)).ceil() as usize * stack;
		let line = rim_backlog(r);
		assert!(line > crossing, "draw {r}: {line} bursts on a crossing");
		assert!(line < disc, "draw {r}: {line} never bursts on an arrival");
		checked += 1;
	}
	assert_eq!(checked, 7, "the sweep covered what it meant to");
}
