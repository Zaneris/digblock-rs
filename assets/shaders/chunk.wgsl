// Solid chunk shader (water has its own fork — water.wgsl).
// RTGI shading (GI-DESIGN.md): ALL lighting arrives through the
// half-res GI resolve buffers — probe-traced indirect plus a per-pixel
// DDA sun visibility. This shader upsamples bilaterally and combines:
// tex * (light_color * lambert * sunvis + indirect).
//
// Nothing else LIGHTS here any more. Baked AO and the Stage-B light
// nibbles came out at the M1 demolition — contact darkening is the path
// tracer's job now. Distance and underwater fog came out with them and
// came back at M2 as one participating medium (medium_common.wgsl),
// applied once at the end to emissive ink and lit surfaces alike. The
// per-block tint jitter came out with the same demolition and came back
// at R23 as `block_tint`, which is a material property rather than a
// light and so sits on the albedo (see that function).

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
	// w1 bit 19: take the per-block tint from the cell one BELOW this
	// one. Set on a door's upper half so its two cells wear one tint.
	@location(7) @interpolate(flat) tint_down: u32,
}

// Packed vertex, two words. w0 — geometry: bits 0-7 x, 8-15 y, 16-23 z
// (TWELFTH-block units 0..=192, the sub-block shape grid), 24-26 normal
// index, 27-28 corner, 29-31 free. w1 — solid material: bits 0-9
// texture layer, 10-13 uv u (twelfths), 14-17 uv v, 18 emissive,
// 19 tint-from-below, 20-31 free. Must agree with `mesher::pack` /
// `solid_word1`.
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
	o.tint_down = (w1 >> 19u) & 1u;
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

// lowbias32 avalanche (the mixer highlight.wgsl carries) over a linear
// combination with a distinct odd stride per axis. The avalanche is what
// destroys the linear relation, so there is no plane of correlated cells
// along any axis. Coordinates are WORLD, not chunk-local — the same
// reason the old CPU-side hash took world coords: chunk-local tiles the
// pattern with a 16-block period, which reads as a grid.
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

// Per-block albedo jitter — one value step and one hue step per block
// cell, so a hillside of a single grass id stops reading as wallpaper.
// This is the M1 casualty coming back (R23), and it comes back somewhere
// else: the old one (b6af702) packed a 3-bit hash into vertex bits 29-31
// and folded it into the AO varying, so it needed a vertex bit, a
// re-mesh, and a mesher that knew its chunk's world origin. None of that
// now — `fs` already has the world position and the face code, and the
// block cell is one floor() away.
//
// It multiplies ALBEDO, and that placement is the load-bearing part. The
// à-trous filter and the temporal accumulator only ever see the lighting
// buffers, so a per-block step written into the light is a signal they
// are built to smooth across exactly the edges it lives on — it would
// blur toward its own mean at stride 1 and then get averaged over 512
// frames of history. On the albedo it lands after the bilateral
// upsample, where nothing downstream can filter, blur or accumulate it.
//
// The probes' material albedo stays untinted, so a block's ±8% does not
// ride its own bounce light. That is deliberate: it is below the noise
// floor of a 1-spp gather, and paying for it would mean a per-cell
// lookup inside every march.
//
// `tint_down` (w1 bit 19) shifts the hashed cell one down. A door is one
// object across two cells, and hashing each half separately gave it two
// tints — the leaf read as two planks sawn from different timber, which
// is exactly the material break this function exists to CREATE between
// neighbouring blocks. Anchoring the upper half on the lower one is a
// shift of the anchor, not an exemption: the door still varies from the
// door beside it, it just stops varying down its own middle.
fn block_tint(world: vec3f, ncode: u32, tint_down: u32) -> vec3f {
	// ±half-range of each step, calibrated on the DIG_VAULT stone wall —
	// the flat one-material surface this exists for — against a sweep of
	// 0, ±4%, ±8% and ±12%. ±4% (the old jitter's ×0.95..1.05, near
	// enough) measured invisible at 1:1: mean 1.62/255 over a daylit
	// terrain frame, and a 148-patch sample of the lit ground plane moved
	// its luma stdev 28.61 → 28.65, i.e. inside the noise. ±12% quilts —
	// the wall stops reading as one material. The hue is new and is what
	// keeps a desaturated stone wall from varying in brightness alone,
	// which reads as grime rather than as stone.
	//
	// Note these are ALBEDO half-ranges, so the visible effect scales
	// with the light: the same wall varies most where it is brightest and
	// the jitter all but vanishes in shade. That is correct for a
	// material property and it is the reason the numbers were picked in a
	// dim torch-lit room rather than in daylight.
	const TINT_VALUE = 0.08;
	const TINT_HUE = 0.03;
	// Step half a twelfth INTO the surface along its own normal: on the
	// face's axis `world` sits exactly on a cell boundary, where floor()
	// answers the neighbour for a +face. Half a twelfth is finer than the
	// thinnest sub-box and coarser than nothing, so the nudge lands
	// inside this cell whatever the shape — a slab's top face at y+4/12
	// and a full block's at y+1 both resolve to the cell they belong to.
	let axis = ncode >> 1u;
	let n = vec3f(f32(axis == 0u), f32(axis == 1u), f32(axis == 2u))
		* select(1.0, -1.0, (ncode & 1u) == 1u);
	var cell = vec3i(floor(world - n * (1.0 / 24.0)));
	cell.y -= i32(tint_down);
	let h = hash3(cell);
	// Two disjoint 16-bit fields of an avalanched hash are independent,
	// so one hash buys both steps.
	let v = (f32(h >> 16u) * (1.0 / 65536.0) - 0.5) * 2.0 * TINT_VALUE;
	// R and B in opposition about a fixed G — a rotation about the grey
	// axis, warm one way and cool the other. It moves luminance by
	// (0.299 − 0.114)·d, under a fifth of the hue step, so the two knobs
	// stay independent enough to tune separately.
	let d = (f32(h & 0xffffu) * (1.0 / 65536.0) - 0.5) * 2.0 * TINT_HUE;
	return (1.0 + v) * vec3f(1.0 + d, 1.0, 1.0 - d);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	// Fullbright ink, through ink_common's shared `emissive_ink` — the ONE
	// definition, so the same block reads identically here, in your own
	// hand (viewmodel.wgsl) and in somebody else's (players.wgsl). The
	// gain, why it rides the ink's own luminance, and what caps it all
	// live on that function.
	//
	// This is the ink alone: `glows` and `emits` are independent block
	// properties, so nothing here changes what a torch CASTS, only how
	// the emitter itself reads.
	var col = select(tex, emissive_ink(tex), i.emissive == 1u);
	// Emissive ink is its own light — no sun, no GI. It still travels to
	// the eye through the medium like everything else, which is why the
	// branch narrowed to the lighting instead of returning early.
	if i.emissive == 0u {
		// Bilateral upsample of the reduced-res GI buffers: the 4 nearest
		// GI texels, each weighted by view-distance proximity and a
		// face-normal match, so lighting never bleeds across the
		// silhouette or around a corner.
		//
		// The ratio is a UNIFORM, not the `/ 2` this hard-coded, which is
		// what lets the GI resolve run at a third or a quarter of the
		// render size (settings::GI_SCALE_DIVISORS). It stays an INTEGER
		// divisor for exactly this line: `base` is the top-left of the 2x2
		// GI neighbourhood covering this pixel, and a fractional ratio
		// would make that neighbourhood drift off the texels it weights.
		let gi_div = max(i32(frame.misc.w), 1);
		let dim = vec2i(textureDimensions(gi_indirect)) - 1;
		let base = (vec2i(i.pos.xy) - gi_div / 2) / gi_div;
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
			// The low 8 bits are the face code; the high bits carry the
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
		// Twilight fades the LIGHT, not the shadow. This used to read
		// `mix(vec3f(1.0), visibility, frame.light_dir.w)` — carried over
		// from the shadow-map era, where fading a cascade out meant fading
		// its shadowing toward "lit" because there was no other answer to
		// give. Against a traced visibility it inverts the meaning: as the
		// caster's strength falls to 0 below ~10 degrees of altitude, the
		// blend returns 1.0, and every surface facing the caster takes
		// FULL, UNSHADOWED direct light — through walls.
		//
		// It is not subtle. A sealed stone vault at sunrise lit one wall
		// to 4x the other (86 vs 19 mean), uniformly across the whole
		// face, and the lit wall FLIPPED at the horizon crossing as the
		// caster changed from sun to moon and its azimuth reversed. It
		// survived forcing sun visibility to zero, which is what a
		// visibility term that is being ignored looks like.
		//
		// Strength belongs on the radiance. At full strength this is
		// identical to what it replaced, so only twilight moves.
		let sh = sun_acc / wsum * sun_tr;
		// Per-block jitter belongs to the material, so it multiplies here
		// and not into either lighting term. Emissive ink never reaches
		// this line, which is the whole of its exclusion: an authored
		// flame is not a surface with a colour to vary, and `ink` above
		// feeds a calibrated gain that a ±8% albedo would detune.
		let albedo = tex * block_tint(i.world, i.ncode, i.tint_down);
		col = albedo * (frame.light_color.rgb * frame.light_dir.w * i.direct * sh + indirect);
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
