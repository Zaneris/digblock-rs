// Solid chunk shader (water has its own fork — water.wgsl).
// RTGI shading (GI-DESIGN.md): all lighting arrives through the
// half-res GI resolve buffers — probe-traced indirect (sky, bounce,
// torches) plus a per-pixel DDA sun visibility that replaces shadow
// maps entirely. This shader just upsamples bilaterally and combines:
// tex * ao * (light_color * lambert * sunvis + indirect), then fog.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

// The GI resolve outputs: rgba16f indirect rgb + view distance,
// rgba8 sun visibility + face-normal code.
@group(2) @binding(0) var gi_indirect: texture_2d<f32>;
@group(2) @binding(1) var gi_sun: texture_2d<f32>;

// INDIRECT_K (the probe-radiance calibration) arrives from
// frame_common.wgsl — shared with every direct probe gatherer.

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
	@location(2) @interpolate(flat) direct: f32,
	@location(3) fog_dist: f32,
	// Per-vertex ambient occlusion factor, interpolated across the quad
	// — contact darkening the probe grid is too coarse to resolve.
	@location(4) ao: f32,
	// World position — the underwater fog needs the ray endpoint.
	@location(5) world: vec3f,
	// Stage-A emissive (w1 bit 21): 1 = fullbright ink — the lighting
	// terms are skipped entirely (fog still applies).
	@location(6) @interpolate(flat) emissive: u32,
	// Face-normal code (axis*2 + sign) matching gi_resolve's encoding,
	// for the bilateral upsample's normal test.
	@location(7) @interpolate(flat) ncode: u32,
}

// Packed vertex, two words. w0 — geometry: bits 0-7 x, 8-15 y, 16-23 z
// (TWELFTH-block units 0..=192, the sub-block shape grid), 24-26 normal
// index, 27-28 ao, 29-30 corner. w1 — solid material: bits 0-9 texture
// layer, 10-13 uv u (twelfths), 14-17 uv v, 18-20 tint, 21 emissive,
// 22-25 blocklight, 26-29 skylight (the light nibbles are legacy — dead
// to this shader, deleted from the format when the old system goes).
// Twelfths → blocks with the whole-block part split off in INTEGER
// space: full-block lattice vertices (multiples of 12) decode exactly
// even under fast-math (a plain /12.0 becomes an inexact reciprocal
// multiply on some backends and shifts triangle edges by an ulp).
fn twelfths(v: u32) -> f32 {
	return f32(v / 12u) + f32(v % 12u) / 12.0;
}

// `origin` is location 1: the chunk's world origin, arriving from the
// instance-step origin buffer the renderer binds once per pass, selected
// by the draw's first_instance (= the chunk's slot). It replaced a
// dynamic-offset UBO, so nothing here binds per chunk — water.wgsl and
// the depth prepass carry the identical attribute and must be changed
// with this one.
@vertex
fn vs(@location(0) packed: vec2u, @location(1) origin: vec4f) -> VsOut {
	let w0 = packed.x;
	let w1 = packed.y;
	let x = twelfths(w0 & 0xffu);
	let y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	let ni = (w0 >> 24u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + origin.xyz;

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = vec2f(twelfths((w1 >> 10u) & 0xfu), twelfths((w1 >> 14u) & 0xfu));
	o.layer = w1 & 0x3ffu;

	o.direct = max(dot(n, -frame.light_dir.xyz), 0.0);
	o.fog_dist = distance(world, frame.cam_pos.xyz);
	o.world = world;
	// 4-level voxel AO from the mesher (0 → 0.6, 3 → 1.0) folded with the
	// per-block tint step (w1 bits 18-20 → 0.95..1.05); tint is constant
	// across the quad, so the product interpolates exactly.
	let jitter = 1.0 + (f32((w1 >> 18u) & 7u) - 3.5) * (0.10 / 7.0);
	o.ao = (0.6 + 0.4 * f32((w0 >> 27u) & 3u) / 3.0) * jitter;
	o.emissive = (w1 >> 21u) & 1u;
	// NORMALS order (W N S B T E) → gi_resolve's axis*2+sign code.
	var code_of = array<u32, 6>(0u, 4u, 5u, 3u, 2u, 1u);
	o.ncode = code_of[ni];
	return o;
}

// Depth prepass entry: position only, feeding the R32F depth copy the
// GI resolve reconstructs world positions from.
@vertex
fn vs_depth(@location(0) packed: vec2u, @location(1) origin: vec4f) -> @builtin(position) vec4f {
	let w0 = packed.x;
	let x = twelfths(w0 & 0xffu);
	let y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	return frame.view_proj * vec4f(vec3f(x, y, z) + origin.xyz, 1.0);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	// Stage-A emissive: the ink is its own light — no sun, no GI, no AO.
	// Fog still applies (a torch across the valley should haze out like
	// everything else does).
	if i.emissive != 0u {
		return vec4f(apply_fog(tex, i.fog_dist, i.world), frame.misc.x);
	}
	// Bilateral upsample of the half-res GI buffers: the 4 nearest
	// half-res texels, each weighted by view-distance proximity and a
	// face-normal match, so lighting never bleeds across the silhouette
	// or around a corner.
	let dim = vec2i(textureDimensions(gi_indirect)) - 1;
	let base = (vec2i(i.pos.xy) - 1) / 2;
	var acc = vec3f(0.0);
	var sun_acc = 0.0;
	var wsum = 0.0;
	for (var t = 0; t < 4; t++) {
		let c = clamp(base + vec2i(t & 1, t >> 1), vec2i(0), dim);
		let ind = textureLoad(gi_indirect, c, 0);
		let sn = textureLoad(gi_sun, c, 0);
		var w = 1.0 / (0.02 + abs(ind.w - i.fog_dist) / max(i.fog_dist, 1.0));
		if u32(round(sn.g * 255.0)) != i.ncode {
			w *= 0.01;
		}
		acc += ind.rgb * w;
		sun_acc += sn.r * w;
		wsum += w;
	}
	let indirect = acc / wsum;
	// Twilight fades the traced sun exactly as it faded mapped shadows.
	let sh = mix(1.0, sun_acc / wsum, frame.light_dir.w);
	var col = tex * i.ao * (frame.light_color.rgb * i.direct * sh + indirect * INDIRECT_K);
	// Distance fog toward the sky horizon, hiding chunk pop-in.
	col = apply_fog(col, i.fog_dist, i.world);
	return vec4f(col, frame.misc.x);
}
