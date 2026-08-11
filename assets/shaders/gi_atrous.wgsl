// The GI resolve's spatial half (SVGF, Schied et al. 2017): an
// edge-stopping a-trous wavelet filter over the half-res lighting targets,
// run twice with a doubling tap stride.
//
// WHY IT EXISTS, measured rather than assumed. Without it every bit of
// variance reduction in this system is temporal, which forces the history
// deep — N_MAX was 512 — and a 512-frame SCREEN-SPACE history cannot
// survive a moving camera. Reprojection is exact for static geometry, but
// the fetch still lands between texels, and resampling a half-res buffer
// every frame for hundreds of frames drags the accumulated image along the
// direction of travel. Walking one block forward and one back with the
// ground three blocks ahead (`--walk`, GI-DESIGN.md "Measuring motion")
// pulled a feathered grey trail several blocks out of a pit shadow.
//
// The bisect: with accumulation switched off entirely the walked and
// warped frames agree to **mean 0.001 over 0.00% of pixels** — the voxel
// window and both probe cascades are bit-identical either way, so none of
// it was upstream. At N_MAX = 16 the smear is simply gone and the crop
// matches the still frame. At 512 it is severe. There is no N that is both
// sharp under motion and clean at rest, and that is precisely the trade a
// spatial filter removes: it buys back the samples a short history gives
// up, so the history no longer has to be the only estimator.
//
// The engine keeps a LONG history anyway, because the cap that shortens it
// is driven by reprojection travel (see gi_resolve.wgsl) and travel is
// exactly zero when the camera holds still. So a still camera still
// converges 512 frames deep with the filter switched off underneath it,
// and only a moving one pays.
//
// Two signals, deliberately still separate: sun visibility is near-binary
// and high-frequency, indirect is smooth. They are filtered through the
// same kernel but their own sigmas, which is the whole reason SVGF and
// Lumen both keep them apart.
//
// NO MOMENT BUFFER. SVGF estimates variance by accumulating first and
// second moments of luminance in a third target. This pass does not need
// to: both estimators here have an ANALYTIC variance from data already
// stored. Sun visibility is a mean of n Bernoulli trials, so its standard
// error is exactly sqrt(p(1-p)/n) with p and n both in the target; the
// indirect term is a Monte Carlo mean whose error scales as 1/sqrt(n)
// about its own magnitude. Deriving sigma instead of measuring it costs a
// whole ping-ponged rgba32f target and a pass of bandwidth, and cannot
// drift out of step with the accumulator the way a separate buffer can.

struct Atrous {
	// x tap stride in texels (1, then 2), y unused.
	step: vec4f,
}

@group(0) @binding(0) var<uniform> at: Atrous;
@group(0) @binding(1) var src_indirect: texture_2d<f32>;
@group(0) @binding(2) var src_sun: texture_2d<f32>;

// The a-trous kernel, 3x3 separable, and the reason the stride doubles
// instead of the kernel growing: two passes at strides 1 and 2 cover 7x7
// for 18 taps. SVGF specifies the 5x5 B3 spline, reaching 11x11 for 50; it
// was built and measured here, and it buys a walk-regime residual of 0.522
// against this one's 0.578 for 0.31 ms against 0.14 — a tenth of the
// remaining error for twice the cost, at a point where the artifact it
// exists to remove is already gone from the picture in both. The wider
// kernel is the one to reach for if the history floor ever drops.
const H = array<f32, 3>(0.25, 0.5, 0.25);
const R = 1;

// Above this the temporal estimate has already converged and the filter is
// measurably a no-op — the near ground, where n sits at N_MAX, showed a
// mean change of 0.00-0.08 with it running. Skipping it there is free at
// rest, which is most of the time a player spends looking at anything.
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
	// Metadata rides through untouched: view distance, the exact face code
	// and the history length are what the NEXT frame validates and filters
	// against, and a filtered code is not a code.
	o.indirect = ci;
	o.sun = cs;
	let code = u32(cs.g);
	// Sky. It carries no lighting and must keep the sentinel code 255 that
	// loses every weight in the scene's bilateral upsample.
	if code > 5u {
		return o;
	}
	let n = max(cs.a, 1.0);
	if n >= N_CONVERGED {
		return o;
	}
	// The analytic sigmas — see the header. Both shrink as 1/sqrt(n), so a
	// pixel the temporal filter has converged admits almost nothing and the
	// picture is left alone; a pixel one frame old admits its whole
	// neighbourhood. That is the entire adaptivity, and it needs no motion
	// test because n already carries one.
	let p = clamp(cs.r, 0.0, 1.0);
	let sun_sigma = max(sqrt(p * (1.0 - p) / n), SUN_SIGMA_MIN);
	let cl = lum(ci.rgb);
	let ind_sigma = max(cl / sqrt(n), IND_SIGMA_MIN);
	let step = i32(at.step.x);
	let dtol = DEPTH_K * at.step.x * max(ci.w, 1.0);

	var w_ind = H[R] * H[R];
	var w_sun = w_ind;
	var a_ind = ci.rgb * w_ind;
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
			// rasterizes this code exactly, so this is a hard reject and not
			// a weight — which is the same reason chunk.wgsl's bilateral
			// upsample can compare it bit-for-bit.
			if u32(ts.g) != code || abs(ti.w - ci.w) > dtol {
				continue;
			}
			let h = H[dx + R] * H[dy + R];
			// Edge stopping on the signals themselves. Without this the
			// kernel blurs straight across a shadow boundary on a flat
			// floor, where depth and normal are identical and only the
			// lighting differs — which is most of what there is to protect
			// in a world made of flat floors.
			let ws = h * exp(-abs(ts.r - cs.r) / (SIGMA_L * sun_sigma));
			let wi = h * exp(-abs(lum(ti.rgb) - cl) / (SIGMA_L * ind_sigma));
			w_sun += ws;
			a_sun += ts.r * ws;
			a_wat += ts.b * ws;
			w_ind += wi;
			a_ind += ti.rgb * wi;
		}
	}
	o.indirect = vec4f(a_ind / w_ind, ci.w);
	o.sun = vec4f(a_sun / w_sun, cs.g, a_wat / w_sun, cs.a);
	return o;
}
