use super::*;
use crate::block::BlockType as B;
use crate::chunk::make_cell;
use crate::shapes::{Facing, door_state};

fn some_cell(b: B) -> Option<u16> {
	Some(b as u16)
}

/// Open air everywhere: the boom runs its whole length.
#[test]
fn open_air_clears_to_max() {
	let air = |_: i32, _: i32, _: i32| some_cell(B::Air);
	let c = clearance(Vec3::new(0.5, 0.5, 0.5), Vec3::X, 12.0, CAM_RADIUS, &air);
	assert_eq!(c, 12.0);
}

/// A flat wall: the boom stops one radius short of the face, within the
/// bisection's epsilon — and the answer is deterministic.
#[test]
fn a_wall_stops_the_boom_a_radius_short() {
	let wall = |x: i32, _: i32, _: i32| some_cell(if x == 6 { B::Stone } else { B::Air });
	let origin = Vec3::new(0.5, 0.5, 0.5);
	let c = clearance(origin, Vec3::X, 20.0, CAM_RADIUS, &wall);
	let want = (6.0 - origin.x) - CAM_RADIUS;
	let eps = CAM_RADIUS / 2f32.powi(7);
	assert!((c - want).abs() <= eps, "{c} vs {want}");
	assert_eq!(
		c,
		clearance(origin, Vec3::X, 20.0, CAM_RADIUS, &wall),
		"two identical calls must answer bit-identically"
	);
}

/// An origin already inside rock answers zero outright.
#[test]
fn a_buried_origin_has_no_clearance() {
	let rock = |_: i32, _: i32, _: i32| some_cell(B::Stone);
	assert_eq!(
		clearance(Vec3::new(0.5, 0.5, 0.5), Vec3::X, 8.0, CAM_RADIUS, &rock),
		0.0
	);
	assert!(sphere_overlaps_solid(
		Vec3::new(0.5, 0.5, 0.5),
		CAM_RADIUS,
		&rock
	));
}

/// The near-clip pin: a one-cell slit that a center RAY threads, but the
/// sphere refuses — this is exactly the case where a ray-based boom puts
/// terrain through the near plane.
#[test]
fn a_slit_a_ray_threads_stops_the_sphere() {
	// Walls at y=0 and y=2 from x=4 on; the y=1 row is open. A ray down
	// the y=1.5 line never hits; a 0.25 sphere at y=1.98.. does. Aim
	// slightly above the slit's middle so the sphere grazes the ceiling.
	let slit = |x: i32, y: i32, _: i32| some_cell(if x >= 4 && y != 1 { B::Stone } else { B::Air });
	let origin = Vec3::new(0.5, 1.9, 0.5);
	let ray_hit = crate::raycast::raycast(origin, Vec3::X, 20.0, &|x, y, z| {
		slit(x, y, z).filter(|_| slit(x, y, z) != some_cell(B::Air))
	});
	// The plain ray at this height passes into the slit past x=4.
	assert!(
		ray_hit.is_none() || ray_hit.unwrap().cell.0 > 4,
		"the slit must be threadable by a ray for this pin to mean anything"
	);
	let c = clearance(origin, Vec3::X, 20.0, CAM_RADIUS, &slit);
	assert!(c < 4.0, "the sphere entered the slit: {c}");
}

/// An open door lets the camera through; a closed one blocks it — the
/// collision-box distinction, mirroring the body's rule.
#[test]
fn an_open_door_lets_the_camera_through() {
	let door = |open: bool| {
		move |x: i32, y: i32, z: i32| {
			if x == 6 && (y == 0 || y == 1) && z == 0 {
				Some(make_cell(
					B::LumberDoor,
					door_state(Facing::PosX, open, y == 1),
				))
			} else if x == 9 {
				some_cell(B::Dirt)
			} else {
				some_cell(B::Air)
			}
		}
	};
	let origin = Vec3::new(4.5, 0.9, 0.5);
	let closed = clearance(origin, Vec3::X, 20.0, CAM_RADIUS, &door(false));
	assert!(closed < 2.0, "the closed leaf must stop the boom: {closed}");
	let open = clearance(origin, Vec3::X, 20.0, CAM_RADIUS, &door(true));
	assert!(
		open > 3.0,
		"the doorway is clear when the leaf is swung: {open}"
	);
}

/// Unloaded cells pass, exactly as physics treats them.
#[test]
fn unloaded_cells_do_not_block() {
	let unloaded = |_: i32, _: i32, _: i32| None;
	assert_eq!(
		clearance(
			Vec3::new(0.5, 0.5, 0.5),
			Vec3::X,
			8.0,
			CAM_RADIUS,
			&unloaded
		),
		8.0
	);
}

/// The embedded-origin probe: starting INSIDE a block (the orbit that
/// pivots on the heartwood), the march walks out of the leading solid
/// and then measures to the next wall; a clear origin answers exactly
/// what the plain probe answers.
#[test]
fn an_embedded_origin_escapes_its_own_block_then_measures() {
	// Solid at x=0 (the subject); a second wall at x=6; air between.
	let world =
		|x: i32, _: i32, _: i32| some_cell(if x == 0 || x == 6 { B::Stone } else { B::Air });
	let origin = Vec3::new(0.5, 0.5, 0.5);
	let c = clearance_out_of(origin, Vec3::X, 20.0, CAM_RADIUS, &world);
	// Past its own block, stopped a radius short of the far wall.
	let want = (6.0 - origin.x) - CAM_RADIUS;
	assert!(c > 1.0, "never escaped: {c}");
	assert!((c - want).abs() <= 0.3, "{c} vs {want}");
	// From clear air the two probes agree bitwise.
	let open = Vec3::new(2.5, 0.5, 0.5);
	assert_eq!(
		clearance_out_of(open, Vec3::X, 20.0, CAM_RADIUS, &world),
		clearance(open, Vec3::X, 20.0, CAM_RADIUS, &world),
	);
	// A ray that never leaves rock has nowhere to stand.
	let rock = |_: i32, _: i32, _: i32| some_cell(B::Stone);
	assert_eq!(
		clearance_out_of(origin, Vec3::X, 8.0, CAM_RADIUS, &rock),
		0.0
	);
}
