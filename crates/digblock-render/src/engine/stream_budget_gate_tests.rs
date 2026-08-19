
use super::*;

/// The trap this gate's split exists to close. The adaptive budget is
/// ONE deadline over jobs, drain, eviction and uploads, and only the
/// jobs can move to a worker pool — so gating it on the executor sent
/// the drain and the uploads back to the fixed floor the moment web
/// workers were switched on. Jobs faster, absorption slower, on
/// exactly the machines that had headroom to spend.
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
