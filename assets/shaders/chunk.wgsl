// Solid chunk shader (water has its own fork — water.wgsl).
// Gamma-space lighting: sky-tinted ambient + tod-colored direct term,
// shadows attenuate only the direct term (strength fades at twilight),
// distance fog toward the sky horizon color.
// Shadows: 5-tap cross PCF on the near cascade, 1 hard tap on the wide
// cascade, blended over the outer 15% of near coverage (no hard seam).

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

@group(2) @binding(0) var shadow_near: texture_depth_2d;
@group(2) @binding(1) var shadow_wide: texture_depth_2d;
@group(2) @binding(2) var shadow_smp: sampler_comparison;

struct ChunkU {
	origin: vec4f,
}

@group(3) @binding(0) var<uniform> chunk: ChunkU;

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

// Face-axis ambient (same order): tops catch the full sky dome, sides
// less (x and z slightly different so adjoining walls separate tonally),
// bottoms least. Scales only the ambient term — lambert already
// differentiates the direct term by orientation.
const AXIS = array<f32, 6>(0.85, 0.78, 0.78, 0.58, 1.0, 0.85);

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) @interpolate(flat) direct: f32,
	@location(3) sc_near: vec3f,
	@location(4) sc_wide: vec3f,
	@location(5) fog_dist: f32,
	// Per-vertex ambient occlusion factor, interpolated across the quad.
	@location(6) ao: f32,
	// Face-axis ambient factor (constant per face).
	@location(7) @interpolate(flat) axis: f32,
}

// Packed vertex, two words. w0 — geometry: bits 0-5 x, 6-11 y, 12-17 z
// (HALF-block units 0..=32), 18-20 normal index, 21-22 ao, 23-24 corner,
// 25-27 tint. w1 — material: bits 0-9 texture layer.
@vertex
fn vs(@location(0) packed: vec2u) -> VsOut {
	let w0 = packed.x;
	let x = f32(w0 & 0x3fu) * 0.5;
	let y = f32((w0 >> 6u) & 0x3fu) * 0.5;
	let z = f32((w0 >> 12u) & 0x3fu) * 0.5;
	let ni = (w0 >> 18u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + chunk.origin.xyz;

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = vec2f(f32((w0 >> 23u) & 1u), f32((w0 >> 24u) & 1u));
	o.layer = packed.y & 0x3ffu;

	o.direct = max(dot(n, -frame.light_dir.xyz), 0.0);
	o.fog_dist = distance(world, frame.cam_pos.xyz);
	// 4-level voxel AO from the mesher (0 → 0.6, 3 → 1.0) folded with the
	// per-block tint step (bits 25-27 → 0.95..1.05); tint is constant
	// across the quad, so the product interpolates exactly.
	let jitter = 1.0 + (f32((w0 >> 25u) & 7u) - 3.5) * (0.10 / 7.0);
	o.ao = (0.6 + 0.4 * f32((w0 >> 21u) & 3u) / 3.0) * jitter;
	o.axis = AXIS[ni];

	let pn = frame.light_near * vec4f(world, 1.0);
	o.sc_near = vec3f(pn.xy * vec2f(0.5, -0.5) + 0.5, pn.z);
	// Wide-cascade normal offset: push the receiver one wide texel
	// (misc.w, world units) along its face normal before projecting.
	// The wide texels are ~26× the near cascade's, so hardware-PCF
	// neighbor compares self-shadow flat ground without it; unlike a
	// depth bias it is geometry-aware and doesn't erase the shadows of
	// one-block terrain steps.
	let pw = frame.light_wide * vec4f(world + n * frame.misc.w, 1.0);
	o.sc_wide = vec3f(pw.xy * vec2f(0.5, -0.5) + 0.5, pw.z);
	return o;
}

// 5-tap cross PCF; both maps share one resolution, so misc.z is the
// texel size for either. Each tap is itself hardware-2×2 filtered.
fn pcf5(tex: texture_depth_2d, sc: vec3f) -> f32 {
	let px = frame.misc.z;
	var v = textureSampleCompareLevel(tex, shadow_smp, sc.xy, sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(px, 0.0), sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(-px, 0.0), sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(0.0, px), sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(0.0, -px), sc.z);
	return v / 5.0;
}

fn shadow_vis(sc_near: vec3f, sc_wide: vec3f, direct: f32) -> f32 {
	if direct <= 0.0 {
		return 0.0; // faces pointing away from the light are fully shadowed
	}
	// Tiny residual depth epsilon (~0.13 world units) on top of the
	// vertex-side normal offset.
	let wsc = vec3f(sc_wide.xy, sc_wide.z - 0.0001);
	// 0 at the near-map center, 1 at its edge (Chebyshev distance in UV).
	let d = max(abs(sc_near.x - 0.5), abs(sc_near.y - 0.5)) * 2.0;
	if d >= 1.0 {
		return pcf5(shadow_wide, wsc);
	}
	let near = pcf5(shadow_near, sc_near);
	// Blend to wide over the outer 15% so the cascade seam never pops.
	let t = smoothstep(0.85, 1.0, d);
	if t <= 0.0 {
		return near;
	}
	return mix(near, pcf5(shadow_wide, wsc), t);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	var vis = 1.0;
	if frame.misc.y > 0.5 {
		vis = shadow_vis(i.sc_near, i.sc_wide, i.direct);
	}
	// Shadow attenuates only the direct term, scaled by twilight
	// strength; ambient is the guaranteed floor. AO attenuates the whole
	// color — contact darkening the shadow maps can't resolve.
	let sh = mix(1.0, vis, frame.light_dir.w);
	var col = tex * i.ao * (frame.ambient.rgb * i.axis + frame.light_color.rgb * i.direct * sh);
	// Distance fog toward the sky horizon, hiding chunk pop-in.
	let f = clamp((i.fog_dist - frame.fog_color.w) * frame.cam_pos.w, 0.0, 1.0);
	col = mix(col, frame.fog_color.rgb, f * f * (3.0 - 2.0 * f));
	return vec4f(col, frame.misc.x);
}
