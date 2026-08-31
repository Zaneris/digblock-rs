use super::*;

fn open(_o: Vec3, _d: Vec3, max: f32, _r: f32) -> f32 {
	max
}

/// look_at round-trips through `dir_from` on the cardinals exactly and
/// in general within float noise.
#[test]
fn look_at_matches_dir_from() {
	let pos = Vec3::new(10.0, 20.0, 30.0);
	for target in [
		pos + Vec3::Z,
		pos + Vec3::X,
		pos - Vec3::Z,
		pos + Vec3::new(3.0, 1.5, -2.0),
	] {
		let p = CamPose::look_at(pos, target);
		let d = crate::camera::dir_from(p.yaw_deg, p.pitch_deg);
		let want = (target - pos).normalize();
		assert!(d.distance(want) < 1e-4, "{d} vs {want}");
	}
}

/// The boom's invariant: after every step, `len <= target` — a clip is
/// worse than a cut. Extension is gradual; the hysteresis swallows a
/// corner-graze flutter entirely.
#[test]
fn the_boom_never_exceeds_clearance_and_extends_smoothly() {
	let mut boom = Boom { len: 8.0 };
	// A sudden obstruction pulls in on the SAME step.
	assert_eq!(boom.step(3.0, 0.016), 3.0);
	// Clearance opens back up: the boom eases out over many frames.
	let first = boom.step(8.0, 0.016);
	assert!(first > 3.0 && first < 3.4, "{first}");
	let mut len = first;
	for _ in 0..600 {
		let next = boom.step(8.0, 0.016);
		assert!(next >= len, "extension went backwards");
		assert!(next <= 8.0, "boom past clearance");
		len = next;
	}
	assert!(len > 7.5, "never re-extended: {len}");
	// A flutter smaller than the hysteresis moves nothing.
	let settled = boom.step(8.0, 0.016);
	assert_eq!(boom.step(settled + 0.03, 0.016), settled);
}

/// Orbit determinism at fixed dt, radius/height honored in open air, and
/// the pose always looks at the center.
#[test]
fn an_open_air_orbit_is_deterministic_and_centered() {
	let center = Vec3::new(100.0, 40.0, 100.0);
	let run = || {
		let mut rig = OrbitRig::new(center, 6.0, 2.0, 30.0);
		let mut poses = Vec::new();
		for _ in 0..120 {
			poses.push(rig.step(1.0 / 60.0, &open));
		}
		poses
	};
	let (a, b) = (run(), run());
	assert_eq!(a, b, "two identical runs must be bit-identical");
	let last = *a.last().unwrap();
	// Fully extended: at the sphere's radius/height from the center.
	let d = last.pos - center;
	let flat = Vec3::new(d.x, 0.0, d.z).length();
	assert!((flat - 6.0).abs() < 0.05, "{flat}");
	assert!((d.y - 2.0).abs() < 0.05, "{}", d.y);
	// And looking at the center.
	let dir = crate::camera::dir_from(last.yaw_deg, last.pitch_deg);
	let want = (center - last.pos).normalize();
	assert!(dir.distance(want) < 1e-3);
}

/// The exit contract: after `leave` finishes, `step` returns None and
/// the last blended poses have converged onto the live first-person pose
/// — the caller then switches branches, restoring exact rest.
#[test]
fn leaving_converges_on_the_live_pose_then_yields_none() {
	let fp = CamPose {
		pos: Vec3::new(50.0, 30.0, 50.0),
		yaw_deg: 40.0,
		pitch_deg: -10.0,
	};
	let mut cine = Cinematic::start(fp, OrbitRig::new(fp.pos + Vec3::Z * 4.0, 5.0, 1.5, 20.0));
	for _ in 0..200 {
		assert!(cine.step(1.0 / 60.0, fp, &open).is_some());
	}
	cine.leave();
	cine.leave(); // idempotent
	let mut last = fp;
	let mut steps = 0;
	while let Some(p) = cine.step(1.0 / 60.0, fp, &open) {
		last = p;
		steps += 1;
		assert!(steps < 600, "the exit blend never finished");
	}
	assert!(steps > 10, "the exit was a cut, not a blend");
	assert!(last.pos.distance(fp.pos) < 0.5, "{:?}", last.pos);
}
