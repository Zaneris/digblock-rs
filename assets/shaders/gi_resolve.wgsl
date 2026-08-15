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
// still exists but is STATIC, hashed from sub-block world cells — so
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
// pins the caster to dir.z == 0 and gi.rs snaps it to a small rational, so
// every shadow ray is an exactly-planar, exactly-rational line through a
// cubic lattice, which aliases identically in every z-slice and
// periodically in x/y. That is the streaking, and it is pinned to the
// WORLD, not the screen. A per-pixel cone offset breaks the rational
// slope pixel by pixel; making it static in TIME is what turns the old
// temporal noise into a fixed dither the spatial filter resolves. The
// offset also self-scales as a penumbra sampler: at contact range the
// direction spread displaces the tested edge by well under a texel
// (contact shadows stay hard), and far from the occluder it dithers
// exactly the band the true penumbra spans.
//
// MRT, both rgba32f (gi::GI_ACC_FORMAT): indirect rgb + view distance,
// and sun visibility + (face-normal code packed with the history length
// into one channel — WebGPU's baseline attachment budget is exactly
// spent, so the penumbra width earned its channel by the pack, and the
// packed pair sits well inside f32's mantissa; every reader is a
// textureLoad, nothing ever hardware-filters these) + sun-ray water path
// + penumbra width in half-res texels.
//
// BOTH targets hold a running average, and that is what dictates the
// format. rgba8 was first and could not: at full history length each
// frame contributes 1/N of a difference, and in 8 bits that rounds to
// zero, so the average stopped moving. rgba16f replaced it and failed the
// SAME way — half-float's mantissa is short enough that near a stored
// value of 1.0 the update `hi + (indirect - hi)/N` rounds away unless the
// difference is a large fraction of the range. Much of the signal range
// never converged: every pixel froze where it stood when the camera
// stopped, leaving a stale pattern locked to the path the player walked,
// permanently. That is what "weirdness that moves with you" was.
//
// It survived because every capture in this engine was a still, which is
// exactly when a temporal artifact has already converged away. `--fly` vs
// `--warp` (GI-DESIGN.md "Measuring motion") is what finally photographed
// it: a large mean difference over a substantial slice of the screen even
// after a long spell of stillness, falling on f32 to the same floor as
// switching accumulation off entirely. Frame-to-frame stability at rest
// improved several times over with it.
//
// The rule is therefore not "use a float format" but that the mantissa has
// to carry the smallest increment the history length asks for: alpha of
// 1/N stalls once the residual falls under about N ulps, which f32 puts
// far under one output LSB. `history_format_resolves_
// the_accumulator` in gi.rs pins N_MAX against the format so raising one
// without the other fails the build rather than the picture.
//
// The gather core and the group(2) world bindings come from
// gi_common.wgsl (prepended by digblock-assets); group 1 is empty.

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
	// face code and depth never change, and a resting pixel at full
	// history admits that change at 1/n a frame — the light pool would
	// fade in over seconds. Inside a sphere the history cap drops to
	// N_MOVING, the same short-history + a-trous regime motion already
	// uses, so the change lands in a small fraction of that time.
	inval: array<vec4f, 4>,
}

@group(0) @binding(0) var<uniform> rz: GiResolve;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
// The prepass's rasterized face-normal code (chunk.wgsl's `fs_depth`); a
// code past the six real faces means no solid was drawn here.
@group(0) @binding(2) var normal_tex: texture_2d<u32>;
// Last frame's outputs, ping-ponged (never copied).
@group(0) @binding(3) var hist_indirect: texture_2d<f32>;
@group(0) @binding(4) var hist_sun: texture_2d<f32>;
// The emitter list (R20) — the same count-prefixed buffer the probe
// pass NEE-samples, bound here for the per-pixel direct term. lights[0].x
// is the count as a bitcast u32; entries are (xyz world center, w class).
@group(0) @binding(5) var<storage, read> lights: array<vec4f>;

// How many frames a still camera keeps sharpening for. alpha is
// 1/n with n climbing to here, so this constant is what "takes a few
// seconds to look accurate" actually means, stated in frames.
const N_MAX = 512.0;

// How deep an average a MOVING pixel may keep. Below this the residual
// noise outruns what two a-trous iterations can absorb; SVGF-class
// denoisers keep a similarly short history for the same reason. Since the
// deterministic sun this cap governs INDIRECT alone (the sun has its own,
// tighter N_SUN), which is what finally allowed raising it: pre-R14 the
// sun smear scaled with this number and pinned it far lower, while the 1-spp
// gather's moving flicker — very visible in a torch-lit interior, where
// indirect is the only light — wanted it deeper. Indirect is smooth, so
// the extra history buys flicker reduction without visible smear; the
// walk-regime rows outside the shadow band are the gate.
const N_MOVING = 48.0;

// How deep the SUN average may ever get, still or not. The sun estimate
// is deterministic per pixel now, so history buys it nothing but edge
// antialiasing where the static dither flips — a handful of frames is
// plenty — and everything a long history costs (the resampling smear
// R9/R13 chased) it no longer pays. This cap, not the filter, is what
// makes the smear bounded BY CONSTRUCTION: a history can only be dragged
// as far as its own length. Indirect keeps the full n — its 1-spp gather is
// genuinely stochastic and does not smear, being smooth.
const N_SUN = 12.0;

// The confidence a DISOCCLUDED pixel starts its history at, primed from
// the probe cascade blend instead of from its own first sample.
//
// A pixel with no valid history used to restart at n = 1: the raw 1-spp
// gather, which in an indirectly-lit interior is speckle — a cave floor's
// single ray mostly finds dark walls and occasionally the bright mouth —
// so every surface the camera revealed (a cave rim, the strip a strafe
// pulls in at the screen edge) visibly "developed" for a moment
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
// the sub-texel phase). Grazing surfaces span more depth per texel than
// the flat relative tolerance allows, and a tolerance that ignores that
// is a flicker band exactly where a face turns edge-on.
const GRAZE_K = 2.0;

// How many multiples of the depth tolerance a same-face tap may miss by
// and still testify that the surface is merely GRAZED, not revealed —
// the gate that keeps the prime off the near ground (see `grazed` in
// fs). Between the two populations there is a wide gap: a grazing
// half-res texel at walking pitch misses by a small multiple of the
// tolerance, an occluder's tap misses by whole blocks — orders of
// magnitude more at any distance the near field cares about.
const PRIME_MARGIN = 8.0;

// tan of the sun cone's half-angle — must match
// SUN_CONE_COS in gi.rs, pinned by `the_penumbra_knows_the_cone` in
// digblock-assets. occluder_t x this = the physical penumbra width in
// blocks; the screen derivatives turn it into texels per pixel.
const SUN_TAN = 0.0087269;

// The widest penumbra the a-trous can reconstruct, in half-res texels:
// its two iterations' strides, scaled up for wide penumbras by
// gi_atrous.wgsl's SUN_STRIDE_MAX, reach exactly this much combined
// support. Beyond this a shadow renders sharper than physical — which
// the stochastic version could not represent either, at the short moving
// history it ran.
const PEN_MAX = 12.0;

// How far the reprojected fetch may miss its own texel, in texels, and
// still be treated as having landed on it.
//
// This is an ARITHMETIC floor, not a motion threshold. The fetch round-trips
// a world position out through `inv_view_proj` and back through
// `prev_view_proj`; against this world's far plane f32 does not return what
// it was given, and a standing camera measures a drift of a fraction of a
// texel (measured near and far on --walk --warp). GI-DESIGN.md used to say
// travel was measurably zero at rest. It is not, and believing it cost twice
// over: the cap below reads arithmetic as movement, and the bilinear fetch
// mixes in neighbours a camera that has not moved has no business touching —
// at full history length that steady-state blur spans several texels, which
// is most of why the resting far field was blotchy.
//
// The value clears the measured drift with margin and stays under half the
// slowest real motion in the scene (the far field's median travel on
// --walk).
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
// probe blend at C0's spacing — contact darkening, a wall's own bounce, a
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
// Retuned at R26 — the reasoning and the sweep live on gi_probe's twin.
const LIGHT_GAIN = 0.75;
// Where the point model's near field stops being taken literally, in
// blocks. Beyond it the falloff is exactly 1/d^2 and NOTHING about the
// pool changes; inside, irradiance holds at 1/LIGHT_NEAR^2.
//
// A voxel emitter is not a point, and a point's 1/d^2 diverges: the
// support block's top face sits a fraction of a block from the torch's
// centre, so it took `emit * LIGHT_GAIN` whole — many times over, before
// the surface albedo, against a display whose white is 1.0. The result
// was a hard-edged ellipse of saturated orange under every torch, texture
// detail gone inside it (measured: the red channel clipped across a
// visible slice of the vault frame, with not one pixel actually white —
// the giveaway that this is per-channel saturation, not a bright
// surface).
//
// The clamp already existed, tighter, for the divide-by-nothing case;
// this only widens it. The number is a DISPLAY fact, not a physical one,
// and this is explicitly a stopgap: the honest fix is an HDR scene target
// and one tonemap on the composite, which would roll the highlight off
// instead of flattening it, and is on the backlog.
//
// This value is a measured trade, not the extreme. Clipping in the vault
// pool band falls by most of an order of magnitude here and bottoms out a
// little further along — but what is left at the bottom IS the torch's
// own emissive sprite, which no falloff can touch (every wider clamp
// measures identically), and buying that last sliver costs a real
// fraction of pool brightness for a pool that already reads faint.
// What is left here is a small hot core at the flame's base, which is
// what a flame should do; what went away is the flat, hard-edged
// ellipse with no floor texture in it. Rooms are unaffected either way:
// the floor and wall bands away from the torch barely move, because
// this cannot touch anything past the radius.
const LIGHT_NEAR = 2.0;
// The NEE shadow ray's iteration cap. Bounded by max_t <= LIGHT_RANGE
// anyway; this covers the worst dense diagonal with margin, because a
// tight cap silently UN-shadows whatever lies past it — the bug
// gi_sun_vis shipped once, its cap set to half what its window demanded.
const GI_NEE_ITERS = 64;

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

// The per-pixel direct term for local lights: ONE shadow ray, but at a
// light RESAMPLED over the whole in-range list rather than at the
// brightest one (R27; R20 built the shadow ray and picked the argmax).
//
// R20's argmax gave one light a zero-variance estimator and left every
// other emitter to the gather's direct-hit lottery — `alb * emit *
// LIGHT_GAIN`, many times over a display whose white is 1.0, on
// vanishingly small odds. Measured on the DIG_TORCHES courtyard,
// same-pose walking flicker climbs with every emitter the argmax does NOT
// cover, and deleting the gather's emitter term outright removes most of
// that flicker for a small fraction of the scene's light. GI-DESIGN.md
// R27 has the tables.
//
// So the gather no longer returns emitter radiance AT ALL (see
// path_radiance) and this covers every emitter instead of one. The
// estimator is Talbot-style RIS (resampled importance sampling) in its
// exact discrete form: the loop already visits every entry in the list,
// so there is no candidate SUBSET to draw — one weighted-reservoir pass
// selects light i with probability exactly p_i / sum(p), and the
// contribution weight is sum(p) / p_sel.
//
// The target function is R20's own score, `emit * ndl / max(d2,
// LIGHT_NEAR^2)` — the unshadowed irradiance up to the emitter's tint.
// That makes the estimator EXACT, not merely unbiased, whenever the
// in-range emitters share a tint and are all visible: f_i = albedo *
// LIGHT_GAIN * p_i, so f_sel / p_sel * sum(p) = albedo * LIGHT_GAIN *
// sum(p), which does not depend on which light was drawn. Every torch
// in the world is one class today, so in the open the pool is as steady
// at n = 1 as the argmax was, while carrying ALL of the light instead of
// the brightest slice of it.
//
// What is left is visibility, which the target function cannot see: the
// draw is proportional to UNSHADOWED contribution, so a point that can
// only see half of what it samples gets a Bernoulli on the pool total
// rather than the old Bernoulli on a huge radiance spike. That residual
// is what a temporal reservoir (ReSTIR DI: reproject the reservoir through
// `prev_vp`, M-clamp) would remove, and it is deliberately NOT built
// here — the measurement decides, and it needs a scene with emitters
// occluded from each other (`DIG_TORCHES_WALL`).
//
// The R20 property that must not regress: this is still what lights the
// block a torch STANDS ON. Every march steps before it tests, so a
// gather ray leaving the support block's top face starts INSIDE the
// torch's cell and exits without registering it — the brightest surface
// in the room can only be lit from here. The NEE ray from that face runs
// `max(dist - margin, 0)` = zero blocks: unoccluded by construction.
struct NeeLight {
	valid: bool,
	dir: vec3f,
	dist: f32,
	// The WHOLE in-range pool's unshadowed irradiance, carried by the one
	// drawn sample (f_sel / p_sel x sum(p)) — already through the falloff
	// and the cosine, so the use site only gates it on the shadow ray.
	// The surface albedo multiplies in the scene shader, like the rest of
	// indirect.
	irradiance: vec3f,
	// How far short of the drawn light the shadow ray must stop
	// (gi_light_margin).
	margin: f32,
}

// One uniform per reservoir update, which is what makes the selection
// correct. Reusing a single draw for every candidate is the tempting
// shortcut and it is WRONG: the tests `u < p_i / sum_i` are then nested
// events on one variable, so the last candidate keeps its exact marginal
// while everything before it is squeezed — equal weights do not come out
// equal. An LCG advanced per ACCEPTED candidate costs a few ALU on the
// in-range lights only.
fn lcg_next(state: ptr<function, u32>) -> f32 {
	*state = *state * 1664525u + 1013904223u;
	return f32(*state >> 8u) * (1.0 / 16777216.0);
}

fn resample_light(p: vec3f, n: vec3f, seed: u32) -> NeeLight {
	var out: NeeLight;
	out.valid = false;
	let count = min(bitcast<u32>(lights[0].x), 256u);
	var rng = seed;
	// sum(p) over every candidate, which IS the pool's total irradiance
	// divided by the emitter tint — so it is both the reservoir's
	// normalizer and the answer.
	var wsum = 0.0;
	var sel_cls = 0u;
	// Only the ENTRY is kept per update. Everything derived from it — the
	// distance, the direction, `gi_light_margin`'s sqrt — is computed
	// once after the loop instead of on every candidate that happens to
	// win the reservoir, which on a cluster is most of them. Same answer,
	// since the winner is the winner. The whole vec4 rather than just the
	// position, because the margin is a question about the w word too.
	var sel = vec4f(0.0);
	for (var i = 1u; i <= count; i++) {
		let l = lights[i];
		let d = l.xyz - p;
		let dist2 = dot(d, d);
		if dist2 > LIGHT_RANGE * LIGHT_RANGE || dist2 < 1e-4 {
			continue;
		}
		let inv_dist = inverseSqrt(dist2);
		let ndl = dot(n, d) * inv_dist;
		if ndl <= 0.0 {
			continue;
		}
		let cls = gi_light_class(l.w);
		// The target function, and the ONLY LUT read on the candidate
		// path: the drawn light's tint is fetched once, below. A tint that
		// varies between classes just makes this a slightly worse target,
		// never a biased one.
		let score = gi_emit(cls) * ndl / max(dist2, LIGHT_NEAR * LIGHT_NEAR);
		if score <= 0.0 {
			continue;
		}
		wsum += score;
		if lcg_next(&rng) * wsum < score {
			out.valid = true;
			sel = l;
			sel_cls = cls;
		}
	}
	if out.valid {
		let d = sel.xyz - p;
		out.dist = length(d);
		out.dir = d / out.dist;
		out.margin = gi_light_margin_of(sel);
		// f_sel / p_sel x sum(p), with the division done in closed form:
		// the target IS the contribution up to the tint, so `score_sel`
		// cancels exactly and never appears. That cancellation is the
		// whole reason this reads as one deterministic multiply rather
		// than as a ratio of two sampled quantities.
		out.irradiance = gi_albedo(sel_cls) * LIGHT_GAIN * wsum;
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
// An emitting cell contributes NOTHING here (R27). `resample_light`
// covers every in-range emitter with a shadow ray, so a gather hit on
// one is either a double count or — where the shadow ray was blocked —
// the old flicker reinstated inside the torch's own shadow. R20
// suppressed exactly the one cell it NEE'd and left the rest paying
// `alb * emit * LIGHT_GAIN`: radiance, so it does not fall off with
// distance, which is why the noise it made GREW with distance while its
// energy shrank. Measured: most of the flicker, a sliver of the light.
//
// The cell still OCCLUDES, which is correct — a torch is an object, and
// the NEE ray is what carries its emission. And the suppression is
// unconditional rather than range-gated: `LIGHT_RANGE` already declares
// that an emitter past it may not matter (gi_probe's `nee`
// enforces the same bound), and this was the one path in the system that
// let one contribute from anywhere inside `GI_GATHER_T`, at full
// radiance and full variance. One reach, everywhere.
//
// The known gap, named because it is not free: with more than
// MAX_LIGHTS emitters inside `flush_lights`' reach, the list truncates
// to the nearest ones to THE CAMERA, so a distant surface's own nearby
// torches can fall off the list and now go dark rather than arriving as
// lottery noise. That trade is deliberate — it was never a usable signal
// — but it is the thing to watch if a base ever outgrows the list.
fn path_radiance(side: i32, h: GiHit, dir: vec3f) -> vec3f {
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
	let alb = gi_albedo(v);
	if gi_emit(v) > 0.0 {
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

	// THE FINAL GATHER. One cosine-weighted ray from this pixel, resolved
	// against the voxel window and terminated in the probe cache.
	//
	// The indirect term used to be the cascade blend at `sp` and nothing
	// else, which is why this engine had no ambient occlusion and could
	// not have had any: C0 samples irradiance at its own spacing and
	// interpolates, so it is smooth by construction and cannot represent
	// darkening at contact scale. The per-probe occlusion DDA stops light
	// leaking THROUGH a wall; it does not darken the corner where two meet.
	// A ray that travels a fraction of a block and returns a wall's dim
	// bounce instead of the sky does, for free — contact darkening is not a
	// separate term here, it is what a gather ray does when something is
	// close.
	//
	// It also demotes the cache, which is the other half of the point. The
	// probe field's convergence transient was measured (GI-DESIGN.md
	// "Measuring motion") to be inherent to its cadence and immune to both
	// better restarts and a vastly larger sweep budget; the way out was
	// never to sweep harder but to stop making the near field depend on it.
	//
	// Beyond the window's reach the ray has no voxel data to march, so
	// those pixels keep the cascade blend — which is also where a per-pixel
	// answer is worth least.
	//
	// "The window's reach" is `gi_lateral_max` and nothing narrower. This
	// used to subtract GI_GATHER_T as well, reserving a whole gather ray's
	// length as margin so that a ray launched at the limit could still run
	// its full length inside the data. That margin was unnecessary and it
	// was expensive: `gi_march` ALREADY breaks on the same bound and
	// returns a miss, so a ray leaving the window reads nothing invalid —
	// it answers sky, which is what the fallback was going to say anyway.
	// All the margin bought was a ring where a pixel that COULD have
	// traced took the cascade blend instead.
	//
	// It cost more than it looks. Past C1's coverage the blend is the
	// analytic sky dome, so distant grass was lit by desaturated sky rather
	// than by its own bounce and came out BLUE: measured over a distant
	// hillside, its green-blue balance recovered most of the way to what a
	// window with real data out there gets when the margin came off — for
	// no memory and no frame time. The change is confined to the horizon
	// band and follows terrain silhouettes rather than a
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
		// The direct term for local lights — see resample_light. Folded
		// into `indirect` BEFORE the accumulation below, so temporal
		// blending, the edit spheres and the atrous machinery all carry it
		// with no new plumbing.
		//
		// The reservoir's seed is an integer pixel/frame hash rather than a
		// draw off `sample2`'s R2 sequence, and that is not laziness: the
		// selection is driven by the per-candidate LCG inside the loop, so
		// stratifying the SEED would buy nothing the reservoir could use.
		// It does have to advance with the frame — the residual variance
		// here is the visibility Bernoulli, and a static world-anchored
		// draw would freeze it into a fixed spatial dither the accumulator
		// can never average out, which is a worse artifact than the one
		// this removes.
		//
		// STUB_GINEE drops the light scan AND its shadow ray. In a scene
		// with no emitter in range `pick.valid` is already false, so this
		// term measures zero there by construction — that is the answer,
		// not a broken run.
		let seed = (u32(uv.x) * 73856093u)
			^ (u32(uv.y) * 19349663u)
			^ (u32(rz.jitter.x) * 83492791u);
		let pick = resample_light(sp, n, seed);
		if pick.valid && !stubbed(STUB_GINEE) {
			// Stop short of the light's own cell (gi_light_margin, shared
			// with the probe nee()) so the whole-cell emitter rule can't
			// self-occlude.
			let sh = gi_march(
				side, rz.jitter.z, rz.cam.xz, sp, pick.dir,
				max(pick.dist - pick.margin, 0.0), GI_NEE_ITERS,
			);
			if !sh.hit {
				nee = pick.irradiance * medium_water_tr(sh.water);
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
			indirect = path_radiance(side, hit, rd) + nee;
		}
	}
	// One sun ray along the EXACT direction, offset inside a cone a little
	// wider than the real sun's disc — but the offset is STATIC and
	// anchored to the WORLD, so the answer is deterministic FOR A WORLD
	// POINT and the penumbra comes from the occluder distance instead of
	// from time. See the header for why the offset stays at all (lattice
	// resonance).
	//
	// World-anchored, not screen-anchored, and the difference is a
	// flicker that shipped and came back: a screen-hashed offset is
	// static per PIXEL, but a moving camera slides every world point
	// across the pixels, re-asking a different fixed question each frame
	// — penumbra bands shimmered under rotation (measured as a clear
	// consecutive-frame delta in the mid-frame terrain bands on the
	// flyover, and rotation is exactly the motion a player's mouse makes).
	// Hashing sub-block world cells gives a point the same offset from
	// every camera. The hash point is pushed slightly INTO the block
	// behind the face, because the shading point sits exactly on the
	// lattice plane the quantization cuts along — on the knife edge, the
	// f32 reconstruction wobble would flip cells frame to frame, which is
	// the flicker again by another route.
	let jq = floor((sp - n * 0.0625) * 8.0);
	let jh = fract(sin(dot(jq, vec3f(12.9898, 78.233, 37.719))) * 43758.5453);
	let u = vec2f(jh, fract(jh * 1.6180339887));
	var sdir = rz.sun_exact;
	sdir = vec4f(sample_cone(sdir.xyz, u, rz.jitter.y), sdir.w);
	// STUB_GISUN answers "lit, dry, no occluder" without marching. Its
	// march carries the largest single ray budget in the frame, so this is
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
	// the whole near field at cache lighting: walked-vs-converged got an
	// order of magnitude worse, worst of all in the bottom of the frame,
	// when the prime fired on every miss indiscriminately. A true reveal
	// is different in KIND, not degree: the tap under it held the
	// OCCLUDER, whole blocks nearer, so its
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
				// along the walk, and at full history weight the
				// accumulated image advects with the player and then needs
				// its whole length to decay. Neither validation below can catch
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
				// Two passes, strict first. Pass 0 is the original flat
				// relative tolerance and the original loop, byte for byte —
				// the hot path pays nothing new. Pass 1 exists for one
				// population: a surface seen nearly edge-on (an east face
				// viewed looking north) legitimately spans MORE depth per
				// half-res texel than that flat tolerance allows, so it
				// rejected all four taps on every frame with any camera
				// motion at all, and
				// those faces churned at n = 1 as a distracting flicker
				// band. Only when the strict pass comes up EMPTY with a
				// same-face near-miss on record does the loop rerun with
				// the tolerance widened to the surface's own per-texel
				// depth span (GRAZE_K x texel_world, SVGF's fwidth(z)
				// rule — already live for the penumbra; an always-widened
				// single pass was built first and measured a real cost
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
						// g packs the face code together with the history
						// length — the code validates, the length is what
						// the accumulator reads (sun.a now carries the
						// penumbra width instead).
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
					// phase sets, not the speed: a pixel creeping a tiny fraction
					// of a texel a frame still sweeps the whole phase range
					// across a full-length history. So there is no useful
					// middle. Either the fetch landed on its texel and the
					// history was free, or it did
					// not and the history has to be short.
					//
					// It used to be `N_MAX/(1 + TRAVEL_K*travel)`, a per-frame RATE,
					// and per-frame parallax collapses with distance while the blur
					// does not. Near ground moves whole texels a frame and was
					// capped short; the same ground far off barely moves and kept
					// nearly all of N_MAX. Measured on --walk: mean n climbed
					// steeply from the near field to the far one, with the
					// walked-vs-warped residual tracking it the whole way, well
					// over the repeat-noise floor, and much of the far field also
					// above N_CONVERGED, where gi_atrous switches itself off.
					// Only the near field was ever in
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
					// the short-history rate instead of at 1/N_MAX.
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
			// walked-vs-converged an order of magnitude worse on the
			// walk's sun-lit ground and several times worse on the
			// flyover (a distance ramp was tried between the two and
			// just moved the bias to
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
				// and gi_indirect is dozens of loads (8 probes x relocation
				// walk x occlusion DDA, twice); that class of call at
				// the resolve's end measured a large frame-rate drop once
				// already. gi_cache_at is a handful, and a prior that
				// decays out in N_PRIOR frames has no business being more
				// exact than the multi-bounce term that already terminates
				// every gather path.
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
	// r = sun visibility, g = the normal code packed with the history
	// length (round keeps the pack exact in f32; the lost fraction
	// perturbs next frame's alpha by under 1/n^2), b = the sun ray's
	// water path in blocks, a = penumbra width in half-res texels. The
	// path travels
	// rather than a transmittance so the scene shader can apply
	// medium_common's per-channel WATER_SIGMA — the same water the view
	// ray goes through.
	o.sun = vec4f(acc_sun, f32(code) + 256.0 * round(n_hist), acc_water, pen);
	return o;
}
