// The GI resolve's spatial half (SVGF, Schied et al. 2017): an edge-stopping
// a-trous wavelet filter over the half-res lighting targets, run twice with a
// doubling tap stride.
//
// **Why it exists.** Without it every bit of variance reduction here is
// temporal, which forces N_MAX deep — and a deep SCREEN-SPACE history cannot
// survive a moving camera: reprojection is exact for static geometry, but the
// fetch still lands between texels, and resampling a half-res buffer for the
// whole length of that history drags the image along the direction of travel.
// There is no N that is both sharp under motion and clean at rest.
//
// Two signals, two gates. INDIRECT is a genuinely stochastic 1-spp gather, so it
// takes the original SVGF trade: filter while the history is short, shut off
// once the temporal estimate has converged. SUN is deterministic with a static
// dither, and its filter is RECONSTRUCTION rather than denoising — gi_resolve
// stores the physical penumbra width in sun.a and this pass blurs the hard 0/1
// answer across exactly that many texels. So it must run AT REST TOO, since a
// deterministic ray never builds a penumbra in time; it must NOT edge-stop on
// the sun value inside the penumbra, because that boundary is the thing to blur
// (pen -> 0 collapses the kernel to identity, so contact stays hard); and it
// spreads the width itself, so the LIT side of an edge blurs symmetrically.
//
// **No moment buffer.** Both estimators have an ANALYTIC variance from data
// already stored — sun visibility is a mean of n Bernoulli trials, so its
// standard error is exactly sqrt(p(1-p)/n) with p and n both in the target.
// Deriving sigma instead of measuring it saves a whole ping-ponged rgba32f and
// cannot drift out of step with the accumulator the way a separate buffer can.

struct Atrous {
	// x tap stride in texels (doubling between the two iterations),
	// y unused.
	step: vec4f,
}

@group(0) @binding(0) var<uniform> at: Atrous;
@group(0) @binding(1) var src_indirect: texture_2d<f32>;
@group(0) @binding(2) var src_sun: texture_2d<f32>;

// The a-trous kernel, separable, and the reason the stride doubles
// instead of the kernel growing: two narrow passes at doubling strides
// cover the same footprint a single wide kernel would, for a fraction of
// the taps. SVGF specifies the B3 spline, which reaches further for
// several times the taps; it was built and measured here, and it buys a
// slightly smaller walk-regime residual for twice the cost, at a point
// where the artifact it exists to remove is already gone from the picture
// in both. The wider kernel is the one to reach for if the history floor
// ever drops.
const H = array<f32, 3>(0.25, 0.5, 0.25);
const R = 1;

// Above this the temporal estimate has already converged and the INDIRECT
// filter is measurably a no-op — the near ground, where n sits at N_MAX,
// showed no visible change with it running. Skipping it there is
// free at rest, which is most of the time a player spends looking at
// anything. The sun reconstruction deliberately does NOT share this gate —
// see the header.
const N_CONVERGED = 256.0;

// How many sigmas of disagreement it takes to stop the filter. Lower is a
// sharper edge and more surviving noise.
const SIGMA_L = 4.0;
// Floors, so a converged pixel (n at N_MAX) still admits its neighbours
// rather than dividing by an sigma of zero and refusing every tap.
const SUN_SIGMA_MIN = 0.02;
const IND_SIGMA_MIN = 0.02;
// Relative view-distance tolerance per texel of stride. A tap further away
// than this is a different surface, not a neighbour — and on a grazing
// plane consecutive texels legitimately differ, so it scales with stride.
const DEPTH_K = 0.02;

// Below this penumbra width (half-res texels) the edge is sharper than a
// texel and there is nothing to reconstruct.
const PEN_MIN = 0.5;
// The sun taps' stride multiplier tops out here: scaled up by it, the two
// iterations' base strides reach exactly gi_resolve's PEN_MAX of combined
// support — the two constants are pinned together by
// `the_penumbra_fits_the_filter` in digblock-assets.
const SUN_STRIDE_MAX = 4.0;

struct VsOut {
	@builtin(position) pos: vec4f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	var o: VsOut;
	let x = f32(i32(vi & 1u) * 4 - 1);
	let y = f32(i32(vi >> 1u) * 4 - 1);
	o.pos = vec4f(x, y, 0.0, 1.0);
	return o;
}

struct FsOut {
	@location(0) indirect: vec4f,
	@location(1) sun: vec4f,
}

fn lum(c: vec3f) -> f32 {
	return dot(c, vec3f(0.2126, 0.7152, 0.0722));
}

@fragment
fn fs(i: VsOut) -> FsOut {
	let dim = vec2i(textureDimensions(src_indirect));
	let c = vec2i(i.pos.xy);
	let ci = textureLoad(src_indirect, c, 0);
	let cs = textureLoad(src_sun, c, 0);
	var o: FsOut;
	// Metadata rides through untouched: view distance and the packed face
	// code + history length are what the NEXT frame validates and filters
	// against, and a filtered code is not a code. The penumbra width in
	// sun.a is the one exception — it is spread below, deliberately.
	o.indirect = ci;
	o.sun = cs;
	let cg = u32(cs.g);
	let code = cg & 255u;
	// Sky. It carries no lighting and must keep the sentinel code that
	// loses every weight in the scene's bilateral upsample.
	if code > 5u {
		return o;
	}
	let n = max(f32(cg >> 8u), 1.0);
	let step = i32(at.step.x);

	// ---- One ring at the base stride serves three customers from the
	// same tap loads: the indirect gather (SVGF's original trade), the
	// Bernoulli sun gather (used only if this pixel stays sub-texel), and
	// the penumbra-width spread — a lit pixel cannot know it borders a
	// shadow without looking, so the spread runs for every surface pixel;
	// max over same-surface taps, written back to sun.a, is what lets the
	// LIT half of an edge blur by iteration 2. Splitting these into three
	// loops was built first and measured a clear cost over the old shader;
	// sharing the loads brought it back.
	let filter_ind = n < N_CONVERGED;
	let cl = lum(ci.rgb);
	let ind_sigma = max(cl / sqrt(n), IND_SIGMA_MIN);
	let p = clamp(cs.r, 0.0, 1.0);
	let sun_sigma = max(sqrt(p * (1.0 - p) / n), SUN_SIGMA_MIN);
	let dtol = DEPTH_K * at.step.x * max(ci.w, 1.0);
	var pen = cs.a;
	var w_ind = H[R] * H[R];
	var a_ind = ci.rgb * w_ind;
	var w_sun = w_ind;
	var a_sun = cs.r * w_sun;
	var a_wat = cs.b * w_sun;
	for (var dy = -R; dy <= R; dy++) {
		for (var dx = -R; dx <= R; dx++) {
			if dx == 0 && dy == 0 {
				continue;
			}
			let t = c + vec2i(dx, dy) * step;
			if any(t < vec2i(0)) || any(t >= dim) {
				continue;
			}
			let ti = textureLoad(src_indirect, t, 0);
			let ts = textureLoad(src_sun, t, 0);
			// A different face is a different surface. The prepass
			// rasterizes this code exactly, so this is a hard reject and
			// not a weight — which is the same reason chunk.wgsl's
			// bilateral upsample can compare it bit-for-bit.
			if (u32(ts.g) & 255u) != code || abs(ti.w - ci.w) > dtol {
				continue;
			}
			pen = max(pen, ts.a);
			let h = H[dx + R] * H[dy + R];
			// Edge stopping on the signals themselves. Without this the
			// kernel blurs straight across a lighting boundary on a flat
			// floor, where depth and normal are identical and only the
			// lighting differs.
			let wi = h * exp(-abs(lum(ti.rgb) - cl) / (SIGMA_L * ind_sigma));
			w_ind += wi;
			a_ind += ti.rgb * wi;
			let ws = h * exp(-abs(ts.r - cs.r) / (SIGMA_L * sun_sigma));
			w_sun += ws;
			a_sun += ts.r * ws;
			a_wat += ts.b * ws;
		}
	}
	if filter_ind {
		o.indirect = vec4f(a_ind / w_ind, ci.w);
	}

	if pen >= PEN_MIN {
		// Penumbra regime: a Gaussian of the width physics asked for, no
		// edge-stop on the sun value (the 0/1 step is exactly the thing to
		// blur into a gradient), stride scaled up so wide penumbras fit
		// inside one ring. Water rides the same weights — it is part of
		// the same ray. Only penumbra pixels pay this second ring.
		let scale = clamp(ceil(pen / 3.0), 1.0, SUN_STRIDE_MAX);
		let sstep = step * i32(scale);
		let sdtol = dtol * scale;
		// Treat the width as ~2 sigma so the kernel's reach matches it.
		let sigma = max(pen * 0.5, PEN_MIN);
		var wp = H[R] * H[R];
		var ap = cs.r * wp;
		var awp = cs.b * wp;
		for (var dy = -R; dy <= R; dy++) {
			for (var dx = -R; dx <= R; dx++) {
				if dx == 0 && dy == 0 {
					continue;
				}
				let t = c + vec2i(dx, dy) * sstep;
				if any(t < vec2i(0)) || any(t >= dim) {
					continue;
				}
				let ti = textureLoad(src_indirect, t, 0);
				let ts = textureLoad(src_sun, t, 0);
				if (u32(ts.g) & 255u) != code || abs(ti.w - ci.w) > sdtol {
					continue;
				}
				let d = length(vec2f(f32(dx), f32(dy)) * f32(sstep));
				let wg = H[dx + R] * H[dy + R] * exp(-0.5 * d * d / (sigma * sigma));
				wp += wg;
				ap += ts.r * wg;
				awp += ts.b * wg;
			}
		}
		o.sun = vec4f(ap / wp, cs.g, awp / wp, pen);
	} else if n < N_CONVERGED {
		// Sub-texel penumbra with a short history: not a gradient to
		// reconstruct but residual noise where the static dither flips —
		// the Bernoulli sums from the shared ring absorb it.
		o.sun = vec4f(a_sun / w_sun, cs.g, a_wat / w_sun, pen);
	} else {
		// Converged interior: untouched but for the spread width.
		o.sun = vec4f(cs.r, cs.g, cs.b, pen);
	}
	return o;
}
