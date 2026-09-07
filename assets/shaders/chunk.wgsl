// Solid chunk shader (water has its own fork — water.wgsl).
// RTGI shading: ALL lighting arrives through the half-res GI
// resolve buffers — probe-traced indirect plus a per-pixel DDA sun visibility.
// Glossy finishes (finish_common.wgsl) add a sun lobe and a probe-cache reflection.
// Diffuse lighting is upsampled bilaterally;
// the participating medium (medium_common.wgsl) is applied once at the end to
// emissive ink and lit surfaces alike, and `block_tint` is a material property
// that sits on the albedo rather than on the light.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

// The GI resolve outputs, both rgba16f: indirect rgb + view distance,
// and sun visibility + face-normal code + the sun ray's water path +
// history length.
@group(3) @binding(0) var resolved_indirect: texture_2d<f32>;
@group(3) @binding(1) var resolved_sun: texture_2d<f32>;

// Java normal() table order (VertTex.glsl), which is also the face code
// order the mesher packs: W(+x), N(+z), S(-z), B(-y), T(+y), E(-x).
const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
);

struct VsOut {
	// @invariant, and load-bearing: the depth prepass runs `vs_depth` in a different pipeline
	// and this pass depth-tests LessEqual against its result. Once the prepass grew a fragment
	// stage and a varying, the two pipelines stopped optimizing the position math in the same
	// context, and an ulp of divergence fails the test — solids full of view-dependent holes.
	// Both entries here carry the marker, as water.wgsl's do.
	@invariant @builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	// Lambert against the caster, flat per face (the geometry is
	// blocky, so a face has exactly one normal).
	@location(2) @interpolate(flat) direct: f32,
	// Distance to the camera — the bilateral upsample's depth guide.
	@location(3) view_dist: f32,
	// Stage-A emissive (the w1 flag): enables the texture’s emission mask.
	@location(4) @interpolate(flat) emissive: u32,
	// Face-normal code (axis*2 + sign) matching gi_resolve's encoding,
	// for the bilateral upsample's normal test.
	@location(5) @interpolate(flat) ncode: u32,
	// World position — the participating medium's segment endpoint.
	@location(6) world: vec3f,
	// The w1 tint-from-below flag: take the per-block tint from the cell
	// one BELOW this one. Set on a door's upper half so its two cells
	// wear one tint.
	@location(7) @interpolate(flat) tint_down: u32,
	// solid_word1's finish bits: matte, polished stone, metal.
	@location(8) @interpolate(flat) finish: u32,
}

// Packed vertex, two words: w0 carries geometry (x, y and z in TWELFTH-block units, the normal
// index and the corner code), w1 the solid material (texture layer, uv, the emissive and
// tint-from-below flags, and the finish). Must agree with `mesher::pack` / `solid_word1`. Twelfths → blocks with
// the whole-block part split off in INTEGER space, so full-block lattice vertices decode exactly
// even under fast-math — a plain float divide becomes an inexact reciprocal on some backends.
fn twelfths(v: u32) -> f32 {
	return f32(v / 12u) + f32(v % 12u) / 12.0;
}

// `origin` is location 1: the chunk's world origin, arriving from the instance-step origin buffer
// the renderer binds once per pass, selected by the draw's first_instance (= the chunk's slot).
// Nothing here binds per chunk — water.wgsl and the depth prepass carry the identical attribute
// and must be changed with this one.
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
	o.tint_down = (w1 >> 19u) & 1u;
	o.finish = (w1 >> 20u) & 3u;
	// NORMALS order (W N S B T E) → gi_resolve's axis*2+sign code.
	var code_of = array<u32, 6>(0u, 4u, 5u, 3u, 2u, 1u);
	o.ncode = code_of[ni];
	o.world = world;
	return o;
}

// Depth prepass: fills the R32F depth copy the GI resolve reconstructs world positions from, AND
// one R8Uint face-normal code per pixel. The code must be RASTERIZED, not derived —
// `cross(dpdy(p), dpdx(p))` at half res is garbage on every silhouette, and `fs` compares it
// against the scene's EXACT per-vertex code, so the two could never agree there. Rasterizing
// makes it exact AND the same number on both sides, because it comes from this one table.
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

// lowbias32 avalanche (the mixer highlight.wgsl carries) over a linear combination with a distinct
// odd stride per axis. The avalanche destroys the linear relation, so there is no plane of
// correlated cells along any axis. Coordinates are WORLD, not chunk-local: chunk-local tiles the
// pattern with the chunk's own period, which reads as a grid.
fn hash3(c: vec3i) -> u32 {
	var x = bitcast<u32>(c.x) * 0x27d4eb2du
		+ bitcast<u32>(c.y) * 0x9e3779b9u
		+ bitcast<u32>(c.z) * 0x85ebca77u;
	x ^= x >> 16u;
	x *= 0x7feb352du;
	x ^= x >> 15u;
	x *= 0x846ca68bu;
	x ^= x >> 16u;
	return x;
}

// Per-block albedo jitter — one value step and one hue step per cell, so a hillside of a single
// grass id stops reading as wallpaper (R23). It multiplies ALBEDO, and that placement is
// load-bearing: the à-trous filter and the accumulator only see the lighting buffers, so a step
// written into the LIGHT is exactly what they smooth away; the probes' albedo stays untinted so a
// block's jitter never rides its own bounce. `tint_down` anchors a door's two cells on one tint.
fn block_tint(world: vec3f, ncode: u32, tint_down: u32) -> vec3f {
	// ±half-range of each step, calibrated on the DIG_VAULT stone wall — the flat
	// one-material surface this exists for. Much smaller is invisible at 1:1; much larger
	// quilts, and the wall stops reading as one material. The hue step keeps a desaturated
	// wall from varying in brightness alone, which reads as grime. These are ALBEDO
	// half-ranges, so the effect scales with the light — correct for a material property.
	const TINT_VALUE = 0.08;
	const TINT_HUE = 0.03;
	// Step half a twelfth INTO the surface along its own normal: on the face's axis `world`
	// sits exactly on a cell boundary, where floor() answers the neighbour for a +face. Half
	// a twelfth is finer than the thinnest sub-box, so the nudge lands inside this cell
	// whatever the shape — a slab's top face and a full block's both resolve to their own.
	let axis = ncode >> 1u;
	let n = vec3f(f32(axis == 0u), f32(axis == 1u), f32(axis == 2u))
		* select(1.0, -1.0, (ncode & 1u) == 1u);
	var cell = vec3i(floor(world - n * (1.0 / 24.0)));
	cell.y -= i32(tint_down);
	let h = hash3(cell);
	// Two disjoint halves of an avalanched hash are independent, so one
	// hash buys both steps.
	let v = (f32(h >> 16u) * (1.0 / 65536.0) - 0.5) * 2.0 * TINT_VALUE;
	// R and B in opposition about a fixed G — a rotation about the grey
	// axis, warm one way and cool the other. It moves luminance by
	// (0.299 − 0.114)·d, under a fifth of the hue step, so the two knobs
	// stay independent enough to tune separately.
	let d = (f32(h & 0xffffu) * (1.0 / 65536.0) - 0.5) * 2.0 * TINT_HUE;
	return (1.0 + v) * vec3f(1.0 + d, 1.0, 1.0 - d);
}

// The local highlight is the probe cache read along the mirrored view ray: the nearest-probe
// gather the resolve itself falls back to, so a torch or a vein lands as a soft reflection for
// a few atlas taps, with no light scan and no shadow march per fragment. Probes hold
// cosine-weighted irradiance, so the reflection is blurred to about the width polished stone
// wants; metal reads as a soft sheen rather than a mirror.
fn finish_reflection(p: vec3f, n: vec3f, v: vec3f) -> vec3f {
	let r = reflect(-v, n);
	return gi_cache_at(i32(frame.gi_misc.x), p, r, frame.cam_pos.xyz, frame.gi0, frame.gi1,
		frame.zenith.rgb, frame.horizon.rgb);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	// The second atlas bank separates mineral emission from the lit host surface.
	let mask = textureSample(blocks_tex, blocks_smp, i.uv,
		i.layer + textureNumLayers(blocks_tex) / 2u).r;
	let emission = select(0.0, mask, i.emissive == 1u);
	var col = emissive_ink(tex);
	if emission < 1.0 {
		// Bilateral upsample of the reduced-res GI buffers: the 4 nearest GI texels, each
		// weighted by view-distance proximity and a face-normal match, so lighting never
		// bleeds across a silhouette or around a corner. The ratio is a UNIFORM, which is
		// what lets the resolve run at a coarser fraction of the render size, and an INTEGER
		// divisor, or the 2x2 neighbourhood `base` names drifts off the texels it weights.
		let gi_div = max(i32(frame.misc.w), 1);
		let dim = vec2i(textureDimensions(resolved_indirect)) - 1;
		let base = (vec2i(i.pos.xy) - gi_div / 2) / gi_div;
		var acc = vec3f(0.0);
		var sun_acc = 0.0;
		var water_acc = 0.0;
		var wsum = 0.0;
		// Nearest-depth fallback: remember the single closest tap in case no tap agrees on
		// the face. Both codes are the SAME number from `code_of` — the resolve rasterizes
		// it rather than deriving it — so a mismatch means the half-res texel genuinely
		// belongs to another surface, not that the two sides disagree about the same one.
		var best = vec4f(0.0);
		var best_sun = vec4f(0.0);
		var best_d = 1e30;
		var matched = false;
		for (var t = 0; t < 4; t++) {
			let c = clamp(base + vec2i(t & 1, t >> 1), vec2i(0), dim);
			let ind = textureLoad(resolved_indirect, c, 0);
			let sn = textureLoad(resolved_sun, c, 0);
			let dd = abs(ind.w - i.view_dist);
			if dd < best_d {
				best_d = dd;
				best = ind;
				best_sun = sn;
			}
			// The low bits are the face code; the high bits carry the
			// history length (gi_resolve packs them so the penumbra width
			// could have sun.a).
			if (u32(sn.g) & 255u) != i.ncode {
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
			// rather than averaging four wrong answers — the old code gave
			// them all the same negligible weight, which normalizes away and
			// left exactly that average, amplifying whichever was least wrong.
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
		// Twilight fades the LIGHT, not the shadow. Blending visibility toward 1.0 by the
		// caster's strength inverts the meaning against a traced visibility: as strength
		// falls to 0 near the horizon, every surface facing the caster takes FULL,
		// UNSHADOWED direct light, through walls, and the lit wall FLIPS at the horizon
		// crossing. Strength belongs on the radiance.
		let sh = sun_acc / wsum * sun_tr;
		// Per-block jitter belongs to the material, so it multiplies here
		// and not into either lighting term. Emissive ink never reaches
		// this line, which is the whole of its exclusion: an authored
		// flame is not a surface with a colour to vary, and `ink` above
		// feeds a calibrated gain that an albedo jitter would detune.
		let albedo = tex * block_tint(i.world, i.ncode, i.tint_down);
		col = albedo * (frame.light_color.rgb * frame.light_dir.w * i.direct * sh + indirect);
		if i.finish != 0u {
			let axis = i.ncode >> 1u;
			let n = vec3f(f32(axis == 0u), f32(axis == 1u), f32(axis == 2u))
				* select(1.0, -1.0, (i.ncode & 1u) == 1u);
			let v = normalize(frame.cam_pos.xyz - i.world);
			let sun = frame.light_color.rgb * frame.light_dir.w * sh
				* finish_lobe(n, v, -frame.light_dir.xyz, i.finish);
			// Schlick at the view angle weights the reflection the way the lobe's own
			// Fresnel weights the sun, so one f0 per finish prices both terms.
			let local = finish_fresnel(i.finish, dot(n, v))
				* finish_reflection(i.world + n * GI_SURFACE_EPS, n, v);
			col = finish_combine(col, tex, i.finish, sun, local);
		}
	}
	col = mix(col, emissive_ink(tex), emission);
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
