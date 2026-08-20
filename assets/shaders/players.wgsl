// Remote players: two draws sharing one pipeline layout.
//
// The BODY is one instanced box per player, generated entirely from
// vertex_index (the spray.wgsl trick). Instance = two vec4s: (eye xyz, yaw) +
// (color rgb, spare). The box takes the capsule collider's own width and
// height, anchored to the eye so it stands where the collider does.
//
// The HELD ITEM is `vs_held` into the SAME `fs`: real item geometry from
// item_geom by way of viewmodel::build — the same authority the inventory icons
// and your own hand use, so a remote's bucket shows its charge. It cannot share
// the body's vertex-index generator and it wants the opposite `cull`, so it is a
// second pipeline over the same three bind groups.
//
// Lighting is RTGI for both: a per-fragment probe gather, a short sun DDA, and
// the shared `sum_lights` NEE over the emitter list (R33, R34) — players never
// read the half-res resolve buffers, whose depth prepass does not know them, so
// a body beside a torch would read black — then the same participating medium
// the terrain goes through, so they sit in the scene rather than in front of it.
// A held item that GLOWS skips all of that for ink_common's `emissive_ink`,
// exactly as chunk.wgsl does for the same block standing in the world.

// The Frame struct + group(0) binding come from frame_common.wgsl, and
// the group(2) GI world bindings + gather core from gi_common.wgsl
// (both prepended by digblock-assets).

@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

// Sun DDA step cap — players are a few boxes; shore-of-the-shadow
// accuracy within this many blocks, probes carry the rest.
const SUN_STEPS = 12;

// Cost-stub mirror (DIG_STUB; renderer.rs's `stub` module owns the names
// and the bit numbers — water.wgsl, gi_resolve.wgsl and confetti.wgsl
// are the others).
// One bit, because players compute exactly one term the resolve also
// stubs: the NEE below must vanish with the resolve's under the same
// knob, or an attribution run measures half of it.
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

// item_geom's face order, indexed by digblock_core::dir::Dir's
// discriminants — the SAME table viewmodel.wgsl uses, and deliberately
// NOT the same order as `NORMALS` above, which is the cube generator's
// own axis order. Two six-entry tables pinned by one Rust enum is
// proportionate; a third consumer would justify folding it into a
// prelude.
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

// The hand. −x is the carrier's RIGHT: yaw is atan2(look.x, look.z), so
// forward is +z, and right = forward × up = −x. The cube this replaced
// sat at the mirrored offset — the carrier's LEFT — under a comment that
// said "right of the body". With a cube nobody could tell; with a torch
// you can, and your own viewmodel rests on the right (REST_X_FRAC).
//
// The y is a CARRY and not a rest: high enough on the body to read as
// held out in front rather than hanging at the hip, which is also most
// of the gap to where a carried light radiates from closed. The two
// stay separate numbers on purpose — `gi::CARRY_DROP` is anchored on
// the capsule axis so mouse-look cannot move the light, while the hand
// is off to the side of the body, so they can never coincide anyway.
const HAND = vec3f(-0.44, -0.5, 0.34);

// Turn the item about its own axis so a cube presents a corner rather
// than a flat card — the same idea as viewmodel.wgsl's REST_YAW, but a
// separate constant on purpose: that one is expressed in VIEW space
// (−z forward) and this one in world space (+z forward), so
// one shared symbol would mean two different rotations under one name.
// The viewmodel's REST_PITCH is deliberately not copied — it is a
// screen-space affordance for something a metre from your eye, and out
// in the world it would just lean the torch over.
const ITEM_YAW = -0.785398163;

// The per-FACE ink bit above the texture layer in the item vertex word
// (viewmodel.rs's `INK_BIT`, and the twin of viewmodel.wgsl's constant).
// Held items are built once for both hands, so both pipelines decode it.
const ITEM_INK_BIT = 65536u;

struct VsOut {
	@builtin(position) pos: vec4f,
	// Flat body color; ignored when a texture layer is set.
	@location(0) albedo: vec3f,
	// uv + array layer; layer < 0 = untextured body box.
	@location(1) uvl: vec3f,
	@location(2) world: vec3f,
	// World-space face normal (flat — a box face has one).
	@location(3) @interpolate(flat) wn: vec3f,
	// Set means fullbright ink (`glows`): skip the lighting entirely.
	@location(4) @interpolate(flat) emissive: f32,
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
	var col: vec3f;
	if in.emissive > 0.5 {
		// Fullbright ink is its own light — no sun, no GI — exactly as
		// chunk.wgsl treats the same block standing in the world, through
		// the same shared function. One torch, three places, one look.
		col = emissive_ink(albedo);
	} else {
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
		// The direct term for local lights (R33): the shared NEE over the
		// same list the resolve reads — a probe carries a torch only as its
		// bounce off the room, and the resolve's half-res NEE can never cover
		// a player, who is not in the depth prepass.
		//
		// `sum_lights` and not the resolve's `resample_light` (R34), because
		// nothing filters this: one drawn light carrying the whole pool needs
		// an accumulator behind it to average its visibility out, and a body
		// has none — it dithered into world-anchored blocks of full pool and
		// pure black that swam across a walking player.
		//
		// The `far` gate is the resolve's own rule: past the window's reach a
		// shadow ray has no data to march.
		var nee = vec3f(0.0);
		let lat = gp.xz - frame.cam_pos.xz;
		let gmax = gi_lateral_max(frame.gi_misc.x);
		if dot(lat, lat) <= gmax * gmax && !stubbed(STUB_GINEE) {
			nee = sum_lights(side, frame.gi_misc.y, frame.cam_pos.xz, gp, in.wn);
		}
		col = albedo * (frame.light_color.rgb * direct * sh + indirect + nee);
	}
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
