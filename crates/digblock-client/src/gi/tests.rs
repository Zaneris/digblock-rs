use super::*;

/// `flush_lights` drops emitters outside the reach every march breaks
/// at, so its bound has to BE the shader's. Read `gi_lateral_max` out
/// of gi_dda.wgsl rather than restating it: a hand-copied twin is how
/// this codebase found the same stepper bug three times, and drifting
/// here silently hides a torch the shader would have traced.
#[test]
fn the_light_reach_matches_the_shaders_bound() {
	let src = digblock_assets::shader::GI_DDA;
	let body = src
		.split("fn gi_lateral_max(side: f32) -> f32 {")
		.nth(1)
		.expect("gi_dda.wgsl must declare gi_lateral_max(side)");
	let expr = body
		.split(';')
		.next()
		.expect("gi_lateral_max must have a return")
		.replace("return", "")
		.replace(char::is_whitespace, "");
	assert_eq!(
		expr, "side*0.5-1.0",
		"gi_lateral_max changed shape — update `lateral_max` to match"
	);
	for wc in [8u32, 20, 32] {
		let side = (wc * 16) as f32;
		assert_eq!(lateral_max(wc), side * 0.5 - 1.0, "window {wc}");
	}
}

/// The cover mask's bit order and the sub-cell march's step bound, read out of gi_dda.wgsl rather
/// than restated: the CPU packs bit `(y*12 + z)*12 + x` (`mask_bits_match_the_shader_decode`),
/// and only agreement makes the two halves one thing. The step bound must hold the most
/// sub-cells one ray can cross in a 12³ grid, or a grazing ray would silently pass a tread.
#[test]
fn the_mask_bit_order_matches_the_shader() {
	let src = digblock_assets::shader::GI_DDA;
	let body = src
		.split("fn gi_sub_bit(c: vec3i) -> u32 {")
		.nth(1)
		.expect("gi_dda.wgsl must declare gi_sub_bit(c)");
	let expr = body
		.split(';')
		.next()
		.expect("gi_sub_bit must have a return")
		.replace("return", "")
		.replace(char::is_whitespace, "");
	assert_eq!(expr, "u32((c.y*12+c.z)*12+c.x)", "gi_sub_bit changed shape");
	let steps: u32 = src
		.split("const GI_SUB_STEPS = ")
		.nth(1)
		.and_then(|s| s.split(';').next())
		.and_then(|s| s.trim().parse().ok())
		.expect("gi_dda.wgsl must declare GI_SUB_STEPS as a literal");
	assert!(
		steps >= 3 * 12 - 2,
		"{steps} sub-steps cannot cross a cell corner to corner"
	);
	let struct_words = |name: &str, src: &str| -> usize {
		src.split(&format!("struct {name} {{"))
			.nth(1)
			.and_then(|s| s.split("mask: array<u32, ").nth(1))
			.and_then(|s| s.split('>').next())
			.and_then(|s| s.trim().parse().ok())
			.unwrap_or_else(|| panic!("{name} must carry a mask of a literal word count"))
	};
	assert_eq!(
		struct_words("GiCover", digblock_assets::shader::GI_COMMON),
		digblock_core::gi::COVER_WORDS
	);
	assert_eq!(
		struct_words("Cover", digblock_assets::shader::GI_PROBE),
		digblock_core::gi::COVER_WORDS
	);
}

/// The shipped ray budgets must fit inside the marches' own hard loop
/// bounds, read out of gi_common.wgsl rather than restated: a budget past
/// the capacity would be silently truncated by the loop, so the shipped
/// numbers would lie about what the frame spends.
#[test]
fn budgets_fit_the_shaders_capacities() {
	let src = digblock_assets::shader::GI_COMMON;
	let capacity = |func: &str| -> u32 {
		src.split(&format!("fn {func}"))
			.nth(1)
			.unwrap_or_else(|| panic!("gi_common.wgsl must declare {func}"))
			.split("for (var i = 0; i <")
			.nth(1)
			.and_then(|s| s.split(';').next())
			.and_then(|s| s.trim().parse().ok())
			.unwrap_or_else(|| panic!("{func}'s loop bound must be a literal"))
	};
	assert!(
		GI_GATHER_ITERS <= capacity("gi_march"),
		"the gather budget exceeds gi_march's loop capacity"
	);
	assert!(
		GI_SUN_ITERS <= capacity("gi_sun_vis"),
		"the sun budget exceeds gi_sun_vis's loop capacity"
	);
}

/// The adaptive budget's floors sit between the history caps they gate: a pixel traces every
/// frame until its indirect history is past N_SUN (so the sun lane, which gates on N_SUN, is
/// never the more eager of the two) and the floor is under N_MOVING, or a moving pixel would
/// never thin out at all.
#[test]
fn the_rate_floors_nest_the_history_caps() {
	let src = digblock_assets::shader::GI_RESOLVE;
	let n_sun = wgsl_f32(src, "N_SUN");
	let floor = wgsl_f32(src, "RATE_N_FULL");
	let moving = wgsl_f32(src, "N_MOVING");
	assert!(
		n_sun <= floor && floor < moving,
		"N_SUN {n_sun} <= RATE_N_FULL {floor} < N_MOVING {moving}"
	);
	assert!(wgsl_f32(src, "IND_PERIOD_MAX") >= 1.0);
	assert!(wgsl_f32(src, "SUN_PERIOD_MAX") >= 1.0);
}

/// The edge gate reads the a-trous's penumbra floor: a tap at or above PEN_MIN is a texel the
/// filter is reconstructing a penumbra across, and the sun lane must trace it every frame.
#[test]
fn the_edge_gate_knows_the_filters_penumbra_floor() {
	assert_eq!(
		wgsl_f32(digblock_assets::shader::GI_RESOLVE, "SUN_EDGE_PEN"),
		wgsl_f32(digblock_assets::shader::GI_ATROUS, "PEN_MIN"),
	);
}

/// A far interior texel re-traces its sun every SUN_PERIOD_MAX frames at the shipped distance.
/// The sun moves meanwhile (`TOD_RATE_DEG_PER_SEC`), and even at a slow 20 fps its travel over
/// one period must stay inside the cone the ray is jittered in: past that, a texel could wake
/// up to find its lit/shadowed answer stale by more than the penumbra it sits in.
#[test]
fn the_far_sun_period_stays_inside_the_cone() {
	let src = digblock_assets::shader::GI_RESOLVE;
	let period = wgsl_f32(src, "SUN_PERIOD_MAX");
	let slow_fps = 20.0;
	let travel_deg = digblock_core::sun::TOD_RATE_DEG_PER_SEC * period / slow_fps;
	let cone_deg = SUN_CONE_COS.acos().to_degrees();
	assert!(
		travel_deg < cone_deg,
		"{travel_deg} deg of sun per period, cone {cone_deg}"
	);
}

/// The selection anchor leads the camera by the shader's `LIGHT_RANGE` —
/// the distance past which a light cannot affect a shaded point — read
/// out of BOTH declaring shaders rather than restated, exactly like the
/// reach pin above.
#[test]
fn the_selection_lead_matches_the_shaders_light_range() {
	let mut found = 0;
	for (name, src) in [
		("gi_common.wgsl", digblock_assets::shader::GI_COMMON),
		("gi_probe.wgsl", digblock_assets::shader::GI_PROBE),
	] {
		let decl = src
			.split("const LIGHT_RANGE")
			.nth(1)
			.unwrap_or_else(|| panic!("{name} must declare LIGHT_RANGE"));
		let value: f32 = decl
			.split('=')
			.nth(1)
			.and_then(|v| v.split(';').next())
			.and_then(|v| v.trim().parse().ok())
			.unwrap_or_else(|| panic!("{name}'s LIGHT_RANGE must be a literal"));
		assert_eq!(
			value, SELECT_AHEAD,
			"{name}'s LIGHT_RANGE and gi::SELECT_AHEAD must agree"
		);
		found += 1;
	}
	assert_eq!(found, 2);
}

/// The behind horizon is one gather march plus one light-reach: `GATHER_T`
/// mirrors `GI_GATHER_T` in gi_resolve.wgsl, read out of the shader rather
/// than restated, exactly like the reach pin above. A march that grows
/// without this moving would cull a light a ray can still observe.
#[test]
fn the_behind_horizon_matches_the_shaders_march() {
	let src = digblock_assets::shader::GI_RESOLVE;
	let decl = src
		.split("const GI_GATHER_T")
		.nth(1)
		.expect("gi_resolve.wgsl must declare GI_GATHER_T");
	let value: f32 = decl
		.split('=')
		.nth(1)
		.and_then(|v| v.split(';').next())
		.and_then(|v| v.trim().parse().ok())
		.expect("GI_GATHER_T must be a literal");
	assert_eq!(
		value, GATHER_T,
		"gi_resolve.wgsl's GI_GATHER_T and gi::GATHER_T must agree"
	);
	assert_eq!(BEHIND_REACH, GATHER_T + SELECT_AHEAD);
}

/// Helpers for the selection tests: a point emitter of the torch class,
/// and the position packed in a selected entry.
#[cfg(test)]
fn point_at(pos: [f32; 3]) -> Emitter {
	Emitter {
		pos,
		class: 1,
		level: 0,
		whitened: false,
		face: None,
	}
}

#[cfg(test)]
fn entry_pos(e: &[f32; 8]) -> [f32; 3] {
	[e[0], e[1], e[2]]
}

/// The cap bounds the selection: at most `cap` entries survive, every
/// candidate survives while it fits, and the count is exact under
/// oversubscription — swept over caps, with the sweep itself counted.
#[test]
fn the_cap_bounds_the_selection() {
	let cam = [0.0, 8.0, 0.0];
	let fwd = [0.0, 0.0, 1.0];
	let n = MAX_LIGHTS + 32;
	let field: Vec<Emitter> = (0..n)
		.map(|i| point_at([(i % 40) as f32 - 20.0, 8.0, (i / 40) as f32 + 2.0]))
		.collect();
	let mut swept = 0;
	for cap in [1, MAX_LIGHTS / 4, MAX_LIGHTS / 2, MAX_LIGHTS] {
		let all = select_lights(field.iter().copied(), &[], cam, fwd, 1000.0, None, cap);
		assert_eq!(all.len(), cap.min(n), "cap {cap}");
		swept += 1;
	}
	assert_eq!(swept, 4);
	// Under the cap nothing is dropped, whatever the cap.
	let few: Vec<Emitter> = field.iter().copied().take(MAX_LIGHTS / 8).collect();
	let all = select_lights(
		few.iter().copied(),
		&[],
		cam,
		fwd,
		1000.0,
		None,
		MAX_LIGHTS / 4,
	);
	assert_eq!(all.len(), few.len());
}

/// Under oversubscription the survivors lean FORWARD: of two clusters at
/// the same camera distance, the one ahead keeps its slots and the one
/// behind loses them. Under the cap the selection is independent of the
/// heading — the common case must pay nothing for the bias.
#[test]
fn an_oversubscribed_list_drops_the_lights_behind_first() {
	let cam = [0.0, 8.0, 0.0];
	let fwd = [0.0, 0.0, 1.0];
	let d = SELECT_AHEAD;
	// Mirror-image clusters, equidistant from the camera.
	let fore: Vec<Emitter> = (0..8).map(|i| point_at([i as f32 - 4.0, 8.0, d])).collect();
	let aft: Vec<Emitter> = (0..8)
		.map(|i| point_at([i as f32 - 4.0, 8.0, -d]))
		.collect();
	let both: Vec<Emitter> = fore.iter().chain(aft.iter()).copied().collect();
	let picked = select_lights(
		both.iter().copied(),
		&[],
		cam,
		fwd,
		1000.0,
		None,
		fore.len(),
	);
	assert_eq!(picked.len(), fore.len());
	for e in &picked {
		assert!(
			entry_pos(e)[2] > 0.0,
			"a light behind the camera survived while its mirror image ahead was dropped"
		);
	}
	// Same field, opposite heading, cap covering everything: the heading
	// must not change WHAT is selected, only (under pressure) what wins.
	let a = select_lights(
		both.iter().copied(),
		&[],
		cam,
		fwd,
		1000.0,
		None,
		MAX_LIGHTS,
	);
	let b = select_lights(
		both.iter().copied(),
		&[],
		cam,
		[0.0, 0.0, -1.0],
		1000.0,
		None,
		MAX_LIGHTS,
	);
	let key = |v: &[[f32; 8]]| {
		let mut k: Vec<[u32; 3]> = v.iter().map(|e| entry_pos(e).map(f32::to_bits)).collect();
		k.sort_unstable();
		k
	};
	assert_eq!(key(&a), key(&b));
}

/// The carrier's own light can never be the one dropped: the eye sits on
/// the ranking key's zero set, so half a block beats any emitter a full
/// block out — even against an oversubscribed field packed AHEAD, where
/// the forward lead is strongest.
#[test]
fn the_carriers_own_light_survives_any_cap() {
	let cam = [0.0, 8.0, 0.0];
	let fwd = [0.0, 0.0, 1.0];
	let own = point_at([0.0, 7.5, 0.3]);
	// Lateral offsets never reach zero, so no field emitter sits ON the
	// eye→anchor segment — the carrier's half-block is the closest thing
	// to it, which is the property under test.
	let field: Vec<Emitter> = (0..MAX_LIGHTS * 2)
		.map(|i| point_at([((i % 16) as f32 - 7.5) * 2.0, 8.0, (i / 16) as f32 + 1.0]))
		.collect();
	for cap in [1, MAX_LIGHTS / 4] {
		let picked = select_lights(
			field.iter().copied(),
			std::slice::from_ref(&own),
			cam,
			fwd,
			1000.0,
			None,
			cap,
		);
		assert!(
			picked.iter().any(|e| entry_pos(e) == own.pos),
			"cap {cap} dropped the carrier's own light"
		);
	}
}

/// The behind horizon is a CULL, not a bias: a light just inside it
/// survives an uncapped selection, its neighbour just past it is dropped
/// with the cap nowhere near — and the horizon follows the heading, so
/// turning around is what brings the light back.
#[test]
fn the_behind_horizon_is_a_cull_not_a_bias() {
	let cam = [0.0, 8.0, 0.0];
	let fwd = [0.0, 0.0, 1.0];
	let inside = point_at([0.0, 8.0, -(BEHIND_REACH - 1.0)]);
	let past = point_at([0.0, 8.0, -(BEHIND_REACH + 1.0)]);
	let both = [inside, past];
	let picked = select_lights(
		both.iter().copied(),
		&[],
		cam,
		fwd,
		1000.0,
		None,
		MAX_LIGHTS,
	);
	assert_eq!(picked.len(), 1, "{picked:?}");
	assert_eq!(entry_pos(&picked[0]), inside.pos);
	// The same field about-face: the culled light is now ahead and kept.
	let turned = select_lights(
		both.iter().copied(),
		&[],
		cam,
		[0.0, 0.0, -1.0],
		1000.0,
		None,
		MAX_LIGHTS,
	);
	assert_eq!(turned.len(), 2);
	// The horizon is the LOOK direction, pitch included, not the yaw: a
	// flyer looking down at the ground they just passed over has it on
	// screen, so a light past the horizon by yaw alone must survive.
	let high = [0.0, 8.0 + BEHIND_REACH, 0.0];
	let down_back = point_at([0.0, 8.0, -(BEHIND_REACH + 8.0)]);
	let pitched = {
		let d = [0.0f32, -(BEHIND_REACH + 8.0), -(BEHIND_REACH + 8.0)];
		let n = (d[1] * d[1] + d[2] * d[2]).sqrt();
		[0.0, d[1] / n, d[2] / n]
	};
	let picked = select_lights(
		[down_back].into_iter(),
		&[],
		high,
		pitched,
		1000.0,
		None,
		MAX_LIGHTS,
	);
	assert_eq!(picked.len(), 1, "pitch must count toward the horizon");
}

/// The sky cull is the FLOOR select_lights is handed: disarmed (None)
/// keeps the cave seams, armed drops what sits under the floor and keeps
/// a light AT it — the lit-bed case the cushion exists for. Every light
/// ahead of the camera, so only the floor decides.
#[test]
fn a_sky_camera_drops_the_cave_lights() {
	let fwd = [0.0, 0.0, 1.0];
	let cam = [0.0, 200.0, 0.0];
	let floor = 120.0f32;
	let cave = point_at([0.0, floor - 10.0, 8.0]);
	let bed = point_at([0.0, floor, 8.0]);
	let both = [cave, bed];
	let picked = select_lights(
		both.iter().copied(),
		&[],
		cam,
		fwd,
		1000.0,
		None,
		MAX_LIGHTS,
	);
	assert_eq!(picked.len(), 2, "disarmed, nothing is culled");
	let picked = select_lights(
		both.iter().copied(),
		&[],
		cam,
		fwd,
		1000.0,
		Some(floor),
		MAX_LIGHTS,
	);
	assert_eq!(picked.len(), 1, "{picked:?}");
	assert_eq!(entry_pos(&picked[0]), bed.pos);
}

/// The floor tally: it arms only once the camera rides `SKY_CULL_ABOVE`
/// over the reference surface, and the reference is a PERCENTILE — a
/// lone shaft in the sample cannot drag it down and disarm the cull,
/// while a quarry wide enough to matter carries the tally with it, which
/// is what keeps a dug-out crater's own lights alive under a high
/// camera. Expectations derived from the constants throughout.
#[test]
fn a_quarry_moves_the_floor_a_lone_shaft_cannot() {
	let flat = 130i32;
	let mut plain: Vec<i32> = vec![flat; 100];
	// Below the arming edge: no floor, whatever the terrain says.
	assert_eq!(
		sky_floor(flat as f32 + SKY_CULL_ABOVE - 1.0, &mut plain),
		None
	);
	let armed = flat as f32 + SKY_CULL_ABOVE;
	assert_eq!(
		sky_floor(armed, &mut plain),
		Some(flat as f32 - SKY_CULL_CUSHION)
	);
	// One shaft to bedrock in a hundred columns: under the percentile,
	// so the floor stands where the plain put it.
	let mut shaft = plain.clone();
	shaft[37] = 5;
	assert_eq!(
		sky_floor(armed, &mut shaft),
		Some(flat as f32 - SKY_CULL_CUSHION)
	);
	// A quarry spanning a third of the sample carries the tally: the
	// floor follows the dug surface, so torches on the quarry bed stay.
	let depth = 60i32;
	let mut quarry = plain.clone();
	for h in quarry.iter_mut().take(33) {
		*h = flat - depth;
	}
	assert_eq!(
		sky_floor(armed, &mut quarry),
		Some((flat - depth) as f32 - SKY_CULL_CUSHION)
	);
	// An empty tally never arms — no sampled column, no floor to stand on.
	assert_eq!(sky_floor(armed, &mut []), None);
}

/// An emptied tally DISARMS rather than guessing, which is the property
/// `Engine::reset_sky_tally` leans on at a teleport and a world swap: the
/// floor those hooks throw away describes terrain the camera has left,
/// and the safe state to be wrong in is the one that culls nothing.
#[test]
fn an_emptied_tally_disarms_the_cull_at_any_height() {
	let mut swept = 0;
	for cam_y in [0.0, 64.0, 200.0, 4000.0] {
		assert_eq!(sky_floor(cam_y, &mut []), None, "cam_y {cam_y}");
		swept += 1;
	}
	assert_eq!(swept, 4);
	// And a disarmed floor is what `select_lights` reads as "cull nothing":
	// the deepest emitter survives a camera arbitrarily far above it.
	let deep = point_at([0.0, -1000.0, 8.0]);
	let picked = select_lights(
		[deep].into_iter(),
		&[],
		[0.0, 4000.0, 0.0],
		[0.0, 0.0, 1.0],
		1e9,
		sky_floor(4000.0, &mut []),
		MAX_LIGHTS,
	);
	assert_eq!(picked.len(), 1, "a disarmed cull drops nothing");
}

/// The unproven-daylight fade (R32) is bit-identical above its top edge because `smoothstep`
/// saturates there — which is only the surface-world guarantee while that edge IS the surface's
/// floor. This crate is the one that sees both the shader source and worldgen, so it holds the two
/// together: the ramp tops out at `SEA_LEVEL`, and its bottom edge sits below its top (a degenerate
/// or inverted smoothstep would fade nothing, or everything).
#[test]
fn the_sky_fade_tops_out_at_sea_level() {
	let src = digblock_assets::shader::MEDIUM_COMMON;
	let wgsl_const = |name: &str| -> f64 {
		let pat = format!("const {name} = ");
		let at = src
			.find(&pat)
			.unwrap_or_else(|| panic!("{name}: not declared in medium_common.wgsl"))
			+ pat.len();
		let rest = &src[at..];
		let end = rest.find(';').expect("unterminated const");
		rest[..end].trim().parse().expect("not a number")
	};
	let full = wgsl_const("SKY_FADE_FULL_Y");
	let zero = wgsl_const("SKY_FADE_ZERO_Y");
	assert_eq!(
		full,
		f64::from(digblock_core::worldgen::SEA_LEVEL),
		"SKY_FADE_FULL_Y must equal SEA_LEVEL — above it the fade must saturate \
			 so the surface world cannot change"
	);
	assert!(
		zero < full,
		"SKY_FADE_ZERO_Y ({zero}) must sit below SKY_FADE_FULL_Y ({full})"
	);
}

/// A carried torch is an entry in the ordinary light list, so the
/// only thing that needs pinning is that the ordinary rules land it
/// in the right place: nearest-first sorting must make the carrier's
/// own light unloseable, and the reach filter must drop a carrier who
/// walked out of the window exactly as it drops a placed torch.
#[test]
fn a_carried_light_is_a_light_like_any_other() {
	let cam = [100.0, 64.0, 100.0];
	let reach = lateral_max(20);
	let class = digblock_core::gi::gi_look(digblock_core::chunk::make_cell(
		digblock_core::block::BlockType::Torch,
		0,
	));
	// A full list of placed torches, every one of them in reach and
	// none of them at the camera.
	let placed: Vec<_> = (0..MAX_LIGHTS)
		.map(|i| Emitter {
			pos: [cam[0] + 2.0 + i as f32 * 0.01, cam[1], cam[2]],
			class,
			level: 0,
			whitened: false,
			face: None,
		})
		.collect();
	let mine = digblock_core::gi::carried_origin(glam::Vec3::from_array(cam));
	let carried = [Emitter {
		pos: mine.to_array(),
		class,
		level: 0,
		whitened: false,
		face: None,
	}];
	let out = select_lights(
		placed.iter().copied(),
		&carried,
		cam,
		[0.0, 0.0, 1.0],
		reach,
		None,
		MAX_LIGHTS,
	);
	assert_eq!(out.len(), MAX_LIGHTS, "the list still fills exactly once");
	let entry = out
		.iter()
		.find(|e| [e[0], e[1], e[2]] == mine.to_array())
		.expect(
			"the light in your own hand is the nearest thing there is — a \
				 full world must not be able to truncate it away",
		);
	assert_eq!(
		entry[3].to_bits() & 255,
		u32::from(class),
		"a carried torch carries the same class byte a placed one does, \
			 so tint and level come from one LUT entry"
	);
	assert_eq!(
		entry[3].to_bits() & LIGHT_CARRIED,
		LIGHT_CARRIED,
		"and the carried flag on top, which is the only thing that \
			 distinguishes it — see LIGHT_CARRIED for why the margin needs it"
	);
	// The placed ones must NOT wear it, or every torch in the world
	// stops stopping its own shadow ray short of its own cell.
	assert!(
		out.iter()
			.filter(|e| [e[0], e[1], e[2]] != mine.to_array())
			.all(|e| e[3].to_bits() & LIGHT_CARRIED == 0),
		"a placed torch is not a carried one"
	);
	// Out past the reach every march breaks at: dropped, like any
	// other emitter no ray could have traced.
	let far = [Emitter {
		pos: [cam[0] + reach + 1.0, cam[1], cam[2]],
		class,
		level: 0,
		whitened: false,
		face: None,
	}];
	assert!(
		select_lights(
			std::iter::empty(),
			&far,
			cam,
			[0.0, 0.0, 1.0],
			reach,
			None,
			MAX_LIGHTS
		)
		.is_empty()
	);
}

/// A per-entry level has to reach the shader ON TOP of the class and
/// the carried flag, and an entry without one has to pack exactly the
/// word it packed before the field existed — otherwise every placed
/// torch in the world quietly changes brightness.
#[test]
fn a_wielders_level_rides_the_entry_and_nothing_elses() {
	let cam = [0.0, 0.0, 0.0];
	let class = digblock_core::gi::gi_look(digblock_core::chunk::make_cell(
		digblock_core::block::BlockType::ResonantPick,
		0,
	));
	let held = |level, whitened| {
		let e = [Emitter {
			pos: [1.0, 0.0, 0.0],
			class,
			level,
			whitened,
			face: None,
		}];
		select_lights(
			std::iter::empty(),
			&e,
			cam,
			[0.0, 0.0, 1.0],
			8.0,
			None,
			MAX_LIGHTS,
		)[0][3]
			.to_bits()
	};
	let plain = held(0, false);
	assert_eq!(
		plain,
		u32::from(class) | LIGHT_CARRIED,
		"no override, no bits"
	);
	let raised = held(13, true);
	assert_eq!(raised & 255, u32::from(class), "the class is untouched");
	assert_eq!(raised & LIGHT_CARRIED, LIGHT_CARRIED, "so is the flag");
	assert_eq!(
		(raised >> LIGHT_LEVEL_SHIFT) & LIGHT_LEVEL_MASK,
		13,
		"and the level reads back out of its own field"
	);
	assert_eq!(
		raised & LIGHT_WHITENED,
		LIGHT_WHITENED,
		"and the tint flag out of its own bit"
	);
	// The two per-holder properties are independent: a light may be
	// raised without being whitened, and the level field must not
	// smear into the flag above it.
	assert_eq!(held(LIGHT_LEVEL_MASK as u8, false) & LIGHT_WHITENED, 0);
}

/// The carried flag is packed here and unpacked in gi_dda.wgsl, which
/// both NEE passes take as a prelude. Nothing but agreement makes the
/// two halves one thing, so agreement is what gets pinned.
#[test]
fn the_lut_ops_match_the_shaders() {
	// The op is a WGSL mirror: nothing in Rust can derive what a shader
	// branches on, so the declaration is the contract. Every shader that
	// reads the LUT must name the same value — change the CPU op alone
	// and the marches would silently take glass for air.
	let decl = format!("const GI_LUT_OP_TINTED = {LUT_OP_TINTED}u;");
	for (name, src) in [
		("gi_common.wgsl", digblock_assets::shader::GI_COMMON),
		("gi_probe.wgsl", digblock_assets::shader::GI_PROBE),
	] {
		assert!(src.contains(&decl), "{name} must declare `{decl}`");
	}
	// Both `gi_solid` twins must keep reading a tinted cell as solid —
	// that is what keeps probe relocation and `gi_occluded` out of a
	// pane. Its threshold is a shader literal like the op, so it gets
	// read back out of the source rather than written down again here.
	for (name, src) in [
		("gi_common.wgsl", digblock_assets::shader::GI_COMMON),
		("gi_probe.wgsl", digblock_assets::shader::GI_PROBE),
	] {
		let threshold = src
			.split(">> 24u) >= ")
			.nth(1)
			.and_then(|s| s.split('u').next())
			.and_then(|s| s.trim().parse::<u32>().ok())
			.unwrap_or_else(|| panic!("{name} must spell its gi_solid threshold"));
		assert!(
			LUT_OP_TINTED >= threshold,
			"{name}: a tinted cell must read as solid ({LUT_OP_TINTED} < {threshold})"
		);
	}
}

#[test]
fn the_carried_flag_matches_the_shaders() {
	for decl in [
		format!("const GI_LIGHT_CARRIED = {LIGHT_CARRIED}u;"),
		format!("const GI_LIGHT_LEVEL_SHIFT = {LIGHT_LEVEL_SHIFT}u;"),
		format!("const GI_LIGHT_LEVEL_MASK = {LIGHT_LEVEL_MASK}u;"),
		format!("const GI_LIGHT_WHITENED = {LIGHT_WHITENED}u;"),
		format!("const GI_LIGHT_QUAD = {LIGHT_QUAD}u;"),
	] {
		assert!(
			digblock_assets::shader::GI_DDA.contains(&decl),
			"gi_dda.wgsl must declare `{decl}`"
		);
	}
	// And it has to stay clear of the class byte it shares the word
	// with, which is what makes the mask legal at all. The other side
	// needs no assert: `Emitter`'s class is a u8.
	assert_eq!(LIGHT_CARRIED & 255, 0);
	// Five tenants in one word now, and the packing is only legal
	// while none of them overlaps another.
	let level = LIGHT_LEVEL_MASK << LIGHT_LEVEL_SHIFT;
	assert_eq!(level & (255 | LIGHT_CARRIED), 0);
	assert_eq!(LIGHT_WHITENED & (255 | LIGHT_CARRIED | level), 0);
	assert_eq!(
		LIGHT_QUAD & (255 | LIGHT_CARRIED | level | LIGHT_WHITENED),
		0
	);
}

/// Every estimator clamps the list's own header count to the buffer's capacity, and that capacity
/// is `MAX_LIGHTS` — the allocation `flush_lights` selects into. The shader cannot see the Rust
/// const, so raising one without the other silently discards exactly the emitters the CPU kept:
/// they sit in the buffer, the header counts them, and every march walks past the tail. Checked as
/// "no unclamped read" rather than a fixed loop count, since nothing stops a third estimator.
#[test]
fn every_light_loop_clamps_to_the_lists_capacity() {
	let src = digblock_assets::shader::GI_COMMON;
	let clamp = "min(bitcast<u32>(lights[0].x), ";
	let mut clamped = 0;
	for tail in src.split(clamp).skip(1) {
		let cap = tail.split("u)").next().expect("the clamp must close");
		assert_eq!(
			cap.parse::<usize>().ok(),
			Some(MAX_LIGHTS),
			"a light loop clamps to {cap}, not MAX_LIGHTS"
		);
		clamped += 1;
	}
	assert!(clamped > 0, "gi_common.wgsl declares no clamped light loop");
	// A bare read is the failure this pin exists for, so the readers
	// are counted rather than assumed to be the estimators. Matched on
	// the bitcast the read needs, which the prose above them does not
	// spell out.
	assert_eq!(
		src.matches("bitcast<u32>(lights[0].x)").count(),
		clamped,
		"a light loop reads the header count without clamping it"
	);
}

/// The whole-chunk reach skip may only ever be CONSERVATIVE: admitting a chunk the per-emitter
/// test then rejects costs a little time, while dropping one that held a reachable light silently
/// puts a room out. Swept against the per-emitter rule over every cell of a band of chunks, the
/// only way to catch the failure that matters — the corner cells, where a chunk's nearest edge is
/// much closer than its centre.
#[test]
fn the_chunk_reach_skip_never_drops_a_reachable_light() {
	let cam = [37.5, 64.0, -12.25];
	let reach = lateral_max(20);
	let n = digblock_core::chunk::CHUNK_SIZE as i32;
	let mut admitted = 0;
	let mut skipped = 0;
	for cz in -14..14 {
		for cx in -14..14 {
			let pos = ChunkPos::new(cx, 4, cz);
			let ok = chunk_in_reach(pos, cam, reach);
			if ok {
				admitted += 1;
			} else {
				skipped += 1;
			}
			// Every cell of the chunk, corners included.
			for lz in 0..=n {
				for lx in 0..=n {
					let (x, z) = ((cx * n + lx) as f32, (cz * n + lz) as f32);
					let (dx, dz) = (x - cam[0], z - cam[2]);
					if dx * dx + dz * dz <= reach * reach {
						assert!(
							ok,
							"chunk {cx},{cz} skipped, but ({x},{z}) is inside the reach"
						);
					}
				}
			}
		}
	}
	// Anti-vacuity in both directions: a test where everything is
	// admitted proves nothing about the skip, and one where nothing is
	// proves nothing about the conservatism.
	assert!(admitted > 0 && skipped > 0, "{admitted} in, {skipped} out");
}

/// A rectangle reaches the shader as a PAIR — position and class word, then extents and facing —
/// and a point source's half of that pair stays zero. The stride is the thing to guard: both NEE
/// passes index `lights[i * 2 ...]`, so an entry that packed one vec4 or three would not fail
/// loudly — every light past the first would be read out of the middle of its neighbour, which
/// looks like a lighting bug rather than a format one.
#[test]
fn a_surface_emitter_packs_its_rectangle_beside_its_position() {
	let cam = [0.0, 0.0, 0.0];
	let class = digblock_core::gi::gi_look(BlockType::Emberore as u16);
	let seam = Emitter {
		pos: [2.0, 3.0, 4.0],
		class,
		level: 0,
		whitened: false,
		face: Some(Face {
			axis: 1,
			positive: true,
			ext: [3.0, 2.0],
		}),
	};
	let point = Emitter {
		pos: [1.0, 0.0, 0.0],
		class,
		level: 0,
		whitened: false,
		face: None,
	};
	let out = select_lights(
		[seam, point].into_iter(),
		&[],
		cam,
		[0.0, 0.0, 1.0],
		32.0,
		None,
		MAX_LIGHTS,
	);
	assert_eq!(out.len(), 2, "one entry each, whatever their shape");
	let e = out
		.iter()
		.find(|e| [e[0], e[1], e[2]] == seam.pos)
		.expect("the seam");
	assert_eq!(
		e[3].to_bits() & LIGHT_QUAD,
		LIGHT_QUAD,
		"a rectangle has to announce itself, or it is read as a point"
	);
	assert_eq!(e[3].to_bits() & 255, u32::from(class), "class untouched");
	assert_eq!([e[4], e[5]], [3.0, 2.0], "extents, in the packer's order");
	// Facing: the axis in the low bits, the sign above it — the same
	// decode `gi_light_sample` does.
	let code = e[6].to_bits();
	assert_eq!(code & 3, 1, "axis");
	assert_eq!(code & 4, 4, "outward normal points up the axis");
	let p = out
		.iter()
		.find(|e| [e[0], e[1], e[2]] == point.pos)
		.expect("the point");
	assert_eq!(p[3].to_bits() & LIGHT_QUAD, 0, "a torch is not a rectangle");
	assert_eq!(
		[p[4], p[5], p[6], p[7]],
		[0.0; 4],
		"and its second half stays zero, so nothing reads a stale extent"
	);
}

/// The sweep must be a PERMUTATION, not merely a shuffle: over one full cycle every probe has to
/// be traced exactly once, or some probes never converge and others burn budget twice. That is why
/// the stride has to be coprime with the count, and it is the kind of property that fails silently
/// — the picture just keeps a stale patch somewhere.
#[test]
fn the_permuted_sweep_visits_every_probe_exactly_once() {
	for dims in [C0_DIMS, C1_DIMS, (1, 1, 1), (7, 5, 3), (16, 16, 16)] {
		let count = dims.0 * dims.1 * dims.2;
		let stride = schedule_stride(count);
		let mut seen = vec![false; count as usize];
		for k in 0..count {
			let pi = ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
			assert!(!seen[pi as usize], "{dims:?}: probe {pi} traced twice");
			seen[pi as usize] = true;
		}
		assert!(
			seen.iter().all(|&s| s),
			"{dims:?}: a probe was never traced"
		);
	}
}

/// `wg.x * stride` is computed in u32 IN THE SHADER, where an
/// overflow silently wraps and the permutation stops being one.
#[test]
fn the_stride_cannot_overflow_the_shaders_multiply() {
	for dims in [C0_DIMS, C1_DIMS, (64, 64, 64), (1, 1, 1)] {
		let count = dims.0 * dims.1 * dims.2;
		let stride = schedule_stride(count);
		assert!(
			u64::from(stride) * u64::from(PROBES_PER_DISPATCH) <= u64::from(u32::MAX),
			"{dims:?}: stride {stride} overflows"
		);
	}
}

/// The history format must be able to REPRESENT the increment its history length asks for: rgba8
/// and rgba16f both let `hi + (x - hi)/N_MAX` round to zero and froze the running average per
/// pixel, in a pattern locked to wherever the player had walked — visible only to a harness that
/// can see motion, since a still has already converged. So the two knobs are pinned against each
/// other: blending at alpha 1/N stalls once the residual falls under about N ulps of the value.
#[test]
fn history_format_resolves_the_accumulator() {
	// N_MAX lives in the shader; read it rather than duplicating it,
	// so the two cannot drift.
	let src = digblock_assets::shader::GI_RESOLVE;
	let n_max: f32 = src
		.split("const N_MAX = ")
		.nth(1)
		.and_then(|s| s.split(';').next())
		.and_then(|s| s.trim().parse().ok())
		.expect("gi_resolve.wgsl must declare `const N_MAX = <f32>;`");

	// Explicit mantissa bits per format — the property under test, not
	// something to infer from a byte count.
	let mantissa_bits = match GI_ACC_FORMAT {
		wgpu::TextureFormat::Rgba32Float => 23,
		wgpu::TextureFormat::Rgba16Float => 10,
		wgpu::TextureFormat::Rgba8Unorm => 8,
		other => panic!("unknown accumulator format {other:?} — add its mantissa here"),
	};
	// Around a stored value of 1.0 (radiance and visibility both sit
	// there), an update rounds away once d/N < ulp/2.
	let stall = n_max * 2f32.powi(-(mantissa_bits + 1));
	let one_lsb = 1.0 / 255.0;
	assert!(
		stall < one_lsb,
		"{GI_ACC_FORMAT:?} with N_MAX {n_max} freezes the average while it is still \
			 {stall:.4} out (one 8-bit LSB is {one_lsb:.4}) — the running mean stops moving \
			 and every pixel keeps whatever it held when the camera stopped"
	);
}

/// Read a `const NAME = <number>;` out of a WGSL source (the pins
/// below hold shader constants against Rust ones, so they read the
/// shader rather than duplicating the number).
fn wgsl_f32(src: &str, name: &str) -> f32 {
	let pat = format!("const {name} = ");
	let at = src
		.find(&pat)
		.unwrap_or_else(|| panic!("{name}: not declared"));
	let rest = &src[at + pat.len()..];
	let end = rest
		.find(';')
		.unwrap_or_else(|| panic!("{name}: unterminated"));
	rest[..end]
		.trim()
		.parse()
		.unwrap_or_else(|e| panic!("{name}: not a number: {e}"))
}

/// The sun target's g channel packs `code + 256 * n`, and every reader unpacks with integer
/// bit-ops — which only works while the packed integer is EXACT in the accumulator format. rgba32f
/// holds integers to 2^24; dropping the target to rgba16f "because the sun history is short now"
/// would corrupt the pack, and the indirect counter inside it that still climbs to N_MAX, long
/// before the average itself froze.
#[test]
fn packed_metadata_stays_exact() {
	let n_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_MAX");
	let mantissa_bits = match GI_ACC_FORMAT {
		wgpu::TextureFormat::Rgba32Float => 23,
		wgpu::TextureFormat::Rgba16Float => 10,
		wgpu::TextureFormat::Rgba8Unorm => 8,
		other => panic!("unknown accumulator format {other:?} — add its mantissa here"),
	};
	let packed_max = 255.0 + 256.0 * n_max;
	let exact_to = 2f32.powi(mantissa_bits + 1);
	assert!(
		packed_max < exact_to,
		"code + 256*N_MAX ({packed_max}) does not round-trip in {GI_ACC_FORMAT:?} \
			 (integers are exact only below {exact_to}) — the face code and history \
			 length come back corrupted"
	);
}

/// The resolve turns the occluder distance into a penumbra width with
/// SUN_TAN, which must be the tangent of the SAME cone the per-pixel
/// direction offset samples (SUN_CONE_COS) — otherwise the dithered
/// edge band and the reconstruction kernel disagree about how wide the
/// penumbra is, and one of them shows.
#[test]
fn the_penumbra_knows_the_cone() {
	let sun_tan = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "SUN_TAN");
	let want = SUN_CONE_COS.acos().tan();
	let rel = (sun_tan - want).abs() / want;
	assert!(
		rel < 0.01,
		"SUN_TAN ({sun_tan}) is not tan(acos(SUN_CONE_COS)) = {want} — the cone and \
			 the reconstruction width have drifted apart"
	);
}

/// The widest penumbra the resolve will store (PEN_MAX) must fit
/// inside what the filter can actually spread: R taps per side, per
/// iteration, at that iteration's stride, scaled by at most
/// SUN_STRIDE_MAX. Widening one constant without the other leaves
/// shadows that claim a width the kernel never delivers.
#[test]
fn the_penumbra_fits_the_filter() {
	let pen_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PEN_MAX");
	let stride_max = wgsl_f32(digblock_assets::shader::GI_ATROUS, "SUN_STRIDE_MAX");
	let reach: f32 = ATROUS_STEPS.iter().map(|s| s * stride_max).sum();
	assert!(
		pen_max <= reach,
		"PEN_MAX ({pen_max}) exceeds the filter's combined support ({reach} texels: \
			 strides {ATROUS_STEPS:?} x SUN_STRIDE_MAX {stride_max})"
	);
}

/// The disocclusion prior primes a fresh pixel's history from the probe cache. It must stay under
/// both caps that could otherwise be asked to correct it: a primed pixel that is immediately
/// moving lives at N_MOVING, and the sun alpha rides min(n, N_SUN) — a prior claiming more
/// confidence than either would slow the very signals the short caps exist to keep responsive.
#[test]
fn the_disocclusion_prior_stays_under_the_caps() {
	let n_prior = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_PRIOR");
	let n_moving = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_MOVING");
	let n_sun = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_SUN");
	assert!(n_prior > 1.0, "a prior of 1 is no prior at all");
	assert!(
		n_prior <= n_sun && n_prior < n_moving,
		"N_PRIOR ({n_prior}) must stay under N_SUN ({n_sun}) and N_MOVING ({n_moving})"
	);
	// And its two gates must exist as real gates: a margin that admits
	// nothing extra would mark every depth rejection as grazing (the
	// prime never fires), a sun ceiling that admits everything would
	// prime fully sun-lit pixels (which shows up as washed-out AO
	// across the walk and fly regimes).
	let margin = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PRIME_MARGIN");
	let sun_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PRIME_SUN_MAX");
	assert!(margin > 1.0, "PRIME_MARGIN ({margin}) gates nothing at ≤1");
	assert!(
		sun_max > 0.0 && sun_max < 1.0,
		"PRIME_SUN_MAX ({sun_max}) must partition lit from unlit"
	);
}

/// The cover table's packed sub-box words must decode in the shader to the twelfths the shape
/// authored — `gi_box_span` unpacks the min triple and then the max triple out of one word. The
/// core side pins the same format from the packer's end (`box_pack_matches_the_shader_decode`);
/// this end pins that the table actually carries the covers, entry 0 the no-box sentinel, and
/// that the LUT is one row per LOOK — the torch's row carrying the torch's light, since the
/// light list names looks and the level has to ride along.
#[test]
fn the_cover_table_carries_every_cover_and_the_lut_every_look() {
	let covers = cover_table();
	let palette = digblock_core::gi::covers();
	assert!(!palette.is_empty());
	assert_eq!(
		covers[0], [0; COVER_ENTRY_WORDS],
		"entry 0 is the no-box sentinel"
	);
	for (i, c) in palette.iter().enumerate() {
		assert_ne!(covers[i + 1][0], 0, "cover {i} lost its bounds");
		assert_eq!(
			covers[i + 1][1],
			0,
			"cover {i}: the pad keeps the mask at word 2"
		);
		assert_eq!(
			&covers[i + 1][2..],
			&c.mask[..],
			"cover {i}: the mask rides verbatim"
		);
	}
	// The editor's slots are left for `write_cover`.
	for slot in 0..digblock_core::gi::PREVIEW_COVER_SLOTS as u8 {
		let at = usize::from(digblock_core::gi::preview_cover_slot(slot));
		assert_eq!(
			covers[at], [0; COVER_ENTRY_WORDS],
			"slot {slot} starts empty"
		);
	}
	let lut = material_lut(&[]);
	assert_eq!(lut[0], [0; 4], "row 0 is air");
	for (i, look) in digblock_core::gi::looks().iter().enumerate() {
		assert_eq!(
			lut[i + 1][1] & 0xff,
			u32::from(look.base.props().light),
			"look {i}"
		);
		assert_ne!(
			lut[i + 1][0] >> 24,
			3,
			"look {i}: the partial op is retired"
		);
	}
	let torch = digblock_core::gi::gi_look(BlockType::Torch as u16);
	assert_eq!(
		lut[torch as usize][1] & 0xff,
		u32::from(BlockType::Torch.props().light)
	);
}

/// Word 1 carries the cast COLOUR above the level, and for every row but the ore it is the row's
/// own albedo. The ores are the exception the field was added for: their textures mean out to
/// stone, so a derived tint would have every tier casting the same colourless light — they state
/// their fleck palette instead, and that palette must actually reach the LUT.
#[test]
fn the_cast_colour_is_the_albedo_except_where_a_vein_says_otherwise() {
	// One distinct mean per layer, so "took the right one" is
	// checkable rather than vacuously true.
	let means: Vec<[u8; 4]> = (0..digblock_core::block::tex::COUNT)
		.map(|i| [i as u8, 200 - i as u8, 40 + i as u8, 255])
		.collect();
	let lut = material_lut(&means);
	let tint = |b: BlockType| {
		let w = lut[usize::from(digblock_core::gi::gi_look(b as u16))][1] >> 8;
		[
			(w & 0xff) as u8,
			((w >> 8) & 0xff) as u8,
			((w >> 16) & 0xff) as u8,
		]
	};
	for b in [
		BlockType::Torch,
		BlockType::Lantern,
		BlockType::EssenceSiphon,
	] {
		let m = means[b.props().tex_top as usize];
		assert_eq!(
			tint(b),
			[m[0], m[1], m[2]],
			"{b:?} must still cast its own texture's colour"
		);
	}
	for &ore in &digblock_core::worldgen::ore::ORES {
		let (core, _) = ore.node_flecks().expect("a tier");
		assert_eq!(tint(ore), core, "{ore:?} casts its flecks, not its rock");
		let m = means[ore.props().tex_top as usize];
		assert_ne!(
			tint(ore),
			[m[0], m[1], m[2]],
			"{ore:?}: the whole point is that this is NOT the texture mean"
		);
		assert!(
			lut[usize::from(digblock_core::gi::gi_look(ore as u16))][1] & 0xff > 0,
			"{ore:?} has a colour and no light to cast in it"
		);
	}
}

/// The tinted word (.z) and its op, swept and pinned at the endpoints. Op 4 rides exactly the
/// translucent bucket; every other full-cube row leaves .z zero (the no-box sentinel the shaped
/// classes rely on). The filter is pinned by its two ENDPOINTS rather than its formula: a
/// fully-covering texture filters light to its own albedo, and a fully-open one filters nothing —
/// the relationship `gi_glass_tr` (gi_common.wgsl) decodes, worked here because WGSL cannot be run.
#[test]
fn the_tinted_word_is_the_bucket_and_pins_its_endpoints() {
	use digblock_core::block::MeshBucket;
	// Full cover: the filter IS the albedo. Open: the filter is white.
	let mut means = vec![[0u8; 4]; digblock_core::block::tex::COUNT as usize];
	let covered = digblock_core::block::BlockType::Glass.props().tex_top as usize;
	means[covered] = [90, 140, 200, 255];
	let lut_covered = material_lut(&means);
	let glass = usize::from(digblock_core::gi::gi_look(BlockType::Glass as u16));
	let z = lut_covered[glass][2];
	assert_eq!(
		[z & 0xff, (z >> 8) & 0xff, (z >> 16) & 0xff],
		[90, 140, 200],
		"full cover filters to the albedo"
	);
	means[covered] = [90, 140, 200, 0];
	let lut_open = material_lut(&means);
	let z = lut_open[glass][2];
	assert_eq!(
		[z & 0xff, (z >> 8) & 0xff, (z >> 16) & 0xff],
		[255, 255, 255],
		"no cover filters nothing"
	);
	// The op is the bucket, and .z is nonzero exactly on op-4 rows,
	// swept over every block's look.
	let mut tinted = 0u32;
	for b in BlockType::ALL {
		if b == BlockType::Air {
			continue;
		}
		let id = usize::from(digblock_core::gi::gi_look(b as u16));
		let op = lut_covered[id][0] >> 24;
		if b.props().bucket == MeshBucket::Translucent {
			tinted += 1;
			assert_eq!(op, 4, "{b:?}");
			assert_ne!(lut_covered[id][2], 0, "{b:?} carries a filter");
		} else {
			assert_ne!(op, 4, "{b:?}");
			assert_eq!(lut_covered[id][2], 0, "{b:?} filter word stays clear");
		}
	}
	assert!(tinted > 0, "the bucket has members to sweep");
}

/// A contiguous cursor is what made convergence sweep as a visible
/// band. Consecutive sweep steps must land far apart in the probe
/// grid — this is the property, not an implementation detail.
#[test]
fn consecutive_sweep_steps_are_not_neighbours() {
	let count = C0_DIMS.0 * C0_DIMS.1 * C0_DIMS.2;
	let stride = schedule_stride(count);
	let probe = |k: u32| ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
	let mut worst = u32::MAX;
	for k in 0..count.min(4096) {
		let d = probe(k).abs_diff(probe(k + 1));
		worst = worst.min(d.min(count - d));
	}
	assert!(
		worst > count / 8,
		"consecutive steps land {worst} apart out of {count} — that is a band"
	);
}

/// The window is sized so that every march reaches the last streamed column at every slider
/// rung: its toroidal half-extent holds the whole residency disc (so no resident chunk is
/// refused by `gi_uploadable`), and `lateral_max` — the bound every march breaks at — is not
/// short of the streamed radius. A window one draw distance on a side failed both by half, and
/// that half was a lit, shadowless, dome-lit ring around any camera high enough to see it (R40).
#[test]
fn the_window_covers_the_streamed_radius() {
	use crate::settings::{DRAW_FLOOR, DRAW_MAX};
	let mut rungs = 0;
	for d in DRAW_FLOOR..=DRAW_MAX {
		let wc = window_chunks_for(d);
		assert!(
			(wc / 2) as i32 >= d,
			"draw {d}: a half-extent of {} chunks leaves resident columns outside the window",
			wc / 2
		);
		let streamed = (d * 16) as f32;
		assert!(
			lateral_max(wc) >= streamed - 1.0,
			"draw {d}: the marches stop at {} blocks, short of the {streamed} streamed",
			lateral_max(wc)
		);
		rungs += 1;
	}
	assert!(rungs > 1, "the sweep examined the slider");
}
