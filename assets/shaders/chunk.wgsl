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
	// World position — the underwater fog needs the ray endpoint.
	@location(8) world: vec3f,
	// Stage-A emissive (w1 bit 21): 1 = fullbright ink — the lighting
	// terms are skipped entirely (fog still applies).
	@location(9) @interpolate(flat) emissive: u32,
	// Stage-B light (w1 bits 22-25 block, 26-29 sky), normalized and
	// interpolated — the per-vertex smooth values the mesher sampled from
	// the flood-fill field (LIGHT-DESIGN.md). x = blocklight, y = sky.
	@location(10) light: vec2f,
}

// Torchlight tint: warm, slightly under white so max() lets sunlight win
// on open ground at noon while the torch owns the night and caves.
const TORCH_COLOR = vec3f(1.0, 0.82, 0.55);
// The ambient floor where no sky reaches: caves are nearly black, not
// noon-bright — torchlight is what makes them readable. Full skylight
// (the entire pre-Stage-B surface) multiplies by exactly 1, so open
// ground looks the same as it always did.
const MIN_AMBIENT = 0.02;
// Direct sun is vetoed, never scaled, by the sky nibble: any cell with
// sky access above ~4/15 hands the beam entirely to the shadow map,
// which alone knows the sun's angle. This is NOT Minecraft's model —
// there the column IS sunlight; here it is only occlusion truth.
const BEAM_SKY = 0.27;

// Packed vertex, two words. w0 — geometry: bits 0-7 x, 8-15 y, 16-23 z
// (TWELFTH-block units 0..=192, the sub-block shape grid), 24-26 normal
// index, 27-28 ao, 29-30 corner. w1 — solid material: bits 0-9 texture
// layer, 10-13 uv u (twelfths), 14-17 uv v, 18-20 tint, 21 emissive,
// 22-25 blocklight, 26-29 skylight.
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
// dynamic-offset UBO on group 3, so nothing here binds per chunk any
// more — water.wgsl and shadow.wgsl carry the identical attribute and
// must be changed with this one.
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
	o.axis = AXIS[ni];
	o.emissive = (w1 >> 21u) & 1u;
	o.light = vec2f(f32((w1 >> 22u) & 0xfu), f32((w1 >> 26u) & 0xfu)) / 15.0;

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
	// Stage-A emissive: the ink is its own light — no sun, no shadow,
	// no AO. Fog still applies (a torch across the valley should haze
	// out like everything else does).
	if i.emissive != 0u {
		return vec4f(apply_fog(tex, i.fog_dist, i.world), frame.misc.x);
	}
	var vis = 1.0;
	if frame.misc.y > 0.5 {
		vis = shadow_vis(i.sc_near, i.sc_wide, i.direct);
	}
	// Shadow attenuates only the direct term, scaled by twilight
	// strength; ambient is the guaranteed floor. AO attenuates the whole
	// color — contact darkening the shadow maps can't resolve.
	let sh = mix(1.0, vis, frame.light_dir.w);
	// Skylight's two jobs are split: the ambient sky dome SCALES with
	// the nibble (under a roof you see less sky), but the direct beam
	// belongs to the shadow map — a 10am sun slants in under an
	// open-sided roof, and only the cascade knows that. The nibble
	// merely VETOES direct near sky=0, where the wide cascade can leak
	// sunlight into terrain it never rendered and the field is the
	// ground truth.
	let skyf = mix(MIN_AMBIENT, 1.0, pow(i.light.y, 1.5));
	let beam = smoothstep(0.0, BEAM_SKY, i.light.y);
	let sun = frame.ambient.rgb * i.axis * skyf
		+ frame.light_color.rgb * i.direct * sh * beam;
	// Stage-B torchlight competes with the sun via a componentwise max —
	// never a sum, so noon faces don't blow out beside a torch. The
	// curve steepens the 16-level falloff; AO still darkens contact
	// creases under torchlight.
	let torch = TORCH_COLOR * pow(i.light.x, 1.8);
	var col = tex * i.ao * max(sun, torch);
	// Distance fog toward the sky horizon, hiding chunk pop-in.
	col = apply_fog(col, i.fog_dist, i.world);
	return vec4f(col, frame.misc.x);
}
