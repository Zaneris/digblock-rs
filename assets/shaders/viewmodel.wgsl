// The first-person viewmodel: the item in your own hand.
//
// Unlike every other pass, this one does NOT use the shared frame uniforms. The
// item is authored in view space and carries its own projection (viewmodel.rs),
// so the camera never enters the maths — the CPU hands over one finished mvp,
// with the light direction already rotated into ITEM space, so the raw face
// normals below dot against it with no normal matrix. No fog: the item is a
// metre from the eye. Vertices are item geometry in twelfths, drawn through the
// shared quad index pattern — same corner order as the chunk mesher, which is
// what lets `cull: Back` keep the half you can see.

struct Vm {
	mvp: mat4x4<f32>,
	// xyz = light direction in item space (toward the ground);
	// w = direct-light strength.
	light: vec4f,
	// rgb = the frame's direct colour; w is set when the item is
	// fullbright ink (`glows`). Derived from the drawn item rather than
	// plumbed in from the engine, so it is not possible to draw a torch
	// and forget the flag.
	light_color: vec4f,
}

@group(0) @binding(0) var<uniform> vm: Vm;

// RTGI camera light (gi_probe.wgsl's cam_light entry): [0] = omni
// probe ambient at the camera, [1].rgb = sun
// transmittance there — rgb, not one grey lane, because water absorbs
// red an order of magnitude harder than blue and a submerged camera's
// sunlight is tinted. The hand is at the camera by definition, so this IS its
// light — indoors the ambient carries the torch glow and the sun term
// dies, no CPU probe reads anywhere.
struct CamLight {
	ambient: vec4f,
	sun: vec4f,
}
@group(0) @binding(1) var<uniform> cam_light: CamLight;

@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

// Indexed by digblock_core::dir::Dir's discriminants, in their order:
// West(+x), North(+z), South(-z), Bottom(-y), Top(+y), East(-x).
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
	@location(3) @interpolate(flat) ink: f32,
}

// The per-FACE ink bit, riding above the texture layer in the same
// vertex word (viewmodel.rs's `INK_BIT`). A tool is a shaded body with
// one lit part set into it — a glowing bulb, a resonating core — which
// the per-DRAW flag below cannot say: it can only make the whole item
// fullbright, and a fullbright tool has no form left.
const ITEM_INK_BIT = 65536u;

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
	let sh = mix(vec3f(1.0), cam_light.sun.rgb, vm.light.w);
	let diffuse = max(dot(n, -vm.light.xyz), 0.0) * sh;

	var out: VsOut;
	out.pos = vm.mvp * vec4f(pos, 1.0);
	out.uv = uv;
	out.layer = layer & (ITEM_INK_BIT - 1u);
	out.ink = f32((layer & ITEM_INK_BIT) != 0u);
	out.shade = cam_light.ambient.rgb + vm.light_color.rgb * diffuse;
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	// Explicit level: the item is a fixed size on screen and the atlas
	// page has no mips, and Tint's derivative-uniformity rule wants the
	// level spelled out anyway.
	let tex = textureSampleLevel(materials, mat_smp, in.uv, i32(in.layer), 0.0);
	// Fullbright ink is its own light: no ambient, no sun, no diffuse —
	// exactly as chunk.wgsl treats the same block standing in the world, and
	// through the same shared `emissive_ink`, so a torch you are holding and
	// a torch on the floor are one object rendered twice.
	//
	// No medium term on either branch, unlike chunk.wgsl. Two independent
	// reasons: the item is one world unit from the eye, where air
	// transmittance is 1.0 to every digit that matters; and this pass carries
	// no frame uniforms to read a medium from. The honest gap that leaves — a
	// submerged camera's hand takes no water extinction — is not new here.
	//
	// Two ways to be ink and one function to draw it: the whole item
	// (`glows`, a torch), or this face alone (a tool's lit part).
	let ink = vm.light_color.w > 0.5 || in.ink > 0.5;
	let lit = select(tex.rgb * in.shade, emissive_ink(tex.rgb), ink);
	return vec4f(lit, 1.0);
}
