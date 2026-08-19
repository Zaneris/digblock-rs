use super::*;

/// The whole reason the edit drain is priced in time rather than
/// counted: the same queue must take proportionally less of a shorter
/// frame. The COUNT it replaced was sized against a native frame and
/// spent that same absolute time on a web frame a fraction as long,
/// which is far more of the interval than it was ever allowed.
#[test]
fn net_edit_budget_scales_with_the_display_interval() {
	let at60 = net_edit_budget_ms(1000.0 / 60.0);
	let at120 = net_edit_budget_ms(1000.0 / 120.0);
	assert!(
		(at120 * 2.0 - at60).abs() < 1e-4,
		"120 Hz must get exactly half: {at120} vs {at60}"
	);
	// `refresh_ms` reads zero until the estimate settles; falling back
	// to anything but the nominal interval would move every baseline.
	assert_eq!(net_edit_budget_ms(0.0), at60);
	assert_eq!(net_edit_budget_ms(-1.0), at60);
}

/// The budget is a slice of the frame, not the frame. Nothing here
/// pins the exact share, but a drain allowed a large fraction of the
/// interval is no longer a budget — the render still has to fit.
#[test]
fn net_edit_budget_leaves_the_frame_its_interval() {
	for hz in [30.0, 60.0, 120.0, 144.0] {
		let interval = 1000.0 / hz;
		let budget = net_edit_budget_ms(interval);
		assert!(
			budget > 0.0 && budget < interval * 0.25,
			"{hz} Hz: {budget} of {interval}"
		);
	}
}
