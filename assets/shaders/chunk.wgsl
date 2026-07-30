// Main chunk shader — solid and water pipelines share it.
// Reproduces the DigBlock Java look: gamma-space lighting,
// max(dot(n,-L),0.4)+0.2 diffuse, shadow window [0.6, 1.0],
// 5-tap cross PCF on the near cascade, 1 hard tap on the wide cascade.

struct Frame {
	view_proj: mat4x4f,
	light_near: mat4x4f,
	light_wide: mat4x4f,
	light_dir: vec4f,
	// x = alpha (1.0 solid / 0.7 water)
	// y = shadows enabled (0.0 until the shadow passes exist)
	// z = near shadow map texel size (1.0 / resolution)
	// w = unused
	misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;
@group(1) @binding(2) var shadow_near: texture_depth_2d;
@group(1) @binding(3) var shadow_wide: texture_depth_2d;
@group(1) @binding(4) var shadow_smp: sampler_comparison;

struct ChunkU {
	origin: vec4f,
}

@group(2) @binding(0) var<uniform> chunk: ChunkU;

// Java normal() table order (VertTex.glsl):
// 0=W(+x) 1=N(+z) 2=S(-z) 3=B(-y) 4=T(+y) 5=E(-x)
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
	@location(2) @interpolate(flat) light: f32,
	@location(3) sc_near: vec3f,
	@location(4) sc_wide: vec3f,
	@location(5) @interpolate(flat) facing: f32,
}

// Packed vertex: bits 0-4 x, 5-9 y, 10-14 z (chunk-local 0..=16),
// 15-17 normal index, 18-19 texture layer, 20-21 corner, 22 water-lip.
@vertex
fn vs(@location(0) packed: u32) -> VsOut {
	let x = f32(packed & 0x1fu);
	var y = f32((packed >> 5u) & 0x1fu);
	let z = f32((packed >> 10u) & 0x1fu);
	if ((packed >> 22u) & 1u) == 1u {
		y -= 0.3; // water surface sits at +0.7 (Java ChunkMeshGenerator)
	}
	let n = NORMALS[(packed >> 15u) & 7u];
	let world = vec3f(x, y, z) + chunk.origin.xyz;

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = vec2f(f32((packed >> 20u) & 1u), f32((packed >> 21u) & 1u));
	o.layer = (packed >> 18u) & 3u;

	let ndl = dot(n, -frame.light_dir.xyz);
	o.light = max(ndl, 0.4) + 0.2; // exact Java formula (VertTex.glsl)
	o.facing = ndl;

	let pn = frame.light_near * vec4f(world, 1.0);
	o.sc_near = vec3f(pn.xy * vec2f(0.5, -0.5) + 0.5, pn.z);
	let pw = frame.light_wide * vec4f(world, 1.0);
	o.sc_wide = vec3f(pw.xy * vec2f(0.5, -0.5) + 0.5, pw.z);
	return o;
}

fn shadow_vis(sc_near: vec3f, sc_wide: vec3f, facing: f32) -> f32 {
	if facing <= 0.0 {
		return 0.0; // faces pointing away from the sun are fully shadowed
	}
	if all(sc_near.xy > vec2f(0.0)) && all(sc_near.xy < vec2f(1.0)) {
		let px = frame.misc.z;
		var v = 0.0;
		v += textureSampleCompare(shadow_near, shadow_smp, sc_near.xy, sc_near.z);
		v += textureSampleCompare(shadow_near, shadow_smp, sc_near.xy + vec2f(px, 0.0), sc_near.z);
		v += textureSampleCompare(shadow_near, shadow_smp, sc_near.xy + vec2f(-px, 0.0), sc_near.z);
		v += textureSampleCompare(shadow_near, shadow_smp, sc_near.xy + vec2f(0.0, px), sc_near.z);
		v += textureSampleCompare(shadow_near, shadow_smp, sc_near.xy + vec2f(0.0, -px), sc_near.z);
		return v / 5.0;
	}
	return textureSampleCompare(shadow_wide, shadow_smp, sc_wide.xy, sc_wide.z);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	var shadow = 1.0;
	if frame.misc.y > 0.5 {
		shadow = 0.6 + 0.4 * shadow_vis(i.sc_near, i.sc_wide, i.facing);
	}
	return vec4f(i.light * shadow * tex, frame.misc.x);
}
