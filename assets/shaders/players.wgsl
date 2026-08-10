// Remote players: one instanced box per player, generated entirely from
// vertex_index (no geometry buffer — the spray.wgsl trick). Instance =
// two vec4s: (eye position xyz, yaw) + (color rgb, mode). Mode 0 is the
// body box: 0.6 × 1.8 × 0.6 anchored to the eye (y −1.5 .. +0.3),
// matching the capsule collider bounds, flat-colored. Mode > 0 is the
// HELD ITEM companion: a small cube at a yaw-rotated hand offset,
// textured from the block array at layer (mode − 1). Lighting is RTGI
// (GI-DESIGN.md): a per-fragment probe gather through gi_common plus a
// short sun DDA — players never read the half-res resolve buffers,
// whose depth prepass doesn't know them — and then the same
// participating medium (medium_common) the terrain beside them goes
// through, so they sit in the scene rather than in front of it.

// The Frame struct + group(0) binding come from frame_common.wgsl, and
// the group(2) GI world bindings + gather core from gi_common.wgsl
// (both prepended by digblock-assets).

@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

// Sun DDA step cap — players are a few boxes; shore-of-the-shadow
// accuracy within this many blocks, probes carry the rest.
const SUN_STEPS = 12;

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

struct VsOut {
	@builtin(position) pos: vec4f,
	// Flat body color; ignored when a texture layer is set.
	@location(0) albedo: vec3f,
	// uv + array layer; layer < 0 = untextured body box.
	@location(1) uvl: vec3f,
	@location(2) world: vec3f,
	// World-space face normal (flat — a box face has one).
	@location(3) @interpolate(flat) wn: vec3f,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) i0: vec4f, // eye pos xyz, yaw
	@location(1) i1: vec4f, // color rgb, mode (0 body, layer+1 held item)
) -> VsOut {
	let face = vi / 6u;
	let corner = vi % 6u;
	let n = NORMALS[face];
	let t = TANGENTS[face];
	let b = cross(n, t);
	let uv = QUAD[corner];
	let local = n * 0.5 + (uv.x - 0.5) * t + (uv.y - 0.5) * b;
	let held = i1.w > 0.5;
	// Body proportions, eye-anchored: y −1.5 .. +0.3 around the eye —
	// or a small cube in the "hand": right of the body, slightly
	// forward and below the eye.
	var scaled: vec3f;
	if (held) {
		scaled = local * 0.35 + vec3f(0.44, -0.62, 0.34);
	} else {
		scaled = vec3f(local.x * 0.6, local.y * 1.8 - 0.6, local.z * 0.6);
	}
	let s = sin(i0.w);
	let c = cos(i0.w);
	let rot = vec3f(scaled.x * c + scaled.z * s, scaled.y, -scaled.x * s + scaled.z * c);
	let world = i0.xyz + rot;
	let wn = vec3f(n.x * c + n.z * s, n.y, -n.x * s + n.z * c);

	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.albedo = i1.rgb;
	out.uvl = vec3f(uv, select(-1.0, i1.w - 1.0, held));
	out.world = world;
	out.wn = wn;
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	// Sampled unconditionally (explicit level — Tint's derivative-
	// uniformity rule), selected only when this is a held-item cube.
	let tex = textureSampleLevel(
		materials,
		mat_smp,
		in.uvl.xy,
		i32(max(in.uvl.z, 0.0) + 0.5),
		0.0,
	);
	let albedo = select(in.albedo, tex.rgb, in.uvl.z >= 0.0);
	// RTGI: quantized-sun lambert x traced visibility + probe indirect —
	// the same combine as chunk.wgsl.
	let side = i32(frame.gi_misc.x);
	let gp = in.world;
	let indirect =
		gi_indirect(side, gp, in.wn, frame.gi0, frame.gi1, frame.zenith.rgb, frame.horizon.rgb, vec2i(4, 2));
	let direct = max(dot(in.wn, -frame.light_dir.xyz), 0.0);
	let sv = gi_sun_vis(side, frame.cam_pos.xz, frame.gi_sun, gp, SUN_STEPS);
	let sh = mix(vec3f(1.0), sv.x * medium_water_tr(sv.y), frame.light_dir.w);
	let col = albedo * (frame.light_color.rgb * direct * sh + indirect);
	// The same medium the terrain beside them goes through — that
	// sharing is the whole reason it is a prelude and not a local.
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
