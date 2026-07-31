// Debug overlay: textured/solid quads in pixel space, alpha-blended,
// no depth. Text samples the font atlas; solid quads point at the solid
// glyph's center so one pipeline draws both.

struct OverlayU {
	// xy = surface size in pixels; zw unused (uniform padding).
	screen_px: vec4f,
}

@group(0) @binding(0) var<uniform> u: OverlayU;
@group(0) @binding(1) var font_tex: texture_2d<f32>;
@group(0) @binding(2) var font_smp: sampler;

struct VsIn {
	// Pixel coordinates, y-down from the top-left corner.
	@location(0) pos: vec2f,
	@location(1) uv: vec2f,
	// 0xAABBGGRR packed color (unpack4x8unorm byte order).
	@location(2) color: u32,
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) color: vec4f,
}

@vertex
fn vs(in: VsIn) -> VsOut {
	var out: VsOut;
	let ndc = in.pos / u.screen_px.xy * 2.0 - 1.0;
	out.pos = vec4f(ndc.x, -ndc.y, 0.0, 1.0);
	out.uv = in.uv;
	out.color = unpack4x8unorm(in.color);
	return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	let mask = textureSample(font_tex, font_smp, in.uv).r;
	return vec4f(in.color.rgb, in.color.a * mask);
}
