// Hotbar block icons: screen-space textured quads sampling the block
// material texture array (the same group-1 bind group the scene uses).
// Colors are gamma-space end-to-end — the surface is non-sRGB and the
// materials are Rgba8Unorm, so sampled values pass straight through;
// the per-vertex tint is authored in gamma space like the overlay's.

struct IconU {
	// xy = surface size in pixels; zw unused (uniform padding).
	screen_px: vec4f,
}

@group(0) @binding(0) var<uniform> u: IconU;
@group(1) @binding(0) var materials: texture_2d_array<f32>;
@group(1) @binding(1) var mat_smp: sampler;

struct VsIn {
	// Pixel coordinates, y-down from the top-left corner.
	@location(0) pos: vec2f,
	@location(1) uv: vec2f,
	@location(2) layer: u32,
	// 0xAABBGGRR packed tint (unpack4x8unorm byte order).
	@location(3) tint: u32,
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) tint: vec4f,
}

@vertex
fn vs(in: VsIn) -> VsOut {
	var out: VsOut;
	let ndc = in.pos / u.screen_px.xy * 2.0 - 1.0;
	out.pos = vec4f(ndc.x, -ndc.y, 0.0, 1.0);
	out.uv = in.uv;
	out.layer = in.layer;
	out.tint = unpack4x8unorm(in.tint);
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	let c = textureSample(materials, mat_smp, in.uv, in.layer);
	return vec4f(c.rgb * in.tint.rgb, c.a * in.tint.a);
}
