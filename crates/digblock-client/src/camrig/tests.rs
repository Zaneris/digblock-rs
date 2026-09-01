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

/// A tracking dolly slides from -> to over its seconds with the lens
/// chasing the target every frame, and holds the end pose once arrived.
/// With an obstruction the boom keeps it in front of the wall on the
/// very step the wall appears.
#[test]
fn a_dolly_tracks_its_line_and_never_clips() {
	let target = Vec3::new(0.0, 10.0, 0.0);
	let (from, to) = (Vec3::new(-5.0, 18.0, -3.0), Vec3::new(5.0, 18.0, -3.0));
	let mut rig = DollyRig::new(from, to, target, 2.0);
	let first = rig.step(1.0 / 60.0, &open);
	assert!(first.pos.distance(from) < 0.5, "starts at from: {first:?}");
	for _ in 0..240 {
		let p = rig.step(1.0 / 60.0, &open);
		let dir = crate::camera::dir_from(p.yaw_deg, p.pitch_deg);
		let want = (target - p.pos).normalize();
		assert!(dir.distance(want) < 1e-3, "the lens chases the target");
	}
	let end = rig.step(1.0 / 60.0, &open);
	// Within the boom's hysteresis of the line's end, and holding there.
	assert!(end.pos.distance(to) < 0.1, "holds the end: {end:?}");
	// A wall 4 blocks out from the target pulls the eye inside it.
	let walled = |_o: Vec3, _d: Vec3, max: f32, _r: f32| max.min(4.0);
	let pulled = rig.step(1.0 / 60.0, &walled);
	assert!(pulled.pos.distance(target) <= 4.0 + 1e-3, "{pulled:?}");
}

/// A drone glide never turns its gimbal, whatever the path does — the
/// property that lets it pass directly over its subject where the
/// dolly's chasing lens would spin the frame.
#[test]
fn a_drone_glide_holds_its_gimbal() {
	let target = Vec3::new(0.0, 10.0, 0.0);
	let (from, to) = (Vec3::new(-5.0, 32.0, 0.0), Vec3::new(5.0, 32.0, 0.0));
	let mut rig = DroneRig::new(from, to, target, 2.0, 90.0, -89.0);
	let first = rig.step(1.0 / 60.0, &open);
	assert!(first.pos.distance(from) < 0.5, "starts at from: {first:?}");
	for _ in 0..240 {
		let p = rig.step(1.0 / 60.0, &open);
		assert_eq!(p.yaw_deg, 90.0, "the gimbal never turns");
		assert_eq!(p.pitch_deg, -89.0);
	}
	let end = rig.step(1.0 / 60.0, &open);
	assert!(end.pos.distance(to) < 0.1, "holds the end: {end:?}");
}

/// The split pivot: an orbit whose center is the subject block itself
/// (boom pre-extended) keeps the lens on the target every frame, and
/// the eye circles the CENTER, not some offset stand-in.
#[test]
fn an_extended_orbit_pivots_on_its_center_and_holds_its_target() {
	let center = Vec3::new(10.0, 20.0, 10.0);
	let mut rig = OrbitRig::new(center, 6.0, 2.0, 30.0).extended();
	for _ in 0..120 {
		let p = rig.step(1.0 / 60.0, &open);
		let d = p.pos - center;
		let flat = Vec3::new(d.x, 0.0, d.z).length();
		assert!((flat - 6.0).abs() < 0.05, "radius held: {flat}");
		let dir = crate::camera::dir_from(p.yaw_deg, p.pitch_deg);
		let want = (rig.target - p.pos).normalize();
		assert!(dir.distance(want) < 1e-3, "lens on the target");
	}
}

/// A shot sequence cuts at each hold and leaves by itself after the
/// last: the pose jumps to the second rig's line at the cut, and the
/// exit converges home with no one calling `leave`.
#[test]
fn shots_cut_then_the_sequence_leaves_itself() {
	let fp = CamPose {
		pos: Vec3::new(50.0, 30.0, 50.0),
		yaw_deg: 0.0,
		pitch_deg: 0.0,
	};
	let orbit = OrbitRig::new(fp.pos + Vec3::Z * 6.0, 5.0, 1.5, 10.0);
	let spring = Vec3::new(200.0, 10.0, 200.0);
	let dolly = DollyRig::new(
		spring + Vec3::new(-4.0, 8.0, -2.0),
		spring + Vec3::new(4.0, 8.0, -2.0),
		spring,
		2.0,
	);
	let mut cine = Cinematic::start_shots(
		fp,
		vec![
			Shot {
				rig: CineRig::Orbit(orbit),
				hold: 2.0,
			},
			Shot {
				rig: CineRig::Dolly(dolly),
				hold: 2.0,
			},
		],
	);
	// Through the first shot: near the orbit's neighborhood.
	let mut last = fp;
	for _ in 0..115 {
		last = cine.step(1.0 / 60.0, fp, &open).expect("first shot");
	}
	assert!(last.pos.distance(fp.pos) < 20.0, "{last:?}");
	// A few frames later the cut has happened: the pose is over the
	// spring, a hard jump away.
	for _ in 0..10 {
		last = cine.step(1.0 / 60.0, fp, &open).expect("second shot");
	}
	assert!(
		last.pos.distance(spring) < 15.0,
		"cut never landed: {last:?}"
	);
	// And after the second hold the sequence leaves on its own.
	let mut steps = 0;
	while let Some(p) = cine.step(1.0 / 60.0, fp, &open) {
		last = p;
		steps += 1;
		assert!(steps < 600, "never left by itself");
	}
	assert!(last.pos.distance(fp.pos) < 0.5, "{last:?}");
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
