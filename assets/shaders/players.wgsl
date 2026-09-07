// Remote players: two draws sharing one pipeline layout. The BODY is one instanced box per
// player, generated entirely from vertex_index (the spray.wgsl trick). Instance = two vec4s:
// (eye xyz, yaw) + (color rgb, spare), the box taking the capsule collider's width and height,
// anchored to the eye so it stands where the collider does.
//
// The HELD ITEM is `vs_held` into the SAME `fs`: real item geometry from item_geom by way of
// viewmodel::build, the same authority the inventory icons and your own hand use, so a remote's
// bucket shows its charge. It cannot share the body's vertex-index generator and wants the
// opposite `cull`, so it is a second pipeline over the same three bind groups.
//
// Lighting is RTGI for both — a probe gather, a short sun DDA, and the shared `sum_lights` NEE
// (R33, R34), because players never read the half-res resolve buffers, whose depth prepass does
// not know them, so a body beside a torch would read black — then the terrain's participating
// medium. A held item that GLOWS skips it for ink_common's `emissive_ink`, as chunk.wgsl does.

// The Frame struct + group(0) binding come from frame_common.wgsl, and
// the group(2) GI world bindings + gather core from gi_common.wgsl
// (both prepended by digblock-assets).

@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

// Sun DDA step cap — players are a few boxes; shore-of-the-shadow
// accuracy within this many blocks, probes carry the rest.
const SUN_STEPS = 12;

// Cost-stub mirror (DIG_STUB; renderer.rs's `stub` module owns the names and the bit numbers
// — water.wgsl, gi_resolve.wgsl and confetti.wgsl are the others). One bit, because players
// compute exactly one term the resolve also stubs: the NEE below must vanish with the
// resolve's under the same knob, or an attribution run measures half of it.
const STUB_GINEE = 1024u;

fn stubbed(bit: u32) -> bool {
	return (u32(frame.misc.z) & bit) != 0u;
}

const QUAD = array<vec2f, 6>(
	vec2f(0.0, 0.0),
	vec2f(1.0, 0.0),
	vec2f(0.0, 1.0),
	vec2f(0.0, 1.0),
	vec2f(1.0, 0.0),
	vec2f(1.0, 1.0),
);

// Face normals and tangents of a unit cube (bitangent = cross).
const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
);
const TANGENTS = array<vec3f, 6>(
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(1.0, 0.0, 0.0),
);

// item_geom's face order, indexed by digblock_core::dir::Dir's discriminants — the SAME table
// viewmodel.wgsl uses, and deliberately NOT the order of `NORMALS` above, which is the cube
// generator's own axis order. A third consumer would justify folding it into a prelude.
const DIR_NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
);

// A cell renders exactly as wide as the cube this replaced did, so a
// held Dirt block is the same SIZE it always was.
// Not the same pixels: the hand moved to the other side of the body
// (HAND, below) and the item now sits at ITEM_YAW, both of which this
// change fixed on purpose.
const HELD_SCALE = 0.35 / 12.0;

// The hand. −x is the carrier's RIGHT: yaw is atan2(look.x, look.z), so forward is +z and
// right = forward × up = −x, matching your own viewmodel's REST_X_FRAC. The y is a CARRY, not
// a rest: high enough to read as held out in front rather than hanging at the hip, closing
// most of the gap to where a carried light radiates from. It stays separate from
// `gi::CARRY_DROP`, anchored on the capsule axis so mouse-look cannot move the light.
const HAND = vec3f(-0.44, -0.5, 0.34);

// Turn the item about its own axis so a cube presents a corner rather than a flat card — the
// same idea as viewmodel.wgsl's REST_YAW, kept separate because that one is in VIEW space (−z
// forward) and this one in world space (+z forward), so one shared symbol would mean two
// different rotations under one name. REST_PITCH is deliberately not copied: it is a
// screen-space affordance for something a metre from your eye and would lean the torch over.
const ITEM_YAW = -0.785398163;

// The per-FACE ink bit above the texture layer in the item vertex word
// (viewmodel.rs's `INK_BIT`, and the twin of viewmodel.wgsl's constant).
// Held items are built once for both hands, so both pipelines decode it.
const ITEM_INK_BIT = 65536u;
// The finish's two bits sit just above it (viewmodel.rs's FINISH_SHIFT).
const ITEM_FINISH_SHIFT = 17u;

struct VsOut {
	@builtin(position) pos: vec4f,
	// Flat body color; ignored when a texture layer is set.
	@location(0) albedo: vec3f,
	// uv + array layer; layer < 0 = untextured body box.
	@location(1) uvl: vec3f,
	@location(2) world: vec3f,
	// World-space face normal (flat — a box face has one).
	@location(3) @interpolate(flat) wn: vec3f,
	// Set enables fullbright ink through the material’s emission mask.
	@location(4) @interpolate(flat) emissive: f32,
	// The held face's finish (finish_common.wgsl); a body is matte.
	@location(5) @interpolate(flat) finish: u32,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) i0: vec4f, // eye pos xyz, yaw
	@location(1) i1: vec4f, // color rgb, spare
) -> VsOut {
	let face = vi / 6u;
	let corner = vi % 6u;
	let n = NORMALS[face];
	let t = TANGENTS[face];
	let b = cross(n, t);
	let uv = QUAD[corner];
	let local = n * 0.5 + (uv.x - 0.5) * t + (uv.y - 0.5) * b;
	// Body proportions, eye-anchored: the box hangs from just above the
	// eye down to the feet, matching the capsule collider.
	let scaled = vec3f(local.x * 0.6, local.y * 1.8 - 0.6, local.z * 0.6);
	let s = sin(i0.w);
	let c = cos(i0.w);
	let rot = vec3f(scaled.x * c + scaled.z * s, scaled.y, -scaled.x * s + scaled.z * c);
	let world = i0.xyz + rot;
	let wn = vec3f(n.x * c + n.z * s, n.y, -n.x * s + n.z * c);

	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.albedo = i1.rgb;
	out.uvl = vec3f(uv, -1.0);
	out.world = world;
	out.wn = wn;
	out.emissive = 0.0;
	out.finish = 0u;
	return out;
}

// The item in a remote's hand: real geometry in twelfths, placed by the
// carrier's yaw. Item yaw first (about the item's own centre), then the
// hand offset, then the carrier's yaw, then their eye — both rotations
// have determinant +1, so `cull: Back` survives them.
@vertex
fn vs_held(
	@location(0) pos: vec3f, // twelfths, from item_geom
	@location(1) uv: vec2f,
	@location(2) layer: u32,
	@location(3) face: u32, // digblock_core::dir::Dir discriminant
	@location(4) i0: vec4f, // eye pos xyz, yaw
	@location(5) i1: vec4f, // emissive, item height in twelfths, spare, spare
) -> VsOut {
	// Centre on the item's own origin the way viewmodel::mvp does — x/z
	// on the cell centre, y on half the item's height — so a door, which
	// stands two cells tall, hangs in the hand the way a cube does.
	let centred = (pos - vec3f(6.0, i1.y * 0.5, 6.0)) * HELD_SCALE;
	let n = DIR_NORMALS[min(face, 5u)];
	let is = sin(ITEM_YAW);
	let ic = cos(ITEM_YAW);
	let li = vec3f(centred.x * ic + centred.z * is, centred.y, -centred.x * is + centred.z * ic);
	let ni = vec3f(n.x * ic + n.z * is, n.y, -n.x * is + n.z * ic);
	let local = li + HAND;
	let s = sin(i0.w);
	let c = cos(i0.w);
	let world = i0.xyz + vec3f(local.x * c + local.z * s, local.y, -local.x * s + local.z * c);

	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.albedo = vec3f(1.0);
	out.uvl = vec3f(uv, f32(layer & (ITEM_INK_BIT - 1u)));
	out.world = world;
	out.wn = vec3f(ni.x * c + ni.z * s, ni.y, -ni.x * s + ni.z * c);
	// Two ways to be ink, exactly as in your own hand (viewmodel.wgsl):
	// the whole item glows (the instance flag), or this one face does
	// (the bit above the layer index). A remote's siphon has to hold the
	// same lit bulb yours does, or the two hands disagree about one item.
	out.emissive = max(i1.x, f32((layer & ITEM_INK_BIT) != 0u));
	out.finish = (layer >> ITEM_FINISH_SHIFT) & 3u;
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	// Sampled unconditionally (explicit level — Tint's derivative-
	// uniformity rule), selected only when this is a held item.
	let tex = textureSampleLevel(
		materials,
		mat_smp,
		in.uvl.xy,
		i32(max(in.uvl.z, 0.0) + 0.5),
		0.0,
	);
	let albedo = select(in.albedo, tex.rgb, in.uvl.z >= 0.0);
	let mask = textureSampleLevel(materials, mat_smp, in.uvl.xy,
		i32(max(in.uvl.z, 0.0) + 0.5) + i32(textureNumLayers(materials) / 2u), 0.0).r;
	let emission = select(0.0, mask, in.emissive > 0.5);
	var col = emissive_ink(albedo);
	if emission < 1.0 {
		// RTGI: quantized-sun lambert x traced visibility + probe indirect
		// — the same combine as chunk.wgsl.
		let side = i32(frame.gi_misc.x);
		let gp = in.world;
		let indirect =
			gi_indirect(side, gp, in.wn, frame.cam_pos.xyz, frame.gi0, frame.gi1, frame.zenith.rgb, frame.horizon.rgb, vec2i(4, 2));
		let direct = max(dot(in.wn, -frame.light_dir.xyz), 0.0);
		let sv = gi_sun_vis(side, frame.gi_misc.y, frame.cam_pos.xz, frame.gi_sun, gp, SUN_STEPS);
		// Strength fades the LIGHT, never the shadow — see chunk.wgsl.
		// `thru` is the glass filter the ray crossed (op 4).
		let sh = sv.vis * medium_water_tr(sv.water) * sv.thru * frame.light_dir.w;
		// The direct term for local lights (R33): the shared NEE over the same list the
		// resolve reads, since a half-res NEE can never cover a player who is not in the
		// depth prepass. `sum_lights` and not `resample_light` (R34) because nothing filters
		// this — one drawn light carrying the whole pool needs an accumulator to average its
		// visibility out, and a body has none. The `far` gate is the resolve's own rule.
		var nee = vec3f(0.0);
		let lat = gp.xz - frame.cam_pos.xz;
		let gmax = gi_lateral_max(frame.gi_misc.x);
		if dot(lat, lat) <= gmax * gmax && !stubbed(STUB_GINEE) {
			nee = sum_lights(side, frame.gi_misc.y, frame.cam_pos.xz, gp, in.wn, frame.light_color.w);
		}
		col = albedo * (frame.light_color.rgb * direct * sh + indirect + nee);
		if in.finish != 0u {
			// The world's glossy rule (chunk.wgsl): the sun lobe through the traced
			// visibility, and the probe cache read along the mirrored view ray.
			let v = normalize(frame.cam_pos.xyz - gp);
			let spec = frame.light_color.rgb * sh * finish_lobe(in.wn, v, -frame.light_dir.xyz, in.finish);
			let mirror = gi_cache_at(side, gp + in.wn * GI_SURFACE_EPS, reflect(-v, in.wn),
				frame.cam_pos.xyz, frame.gi0, frame.gi1, frame.zenith.rgb, frame.horizon.rgb);
			let local = finish_fresnel(in.finish, dot(in.wn, v)) * mirror
				+ finish_lamps(gp + in.wn * GI_SURFACE_EPS, in.wn, v, in.finish);
			col = finish_combine(col, albedo, in.finish, spec, local);
		}
	}
	col = mix(col, emissive_ink(albedo), emission);
	// The same medium the terrain beside them goes through — that
	// sharing is the whole reason it is a prelude and not a local. It
	// runs on BOTH branches: the branch narrows to the lighting rather
	// than returning early, because ink still travels to the eye.
	let ray = normalize(in.world - frame.cam_pos.xyz);
	let sky = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	let lit = medium_view(
		col,
		frame.cam_pos.xyz,
		in.world,
		sky,
		frame.zenith.rgb,
		frame.water_misc,
		frame.misc.y,
	);
	return vec4f(lit, 1.0);
}
