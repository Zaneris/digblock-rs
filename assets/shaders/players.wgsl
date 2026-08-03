// Remote players: one instanced box per player, generated entirely from
// vertex_index (no geometry buffer — the spray.wgsl trick). Instance =
// two vec4s: (eye position xyz, yaw) + (color rgb, spare). The box spans
// 0.6 × 1.8 × 0.6 anchored to the eye (y −1.5 .. +0.3), matching the
// capsule collider bounds, and is lit with a flat lambert + the standard
// distance fog so it sits in the scene like terrain does.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

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
	@location(0) col: vec4f,
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
	// Body proportions, eye-anchored: y −1.5 .. +0.3 around the eye.
	let scaled = vec3f(local.x * 0.6, local.y * 1.8 - 0.6, local.z * 0.6);
	let s = sin(i0.w);
	let c = cos(i0.w);
	let rot = vec3f(scaled.x * c + scaled.z * s, scaled.y, -scaled.x * s + scaled.z * c);
	let world = i0.xyz + rot;
	let wn = vec3f(n.x * c + n.z * s, n.y, -n.x * s + n.z * c);

	// Flat lambert against the shared light rig + ambient.
	let diffuse = max(dot(wn, -frame.light_dir.xyz), 0.0) * frame.light_dir.w;
	var col = i1.rgb * (frame.ambient.rgb + frame.light_color.rgb * diffuse);
	// Standard distance fog (shared helper — matches every other pass).
	let dist = distance(world, frame.cam_pos.xyz);
	col = apply_fog(col, dist, world);

	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.col = vec4f(col, 1.0);
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	return in.col;
}
