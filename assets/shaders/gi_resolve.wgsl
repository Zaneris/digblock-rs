// RTGI resolve: the half-res fullscreen pass that turns probes into per-pixel
// lighting — the whole replacement for the shadow-map system. World position from the R32F depth
// copy, face normal from the prepass code, then indirect = the shared gi_common cascade
// fallthrough, direct = ONE sun-visibility DDA per pixel. The gather core and the group(2) world
// bindings come from gi_common.wgsl; group 1 is empty.
//
// The sun ray is DETERMINISTIC per world point and the penumbra is reconstructed SPATIALLY (the
// SIGMA / FidelityFX pattern): occluder_t x tan(cone) is the physical width and gi_atrous blurs
// that many texels, so N_SUN antialiases the dithered edge and smear is bounded by construction
// (R13, R14). The cone jitter cannot be dropped — without it every shadow ray is an exactly-planar
// rational line aliasing identically in every z-slice — and it hashes WORLD cells, not pixels.
//
// MRT, both rgba32f (gi::GI_ACC_FORMAT), every reader a textureLoad. **The format is
// load-bearing:** both targets hold a running average, and rgba8 and rgba16f rounded 1/N of a
// difference to zero at full history. `history_format_resolves_the_accumulator` pins it to N_MAX.

struct GiResolve {
	inv_view_proj: mat4x4f,
	// xyz cam pos, w window side in blocks.
	cam: vec4f,
	// C0 dims xyz, w its probe spacing in blocks.
	dims0: vec4f,
	// C1 dims xyz, w its (coarser) probe spacing in blocks.
	dims1: vec4f,
	// xyz quantized dir toward the sun, w sun-above-horizon (0/1).
	sun: vec4f,
	// rgb the caster colour, w its twilight strength (the scene pass
	// applies the same value to the sun lane via light_dir.w).
	sun_color: vec4f,
	zenith: vec4f,
	horizon: vec4f,
	// Last frame's view-projection — the codebase's first temporal state.
	// Solids are static geometry under a moving camera, so reprojecting a
	// world position through this is EXACT and no motion-vector buffer is
	// needed.
	prev_vp: mat4x4f,
	// xyz last frame's camera, w history valid (0 on the first frame
	// after a target rebuild, a renderer rebuild, or a sun jump); 2 when
	// the camera has not moved since — exact, from the CPU's matrix
	// compare, where the reprojection round-trip below drifts far from
	// the world origin.
	prev_cam: vec4f,
	// xyz EXACT unquantized dir toward the sun, w above-horizon (0/1).
	sun_exact: vec4f,
	// x frame counter (the sample sequence's rotation — the GATHER ray's;
	// the sun ray's sequence is deliberately frozen at frame 0), y cos of
	// the sun cone's half-angle, z the sky ceiling (R18), w the cost-stub
	// mask (DIG_STUB).
	jitter: vec4f,
	// Screen-history invalidation spheres (xyz center, w radius; w = 0 is
	// an empty slot) — the same ≤4 edit spheres the probe pass consumed
	// this frame, held live for a few sweep periods by gi.rs. A lighting
	// edit (a torch placed, a wall dug) changes radiance on surfaces whose
	// face code and depth never change, and a resting pixel at full
	// history admits that change at 1/n a frame — the light pool would
	// fade in over seconds. Inside a sphere the history cap drops to
	// N_MOVING, the same short-history + a-trous regime motion already
	// uses, so the change lands in a small fraction of that time.
	inval: array<vec4f, 4>,
	// x the gather ray's iteration budget, y the sun ray's (gi/resolve.rs
	// owns the shipped values). Budgets, not capacities: each march's own
	// hard loop bound stays the ceiling, so an over-budget value runs the
	// full march rather than overrunning anything — and a budget can only
	// truncate, which silently un-shadows whatever lies past the cut, so
	// it moves on a MEASURED call, never a hunch. z and w are the two
	// lanes' refresh distances (`refresh_period`): the blocks of view
	// distance per frame of period, indirect then sun; 0 disables a lane's
	// gating (gi/resolve.rs's GI_*_RATE_BLOCKS, or DIG_GI_RATE).
	budget: vec4f,
}

@group(0) @binding(0) var<uniform> rz: GiResolve;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
// The prepass's rasterized face-normal code (chunk.wgsl's `fs_depth`); a
// code past the six real faces means no solid was drawn here.
@group(0) @binding(2) var normal_tex: texture_2d<u32>;
// Last frame's outputs, ping-ponged (never copied).
@group(0) @binding(3) var hist_indirect: texture_2d<f32>;
@group(0) @binding(4) var hist_sun: texture_2d<f32>;
// The emitter list rides the SHARED gather group since R33 (gi_common's
// group(2) binding(5)), along with `resample_light` and the LIGHT_*
// calibration — players run the same estimator now, and sharing the
// declaration is what keeps the two from drifting.

// How many frames a still camera keeps sharpening for. alpha is
// 1/n with n climbing to here, so this constant is what "takes a few
// seconds to look accurate" actually means, stated in frames.
const N_MAX = 512.0;

// How deep an average a MOVING pixel may keep. Below this the residual noise outruns what two
// a-trous iterations can absorb; SVGF-class denoisers keep a similarly short history for the same
// reason. This governs INDIRECT alone — the sun has its own, tighter N_SUN — which is what allows
// it this deep: indirect is smooth, so the extra history buys flicker reduction without visible
// smear, and the walk-regime rows outside the shadow band are the gate.
const N_MOVING = 48.0;

// How deep the SUN average may ever get, still or not. The sun estimate is deterministic per
// pixel, so history buys it nothing but edge antialiasing where the static dither flips, and it
// no longer pays the resampling smear R9/R13 chased. This cap, not the filter, is what bounds
// the smear BY CONSTRUCTION: a history can only be dragged as far as its own length. Indirect
// keeps the full n — its 1-spp gather is stochastic and smooth, so it does not smear.
const N_SUN = 12.0;

// The confidence a DISOCCLUDED pixel starts its history at, primed from the probe cascade blend
// instead of from its own first sample — the blend is the engine's own converged WORLD-SPACE
// answer for that point, wrong only by the contact darkening the per-pixel gather adds, so a
// reveal sharpens instead of "developing" out of 1-spp speckle. Kept under N_SUN and well under
// N_MOVING: a prior must never claim more confidence than a moving pixel is allowed to keep.
const N_PRIOR = 8.0;

// The prime fires only where the sun term is under this — see the gate
// in fs. A sun-lit pixel's indirect is a minor correction the prime can
// only bias; a shadowed pixel's indirect is its whole light, and that
// is where the reveal ghost lives.
const PRIME_SUN_MAX = 0.2;

// How many texels of the surface's own depth gradient the history depth
// tolerance widens to (the taps sit within one texel of the fetch, plus
// the sub-texel phase). Grazing surfaces span more depth per texel than
// the flat relative tolerance allows, and a tolerance that ignores that
// is a flicker band exactly where a face turns edge-on.
const GRAZE_K = 2.0;

// How many multiples of the depth tolerance a same-face tap may miss by and still testify that
// the surface is merely GRAZED, not revealed — the gate that keeps the prime off the near ground
// (see `grazed` in fs). The two populations are far apart: a grazing half-res texel at walking
// pitch misses by a small multiple of the tolerance, an occluder's tap by whole blocks.
const PRIME_MARGIN = 8.0;

// tan of the sun cone's half-angle — must match
// SUN_CONE_COS in gi.rs, pinned by `the_penumbra_knows_the_cone` in
// digblock-assets. occluder_t x this = the physical penumbra width in
// blocks; the screen derivatives turn it into texels per pixel.
const SUN_TAN = 0.0087269;

// The widest penumbra the a-trous can reconstruct, in half-res texels: its two iterations'
// strides, scaled up for wide penumbras by gi_atrous.wgsl's SUN_STRIDE_MAX, reach exactly this
// much combined support. Beyond this a shadow renders sharper than physical.
const PEN_MAX = 12.0;

// How far the reprojected fetch may miss its own texel, in texels, and still be treated as having
// landed on it. An ARITHMETIC floor, not a motion threshold: the fetch round-trips a world
// position out through `inv_view_proj` and back through `prev_view_proj`, and against this world's
// far plane f32 does not return what it was given, so a standing camera measures a drift of a
// fraction of a texel. Clears that drift with margin, under half the slowest real motion.
const TRAVEL_EPS = 0.15;

// --- The adaptive ray budget: a pixel with a long, valid history does not need a fresh sample
// every frame (the probe state machine of DDGI, applied to the screen). Each lane — the indirect
// (NEE + gather) and the sun — traces this frame with probability 1/period, where the period is
// a CONTINUOUS function of view distance: a far march is the long one, and the on-screen footprint
// of its answer shrinks with the same distance, so the near field refreshes every frame and the
// horizon progressively less. No cascades, no steps. A skipped lane passes its history through
// untouched and counts no sample. The roll is per 8x8 TILE, not per pixel, so a wave either
// works or idles — a per-pixel roll would leave the lanes that skipped waiting on the ones that
// did not, and save nothing.

// Below this history length the indirect lane traces every frame regardless of distance: a young
// average is still noise, and skipping only makes it stay noise longer. Above N_SUN (the sun's
// whole history) and below N_MOVING, pinned by `the_rate_floors_nest_the_history_caps`.
const RATE_N_FULL = 16.0;
// The indirect lane's longest period. Its estimate is stochastic and the a-trous hides its
// variance, so it thins out gently: convergence to N_MAX far away takes this many times longer.
const IND_PERIOD_MAX = 4.0;
// The sun lane's longest period. Deterministic per world point, so a resting interior texel
// re-asks a question it already knows the answer to; only the sun's own motion changes it, and
// `the_far_sun_period_stays_inside_the_cone` bounds that motion per period.
const SUN_PERIOD_MAX = 16.0;
// A history tap carrying at least this penumbra width marks an EDGE texel, which traces every
// frame — gi_atrous's PEN_MIN, pinned by `the_edge_gate_knows_the_filters_penumbra_floor`. Two
// reasons the edges are never skipped: the sun history's bounded-smear argument (R13) holds only
// where resampling is lossless, i.e. on a locally constant field; and the a-trous DILATES the
// penumbra lane by max over its ring, so a skipped trace must never re-feed a dilated width.
const SUN_EDGE_PEN = 0.5;
// The smallest difference between two sun taps that counts as a hard edge (see `sun_edge`).
const SUN_EDGE_EPS = 0.002;

// The period a lane traces at, this far from the camera: one frame within `blocks`, then
// growing linearly with distance to `max_period`. `blocks` <= 0 disables the gating (period 1).
fn refresh_period(view_dist: f32, blocks: f32, max_period: f32) -> f32 {
	if blocks <= 0.0 {
		return 1.0;
	}
	return clamp(view_dist / blocks, 1.0, max_period);
}

// One uniform per 8x8 half-res tile per frame, shared by every pixel of the tile, so a lane's
// skip is wave-coherent (see the note above). A different tile hash every frame, so the tiles
// that trace this frame are not the ones that traced last frame.
fn tile_roll(uv: vec2i, frame_i: u32) -> f32 {
	let t = vec2u(uv) >> vec2u(3u);
	var h = (t.x * 0x9E3779B1u) ^ (t.y * 0x85EBCA77u) ^ (frame_i * 0xC2B2AE3Du);
	h ^= h >> 15u;
	h *= 0x2C1B3C6Du;
	h ^= h >> 12u;
	return f32(h >> 8u) * (1.0 / 16777216.0);
}

// R2, the 2-D low-discrepancy sequence (Roberts 2018): the plastic
// constant's reciprocal powers. Behind one function so a spatiotemporal
// blue-noise tile can replace it — STBN is measurably better at 1 spp
// under a temporal filter, but it costs an embedded asset, so it is not
// worth paying for until hashed noise is SHOWN to be insufficient.
const R2 = vec2f(0.7548776662466927, 0.5698402909980532);

fn sample2(px: vec2i, frame_i: f32) -> vec2f {
	// Per-pixel offset (Cranley-Patterson rotation) so neighbours do not
	// share a sample, plus the per-frame advance.
	let h = fract(sin(dot(vec2f(px), vec2f(12.9898, 78.233))) * 43758.5453);
	return fract(R2 * frame_i + vec2f(h, fract(h * 1.6180339887)));
}

// A direction inside a cone of half-angle acos(cos_max) about `dir`.
fn sample_cone(dir: vec3f, u: vec2f, cos_max: f32) -> vec3f {
	let cos_t = mix(1.0, cos_max, u.x);
	let sin_t = sqrt(max(1.0 - cos_t * cos_t, 0.0));
	let phi = 6.2831853 * u.y;
	let up = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(dir.y) > 0.9);
	let tx = normalize(cross(up, dir));
	let ty = cross(dir, tx);
	return normalize(tx * (sin_t * cos(phi)) + ty * (sin_t * sin(phi)) + dir * cos_t);
}

// How far the final-gather ray looks before giving up and asking the cache. It only has to cover
// the range where a per-pixel answer beats a probe blend at C0's spacing — contact darkening, a
// wall's own bounce, a tree's shade; past that the cascades already hold the right answer and
// tracing further only buys variance. The ITERATION budget rides `rz.budget` (gi/resolve.rs), so
// a platform can spend fewer steps over the same reach without a second shader.
const GI_GATHER_T = 32.0;

// A cosine-weighted direction about `n` (Malley's method). Cosine
// sampling is what makes this estimator weightless: the cos theta in the
// rendering equation and the 1/pdf cancel exactly, so the gather is the
// returned radiance itself with no factor to get wrong.
fn cosine_hemi(n: vec3f, u: vec2f) -> vec3f {
	let r = sqrt(u.x);
	let phi = 6.2831853 * u.y;
	let up = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(n.y) > 0.9);
	let tx = normalize(cross(up, n));
	let ty = cross(n, tx);
	return normalize(tx * (r * cos(phi)) + ty * (r * sin(phi)) + n * sqrt(max(1.0 - u.x, 0.0)));
}


// Cost stubs (DIG_STUB; renderer.rs's `stub` module owns the names and the bit numbers, and this
// is the mirror — water.wgsl is the other one). `rz.jitter.w` is a UNIFORM, so every branch below
// is uniform: an un-stubbed run pays nothing and a stubbed one cannot diverge inside a wave.
const STUB_GISUN = 256u;
const STUB_GIGATHER = 512u;
const STUB_GINEE = 1024u;
const STUB_GIFLAT = 2048u;
const STUB_GILOD = 65536u;
const STUB_GISCREEN = 131072u;
const STUB_GISTEADY = 524288u;

fn stubbed(bit: u32) -> bool {
	return (u32(rz.jitter.w) & bit) != 0u;
}

// gi_resolve's normal-code order: axis * 2 + sign bit, indexed by the
// code chunk.wgsl's `code_of` produces. One table, two readers.
const CODE_NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
);

// The face code a normal from CODE_NORMALS carries — the inverse of that table, so a hit point's
// code can be compared against what the prepass stored at its pixel.
fn code_of_normal(n: vec3f) -> u32 {
	let axis = select(select(2u, 1u, n.y != 0.0), 0u, n.x != 0.0);
	return axis * 2u + select(0u, 1u, n[axis] < 0.0);
}

// Last frame's sun visibility at `hp`, read back off the screen (R42): the per-pixel sun ray
// already answered it for every surface the camera saw, so a gather hit on one of them needs no
// second march. Validated exactly as the history fetch is — the face code the prepass stored
// and the distance the point WOULD have been at from the old camera — so a hit behind an occluder
// or off the edge fails and takes the march instead. x < 0 is that failure; else x is the
// visibility with the glass luminance the sun lane folds in, y the water path.
fn screen_sun(hp: vec3f, hn: vec3f) -> vec2f {
	let none = vec2f(-1.0, 0.0);
	if rz.prev_cam.w < 0.5 {
		return none;
	}
	let pc = rz.prev_vp * vec4f(hp, 1.0);
	if pc.w <= 0.0 {
		return none;
	}
	let nd = pc.xy / pc.w;
	let huv = vec2f(nd.x * 0.5 + 0.5, 0.5 - nd.y * 0.5);
	if any(huv < vec2f(0.0)) || any(huv >= vec2f(1.0)) {
		return none;
	}
	let hdim = vec2f(textureDimensions(hist_indirect));
	let t = vec2i(huv * hdim);
	let hs = textureLoad(hist_sun, t, 0);
	if (u32(hs.g) & 255u) != code_of_normal(hn) {
		return none;
	}
	// A secondary point inside a one-sample estimate: the history's own depth tolerance, loosened
	// by the fraction of a cell `hp` stands off its face.
	let want = length(hp - rz.prev_cam.xyz);
	let hi = textureLoad(hist_indirect, t, 0);
	if abs(hi.w - want) > 0.02 * max(want, 1.0) + 0.1 {
		return none;
	}
	return vec2f(hs.r, hs.b);
}

// The tail of the gather path: radiance arriving from `dir` after the ray resolved — the probe
// cascades as the cache that TERMINATES a path, so one ray buys the whole multi-bounce series.
// An emitting cell contributes NOTHING here (R27): `resample_light` already covers every in-range
// emitter with a shadow ray, so a gather hit on one is a double count or flicker inside the
// torch's own shadow. The cell still OCCLUDES, and the NEE ray carries its emission.
fn path_radiance(side: i32, h: GiHit, dir: vec3f) -> vec3f {
	let tr = medium_water_tr(h.water) * h.thru;
	if !h.hit {
		// Open in that direction. NOT the sun disc as well: gi_probe.wgsl
		// adds a sun lobe here because a probe has no separate direct term,
		// but this pass computes direct light explicitly below, and adding
		// it to both halves would double-count the sun on every surface
		// facing it.
		let sky = gi_sky_radiance(dir, rz.zenith.rgb, rz.horizon.rgb);
		if h.sky {
			// PROVEN open — the R18 ceiling test passed. Full sky at any
			// depth: this is what keeps a shaft's floor lit from above.
			return sky * tr;
		}
		// Unproven: the ray merely ran out (GI_GATHER_T, the iteration cap, or the window's
		// edge). At the surface the distinction is moot — the fade saturates. Deep down it was
		// the leak (R32): a ray down a long tunnel exhausts unobstructed and would answer full
		// daylight, so terminate in the probe cache at the exhaust point (the DDGI pattern),
		// mixed toward today's sky across the ramp band so the transition has no seam.
		let ep = vec3f(h.cell) + 0.5;
		let fade = medium_sky_fade(ep.y);
		if fade >= 1.0 {
			return sky * tr;
		}
		let c = gi_cache_at(
			side,
			ep,
			dir,
			rz.cam.xyz,
			rz.dims0,
			rz.dims1,
			rz.zenith.rgb,
			rz.horizon.rgb,
		);
		return mix(c, sky, fade) * tr;
	}
	let alb = gi_albedo(h.cls);
	if gi_emit(h.cls) > 0.0 {
		return vec3f(0.0);
	}
	// The hit face, from the axis the DDA last crossed.
	var hn = vec3f(0.0);
	if h.axis == 0u {
		hn.x = -sign(dir.x);
	} else if h.axis == 1u {
		hn.y = -sign(dir.y);
	} else {
		hn.z = -sign(dir.z);
	}
	let hp = vec3f(h.cell) + 0.5 + hn * 0.6;
	// Irradiance at the hit = the cache (every bounce beyond this one, which
	// is what makes one ray buy the whole series) plus this bounce's own
	// direct sun. Both are deliberately the cheap versions — see
	// gi_cache_at; this is a secondary point inside a temporally filtered
	// one-sample estimate, and refining it costs far more than it is worth.
	var e = gi_cache_at(
		side,
		hp,
		hn,
		rz.cam.xyz,
		rz.dims0,
		rz.dims1,
		rz.zenith.rgb,
		rz.horizon.rgb,
	);
	if rz.sun.w > 0.5 {
		let ndl = max(dot(hn, rz.sun_exact.xyz), 0.0);
		if ndl > 0.0 {
			// Off the screen first (R42); the march is the fallback for a hit the camera did
			// not see. That march spends the GATHER budget, not the primary sun ray's: it
			// shades a point inside a one-sample estimate, so it inherits the cheap tier.
			var ss = vec2f(-1.0, 0.0);
			if !stubbed(STUB_GISCREEN) {
				ss = screen_sun(hp, hn);
			}
			if ss.x >= 0.0 {
				e += rz.sun_color.rgb * ndl * ss.x * medium_water_tr(ss.y);
			} else {
				let sv = gi_sun_vis(side, rz.jitter.z, rz.cam.xz, rz.sun, hp, i32(rz.budget.x));
				e += rz.sun_color.rgb * ndl * sv.vis * medium_water_tr(sv.water) * sv.thru;
			}
		}
	}
	return alb * e * tr;
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) ndc: vec2f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	var o: VsOut;
	let x = f32(i32(vi & 1u) * 4 - 1);
	let y = f32(i32(vi >> 1u) * 4 - 1);
	o.pos = vec4f(x, y, 0.0, 1.0);
	o.ndc = vec2f(x, y);
	return o;
}

struct FsOut {
	@location(0) indirect: vec4f,
	@location(1) sun: vec4f,
}

@fragment
fn fs(i: VsOut) -> FsOut {
	var o: FsOut;
	let dim = vec2f(textureDimensions(depth_tex));
	let uv = vec2i(vec2f(i.ndc.x * 0.5 + 0.5, 0.5 - i.ndc.y * 0.5) * dim);
	let d = textureLoad(depth_tex, clamp(uv, vec2i(0), vec2i(dim) - 1), 0).r;
	// World position and its screen derivatives, taken HERE — before the sky early-out and
	// before the silhouette branch below — because a derivative may only be evaluated in
	// UNIFORM control flow. Tint enforces that and naga does not, so taking them where they
	// are used compiles clean on desktop and makes `gi resolve` an INVALID pipeline in Chrome.
	// The value is free to be used inside a branch; only the call site has to be uniform.
	let clip = vec4f(i.ndc, d, 1.0);
	let wpos4 = rz.inv_view_proj * clip;
	let p = wpos4.xyz / wpos4.w;
	let ddx_p = dpdx(p);
	let ddy_p = dpdy(p);
	if d >= 1.0 {
		// Sky: lit, no water, and a normal code that matches no real face
		// — so a sky texel never wins a weight in the scene's bilateral
		// upsample — plus the shortest possible history (packed with the
		// code) so nothing accumulates against it, and no penumbra.
		o.indirect = vec4f(0.0, 0.0, 0.0, 1e9);
		o.sun = vec4f(1.0, 255.0 + 256.0, 0.0, 0.0);
		return o;
	}
	// EXACT face normal, rasterized by the prepass. This was
	// `normalize(cross(dpdy(p), dpdx(p)))` — a half-res derivative,
	// correct only where the 2x2 quad lies on one face and garbage on
	// every silhouette, where it poisoned the ray origin, the ambient-
	// cube axis weights and the backface term at once.
	var code = textureLoad(normal_tex, clamp(uv, vec2i(0), vec2i(dim) - 1), 0).r;
	if code > 5u {
		// No solid was rasterized at this exact texel even though depth
		// says something is here — a half-res sample landing on the far
		// side of a silhouette. Fall back to the derivative rather than
		// indexing out of the table.
		let g = normalize(cross(ddy_p, ddx_p));
		let a = abs(g);
		if a.x > a.y && a.x > a.z {
			code = select(0u, 1u, g.x < 0.0);
		} else if a.y > a.z {
			code = 2u + select(0u, 1u, g.y < 0.0);
		} else {
			code = 4u + select(0u, 1u, g.z < 0.0);
		}
	}
	let n = CODE_NORMALS[code];
	// The true surface point: each march applies its own GI_SURFACE_EPS,
	// so pushing off here as well is what stacked into more than a whole
	// cell of geometry that could not shadow anything.
	let sp = p;
	let side = i32(rz.cam.w);
	// STUB_GIFLAT: the `wflat` twin. Everything below — three ray marches,
	// the bilinear history fetch and the prime — replaced by a constant, so
	// whatever this saves bounds the whole resolve. It sits AFTER the
	// depth/normal fetch on purpose: the raster and attachment floor is not
	// a term anyone can remove, so it should not be inside the number.
	if stubbed(STUB_GIFLAT) {
		o.indirect = vec4f(0.25, 0.25, 0.25, length(p - rz.cam.xyz));
		o.sun = vec4f(1.0, f32(code) + 256.0, 0.0, 0.0);
		return o;
	}

	let view_dist = length(p - rz.cam.xyz);
	// This texel's world footprint — the penumbra's texel conversion and the grazing history
	// tolerance both read it. Taken here, ahead of the history fetch it now serves.
	let texel_world = max(max(length(ddx_p), length(ddy_p)), 1e-4);
	let lat = sp.xz - rz.cam.xz;
	let gather_max = gi_lateral_max(f32(side));
	let far = dot(lat, lat) > gather_max * gather_max;

	// Reproject into last frame. Exact for static geometry, so this needs no motion vectors —
	// only a validity test. It runs BEFORE the marches now: what it finds decides whether the
	// marches run at all.
	var wsum = 0.0;
	var s_ind = vec3f(0.0);
	var s_sun = 0.0;
	var s_wat = 0.0;
	var s_n = 0.0;
	var s_pen = 0.0;
	// The sun taps' range and widest penumbra — the edge gate's two signals.
	var sun_lo = 1.0;
	var sun_hi = 0.0;
	var pen_max = 0.0;
	var still = false;
	// Did a same-face history tap fail depth only MARGINALLY? Then the surface is still on
	// screen and the miss is a grazing fetch the tolerance clipped, not a reveal. The near
	// ground fails all four taps this way on most walking frames, and priming those pixels from
	// the cache locks the whole near field at cache lighting. A true reveal differs in KIND: its
	// tap held the OCCLUDER, whole blocks nearer, so the miss is many multiples of the tolerance.
	var grazed = false;
	if rz.prev_cam.w > 0.5 {
		let pc = rz.prev_vp * vec4f(p, 1.0);
		if pc.w > 0.0 {
			let nd = pc.xy / pc.w;
			let huv = vec2f(nd.x * 0.5 + 0.5, 0.5 - nd.y * 0.5);
			if all(huv >= vec2f(0.0)) && all(huv <= vec2f(1.0)) {
				let hdim = vec2f(textureDimensions(hist_indirect));
				// BILINEAR, per tap validated. A nearest tap smears the image along the
				// direction of travel: rounding the reprojected position pulls history from
				// a world point up to half a texel away, biased by the sub-texel phase's
				// monotonic drift and invisible to both validations below. Below TRAVEL_EPS
				// the fetch snaps onto the lattice, copying this pixel's own history.
				let hp_raw = huv * hdim - 0.5;
				still = rz.prev_cam.w > 1.5 || length(hp_raw + 0.5 - i.pos.xy) < TRAVEL_EPS;
				let hp = select(hp_raw, i.pos.xy - 0.5, still);
				let b0 = floor(hp);
				let f = hp - b0;
				// Validate on the exact face code AND on the distance the
				// point WOULD have been at from the old camera — comparing
				// stored view distances directly would reject the whole
				// screen the moment the camera moves.
				let want = length(p - rz.prev_cam.xyz);
				let base = 0.02 * max(want, 1.0);
				// Two passes, strict first, so the hot path pays nothing new. Pass 1 exists
				// for one population: a surface seen nearly edge-on spans MORE depth per
				// half-res texel than the flat tolerance allows, and churned at n = 1 as a
				// flicker band. It reruns only on an EMPTY strict pass with a same-face
				// near-miss, widened per SVGF's fwidth(z), capped at PRIME_MARGIN's reveal gate.
				for (var relax = 0; relax < 2; relax++) {
					let tol = select(
						base,
						clamp(GRAZE_K * texel_world, base, PRIME_MARGIN * base),
						relax == 1,
					);
					for (var k = 0; k < 4; k++) {
						let off = vec2f(f32(k & 1), f32(k >> 1u));
						let w = mix(1.0 - f.x, f.x, off.x) * mix(1.0 - f.y, f.y, off.y);
						let t = vec2i(b0 + off);
						if w <= 0.0 || any(t < vec2i(0)) || any(t >= vec2i(hdim)) {
							continue;
						}
						let hi = textureLoad(hist_indirect, t, 0);
						let hs = textureLoad(hist_sun, t, 0);
						// g packs the face code together with the history
						// length — the code validates, the length is what
						// the accumulator reads (sun.a carries the penumbra
						// width).
						let hg = u32(hs.g);
						if (hg & 255u) != code {
							continue;
						}
						let miss = abs(hi.w - want);
						if miss > tol {
							grazed = grazed || miss <= PRIME_MARGIN * base;
							continue;
						}
						wsum += w;
						s_ind += hi.rgb * w;
						s_sun += hs.r * w;
						s_wat += hs.b * w;
						s_n += f32(hg >> 8u) * w;
						s_pen += hs.a * w;
						sun_lo = min(sun_lo, hs.r);
						sun_hi = max(sun_hi, hs.r);
						pen_max = max(pen_max, hs.a);
					}
					if wsum > 0.0 || !grazed {
						break;
					}
				}
			}
		}
	}
	// Renormalize over the taps that survived, so a rejected corner
	// darkens nothing — it just stops voting.
	let have_hist = wsum > 0.0;
	var cap = N_MAX;
	var n_hist = 0.0;
	var in_inval = false;
	if have_hist {
		let inv = 1.0 / wsum;
		s_ind *= inv;
		s_sun *= inv;
		s_wat *= inv;
		s_n *= inv;
		s_pen *= inv;
		// How deep an average this pixel is ALLOWED to keep — two states, not a
		// curve: reprojection is exact for static geometry, so a long history
		// costs only the resampling that carries it, and off-lattice that blur
		// COMPOUNDS at the sub-texel phase's rate. A per-frame RATE was declined
		// (R13); gi_atrous is what makes the short end affordable.
		cap = select(N_MOVING, N_MAX, still);
		// A lighting edit near this surface: the probes are
		// already re-tracing it, so let the screen follow at
		// the short-history rate instead of at 1/N_MAX — and
		// trace every frame while it does.
		for (var si = 0; si < 4; si++) {
			let s = rz.inval[si];
			if s.w > 0.0 && distance(p, s.xyz) < s.w {
				cap = min(cap, N_MOVING);
				in_inval = true;
			}
		}
		n_hist = min(min(s_n + 1.0, N_MAX), cap);
	}

	// THE GATE. Both lanes trace whenever there is no history to carry (a reveal, a graze, the
	// first frame, a sun jump) or an edit sphere covers the point; past that each lane thins
	// out with distance on its own period. The sun lane additionally never skips an EDGE texel
	// — a penumbra or a hard edge among its taps — see SUN_EDGE_PEN. One roll serves both lanes,
	// so with the shipped periods a tile that traces its sun also traces its gather.
	let roll = tile_roll(uv, u32(rz.jitter.x));
	let steady = have_hist && !in_inval && !stubbed(STUB_GISTEADY);
	var ind_period = 1.0;
	if steady && n_hist >= RATE_N_FULL {
		ind_period = refresh_period(view_dist, rz.budget.z, IND_PERIOD_MAX);
		// Moving, the history is resampled off-lattice every frame and
		// that blur compounds until a fresh sample lands (R13): never
		// let it compound more than twice.
		if !still {
			ind_period = min(ind_period, 2.0);
		}
	}
	// Taps that differ by less than the sun target can meaningfully carry are the same field:
	// the a-trous averages equal neighbours and the filtered value may sit an ulp off its inputs.
	let sun_edge = pen_max >= SUN_EDGE_PEN || sun_hi - sun_lo > SUN_EDGE_EPS;
	var sun_period = 1.0;
	if steady && n_hist >= N_SUN && !sun_edge {
		sun_period = refresh_period(view_dist, rz.budget.w, SUN_PERIOD_MAX);
	}
	let trace_ind = roll < 1.0 / ind_period;
	let trace_sun = roll < 1.0 / sun_period;

	// THE FINAL GATHER: one cosine-weighted ray from this pixel, terminated in the probe cache.
	// This is where contact darkening comes from — the cascade blend is smooth by construction
	// and cannot represent it, while a ray travelling a fraction of a block returns a wall's dim
	// bounce instead of sky. **The reach is `gi_lateral_max` and nothing narrower**: past C1's
	// coverage the blend is the analytic sky dome, so anything less turned distant grass BLUE.
	var indirect = vec3f(0.0);
	var nee = vec3f(0.0);
	if trace_ind && far {
		indirect =
			gi_indirect(side, sp, n, rz.cam.xyz, rz.dims0, rz.dims1, rz.zenith.rgb, rz.horizon.rgb, vec2i(8, 4));
	} else if trace_ind {
		// The direct term for local lights — see resample_light. Folded into `indirect` BEFORE
		// the accumulation below, so temporal blending, the edit spheres and the atrous
		// machinery all carry it with no new plumbing. The reservoir's seed is an integer
		// pixel/frame hash — the per-candidate LCG drives the selection, so stratifying the SEED
		// buys nothing — but it must advance, or the visibility Bernoulli freezes into a dither.
		let seed = (u32(uv.x) * 73856093u)
			^ (u32(uv.y) * 19349663u)
			^ (u32(rz.jitter.x) * 83492791u);
		let pick = resample_light(sp, n, seed);
		if pick.valid && !stubbed(STUB_GINEE) {
			// Stop short of the light's own cell (gi_light_margin, shared
			// with the probe nee()) so the whole-cell emitter rule can't
			// self-occlude.
			let reach = max(pick.dist - pick.margin, 0.0);
			let sh = gi_march(
				side, rz.jitter.z, rz.cam.xz, sp, pick.dir, reach, GI_NEE_ITERS, reach, 0u,
			);
			if !sh.hit {
				// `sh.thru` is the glass crossed on the way to the light.
				// The probe pass's nee() multiplies it on the identical
				// estimator, and the two must price one light alike or a
				// lamp behind a stained pane lands white here and tinted
				// in the bounce around it.
				nee = pick.irradiance * medium_water_tr(sh.water) * sh.thru;
			}
		}
		// A second, decorrelated draw from the same sequence — sharing the
		// sun ray's sample would tie the gather direction to the sun cone
		// and correlate two estimates that must average independently.
		//
		// STUB_GIGATHER falls back to the cheap cache lookup, which is
		// exactly the pre-R8 design: the difference between the two is what
		// the final gather costs, and R8 measured it going DOWN.
		if stubbed(STUB_GIGATHER) {
			indirect = gi_cache_at(
				side, sp, n, rz.cam.xyz, rz.dims0, rz.dims1,
				rz.zenith.rgb, rz.horizon.rgb,
			) + nee;
		} else {
			let ui = sample2(uv + vec2i(7919, 104729), rz.jitter.x);
			let rd = cosine_hemi(n, ui);
			// Where the ray stops entering bricks and crosses them by summary (R41): a brick's
			// length of travel — one cosine sample's footprint is the size of its travel — OR
			// sooner, once this texel itself spans more than a block of world: a half-res texel
			// that already covers a brick cannot resolve a bounce at cell scale, so the far
			// field's gather crosses by summary from its first brick (the Lumen/Teardown
			// footprint rule). The footprint is clamped because the derivative is garbage on a
			// silhouette. STUB_GILOD keeps the whole reach at cell level — the pre-R41 gather.
			let coarse_from = select(
				GI_BRICK / clamp(texel_world, 1.0, 8.0),
				GI_GATHER_T,
				stubbed(STUB_GILOD),
			);
			let hit = gi_march(
				side, rz.jitter.z, rz.cam.xz, sp, rd, GI_GATHER_T, i32(rz.budget.x),
				coarse_from, seed ^ 0x5bd1e995u,
			);
			indirect = path_radiance(side, hit, rd) + nee;
		}
	}
	// One sun ray along the EXACT direction, offset inside a cone a little wider than the sun's
	// disc — STATIC and anchored to the WORLD, so the answer is deterministic FOR A WORLD POINT
	// and the penumbra comes from the occluder distance, not time. Screen-anchored, a moving
	// camera re-asks a different fixed question per world point and penumbra bands shimmer.
	// The hash point is pushed INTO the block, off the lattice plane f32 wobble flips cells on.
	let jq = floor((sp - n * 0.0625) * 8.0);
	let jh = fract(sin(dot(jq, vec3f(12.9898, 78.233, 37.719))) * 43758.5453);
	let u = vec2f(jh, fract(jh * 1.6180339887));
	var sdir = rz.sun_exact;
	sdir = vec4f(sample_cone(sdir.xyz, u, rz.jitter.y), sdir.w);
	// STUB_GISUN answers "lit, dry, no occluder" without marching. Its
	// march carries the largest single ray budget in the frame, so this is
	// the term to reach for first.
	var sv: GiSunVis;
	sv.vis = sdir.w;
	sv.water = 0.0;
	sv.dist = 0.0;
	sv.thru = vec3f(1.0);
	if trace_sun && !stubbed(STUB_GISUN) {
		sv = gi_sun_vis(side, rz.jitter.z, rz.cam.xz, sdir, sp, i32(rz.budget.y));
	}
	// Penumbra width: occluder distance x tan(cone half-angle) is the physical width in blocks;
	// dividing by this texel's world footprint converts it to half-res texels with both the
	// projection and the surface's grazing angle accounted for — a grazing texel spans more
	// world, so the same penumbra is fewer texels there. Lit pixels answer 0; gi_atrous spreads
	// the width outward from the shadowed side, which lets the lit half of the edge blur too.
	// A skipped sun lane writes 0: by the edge gate its taps held nothing wider than
	// SUN_EDGE_PEN, and the a-trous re-dilates a neighbour's width over it regardless.
	var pen = 0.0;
	if trace_sun {
		pen = clamp(sv.dist * SUN_TAN / texel_world, 0.0, PEN_MAX);
	}

	// The transmitted sun's CHROMATIC residual rides the rgb indirect lane: acc_sun below
	// carries the glass filter's luminance (the sun target has no colour lanes) and this adds
	// exactly the difference, so the two lanes SUM to the full rgb transmission chunk.wgsl
	// multiplies out and a red pane's pool lands red, not grey. Zero wherever thru is 1,
	// NEGATIVE in the channels the filter absorbs, and it rides the indirect accumulator — so
	// it lands only on a frame the indirect lane traces (an untraced sun lane's thru is 1).
	let thru_lum = dot(sv.thru, vec3f(0.299, 0.587, 0.114));
	if trace_ind && any(sv.thru != vec3f(1.0)) {
		let ndl = clamp(dot(n, rz.sun_exact.xyz), 0.0, 1.0);
		// STRENGTH rides the residual explicitly. The sun lane is a bare
		// visibility fraction that the scene pass scales by
		// `light_color * light_dir.w`; this lane is a finished radiance
		// that nothing scales downstream. Leave it off and the halves
		// stop summing to the transmission below full sun — and since
		// the residual is NEGATIVE in the channels the filter absorbs,
		// a strength under lum(T) drives them past zero and they start
		// subtracting real light out of the twilight.
		indirect += rz.sun_color.rgb
			* (ndl * sv.vis * rz.sun_color.w)
			* medium_water_tr(sv.water)
			* (sv.thru - vec3f(thru_lum));
	}
	var acc = indirect;
	var acc_sun = sv.vis * thru_lum;
	var acc_water = sv.water;
	var n_out = n_hist;
	if have_hist {
		if trace_ind {
			acc = mix(s_ind, indirect, 1.0 / n_hist);
		} else {
			// Carried through untouched: no sample, no count.
			acc = s_ind;
			n_out = min(s_n, cap);
		}
		// The sun blends at its own, much shorter depth — the
		// estimate is deterministic, so this converges to the
		// same answer and only the response time differs. The
		// water path rides the sun's alpha: it is part of the
		// same ray.
		if trace_sun {
			let a_sun = 1.0 / min(n_hist, N_SUN);
			acc_sun = mix(s_sun, sv.vis * thru_lum, a_sun);
			acc_water = mix(s_wat, sv.water, a_sun);
		} else {
			acc_sun = s_sun;
			acc_water = s_wat;
		}
	} else if grazed {
		// The surface never left the screen — the fetch just grazed
		// the depth tolerance. Restart unbiased at n = 1, exactly the
		// old behavior: the sample is noisy but centered, gi_atrous
		// absorbs it, and next frame the fetch usually lands again.
		n_out = 1.0;
	} else {
		// Disocclusion: nothing on screen held this surface last frame. Prime the
		// accumulator from the cascade blend (see N_PRIOR) rather than from the raw
		// gather, but only where INDIRECT is the light: on sun-lit ground the prime's
		// bias measured an order of magnitude worse, while in shadow 1-spp speckle IS
		// the whole signal. The sun channel needs no prime — its ray is deterministic.
		let sun_lit =
			sv.vis * clamp(dot(n, rz.sun_exact.xyz), 0.0, 1.0) * rz.sun_exact.w;
		if sun_lit < PRIME_SUN_MAX {
			// The CHEAP cache lookup, not the full cascade blend. Priming is bursty —
			// a camera swing into a cave reveals a hundred thousand pixels in one frame
			// — and gi_indirect is dozens of loads, which measured a large frame-rate
			// drop at the resolve's end. A prior decaying out in N_PRIOR frames has no
			// business being more exact than the term that terminates every gather path.
			var prior = indirect;
			if !far {
				// Plus the deterministic NEE term (already computed):
				// the cache holds torch light only at probe resolution,
				// and a cave-mouth reveal beside a torch must not start
				// its history at a dimmer pool than the one every
				// following sample will vote for.
				prior = gi_cache_at(
					side, sp, n, rz.cam.xyz, rz.dims0, rz.dims1, rz.zenith.rgb, rz.horizon.rgb
				) + nee;
			}
			n_out = N_PRIOR;
			acc = mix(prior, indirect, 1.0 / N_PRIOR);
		} else {
			n_out = 1.0;
		}
	}
	o.indirect = vec4f(acc, view_dist);
	// r = sun visibility, g = the normal code packed with the history
	// length (round keeps the pack exact in f32; the lost fraction
	// perturbs next frame's alpha by under 1/n^2), b = the sun ray's
	// water path in blocks, a = penumbra width in half-res texels. The
	// path travels
	// rather than a transmittance so the scene shader can apply
	// medium_common's per-channel WATER_SIGMA — the same water the view
	// ray goes through.
	o.sun = vec4f(acc_sun, f32(code) + 256.0 * round(n_out), acc_water, pen);
	return o;
}
