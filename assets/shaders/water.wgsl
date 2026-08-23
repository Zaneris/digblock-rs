// Water shader — fork of chunk.wgsl with the shared two-word vertex: word 0 is the solid geometry
// packing, word 1 the texture layer with the per-vertex surface payload above it (drop code,
// falling flag, flow dir, splash code, fall distance).
//
// The drop code sinks a vertex below its lattice height by the strongest water amount at that
// CORNER, so adjacent cells share exactly the level the sim computed and there are no cracks.
//
// Lighting is RTGI: a probe gather at the surface world position evaluated PER VERTEX and
// interpolated (R17 — water quads are strictly per-cell against a probe field spaced wider and
// smooth by construction), plus a short per-fragment sun DDA, which stays per pixel because shadow
// edges on the surface are the one lighting signal sharper than the lattice.
//
// Water never reads the half-res resolve buffers, whose depth belongs to the solids BEHIND it. **It
// writes OPAQUE** — the fragment owns its own backdrop (see the alpha note in `fs`).

// The Frame struct + group(0) binding(0) come from frame_common.wgsl (prepended by digblock-assets,
// before sky_common). The water pipelines extend group 0 with the scene targets: the pre-water
// solid depth copy and the offscreen scene color, read for column thickness, absorption, soft
// shores, refraction and the SSR march. The depth arrives as an R32Float color (see
// depth_blit.wgsl — WebGPU forbids copying into a depth texture), so loads return vec4.
@group(0) @binding(1) var scene_depth: texture_2d<f32>;
@group(0) @binding(2) var scene_tex: texture_2d<f32>;
@group(0) @binding(3) var scene_smp: sampler;

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

// Group 2 is the shared GI world data, declared by gi_common.wgsl
// (prepended between sky_common and this file).

// The sun DDA's step cap: water covers big screen areas, so it affords
// a shorter march than the resolve's — shore/cliff shadows within this
// many blocks of the surface; beyond, the probes' indirect term
// carries what's left (P5 revisits the number against the bench).
const SUN_STEPS = 12;

// Below this much RADIANCE a term is not worth a ray march. An IMPORTANCE threshold, not a quality
// knob: the error it admits is exactly `weight × (what the march would have returned)`, and the
// gate multiplies the weight by the light colour before comparing, so it is in the pixel's own
// units. That weighting is load-bearing: the engine has no tonemap, so radiance is HDR and a small
// weight against a bright sky is a proportionally larger error. Bare weights banded DIG_RIVER.
const SUN_MIN_L = 0.002;
// The mirror gate gets NO tolerance, for the same reason. `mirror_w` is
// pinned to EXACTLY zero on the underside of a surface seen from below
// (`below`), so skipping there is exact rather than approximate — and
// that is where the cost this gate retires sat. Anything above zero is a
// real reflection against an HDR sky, and gets marched.
const MIRROR_MIN_W = 0.0;

// Cost stubs (DIG_STUB; renderer.rs's `stub` module owns the names and the bit numbers, and this is
// the mirror). `frame.misc.z` is a UNIFORM, so every branch below is uniform: an un-stubbed run
// pays nothing and a stubbed one cannot diverge inside a wave, which is what makes the difference
// between two runs the term and only the term.
const STUB_WVERT = 1u;
const STUB_WSUN = 2u;
const STUB_WSSR = 4u;
const STUB_WRIPPLE = 8u;
const STUB_WREFRACT = 16u;
const STUB_WFLAT = 32u;

fn stubbed(bit: u32) -> bool {
	return (u32(frame.misc.z) & bit) != 0u;
}

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

// Surface drop per code, in levels — LINEAR, and that is load-bearing: d is DROP_LEVELS minus the
// strongest water amount at the corner, so d / DROP_LEVELS puts the vertex at exactly the level the
// sim computed. Not the old hand-tuned concave curve, which fitted corners taking the MEAN and
// falsified the codes once they took the max. A zero code pins full height, the full code is the
// cascade plunge; divided, not tabulated, so the mapping cannot drift from what the mesher packs.
const DROP_LEVELS = 9.0;

struct VsOut {
	// Invariant: the depth prepass and color pass run this vs in two different pipelines, and the
	// color pass depth-tests LessEqual against the prepass result. Without @invariant the compiler
	// may contract the position math differently per pipeline (the depth pipeline strips every
	// varying, changing the optimization context), and an ulp of divergence fails the depth test —
	// water full of view-dependent holes.
	@invariant @builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) @interpolate(flat) direct: f32,
	// Probe-field indirect, gathered in the vertex shader (see header) and
	// perspective-interpolated across the cell-sized quad.
	@location(3) indirect: vec3f,
	@location(5) view_dist: f32,
	// Splash code: highest on the impact cell, stepping down through the
	// surrounding cells to zero for no splash at all.
	@location(7) @interpolate(flat) splash: u32,
	// World position (ripple field domain + view vector + GI gather).
	@location(8) world: vec3f,
	@location(9) @interpolate(flat) ni: u32,
	@location(10) @interpolate(flat) flow: u32,
	@location(11) @interpolate(flat) falling: u32,
	// Blocks fallen below the lip, continuous down a sheet (top-plane
	// verts carry the cell's count, bottom-plane one more). Saturates at
	// the ceiling of the field the mesher packs it into.
	@location(12) fall_y: f32,
	// Interpolated drop code (zero is full height, full is the plunge) —
	// the lip highlight keys on the plunging edge of a fall's crest.
	@location(13) drop: f32,
}

// 1D value noise (smoothstep-interpolated hash) — waterfall streaks.
fn vnoise1(x: f32) -> f32 {
	let i = floor(x);
	let f = x - i;
	let a = sky_hash13(vec3f(i, 3.7, 9.1));
	let b = sky_hash13(vec3f(i + 1.0, 3.7, 9.1));
	return mix(a, b, f * f * (3.0 - 2.0 * f));
}

// Flow-dir codes as xz vectors; the first entry is still water.
const FLOW_DIRS = array<vec2f, 5>(
	vec2f(0.0, 0.0),
	vec2f(1.0, 0.0),
	vec2f(-1.0, 0.0),
	vec2f(0.0, 1.0),
	vec2f(0.0, -1.0),
);

// Analytic ripple slope field: three sine octaves summed as a height
// gradient — pure ALU, no texture fetches. `boost` scales amplitude
// (impact turbulence hook); octaves fade out with distance and the
// whole field rolls off so far water can't alias into pixel shimmer.
fn ripple_grad(p: vec2f, t: f32, flow: vec2f, boost: f32, dist: f32) -> vec2f {
	// Wave directions: aligned to the current, with the other two fanned
	// to either side of it, when the cell flows; fixed spread directions
	// on still water.
	var d0 = vec2f(0.80, 0.60);
	var d1 = vec2f(-0.71, 0.71);
	var d2 = vec2f(0.97, -0.24);
	if dot(flow, flow) > 0.5 {
		d0 = flow;
		d1 = vec2f(0.766 * flow.x - 0.643 * flow.y, 0.643 * flow.x + 0.766 * flow.y);
		d2 = vec2f(0.766 * flow.x + 0.643 * flow.y, -0.643 * flow.x + 0.766 * flow.y);
	}
	// Slope amplitudes, frequencies (rad/block), phase speeds: a rough
	// spectrum — long waves carry the most slope, short ones detail.
	let s = vec3f(0.10, 0.06, 0.035) * boost;
	let f = vec3f(0.9, 1.9, 4.1);
	let spd = vec3f(1.2, 1.7, 2.6);
	let fade = vec3f(
		1.0,
		1.0 - smoothstep(48.0, 120.0, dist),
		1.0 - smoothstep(16.0, 48.0, dist),
	) / (1.0 + dist * 0.015);
	// Advect phase with the top-face surface scroll so ripples ride the
	// current instead of swimming against it.
	let q = p - flow * (t * 0.4);
	var g = d0 * (s.x * fade.x * cos(dot(d0, q) * f.x + t * spd.x));
	g += d1 * (s.y * fade.y * cos(dot(d1, q) * f.y + t * spd.y));
	g += d2 * (s.z * fade.z * cos(dot(d2, q) * f.z + t * spd.z));
	return g;
}

// Reflected environment along `ray` from `origin`: a screen-space march over the pre-water solid
// depth (desktop, water_misc.y), with the procedural sky (sky_base + stars) as the always-available
// fallback — so shoreline blocks, trees and cliffs mirror where the screen knows them, and open
// water still mirrors the actual sky. The sun/moon discs are deliberately absent: the analytic
// specular lobe plays their role on the rippled surface.
fn reflection_color(origin: vec3f, ray: vec3f, dist: f32, weight: f32) -> vec3f {
	var sky = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	sky += vec3f(sky_stars(ray)) * frame.sun_dir.w * smoothstep(0.0, 0.3, ray.y);
	// The analytic fallback is UNPROVEN sky — an underground pool must
	// not mirror the daytime dome through its ceiling (R32; the SSR march
	// below is real screen data and needs no fade). Saturates to 1 at the
	// ramp's top edge, so open water is bit-identical.
	sky *= medium_sky_fade(origin.y);
	// `weight` is the Fresnel blend this result is about to be mixed
	// with, and the march is gated on it for the same reason the sun
	// DDA above is gated on its own: it was running at full cost where
	// it cannot show. Submerged, looking at the underside of a surface,
	// `mirror_w` is pinned to EXACTLY zero — and `reflection_color` is
	// called as a function argument, so the march's dependent depth loads
	// ran anyway and the answer was multiplied by nothing. That alone was a
	// substantial slice of the water pass on the submerged pose (BENCH.md
	// holds the figure).
	// Above water it retires the other degenerate case: head-on, Schlick
	// bottoms out at its F0 floor, so a look straight down at calm water
	// was paying the full march for a sliver of a pixel.
	if weight <= MIRROR_MIN_W || frame.water_misc.y < 0.5 || stubbed(STUB_WSSR) {
		return sky;
	}
	// World-space steps with a growing stride, projected per step; a depth crossing within a
	// thickness bound is a hit. The stride scales with the fragment's distance (its pixel
	// footprint), so a fixed tap count resolves fine shoreline detail up close AND reaches the far
	// shore from open water, where the reflection is low-frequency and coarse hits look right.
	// Explicit-LOD fetches are legal in this varying branch; the march is gated by a uniform flag.
	let dims = vec2f(textureDimensions(scene_depth));
	var t_step = 0.4 * (1.0 + dist * 0.04);
	var t = t_step * 2.0; // skip immediate self-intersection
	for (var i = 0; i < 28; i++) {
		let p = origin + ray * t;
		let clip = frame.view_proj * vec4f(p, 1.0);
		if clip.w <= 0.0 {
			break;
		}
		let ndc = clip.xyz / clip.w;
		if abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0 {
			break;
		}
		let uv = vec2f(ndc.x, -ndc.y) * 0.5 + 0.5;
		let d = textureLoad(scene_depth, vec2i(uv * (dims - 1.0)), 0).x;
		if d < ndc.z {
			// Solid in front of the ray point. Reject silhouette
			// false-hits where the surface is far behind the ray.
			let wp = frame.inv_view_proj * vec4f(ndc.xy, d, 1.0);
			if distance(wp.xyz / wp.w, p) < t_step * 4.0 {
				let hit = textureSampleLevel(scene_tex, scene_smp, uv, 0.0).rgb;
				// Fade to sky at screen edges so misses never pop.
				let edge = min(1.0 - abs(ndc.x), 1.0 - abs(ndc.y));
				return mix(sky, hit, smoothstep(0.0, 0.15, edge));
			}
		}
		t += t_step;
		t_step *= 1.12;
	}
	return sky;
}

// Face normal perturbed by the ripple gradient: top faces get the
// full height-field normal, side/bottom faces the gradient component
// tangent to the face at half strength.
fn ripple_normal(ni: u32, g: vec2f) -> vec3f {
	if ni == 4u {
		return normalize(vec3f(-g.x, 1.0, -g.y));
	}
	let n = NORMALS[ni];
	let perturb = vec3f(-g.x, 0.0, -g.y);
	return normalize(n + (perturb - n * dot(perturb, n)) * 0.5);
}

// See chunk.wgsl: exact twelfths → blocks decode.
fn twelfths(v: u32) -> f32 {
	return f32(v / 12u) + f32(v % 12u) / 12.0;
}

// The one world-position decode, shared by the color and depth-prepass
// vertex entries so their @invariant positions are built from the same
// expressions — the LessEqual depth test between the two pipelines
// depends on it (see VsOut.pos).
fn water_world(w0: u32, w1: u32, origin: vec3f) -> vec3f {
	// Geometry decode identical to chunk.wgsl: twelfth-block units, with
	// the whole-block part split off in integer space for exactness.
	let x = twelfths(w0 & 0xffu);
	var y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	// Surface drop: the shared corner code, straight through DROP_LEVELS.
	// The water payload sits above the layer field in w1.
	y -= f32(min((w1 >> 10u) & 0xfu, 9u)) / DROP_LEVELS;
	return vec3f(x, y, z) + origin;
}

// Location 1 is the instance-step chunk origin — see chunk.wgsl.
@vertex
fn vs(@location(0) packed: vec2u, @location(1) origin: vec4f) -> VsOut {
	let w0 = packed.x;
	let w1 = packed.y;
	let ni = (w0 >> 24u) & 7u;
	let n = NORMALS[ni];
	let world = water_world(w0, w1, origin.xyz);

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	// Raw corner uv (w0 corner code) — all scroll/advection happens in fs.
	o.uv = vec2f(f32((w0 >> 29u) & 1u), f32((w0 >> 30u) & 1u));
	o.splash = (w1 >> 18u) & 3u;
	o.layer = w1 & 0x3ffu;
	o.world = world;
	o.ni = ni;
	o.flow = (w1 >> 15u) & 7u;
	o.falling = (w1 >> 14u) & 1u;
	o.fall_y = f32((w1 >> 20u) & 0xfu);
	o.drop = f32(min((w1 >> 10u) & 0xfu, 9u));

	o.direct = max(dot(n, -frame.light_dir.xyz), 0.0);
	o.view_dist = distance(world, frame.cam_pos.xyz);
	// The probe gather, hoisted out of the fragment shader (R17). The
	// blend's own coverage checks keep this cheap where it doesn't
	// matter: a vertex outside both cascades (all water beyond the
	// cascades' reach) early-outs to the analytic dome before touching a
	// texel, so only near water — the fragments that used to pay the
	// whole blend EACH — pays the full path, once per corner instead of
	// once per pixel.
	o.indirect = vec3f(0.25);
	if !stubbed(STUB_WVERT) {
		o.indirect = gi_indirect(
			i32(frame.gi_misc.x),
			world,
			n,
			frame.cam_pos.xyz,
			frame.gi0,
			frame.gi1,
			frame.zenith.rgb,
			frame.horizon.rgb,
			vec2i(4, 2),
		);
	}
	return o;
}

struct VsDepthOut {
	// Same invariance contract as VsOut.pos — see there.
	@invariant @builtin(position) pos: vec4f,
}

// Depth-prepass vertex entry: position only, so the prepass rasterizes
// the same geometry without paying the vs probe gather twice. Its
// position comes from the same water_world decode and the same
// @invariant multiply as the color pass — the LessEqual test between
// the two pipelines tolerates nothing less.
@vertex
fn vs_depth(@location(0) packed: vec2u, @location(1) origin: vec4f) -> VsDepthOut {
	var o: VsDepthOut;
	o.pos = frame.view_proj * vec4f(water_world(packed.x, packed.y, origin.xyz), 1.0);
	return o;
}

// Depth-prepass fragment entry: color writes masked off, recording only
// the nearest water depth so the color pass blends a single layer.
@fragment
fn fs_depth() -> @location(0) vec4f {
	return vec4f(0.0);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	// The raster + vertex floor: everything below removed, so what a run
	// with this bit still costs is geometry, binning and the vertex
	// gather, which no in-shader stub of a fragment term can isolate.
	if stubbed(STUB_WFLAT) {
		return vec4f(0.1, 0.2, 0.3, 1.0);
	}
	let t = frame.light_color.w;
	// Motion per face: falling sides run downward, flowing tops along the
	// flow dir (sample offset opposes feature motion). rate × len =
	// speed; len bounds how far a layer slides.
	let vertical = i.ni == 3u || i.ni == 4u;
	// Blocks fallen at this pixel (the bias tames the lip singularity).
	let y_fallen = i.fall_y + 0.5;
	var base_uv = i.uv;
	var adv = vec2f(0.0);
	var rate = 0.0;
	var len = 0.0;
	if i.falling == 1u && !vertical {
		// Gravity stretch: v follows free-fall time of flight (√y), so
		// features elongate and their world speed grows ∝ √y down the
		// sheet. Past the packed fall distance's cap the texture smears,
		// hidden under full aeration whitening by then.
		base_uv = vec2f(i.uv.x, sqrt(y_fallen) * 2.2);
		adv = vec2f(0.0, -1.0);
		rate = 0.25;
		len = 6.0;
	} else if i.ni == 4u && i.flow != 0u {
		adv = -FLOW_DIRS[min(i.flow, 4u)];
		rate = 0.10;
		len = 4.0;
	}
	// Two-phase advection (Max/Becker): two layers half a period
	// apart, each visible only around the middle of its slide, so flow
	// reads as endless motion without the conveyor-belt repeat of an
	// unbounded scroll. Static faces collapse to layer A (wb = 0).
	// Both samples stay outside control flow (derivative rules).
	let pa = fract(t * rate);
	let pb = fract(t * rate + 0.5);
	let uv_a = fract(base_uv + adv * (pa * len));
	let uv_b = fract(base_uv + adv * (pb * len) + vec2f(0.37, 0.61));
	let wb = abs(2.0 * pa - 1.0) * min(len, 1.0);
	let tex = mix(
		textureSample(blocks_tex, blocks_smp, uv_a, i.layer).rgb,
		textureSample(blocks_tex, blocks_smp, uv_b, i.layer).rgb,
		wb,
	);
	// Aeration: falling water entrains air and whitens with distance
	// fallen — glassy at the lip, diffuse froth at the base. This is
	// scattering AT the surface, not absorption through the body, so it
	// survives the demolition that took the depth tint: the surface
	// texture now only ever reaches the eye through this term.
	let aer = smoothstep(0.5, 6.0, y_fallen) * f32(i.falling);
	let froth_tex = mix(tex, vec3f(0.93, 0.96, 1.0), 0.55);
	// Falling-sheet structure: vertical streak noise tears the sheet
	// into ropes with translucent gaps, and clump pulses ride the
	// stretched v — two layers (front fast/bright, back slow/dim) give
	// the sheet depth. s is neutral at its midpoint.
	var s = 0.5;
	if i.falling == 1u && !vertical {
		let h = select(i.world.x, i.world.z, i.ni == 0u || i.ni == 5u);
		let front = 0.6 * vnoise1(h * 3.0) + 0.4 * vnoise1(h * 7.0 + 13.7);
		let back = 0.6 * vnoise1(h * 3.0 + 31.1) + 0.4 * vnoise1(h * 7.0 + 47.3);
		let cf = smoothstep(0.6, 1.0, sin(base_uv.y * 2.5 - t * 5.0 + front * 6.28));
		let cb = smoothstep(0.6, 1.0, sin(base_uv.y * 1.5 - t * 3.0 + back * 6.28));
		s = mix((back + cb * 0.5) * 0.6, front + cf * 0.5, clamp(front, 0.0, 1.0));
	}
	// Rippled normal + Schlick fresnel. Impact turbulence: splash cells
	// churn harder.
	let boost = 1.0 + 0.5 * f32(i.splash);
	var g = vec2f(0.0);
	if !stubbed(STUB_WRIPPLE) {
		g = ripple_grad(i.world.xz, t, FLOW_DIRS[min(i.flow, 4u)], boost, i.view_dist);
	}
	let nrm = ripple_normal(i.ni, g);
	let vdir = normalize(frame.cam_pos.xyz - i.world);
	let mirror = 1.0 - 0.7 * aer;
	let fres = (0.02 + 0.98 * pow(1.0 - abs(dot(vdir, nrm)), 5.0)) * mirror;
	// Glint from the active caster (sun by day, moon by night) on the
	// rippled normal. The exponent widens with distance so the glitter
	// path stays stable instead of aliasing into per-pixel shimmer;
	// (e+2) keeps lobe energy roughly constant as it widens. Fresnel
	// at the half vector: bright at low sun, subtle from above.
	let e = mix(240.0, 24.0, smoothstep(8.0, 120.0, i.view_dist));
	let hs = normalize(vdir - frame.light_dir.xyz);
	let fres_h = 0.02 + 0.98 * pow(1.0 - max(dot(vdir, hs), 0.0), 5.0);
	let spec = pow(max(dot(nrm, hs), 0.0), e) * (e + 2.0) / 242.0 * fres_h;
	// Water column thickness along the view ray: reconstruct the solid
	// scene point behind this fragment from the pre-water depth copy
	// (exact via inv_view_proj — no projection assumptions). Kept for
	// the refraction offset and the shore foam line, which are geometry;
	// the Beer-Lambert tint it used to drive is M2's medium now.
	let dims = vec2f(textureDimensions(scene_depth));
	// Underside view (submerged camera, back of the face): the "scene
	// behind" is the sky above the surface, so the reconstructed
	// thickness is bogus (~far plane). The water column is behind the
	// camera, not behind the surface — treat the pane as thin. Hoisted
	// above the thickness block because `mirror_w` needs it either way.
	let below = frame.water_misc.x > 0.5 && dot(vdir, NORMALS[i.ni]) < 0.0;
	var thick = 0.0;
	var suv = i.pos.xy / dims;
	if !stubbed(STUB_WREFRACT) {
		let d_scene = textureLoad(scene_depth, vec2i(i.pos.xy), 0).x;
		let ndc = (i.pos.xy / dims * 2.0 - 1.0) * vec2f(1.0, -1.0);
		let wp = frame.inv_view_proj * vec4f(ndc, d_scene, 1.0);
		let scene_dist = distance(wp.xyz / wp.w, frame.cam_pos.xyz);
		thick = select(max(scene_dist - i.view_dist, 0.0), 0.0, below);
		// Refraction: sample the scene through a ripple-perturbed offset
		// that grows with thickness (clamped so shores don't smear), and
		// falls back to the straight sample when the offset ray would pull
		// something in FRONT of the water across the surface.
		let off = g * (0.10 * min(thick, 1.5));
		let suv2 = clamp(suv + off, vec2f(0.0), vec2f(1.0));
		let d_off = textureLoad(scene_depth, vec2i(suv2 * (dims - 1.0)), 0).x;
		if d_off >= i.pos.z {
			suv = suv2;
		}
	}
	// What is behind the surface, seen through the water body: transport computed rather than
	// tuned, over the reconstructed column. Water tints by DEPTH because red is absorbed an order
	// of magnitude harder than blue (medium_common's measured sigma), not because a constant said
	// which way to lean. Below, the contact foam line where water meets ground — meaningless on the
	// underside, where `thick` is pinned to zero and would foam everything.
	let shore = (1.0 - smoothstep(0.05, 0.45, thick)) * f32(!below);
	// Glassy crest sheen where flow tips over a lip: strongest in the
	// first fallen block, keyed to the plunging drop edge.
	let lip = f32(i.falling) * (1.0 - min(i.fall_y, 1.0)) * smoothstep(6.0, 9.0, i.drop);
	// RTGI: probe-field indirect at the surface, per VERTEX and interpolated (see the header), plus
	// a short per-fragment sun DDA. `sh` multiplies foam and specular, never the indirect — a torch
	// has no sun glint lobe to borrow. The DDA is GATED ON ITS OWN COEFFICIENT, the biggest saving
	// in the pass: water has no diffuse sun response, so sun reaches the pixel only through foam,
	// glint and lip sheen — all ~0 on calm open water or any surface seen from below.
	let side = i32(frame.gi_misc.x);
	let indirect = i.indirect;
	let lc = max(frame.light_color.r, max(frame.light_color.g, frame.light_color.b));
	let sun_w = lc * max(
		// Foam takes a fixed share of the light; the mixes carry their own
		// weights.
		0.8 * max(aer, max(shore * 0.25, 0.09 * f32(i.splash))),
		max(spec * 2.0 * mirror, lip * 0.15),
	);
	var sv: GiSunVis;
	sv.vis = 1.0;
	sv.water = 0.0;
	sv.dist = 0.0;
	sv.thru = vec3f(1.0);
	if sun_w > SUN_MIN_L && !stubbed(STUB_WSUN) {
		sv = gi_sun_vis(side, frame.gi_misc.y, frame.cam_pos.xz, frame.gi_sun, i.world, SUN_STEPS);
	}
	// Strength fades the LIGHT, never the shadow — see chunk.wgsl.
	// `thru` is the glass filter the ray crossed (op 4).
	let sh = sv.vis * medium_water_tr(sv.water) * sv.thru * frame.light_dir.w;
	// Froth light: the same direct+indirect mix the spray billboards
	// use (spray.wgsl), shadowed like the surface it sits on — so foam
	// dims with the night instead of glowing unlit white.
	let froth_light = frame.light_color.rgb * 0.8 * sh + indirect;
	var col = textureSampleLevel(scene_tex, scene_smp, suv, 0.0).rgb;
	let vray = -vdir;
	// The down-welling source fades with depth (R32), exactly as
	// medium_view's submerged branch does — an underground pool's
	// backdrop must not in-scatter the daytime zenith.
	col = medium_water_seg(
		col,
		medium_water_light(
			frame.zenith.rgb * medium_sky_fade(i.world.y),
			max(-vray.y, 0.0) * thick * 0.5,
		),
		thick,
	);
	// How much of this pixel is still the refracted BACKDROP, which already carries chunk.wgsl's
	// medium the whole way from the camera — fogging it again hazes distant water about twice as
	// hard as the shore beside it, a visible step at the waterline. Only the surface's own light
	// (froth, mirror, glint) has not travelled yet, and every term below is a `mix`, so one scalar
	// tracks the split exactly.
	var backdrop = 1.0;
	let backdrop_col = col;
	// Churned water is full of bubbles and reads white whatever the
	// depth behind it; the mirror terms damp out over the same ramp.
	col = mix(col, froth_tex * froth_light, aer);
	backdrop *= 1.0 - aer;
	col = mix(col, vec3f(0.95, 0.97, 1.0) * froth_light, shore * 0.25);
	backdrop *= 1.0 - shore * 0.25;
	// Reflected sky, fresnel-blended: water mirrors the horizon at
	// grazing angles and stays see-through head-on. SSR rides inside
	// `reflection_color` and is explicitly staying.
	var rr = reflect(-vdir, nrm);
	rr.y = max(rr.y, 0.02);
	let mirror_w = fres * select(1.0 - 0.7 * frame.water_misc.x, 0.0, below);
	col = mix(col, reflection_color(i.world, normalize(rr), i.view_dist, mirror_w), mirror_w);
	backdrop *= 1.0 - mirror_w;
	// Splash foam (after the mirror mix — froth sits on the surface):
	// a STATIC wash, brightest on the impact cell and fading with the
	// splash code out to the edge of the code's reach. Deliberately
	// timeless. This was an expanding ring keyed to a fract of the clock
	// plus a wobble on the impact churn, and the two together read as the
	// impact strobing white on a loop — the splash's motion belongs in
	// the droplets (spray.wgsl), which throw upward, not in the pool
	// surface brightening and dimming under them. The ripple normals still
	// churn harder here (`boost` above), so the foam has movement in it
	// without the whole cell pulsing.
	if i.splash > 0u {
		let foam_w = 0.09 * f32(i.splash) * (1.0 - 0.5 * i.uv.y);
		col = mix(col, froth_light, foam_w);
		backdrop *= 1.0 - foam_w;
	}
	col += frame.light_color.rgb * spec * sh * 2.0 * mirror;
	col += frame.light_color.rgb * lip * sh * 0.15;
	// Alpha: the falling-sheet streak gate, and nothing else. This fragment sampled the scene
	// itself and ran the whole water transport across `thick` of it, so blending it over the same
	// backdrop again re-adds an UNABSORBED copy — the whole picture under a bay's worth of water.
	// The streak gaps stay: a torn falling sheet is genuinely see-through and `thick`, rebuilt from
	// depth, cannot know about the tear; `s` is neutral on a still surface, so this is exactly 1.0.
	let alpha = clamp(0.75 + 0.5 * s, 0.0, 1.0);
	// The camera-to-surface medium, applied to the surface's own light only (see `backdrop` above).
	// Every term above is a linear mix, so one scalar tracks coverage exactly: peel the backdrop
	// off, normalise what remains to a full-coverage radiance, fog THAT, and re-weight. Fogging the
	// composite instead would in-scatter a full pixel's worth of haze behind a pane that is mostly
	// see-through.
	let cover = max(1.0 - backdrop, 1e-4);
	let back = backdrop_col * backdrop;
	let sky_in = sky_base(vray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	let surf = medium_view(
		(col - back) / cover,
		frame.cam_pos.xyz,
		i.world,
		sky_in,
		frame.zenith.rgb,
		frame.water_misc,
		frame.misc.y,
	) * cover;
	return vec4f(back + surf, alpha);
}
