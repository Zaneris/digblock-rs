// Solid chunk shader (water has its own fork — water.wgsl).
// RTGI shading (GI-DESIGN.md): ALL lighting arrives through the
// half-res GI resolve buffers — probe-traced indirect plus a per-pixel
// DDA sun visibility. This shader upsamples bilaterally and combines:
// tex * (light_color * lambert * sunvis + indirect).
//
// Nothing else shades here any more. Baked AO, the Stage-B light
// nibbles and the per-block tint jitter came out at the M1 demolition —
// contact darkening is the path tracer's job now. Distance and
// underwater fog came out with them and came back at M2 as one
// participating medium (medium_common.wgsl), applied once at the end to
// emissive ink and lit surfaces alike.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

// The GI resolve outputs, both rgba16f: indirect rgb + view distance,
// and sun visibility + face-normal code + the sun ray's water path +
// history length.
@group(2) @binding(0) var gi_indirect: texture_2d<f32>;
@group(2) @binding(1) var gi_sun: texture_2d<f32>;

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
	// @invariant, and load-bearing: the depth prepass runs `vs_depth` in
	// a different pipeline and this pass depth-tests LessEqual against
	// its result. Once the prepass grew a fragment stage and a varying
	// (the face-normal code), the two pipelines no longer optimize the
	// position math in the same context, and an ulp of divergence fails
	// the test — solids full of view-dependent holes. water.wgsl learned
	// this the hard way; both entries here carry the marker.
	@invariant @builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	// Lambert against the caster, flat per face (the geometry is
	// blocky, so a face has exactly one normal).
	@location(2) @interpolate(flat) direct: f32,
	// Distance to the camera — the bilateral upsample's depth guide.
	@location(3) view_dist: f32,
	// Stage-A emissive (w1 bit 18): 1 = fullbright ink — the lighting
	// terms are skipped entirely.
	@location(4) @interpolate(flat) emissive: u32,
	// Face-normal code (axis*2 + sign) matching gi_resolve's encoding,
	// for the bilateral upsample's normal test.
	@location(5) @interpolate(flat) ncode: u32,
	// World position — the participating medium's segment endpoint.
	@location(6) world: vec3f,
}

// Packed vertex, two words. w0 — geometry: bits 0-7 x, 8-15 y, 16-23 z
// (TWELFTH-block units 0..=192, the sub-block shape grid), 24-26 normal
// index, 27-28 corner, 29-31 free. w1 — solid material: bits 0-9
// texture layer, 10-13 uv u (twelfths), 14-17 uv v, 18 emissive,
// 19-31 free. Must agree with `mesher::pack` / `solid_word1`.
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
	o.view_dist = distance(world, frame.cam_pos.xyz);
	o.emissive = (w1 >> 18u) & 1u;
	// NORMALS order (W N S B T E) → gi_resolve's axis*2+sign code.
	var code_of = array<u32, 6>(0u, 4u, 5u, 3u, 2u, 1u);
	o.ncode = code_of[ni];
	o.world = world;
	return o;
}

// Depth prepass: fills the R32F depth copy the GI resolve reconstructs
// world positions from, AND — since M3 — one R8Uint face-normal code
// per pixel.
//
// That second output is what fixed two bugs at once. The resolve used to
// take its normal from `cross(dpdy(p), dpdx(p))` at half res, which is
// garbage on every silhouette and poisoned the ray origin, the ambient-
// cube axis weights and the backface term. It then wrote that derived
// code into the sun buffer, where `fs` below compares it against the
// scene's EXACT per-vertex code — the two cannot agree at a silhouette,
// so all four bilateral taps collapsed to w *= 0.01 and `acc / wsum`
// amplified whichever tap was least wrong. Those were the horizontal
// ticks along every edge.
//
// Rasterizing the code makes it exact AND makes it the same number on
// both sides of the comparison, because it comes from this one table.
struct DepthOut {
	@invariant @builtin(position) pos: vec4f,
	@location(0) @interpolate(flat) ncode: u32,
}

@vertex
fn vs_depth(@location(0) packed: vec2u, @location(1) origin: vec4f) -> DepthOut {
	let w0 = packed.x;
	let x = twelfths(w0 & 0xffu);
	let y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	var code_of = array<u32, 6>(0u, 4u, 5u, 3u, 2u, 1u);
	var o: DepthOut;
	o.pos = frame.view_proj * vec4f(vec3f(x, y, z) + origin.xyz, 1.0);
	o.ncode = code_of[(w0 >> 24u) & 7u];
	return o;
}

@fragment
fn fs_depth(i: DepthOut) -> @location(0) u32 {
	return i.ncode;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	var col = tex;
	// Emissive ink is its own light — no sun, no GI. It still travels to
	// the eye through the medium like everything else, which is why the
	// branch narrowed to the lighting instead of returning early.
	if i.emissive == 0u {
		// Bilateral upsample of the half-res GI buffers: the 4 nearest
		// half-res texels, each weighted by view-distance proximity and a
		// face-normal match, so lighting never bleeds across the
		// silhouette or around a corner.
		let dim = vec2i(textureDimensions(gi_indirect)) - 1;
		let base = (vec2i(i.pos.xy) - 1) / 2;
		var acc = vec3f(0.0);
		var sun_acc = 0.0;
		var water_acc = 0.0;
		var wsum = 0.0;
		// Nearest-depth fallback: remember the single closest tap in case
		// no tap agrees on the face. Both codes are now the SAME number
		// from chunk.wgsl's `code_of` — the resolve rasterizes it rather
		// than deriving it — so a mismatch means the half-res texel
		// genuinely belongs to another surface, not that the two sides
		// disagree about the same one.
		var best = vec4f(0.0);
		var best_sun = vec4f(0.0);
		var best_d = 1e30;
		var matched = false;
		for (var t = 0; t < 4; t++) {
			let c = clamp(base + vec2i(t & 1, t >> 1), vec2i(0), dim);
			let ind = textureLoad(gi_indirect, c, 0);
			let sn = textureLoad(gi_sun, c, 0);
			let dd = abs(ind.w - i.view_dist);
			if dd < best_d {
				best_d = dd;
				best = ind;
				best_sun = sn;
			}
			if u32(sn.g) != i.ncode {
				continue;
			}
			matched = true;
			let w = 1.0 / (0.02 + dd / max(i.view_dist, 1.0));
			acc += ind.rgb * w;
			sun_acc += sn.r * w;
			water_acc += sn.b * w;
			wsum += w;
		}
		if !matched {
			// Every tap is another surface. Take the nearest one whole
			// rather than averaging four wrong answers — the old code
			// weighted them 0.01 each, which normalizes away and left
			// exactly that average, amplifying whichever was least wrong.
			acc = best.rgb;
			sun_acc = best_sun.r;
			water_acc = best_sun.b;
			wsum = 1.0;
		}
		let indirect = acc / wsum;
		// The sun ray's own water column, attenuated by the SAME sigma
		// the view ray uses — so sunlight reaching a submerged surface is
		// red-starved, not just dim.
		let sun_tr = medium_water_tr(water_acc / wsum);
		// Twilight fades the traced sun the way it faded mapped shadows.
		let sh = mix(vec3f(1.0), sun_acc / wsum * sun_tr, frame.light_dir.w);
		col = tex * (frame.light_color.rgb * i.direct * sh + indirect);
	}
	// The medium, from this surface to the eye. In-scatter is `sky_base`
	// itself, so at distance the surface fades into exactly the sky pixel
	// behind it — including the streaming rim, which is why the world's
	// edge stops being visible rather than being hidden.
	let ray = normalize(i.world - frame.cam_pos.xyz);
	let sky = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	let lit = medium_view(
		col,
		frame.cam_pos.xyz,
		i.world,
		sky,
		frame.zenith.rgb,
		frame.water_misc,
		frame.misc.y,
	);
	return vec4f(lit, frame.misc.x);
}
