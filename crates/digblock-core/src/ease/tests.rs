use super::*;

/// The load-bearing promise: every curve's endpoints are EXACT, because
/// exact-rest consumers compare with `==`.
#[test]
fn every_curve_lands_bitwise_on_its_endpoints() {
	for c in Curve::ALL {
		assert_eq!(c.apply(0.0), 0.0, "{c:?}(0)");
		assert_eq!(c.apply(1.0), 1.0, "{c:?}(1)");
		// And clamps: outside input is the endpoint, not an excursion.
		assert_eq!(c.apply(-0.5), 0.0, "{c:?}(-0.5)");
		assert_eq!(c.apply(1.5), 1.0, "{c:?}(1.5)");
	}
}

/// The plain shapes never go backwards; the overshoot pair is exempt by
/// design (out_back dips past 1.0 on purpose).
#[test]
fn plain_curves_are_monotonic() {
	for c in [
		Curve::Linear,
		Curve::Smooth,
		Curve::InQuad,
		Curve::OutQuad,
		Curve::InOutQuad,
		Curve::InCubic,
		Curve::OutCubic,
		Curve::InOutCubic,
	] {
		let mut prev = 0.0f32;
		for i in 1..=100 {
			let v = c.apply(i as f32 / 100.0);
			assert!(v >= prev, "{c:?} steps back at {i}: {v} < {prev}");
			prev = v;
		}
	}
}

/// The pop curves actually pop: somewhere inside the run they exceed 1.
#[test]
fn overshoot_curves_overshoot_then_settle() {
	for c in [Curve::OutBack, Curve::OutElastic] {
		let peak = (1..100)
			.map(|i| c.apply(i as f32 / 100.0))
			.fold(0.0f32, f32::max);
		assert!(peak > 1.0, "{c:?} never exceeds 1 (peak {peak})");
	}
}

/// Damping covers the same ground whether a span arrives as one dt or
/// two halves — the whole point of the exp form over `k * dt` steps.
#[test]
fn exp_decay_is_frame_rate_independent_and_converges() {
	let one = exp_decay(0.0, 10.0, 3.0, 0.4);
	let mut half = exp_decay(0.0, 10.0, 3.0, 0.2);
	half = exp_decay(half, 10.0, 3.0, 0.2);
	assert!((one - half).abs() < 1e-4, "{one} vs {half}");
	// And it converges: many steps land within any epsilon of target.
	let mut v = 0.0;
	for _ in 0..400 {
		v = exp_decay(v, 10.0, 3.0, 0.05);
	}
	assert!((v - 10.0).abs() < 1e-3, "{v}");
	// The Vec3 form is the same math per lane.
	let v3 = exp_decay3(Vec3::ZERO, Vec3::splat(10.0), 3.0, 0.4);
	assert!((v3.x - one).abs() < 1e-6);
}

#[test]
fn remap_maps_the_ends_and_the_middle() {
	assert_eq!(remap(5.0, 0.0, 10.0, 0.0, 1.0), 0.5);
	assert_eq!(remap(0.0, 0.0, 10.0, 3.0, 7.0), 3.0);
	assert_eq!(remap(10.0, 0.0, 10.0, 3.0, 7.0), 7.0);
}
