use super::*;

fn declaration(name: &str) -> &str {
	digblock_assets::shader::PLAYERS
		.split(&format!("const {name} = "))
		.nth(1)
		.expect("held geometry declares its transform")
		.split(';')
		.next()
		.unwrap()
		.trim()
}

#[test]
fn remote_flame_stays_on_the_rendered_torch_tip() {
	let hand: Vec<f32> = declaration("HAND")
		.trim_start_matches("vec3f(")
		.trim_end_matches(')')
		.split(',')
		.map(|v| v.trim().parse().unwrap())
		.collect();
	let scale: Vec<f32> = declaration("HELD_SCALE")
		.split('/')
		.map(|v| v.trim().parse().unwrap())
		.collect();
	let scale = scale[0] / scale[1];
	let item_yaw: f32 = declaration("ITEM_YAW").parse().unwrap();
	let height = crate::item_geom::height(crate::ui::hud::IconKind::Shaped(
		digblock_core::block::BlockType::Torch,
	)) as f32;
	for yaw in [-3.0, -1.0, 0.0, 1.5, 3.0] {
		let i = [13.0, 42.0, -7.0, yaw, 1.0, height, 0.0, 0.0];
		let local = (tip() * 12.0 - Vec3::new(6.0, height * 0.5, 6.0)) * scale;
		let rendered = Mat4::from_rotation_y(yaw).transform_point3(
			Mat4::from_rotation_y(item_yaw).transform_point3(local)
				+ Vec3::new(hand[0], hand[1], hand[2]),
		) + Vec3::new(i[0], i[1], i[2]);
		assert!(
			remote_transform(&i)
				.transform_point3(Vec3::ZERO)
				.distance(rendered)
				< 0.0001,
			"the flame detached from the held torch at yaw {yaw}"
		);
	}
}

#[test]
fn the_draw_covers_every_shader_flame_quad() {
	assert!(
		digblock_assets::shader::FLAME.contains(&format!("const QUADS: u32 = {QUADS}u;")),
		"flame geometry and the CPU draw range disagree, dropping tongues or embers"
	);
}
