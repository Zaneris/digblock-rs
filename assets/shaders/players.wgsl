// Remote players: one instanced box per player, generated entirely from
// vertex_index (no geometry buffer — the spray.wgsl trick). Instance =
// two vec4s: (eye position xyz, yaw) + (color rgb, mode). Mode 0 is the
// body box: 0.6 × 1.8 × 0.6 anchored to the eye (y −1.5 .. +0.3),
// matching the capsule collider bounds, flat-colored. Mode > 0 is the
// HELD ITEM companion: a small cube at a yaw-rotated hand offset,
// textured from the block array at layer (mode − 1). Both are lit with
// a flat lambert + the standard distance fog so they sit in the scene
// like terrain does.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

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
	// Light (ambient + diffuse) to multiply the albedo by.
	@location(0) shade: vec3f,
	// Flat body color; ignored when a texture layer is set.
	@location(1) albedo: vec3f,
	// uv + array layer; layer < 0 = untextured body box.
	@location(2) uvl: vec3f,
	@location(3) world: vec3f,
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

	// Flat lambert against the shared light rig + ambient.
	let diffuse = max(dot(wn, -frame.light_dir.xyz), 0.0) * frame.light_dir.w;

	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.shade = frame.ambient.rgb + frame.light_color.rgb * diffuse;
	out.albedo = i1.rgb;
	out.uvl = vec3f(uv, select(-1.0, i1.w - 1.0, held));
	out.world = world;
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
	var col = albedo * in.shade;
	col = apply_fog(col, distance(in.world, frame.cam_pos.xyz), in.world);
	return vec4f(col, 1.0);
}
