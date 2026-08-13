// RTGI resolve (GI-DESIGN.md): the half-res fullscreen pass that turns
// probes into per-pixel lighting. World position from the R32F depth
// copy, face normal from derivatives (exact on a blocky world), then:
// indirect = the shared gi_common cascade fallthrough (8 surrounding
// C0 probes, trilinear x backface x exact short-DDA occlusion through
// the voxel window — the blocky-world replacement for DDGI's
// visibility maps — then C1, then the analytic dome); direct = ONE
// sun-visibility DDA per pixel — the entire replacement for the
// shadow-map system.
//
// M4 made that sun ray STOCHASTIC and averaged it through a reprojected
// history; the smear fixes that followed (R9, R13) established that the
// screen history's RESAMPLING is the one artifact no validation can
// remove — repeated off-lattice bilinear fetches drag the accumulated
// image along the direction of travel, and the only signal sharp enough
// to show it is this pass's sun visibility. So the sun ray is now
// DETERMINISTIC per world point and the penumbra is reconstructed
// SPATIALLY (the SIGMA / FidelityFX-shadow pattern): the cone jitter
// still exists but is STATIC, hashed from eighth-block world cells — so
// a world point asks the same question every frame from every camera,
// and its history has nothing stochastic left to average (per PIXEL was
// tried first and shimmered under rotation — see the jitter's comment in
// fs below). `gi_sun_vis` returns the occluder
// distance, occluder_t x tan(cone) is the physical penumbra width, and
// gi_atrous blurs exactly that many texels. Sun history is then capped
// at N_SUN frames (see below) purely to antialias the dithered edge, and
// the smear is bounded by construction.
//
// The jitter cannot be dropped outright. Quantization exists so PROBES
// hold still between updates; on a per-pixel ray it is bug 1 — sun.rs
// pins the caster to dir.z == 0 and gi.rs snaps it to 1/38 steps, so
// every shadow ray is an exactly-planar, exactly-rational line through a
// cubic lattice, which aliases identically in every z-slice and
// periodically in x/y. That is the streaking, and it is pinned to the
// WORLD, not the screen. A per-pixel cone offset breaks the rational
// slope pixel by pixel; making it static in TIME is what turns the old
// temporal noise into a fixed dither the spatial filter resolves. The
// offset also self-scales as a penumbra sampler: at 1 block of occluder
// distance the direction spread displaces the tested edge by well under
// a texel (contact shadows stay hard), at 20 blocks it dithers exactly
// the band the true penumbra spans.
//
// MRT, both rgba32f (gi::GI_ACC_FORMAT): indirect rgb + view distance,
// and sun visibility + (face-normal code packed with the history length,
// `code + 256*n` — WebGPU's baseline 32-byte attachment budget is
// exactly spent, so the penumbra width earned its channel by the pack,
// and 255 + 256*512 sits well inside f32's 24-bit mantissa; every
// reader is a textureLoad, nothing ever hardware-filters these) +
// sun-ray water path + penumbra width in half-res texels.
//
// BOTH targets hold a running average, and that is what dictates the
// format. rgba8 was first and could not: at a history length of 512 each
// frame contributes 1/512 of a difference, and in 8 bits that rounds to
// zero, so the average stopped moving. rgba16f replaced it and failed the
// SAME way — half-float carries a 10-bit mantissa, so near a stored value
// of 1.0 the ulp is ~1e-3 and `hi + (indirect - hi)/512` rounds away
// unless the difference exceeds about 0.25. A quarter of the signal range
// never converged: every pixel froze where it stood when the camera
// stopped, leaving a stale pattern locked to the path the player walked,
// permanently. That is what "weirdness that moves with you" was.
//
// It survived because every capture in this engine was a still, which is
// exactly when a temporal artifact has already converged away. `--fly` vs
// `--warp` (GI-DESIGN.md "Measuring motion") is what finally photographed
// it: mean 0.988 over 10.4% of pixels after 25 SECONDS of stillness, gone
// to 0.200 / 1.4% on f32 — the same floor as switching accumulation off
// entirely. Frame-to-frame stability at rest improved 7x with it.
//
// The rule is therefore not "use a float format" but that the mantissa has
// to carry the smallest increment the history length asks for: alpha of
// 1/N stalls once the residual falls under about N ulps, which f32 puts at
// ~1e-5 here — far under one 8-bit output LSB. `history_format_resolves_
// the_accumulator` in gi.rs pins N_MAX against the format so raising one
// without the other fails the build rather than the picture.
//
// The gather core and the group(2) world bindings come from
// gi_common.wgsl (prepended by digblock-assets); group 1 is empty.

struct GiResolve {
	inv_view_proj: mat4x4f,
	// xyz cam pos, w window side in blocks.
	cam: vec4f,
	// C0 dims xyz, w spacing (2).
	dims0: vec4f,
	// C1 dims xyz, w spacing (8).
	dims1: vec4f,
	// xyz quantized dir toward the sun, w sun-above-horizon (0/1).
	sun: vec4f,
	sun_color: vec4f,
	zenith: vec4f,
	horizon: vec4f,
	// Last frame's view-projection — the codebase's first temporal state.
	// Solids are static geometry under a moving camera, so reprojecting a
	// world position through this is EXACT and no motion-vector buffer is
	// needed.
	prev_vp: mat4x4f,
	// xyz last frame's camera, w history valid (0 on the first frame
	// after a target rebuild, a renderer rebuild, or a sun jump).
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
	// face code and depth never change, and a resting pixel at n = 512
	// admits that change at 1/512 a frame — the light pool would fade in
	// over seconds. Inside a sphere the history cap drops to N_MOVING, the
	// same short-history + a-trous regime motion already uses, so the
	// change lands in ~24 frames.
	inval: array<vec4f, 4>,
}

@group(0) @binding(0) var<uniform> rz: GiResolve;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
// The prepass's rasterized face-normal code (chunk.wgsl's `fs_depth`).
// 255 = no solid was drawn here.
@group(0) @binding(2) var normal_tex: texture_2d<u32>;
// Last frame's outputs, ping-ponged (never copied).
@group(0) @binding(3) var hist_indirect: texture_2d<f32>;
@group(0) @binding(4) var hist_sun: texture_2d<f32>;
// The emitter list (R20) — the same count-prefixed buffer the probe
// pass NEE-samples, bound here for the per-pixel direct term. lights[0].x
// is the count as a bitcast u32; entries are (xyz world center, w class).
@group(0) @binding(5) var<storage, read> lights: array<vec4f>;

// How many frames a still camera keeps sharpening for. alpha is
// 1/n with n climbing to here, so this is what "takes a few seconds to
// look accurate" actually means: ~5 s at 100 fps.
const N_MAX = 512.0;

// How deep an average a MOVING pixel may keep. Below this the residual
// noise outruns what two a-trous iterations can absorb; SVGF-class
// denoisers live at 4-32 for the same reason. Since the deterministic
// sun this cap governs INDIRECT alone (the sun has its own, tighter
// N_SUN), which is what finally allowed raising it: pre-R14 the sun
// smear scaled with this number and pinned it at 24, while the 1-spp
// gather's moving flicker — very visible in a torch-lit interior, where
// indirect is the only light — wanted it deeper. Indirect is smooth, so
// the extra history buys flicker reduction without visible smear; the
// walk-regime rows outside the shadow band are the gate.
const N_MOVING = 48.0;

// How deep the SUN average may ever get, still or not. The sun estimate
// is deterministic per pixel now, so history buys it nothing but edge
// antialiasing where the static dither flips — a dozen frames is plenty —
// and everything a long history costs (the resampling smear R9/R13
// chased) it no longer pays. This cap, not the filter, is what makes the
// smear bounded BY CONSTRUCTION: a 12-frame history can only be dragged
// 12 frames far. Indirect keeps the full n — its 1-spp gather is
// genuinely stochastic and does not smear, being smooth.
const N_SUN = 12.0;

// The confidence a DISOCCLUDED pixel starts its history at, primed from
// the probe cascade blend instead of from its own first sample.
//
// A pixel with no valid history used to restart at n = 1: the raw 1-spp
// gather, which in an indirectly-lit interior is speckle — a cave floor's
// single ray mostly finds dark walls and occasionally the bright mouth —
// so every surface the camera revealed (a cave rim, the strip a strafe
// pulls in at the screen edge) visibly "developed" for a half second
// while n climbed. The cascade blend is the engine's own converged
// WORLD-SPACE answer for that exact point — smooth, wrong only by the
// contact darkening the per-pixel gather exists to add — so the reveal
// starts from lighting that is already right at probe resolution and
// sharpens, instead of starting from noise and converging. This is the
// irradiance-cache disocclusion pattern (GI-1.0's hash-grid fallback,
// Lumen's world-probe fallback, NVIDIA's cache-mix formulation), applied
// as a PRIOR: acc starts at the blend with this much virtual history, so
// the prime decays out at the accumulator's own 1/n rate (~N_PRIOR/n
// residual weight) with no per-frame blend machinery.
//
// Kept under N_SUN and well under N_MOVING: the prior must never claim
// more confidence than a moving pixel is allowed to keep, or the real
// samples that follow can't correct it at the rate motion demands.
const N_PRIOR = 8.0;

// The prime fires only where the sun term is under this — see the gate
// in fs. A sun-lit pixel's indirect is a minor correction the prime can
// only bias; a shadowed pixel's indirect is its whole light, and that
// is where the reveal ghost lives.
const PRIME_SUN_MAX = 0.2;

// How many texels of the surface's own depth gradient the history depth
// tolerance widens to (the taps sit within one texel of the fetch, plus
// the sub-texel phase). Grazing surfaces span more than the flat 2% per
// texel, and a tolerance that ignores that is a flicker band exactly
// where a face turns edge-on.
const GRAZE_K = 2.0;

// How many multiples of the depth tolerance a same-face tap may miss by
// and still testify that the surface is merely GRAZED, not revealed —
// the gate that keeps the prime off the near ground (see `grazed` in
// fs). Between the two populations there is a wide gap: a grazing
// half-res texel at walking pitch misses by a few tolerances, an
// occluder's tap misses by whole blocks (tens of tolerances at any
// distance the near field cares about).
const PRIME_MARGIN = 8.0;

// tan of the sun cone's half-angle (0.5 degrees) — must match
// SUN_CONE_COS in gi.rs, pinned by `the_penumbra_knows_the_cone` in
// digblock-assets. occluder_t x this = the physical penumbra width in
// blocks; the screen derivatives turn it into texels per pixel.
const SUN_TAN = 0.0087269;

// The widest penumbra the a-trous can reconstruct, in half-res texels:
// its two iterations reach stride 1 + 2 taps scaled up to 4x for wide
// penumbras (gi_atrous.wgsl), 12 texels of combined support. Beyond
// this a shadow renders sharper than physical — which the stochastic
// version could not represent either at a 24-frame moving history.
const PEN_MAX = 12.0;

// How far the reprojected fetch may miss its own texel, in texels, and
// still be treated as having landed on it.
//
// This is an ARITHMETIC floor, not a motion threshold. The fetch round-trips
// a world position out through `inv_view_proj` and back through
// `prev_view_proj`; with a 640-block far plane f32 does not return what it
// was given, and a standing camera measures a drift of 0.055-0.11 texels
// (median 0.06 near, 0.07 far, on --walk --warp). GI-DESIGN.md used to say
// travel was measurably zero at rest. It is not, and believing it cost twice
// over: the cap below reads arithmetic as movement, and the bilinear fetch
// mixes in neighbours a camera that has not moved has no business touching —
// at n = 512 that steady-state blur is several texels wide, which is most of
// why the resting far field was blotchy.
//
// 0.15 clears the measured drift with margin and is under half the slowest
// real motion in the scene (the far field's 0.36 median on --walk).
const TRAVEL_EPS = 0.15;

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

// How far the final-gather ray looks before giving up and asking the
// cache. It only has to cover the range where a per-pixel answer beats a
// 2-block-spaced probe blend — contact darkening, a wall's own bounce, a
// tree's shade. Past that the cascades already hold the right answer and
// tracing further would just buy variance.
const GI_GATHER_T = 32.0;
const GI_GATHER_ITERS = 96;

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

// Local-light calibration, the pinned twins of gi_probe.wgsl's — the
// resolve's deterministic NEE ray and the probes' stochastic one must
// price the same torch identically or the direct pool and its bounce
// drift apart (`resolve_and_probe_agree_on_light_calibration`).
const LIGHT_RANGE = 24.0;
// 0.75 since R26 — the retune's measured table lives on gi_probe's twin.
const LIGHT_GAIN = 0.75;
// Where the point model's near field stops being taken literally, in
// blocks. Beyond it the falloff is exactly 1/d^2 and NOTHING about the
// pool changes; inside, irradiance holds at 1/LIGHT_NEAR^2.
//
// A voxel emitter is not a point, and a point's 1/d^2 diverges: the
// support block's top face sits half a block from the torch's centre, so
// it took `emit * LIGHT_GAIN` whole — 8.4 before the surface albedo,
// against a display whose white is 1.0. The result was a hard-edged
// ellipse of saturated orange under every torch, texture detail gone
// inside it (measured: the red channel clipped over 3% of the vault
// frame, with not one pixel actually white — the giveaway that this is
// per-channel saturation, not a bright surface).
//
// The clamp already existed at 1.0 for the divide-by-nothing case; this
// only widens it. The number is a DISPLAY fact, not a physical one, and
// this is explicitly a stopgap: the honest fix is an HDR scene target
// and one tonemap on the composite, which would roll the highlight off
// instead of flattening it, and is on the backlog.
//
// 2.0 is a measured trade, not the extreme. Clipping in the vault pool
// band goes 2.96% -> 0.40% here and bottoms out at 0.13% by 2.5 — but
// 0.13% IS the torch's own emissive sprite, which no falloff can touch
// (2.5 and 3.0 measure identically), and buying that last 0.27% costs
// another 14% of pool brightness for a pool that already reads faint.
// What is left at 2.0 is a small hot core at the flame's base, which is
// what a flame should do; what went away is the flat, hard-edged
// ellipse with no floor texture in it. Rooms are unaffected either way:
// the floor and wall bands away from the torch move under 0.5%, because
// this cannot touch anything past the radius.
const LIGHT_NEAR = 2.0;
// The NEE shadow ray's iteration cap. Bounded by max_t <= LIGHT_RANGE
// anyway; 64 covers the worst dense diagonal with margin, because a
// tight cap silently UN-shadows whatever lies past it — the bug
// gi_sun_vis shipped once at 160 iterations against a 320-block window.
const GI_NEE_ITERS = 64;
// "No sampled light" sentinel for path_radiance's suppression compare:
// no gather hit can land here (cells live in y 0..255).
const NEE_NO_CELL = vec3i(1073741824);

// Cost stubs (DIG_STUB; renderer.rs's `stub` module owns the names and
// the bit numbers, and this is the mirror — water.wgsl is the other one).
// `rz.jitter.w` is a UNIFORM, so every branch below is uniform: an
// un-stubbed run pays nothing and a stubbed one cannot diverge inside a
// wave. Water had this facility and the resolve did not, which is why the
// frame's most expensive pass has never had a single term attributed.
const STUB_GISUN = 256u;
const STUB_GIGATHER = 512u;
const STUB_GINEE = 1024u;
const STUB_GIFLAT = 2048u;

fn stubbed(bit: u32) -> bool {
	return (u32(rz.jitter.w) & bit) != 0u;
}

// The per-pixel direct term for local lights (R20): ONE deterministic
// shadow ray to the DOMINANT emitter, the same move R14 made for the
// sun applied to torches (Q2RTX / GI-1.0 / RTXDI lineage — direct
// light gets its own low-variance estimator, separated from the noisy
// 1-spp gather). Before this, a pixel's torch light arrived only when
// the gather ray geometrically struck the torch's cell, re-randomized
// every frame: a high-magnitude Bernoulli trial whose flicker no
// history depth could drown (the N_MOVING sweep is in GI-DESIGN.md).
// Deterministic means zero variance: the pool is steady at n = 1.
//
// It is also what finally lights the block a torch STANDS ON. Every
// march steps before it tests, so a gather ray leaving the support
// block's top face starts INSIDE the torch's cell and exits without
// ever registering it — the brightest surface in the room structurally
// could not receive the direct term. The NEE ray from that face runs
// max(0.5 - 0.9, 0) = 0 blocks: unoccluded by construction.
struct NeeLight {
	valid: bool,
	cell: vec3i,
	dir: vec3f,
	dist: f32,
	dist2: f32,
	ndl: f32,
	// Emitter tint x emit x LIGHT_GAIN — irradiance-side, the surface
	// albedo multiplies in the scene shader like the rest of indirect.
	radiance: vec3f,
	// How far short of it the shadow ray must stop (gi_light_margin).
	margin: f32,
}

fn pick_dominant_light(p: vec3f, n: vec3f) -> NeeLight {
	var out: NeeLight;
	out.valid = false;
	let count = min(bitcast<u32>(lights[0].x), 256u);
	var best = 0.0;
	for (var i = 1u; i <= count; i++) {
		let l = lights[i];
		let d = l.xyz - p;
		let dist2 = dot(d, d);
		if dist2 > LIGHT_RANGE * LIGHT_RANGE || dist2 < 1e-4 {
			continue;
		}
		let ndl = dot(n, d) * inverseSqrt(dist2);
		if ndl <= 0.0 {
			continue;
		}
		let cls = bitcast<u32>(l.w);
		let emit = gi_emit(cls);
		let score = emit * ndl / max(dist2, LIGHT_NEAR * LIGHT_NEAR);
		if score > best {
			best = score;
			out.valid = true;
			out.cell = vec3i(floor(l.xyz));
			out.dist2 = dist2;
			out.dist = sqrt(dist2);
			out.dir = d / out.dist;
			out.ndl = ndl;
			out.radiance = gi_albedo(cls) * emit * LIGHT_GAIN;
			out.margin = gi_light_margin(l.xyz);
		}
	}
	return out;
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

// The tail of the gather path: radiance arriving from `dir` after the ray
// resolved. This is where the probe cascades stop being "the answer" and
// become what they were always meant to be — the cache that terminates a
// path, so one ray buys the whole multi-bounce series.
//
// `skip_cell` is the cell of the light the NEE ray sampled this pixel
// (NEE_NO_CELL when none): its emission is suppressed here WHETHER OR
// NOT the shadow ray connected, because the picked light's direct term
// is the deterministic estimate alone — counting the stochastic hit too
// doubles its energy, and letting it stand in for a blocked NEE ray
// puts the old flicker right back in the torch's shadow. Other
// emitters keep the direct-hit term: they are not being NEE-sampled,
// so the gather is still their only direct path.
fn path_radiance(side: i32, h: GiHit, dir: vec3f, skip_cell: vec3i) -> vec3f {
	let tr = medium_water_tr(h.water);
	if !h.hit {
		// Open in that direction. NOT the sun disc as well: gi_probe.wgsl
		// adds a sun lobe here because a probe has no separate direct term,
		// but this pass computes direct light explicitly below, and adding
		// it to both halves would double-count the sun on every surface
		// facing it.
		return gi_sky_radiance(dir, rz.zenith.rgb, rz.horizon.rgb) * tr;
	}
	let v = gi_cell(side, h.cell);
	let emit = gi_emit(v);
	let alb = gi_albedo(v);
	if emit > 0.0 {
		if all(h.cell == skip_cell) {
			return vec3f(0.0);
		}
		return alb * emit * LIGHT_GAIN * tr;
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
			let sv = gi_sun_vis(side, rz.jitter.z, rz.cam.xz, rz.sun, hp, GI_GATHER_ITERS);
			e += rz.sun_color.rgb * ndl * sv.x * medium_water_tr(sv.y);
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
	// World position and its screen derivatives, taken HERE — before the
	// sky early-out and before the silhouette branch below — because a
	// derivative may only be evaluated in UNIFORM control flow. Tint
	// enforces that and naga does not, so taking them where they were
	// used compiled clean on desktop and made `gi resolve` an INVALID
	// pipeline in Chrome: no GI at all in the browser, every frame, since
	// the exact-normal fallback was written. Evaluating them
	// unconditionally is the whole fix; the value is free to be used
	// inside a branch, only the call site has to be uniform.
	let clip = vec4f(i.ndc, d, 1.0);
	let wpos4 = rz.inv_view_proj * clip;
	let p = wpos4.xyz / wpos4.w;
	let ddx_p = dpdx(p);
	let ddy_p = dpdy(p);
	if d >= 1.0 {
		// Sky: lit, no water, and a normal code of 255 that matches no
		// real face — so a sky texel never wins a weight in the scene's
		// bilateral upsample, and a history length of 1 (packed with the
		// code) so nothing accumulates against it. Penumbra 0.
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
	// so pushing off here as well is what stacked to 1.2 blocks of
	// geometry that could not shadow anything.
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

	// THE FINAL GATHER. One cosine-weighted ray from this pixel, resolved
	// against the voxel window and terminated in the probe cache.
	//
	// The indirect term used to be the cascade blend at `sp` and nothing
	// else, which is why this engine had no ambient occlusion and could
	// not have had any: C0 samples irradiance every 2 blocks and
	// interpolates, so it is smooth by construction and cannot represent
	// darkening at contact scale. The per-probe occlusion DDA stops light
	// leaking THROUGH a wall; it does not darken the corner where two meet.
	// A ray that travels 0.3 blocks and returns a wall's dim bounce instead
	// of the sky does, for free — contact darkening is not a separate term
	// here, it is what a gather ray does when something is close.
	//
	// It also demotes the cache, which is the other half of the point. The
	// probe field's convergence transient was measured (GI-DESIGN.md
	// "Measuring motion") to be inherent to its cadence and immune to both
	// better restarts and 16x the sweep budget; the way out was never to
	// sweep harder but to stop making the near field depend on it.
	//
	// Beyond the window's reach the ray has no voxel data to march, so
	// those pixels keep the cascade blend — which is also where a per-pixel
	// answer is worth least.
	//
	// "The window's reach" is `gi_lateral_max` and nothing narrower. This
	// used to subtract GI_GATHER_T as well, reserving a 32-block margin so
	// that a gather ray launched at the limit could still run its full
	// length inside the data. That margin was unnecessary and it was
	// expensive: `gi_march` ALREADY breaks on the same bound and returns a
	// miss, so a ray leaving the window reads nothing invalid — it answers
	// sky, which is what the fallback was going to say anyway. All the
	// margin bought was 32 blocks of ring where a pixel that COULD have
	// traced took the cascade blend instead.
	//
	// It cost more than it looks. Past C1's 128 blocks the blend is the
	// analytic sky dome, so distant grass was lit by desaturated sky rather
	// than by its own bounce and came out BLUE: measured over a hillside at
	// ~200 blocks, G-B went from -9.4 to -4.5 when the margin came off,
	// three quarters of the way to what a window with real data out there
	// gets (-3.1) — for no memory and no frame time. The change is confined
	// to the horizon band and follows terrain silhouettes rather than a
	// camera-centred arc, so it moves the existing hard switch outward
	// rather than adding a new one, and it lands where the participating
	// medium has already faded the surface. See the R8 note in GI-DESIGN.md
	// for why widening the window itself was measured and declined.
	let lat = sp.xz - rz.cam.xz;
	let gather_max = gi_lateral_max(f32(side));
	let far = dot(lat, lat) > gather_max * gather_max;
	var indirect: vec3f;
	var nee = vec3f(0.0);
	if far {
		indirect =
			gi_indirect(side, sp, n, rz.cam.xyz, rz.dims0, rz.dims1, rz.zenith.rgb, rz.horizon.rgb, vec2i(8, 4));
	} else {
		// The deterministic direct term for the dominant local light —
		// see pick_dominant_light. Folded into `indirect` BEFORE the
		// accumulation below, so temporal blending, the edit spheres and
		// the atrous machinery all carry it with no new plumbing: a
		// deterministic input just passes through an average unchanged.
		var skip_cell = NEE_NO_CELL;
		// STUB_GINEE drops the light scan AND its shadow ray. In a scene
		// with no emitter in range `pick.valid` is already false, so this
		// term measures zero there by construction — that is the answer,
		// not a broken run.
		let pick = pick_dominant_light(sp, n);
		if pick.valid && !stubbed(STUB_GINEE) {
			skip_cell = pick.cell;
			// Stop short of the light's own cell (gi_light_margin, shared
			// with the probe nee()) so the whole-cell emitter rule can't
			// self-occlude.
			let sh = gi_march(
				side, rz.jitter.z, rz.cam.xz, sp, pick.dir,
				max(pick.dist - pick.margin, 0.0), GI_NEE_ITERS,
			);
			if !sh.hit {
				nee = pick.radiance * (pick.ndl / max(pick.dist2, LIGHT_NEAR * LIGHT_NEAR))
					* medium_water_tr(sh.water);
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
			let hit = gi_march(side, rz.jitter.z, rz.cam.xz, sp, rd, GI_GATHER_T, GI_GATHER_ITERS);
			indirect = path_radiance(side, hit, rd, skip_cell) + nee;
		}
	}
	// One sun ray along the EXACT direction, offset inside a cone about
	// twice the real sun's angular radius — but the offset is STATIC and
	// anchored to the WORLD, so the answer is deterministic FOR A WORLD
	// POINT and the penumbra comes from the occluder distance instead of
	// from time. See the header for why the offset stays at all (lattice
	// resonance).
	//
	// World-anchored, not screen-anchored, and the difference is a
	// flicker that shipped and came back: a screen-hashed offset is
	// static per PIXEL, but a moving camera slides every world point
	// across the pixels, re-asking a different fixed question each frame
	// — penumbra bands shimmered under rotation (measured +0.08..+0.16
	// consecutive-frame delta in the mid-frame terrain bands on the
	// flyover, and rotation is exactly the motion a player's mouse makes).
	// Hashing eighth-block world cells gives a point the same offset from
	// every camera. The hash point is pushed a sixteenth INTO the block
	// behind the face, because the shading point sits exactly on the
	// lattice plane the quantization cuts along — on the knife edge, the
	// f32 reconstruction wobble would flip cells frame to frame, which is
	// the flicker again by another route.
	let jq = floor((sp - n * 0.0625) * 8.0);
	let jh = fract(sin(dot(jq, vec3f(12.9898, 78.233, 37.719))) * 43758.5453);
	let u = vec2f(jh, fract(jh * 1.6180339887));
	var sdir = rz.sun_exact;
	sdir = vec4f(sample_cone(sdir.xyz, u, rz.jitter.y), sdir.w);
	// STUB_GISUN answers "lit, dry, no occluder" without marching. 320
	// iterations is the largest single ray budget in the frame, so this is
	// the term to reach for first.
	var sv = vec3f(sdir.w, 0.0, 0.0);
	if !stubbed(STUB_GISUN) {
		sv = gi_sun_vis(side, rz.jitter.z, rz.cam.xz, sdir, sp, 320);
	}
	let view_dist = length(p - rz.cam.xyz);
	// Penumbra width: occluder distance x tan(cone half-angle) is the
	// physical width in blocks; dividing by this texel's world footprint
	// (the derivatives, already taken unconditionally above) converts it
	// to half-res texels with the projection AND the surface's grazing
	// angle both accounted for — a grazing texel spans more world, so the
	// same world-width penumbra is fewer texels there, which is correct.
	// Lit pixels answer 0; gi_atrous spreads the width outward from the
	// shadowed side (max over taps), which is what lets the lit half of
	// the edge blur too.
	let texel_world = max(max(length(ddx_p), length(ddy_p)), 1e-4);
	let pen = clamp(sv.z * SUN_TAN / texel_world, 0.0, PEN_MAX);

	// Reproject into last frame. Exact for static geometry, so this needs
	// no motion vectors — only a validity test.
	var n_hist = 0.0;
	var acc = indirect;
	var acc_sun = sv.x;
	var acc_water = sv.y;
	// Did a same-face history tap fail depth only MARGINALLY? Then the
	// surface is still on screen and the miss is a grazing fetch the
	// tolerance clipped, not a reveal. The near ground fails all four
	// taps this way on most walking frames — adjacent half-res texels on
	// a grazing plane legitimately differ by more than the tolerance —
	// and priming those pixels from the cache on EVERY such frame locked
	// the whole near field at cache lighting: walked-vs-converged went
	// 0.69 -> 6.3 (bottom third 13) when the prime fired on every miss
	// indiscriminately. A true reveal is different in KIND, not degree:
	// the tap under it held the OCCLUDER, whole blocks nearer, so its
	// miss is many multiples of the tolerance.
	var grazed = false;
	if rz.prev_cam.w > 0.5 {
		let pc = rz.prev_vp * vec4f(p, 1.0);
		if pc.w > 0.0 {
			let nd = pc.xy / pc.w;
			let huv = vec2f(nd.x * 0.5 + 0.5, 0.5 - nd.y * 0.5);
			if all(huv >= vec2f(0.0)) && all(huv <= vec2f(1.0)) {
				let hdim = vec2f(textureDimensions(hist_indirect));
				// BILINEAR, per tap validated. A nearest tap here is what
				// smeared the image along the direction of travel: at half
				// res a texel spans a large slice of world on any grazing
				// surface, and rounding the reprojected position to the
				// nearest one pulls history from a world point up to half a
				// texel away. That error is not noise — the sub-texel phase
				// drifts monotonically as the camera walks, so it is BIASED
				// along the walk, and at 511/512 history weight the
				// accumulated image advects with the player and then needs
				// ~512 frames to decay. Neither validation below can catch
				// it: the neighbouring texel is the same face at the same
				// distance, just the wrong point on it.
				//
				// Below TRAVEL_EPS the miss is the f32 round trip and not
				// the camera, so snap onto the lattice: bilinear then
				// degenerates to a copy of this pixel's own history, which
				// is exactly what a camera that has not moved should get.
				// It is also what lets `travel` below read a true zero
				// instead of arithmetic.
				let hp_raw = huv * hdim - 0.5;
				let still = length(hp_raw + 0.5 - i.pos.xy) < TRAVEL_EPS;
				let hp = select(hp_raw, i.pos.xy - 0.5, still);
				let b0 = floor(hp);
				let f = hp - b0;
				// Validate on the exact face code AND on the distance the
				// point WOULD have been at from the old camera — comparing
				// stored view distances directly would reject the whole
				// screen the moment the camera moves.
				let want = length(p - rz.prev_cam.xyz);
				let base = 0.02 * max(want, 1.0);
				var wsum = 0.0;
				var s_ind = vec3f(0.0);
				var s_sun = 0.0;
				var s_wat = 0.0;
				var s_n = 0.0;
				// Two passes, strict first. Pass 0 is the original flat 2%
				// tolerance and the original loop, byte for byte — the hot
				// path pays nothing new. Pass 1 exists for one population:
				// a surface seen nearly edge-on (an east face viewed
				// looking north) legitimately spans MORE depth than 2% per
				// half-res texel, so the flat tolerance rejected all four
				// taps on every frame with any camera motion at all, and
				// those faces churned at n = 1 as a distracting flicker
				// band. Only when the strict pass comes up EMPTY with a
				// same-face near-miss on record does the loop rerun with
				// the tolerance widened to the surface's own per-texel
				// depth span (GRAZE_K x texel_world, SVGF's fwidth(z)
				// rule — already live for the penumbra; an always-widened
				// single pass was built first and measured +0.2-0.4 ms
				// for the whole screen's benefit of nobody). The widening
				// is capped at PRIME_MARGIN base tolerances, sharing the
				// reveal threshold, so a silhouette gap stays rejected
				// and the graze/reveal classification cannot disagree
				// with the acceptance rule.
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
						// g packs `code + 256*n` — low bits validate, high
						// bits are the history length (sun.a now carries
						// the penumbra width instead).
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
					}
					if wsum > 0.0 || !grazed {
						break;
					}
				}
				// Renormalize over the taps that survived, so a rejected
				// corner darkens nothing — it just stops voting.
				if wsum > 0.0 {
					let inv = 1.0 / wsum;
					// How deep an average this pixel is ALLOWED to keep, and it
					// is a two-state answer rather than a curve. Reprojection is
					// exact for static geometry, so the only thing a long history
					// costs is the resampling done to carry it — and once the
					// fetch is off-lattice at all, each frame mixes in the
					// neighbours and that blur COMPOUNDS at a rate the sub-texel
					// phase sets, not the speed: a pixel creeping a hundredth of a
					// texel a frame still sweeps the whole phase range across a
					// 512-frame history. So there is no useful middle. Either the
					// fetch landed on its texel and the history was free, or it did
					// not and the history has to be short.
					//
					// It used to be `N_MAX/(1 + TRAVEL_K*travel)`, a per-frame RATE,
					// and per-frame parallax collapses with distance while the blur
					// does not. Ground three blocks out moves a couple of texels a
					// frame and was capped near 40; the same ground at thirty moves
					// 0.03 and kept 486. Measured on --walk: mean n 15 near, 56 mid,
					// 195 far, with the walked-vs-warped residual tracking it at
					// 0.085 / 0.234 / 0.633 over a 0.005 repeat-noise floor, and a
					// quarter of the far field also above N_CONVERGED, where
					// gi_atrous switches itself off. Only the near field was ever in
					// the regime R9's N_MAX bisect measured, which is why that fix
					// looked complete and was not (R13).
					//
					// What makes the short end affordable is gi_atrous:
					// give up temporal samples and the spatial filter hands
					// them back. Without it there is no setting of this that
					// is both sharp under motion and clean at rest.
					var cap = select(N_MOVING, N_MAX, still);
					// A lighting edit near this surface: the probes are
					// already re-tracing it, so let the screen follow at
					// the short-history rate instead of 1/512.
					for (var si = 0; si < 4; si++) {
						let s = rz.inval[si];
						if s.w > 0.0 && distance(p, s.xyz) < s.w {
							cap = min(cap, N_MOVING);
						}
					}
					n_hist = min(min(s_n * inv + 1.0, N_MAX), cap);
					let a = 1.0 / n_hist;
					acc = mix(s_ind * inv, indirect, a);
					// The sun blends at its own, much shorter depth — the
					// estimate is deterministic, so this converges to the
					// same answer and only the response time differs. The
					// water path rides the sun's alpha: it is part of the
					// same ray.
					let a_sun = 1.0 / min(n_hist, N_SUN);
					acc_sun = mix(s_sun * inv, sv.x, a_sun);
					acc_water = mix(s_wat * inv, sv.y, a_sun);
				}
			}
		}
	}
	if n_hist < 1.0 {
		if grazed {
			// The surface never left the screen — the fetch just grazed
			// the depth tolerance. Restart unbiased at n = 1, exactly the
			// old behavior: the sample is noisy but centered, gi_atrous
			// absorbs it, and next frame the fetch usually lands again.
			n_hist = 1.0;
		} else {
			// Disocclusion: nothing on screen held this surface last frame
			// (offscreen, a different face, or history whole blocks off —
			// the occluder's). Prime the accumulator from the cascade
			// blend — see N_PRIOR — rather than restarting at the raw
			// gather, but only where INDIRECT is the light. On a sun-lit
			// surface the gather term is a minor correction, so the
			// prime can't help the picture there — but its bias can
			// hurt it: the cache is missing exactly the contact-scale
			// structure the gather adds, and a flat prime measured
			// walked-vs-converged 0.69 -> 6.3 on the walk's sun-lit
			// ground and 1.41 -> 4.33 on the flyover (a distance ramp
			// was tried between the two and just moved the bias to
			// whatever band the ramp allowed). In shadow, in a cave, at
			// night — where the reveal ghost actually lives, because
			// 1-spp speckle IS the whole signal there — the sun term is
			// zero and the prime runs at full strength. The sun channel
			// itself needs no prime: its ray is deterministic, so the
			// current answer is already the converged one.
			let sun_lit =
				sv.x * clamp(dot(n, rz.sun_exact.xyz), 0.0, 1.0) * rz.sun_exact.w;
			if sun_lit < PRIME_SUN_MAX {
				// The CHEAP cache lookup, not the full cascade blend.
				// Priming is bursty by nature — a camera swing into a
				// cave reveals a hundred thousand pixels in one frame —
				// and gi_indirect is ~112 loads (8 probes x relocation
				// walk x occlusion DDA, twice); that class of call at
				// the resolve's end measured 107 -> 69 fps once already.
				// gi_cache_at is 4 loads, and a prior that decays out in
				// N_PRIOR frames has no business being more exact than
				// the multi-bounce term that already terminates every
				// gather path.
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
				n_hist = N_PRIOR;
				acc = mix(prior, indirect, 1.0 / N_PRIOR);
			} else {
				n_hist = 1.0;
			}
		}
	}
	o.indirect = vec4f(acc, view_dist);
	// r = sun visibility, g = normal code + 256 x history length (round
	// keeps the pack exact in f32; the lost fraction perturbs next
	// frame's alpha by under 1/n^2), b = the sun ray's water path in
	// blocks, a = penumbra width in half-res texels. The path travels
	// rather than a transmittance so the scene shader can apply
	// medium_common's per-channel WATER_SIGMA — the same water the view
	// ray goes through.
	o.sun = vec4f(acc_sun, f32(code) + 256.0 * round(n_hist), acc_water, pen);
	return o;
}
