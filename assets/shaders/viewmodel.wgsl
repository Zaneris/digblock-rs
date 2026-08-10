// The first-person viewmodel: the item in your own hand.
//
// Unlike every other pass, this one does NOT use the shared frame
// uniforms. The item is authored in view space and carries its own
// projection (see viewmodel.rs), so the camera never enters the maths —
// the CPU hands over one finished mvp. The light rig comes along in the
// same uniform, with the direction already rotated into ITEM space, so
// the raw face normals below can be dotted against it with no normal
// matrix. There is no fog: the item is a metre from the eye.
//
// Vertices are item geometry in twelfths, drawn through the shared quad
// index pattern — same corner order as the chunk mesher, which is what
// lets `cull: Back` keep the half you can see.

struct Vm {
	mvp: mat4x4<f32>,
	// xyz = light direction in item space (toward the ground);
	// w = direct-light strength.
	light: vec4f,
	light_color: vec4f,
}

@group(0) @binding(0) var<uniform> vm: Vm;

// RTGI camera light (gi_probe.wgsl's cam_light entry): [0] = omni
// probe ambient at the camera (already INDIRECT_K-scaled), [1].x = sun
// visibility. The hand is at the camera by definition, so this IS its
// light — indoors the ambient carries the torch glow and the sun term
// dies, no CPU probe reads anywhere.
struct CamLight {
	ambient: vec4f,
	sun: vec4f,
}
@group(0) @binding(1) var<uniform> cam_light: CamLight;

@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

// Indexed by digblock_core::dir::Dir's discriminants:
// 0=West(+x) 1=North(+z) 2=South(-z) 3=Bottom(-y) 4=Top(+y) 5=East(-x).
const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
);

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) shade: vec3f,
}

@vertex
fn vs(
	@location(0) pos: vec3f, // twelfths
	@location(1) uv: vec2f,
	@location(2) layer: u32,
	@location(3) face: u32,
) -> VsOut {
	let f = min(face, 5u);
	let n = NORMALS[f];
	// Twilight fade (vm.light.w) and traced sun visibility both gate
	// the direct term, exactly as `sh` does for world surfaces.
	let sh = mix(1.0, cam_light.sun.x, vm.light.w);
	let diffuse = max(dot(n, -vm.light.xyz), 0.0) * sh;

	var out: VsOut;
	out.pos = vm.mvp * vec4f(pos, 1.0);
	out.uv = uv;
	out.layer = layer;
	out.shade = cam_light.ambient.rgb + vm.light_color.rgb * diffuse;
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	// Explicit level: the item is a fixed size on screen and the atlas
	// page has no mips, and Tint's derivative-uniformity rule wants the
	// level spelled out anyway.
	let tex = textureSampleLevel(materials, mat_smp, in.uv, i32(in.layer), 0.0);
	return vec4f(tex.rgb * in.shade, 1.0);
}
