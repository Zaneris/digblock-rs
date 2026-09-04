// Water shader — fork of chunk.wgsl with the shared two-word vertex: word 0 is the solid geometry
// packing, word 1 the per-vertex surface payload (drop code, falling flag, flow dir, splash code,
// fall distance) above the ten-bit field the solid stream uses for its texture layer and water
// leaves zero. Water samples no texture: its surface is bed, mirror and glint.
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
// The per-vertex gust field → 1: one amplitude everywhere, the pre-wind surface.
const STUB_WGUST = 1048576u;

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
	// Gust amplitude at the corner: a 24-block field on a 1-block quad, so per-vertex is exact
	// (R17 again) and free per pixel.
	@location(1) gust: f32,
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

// The gust at a surface point, evaluated per VERTEX: the fbm's lane sits in ~[0.2, 0.8], and
// the map stretches that band to ~0.35..1.4, calm patches beside rough ones, drifting on the wind.
fn gust_field(xz: vec2f, t: f32) -> f32 {
	let n = sky_fbm2((xz - sky_wind_drift(t)) / 24.0, 11u).x;
	return mix(0.35, 1.4, smoothstep(0.2, 0.8, n));
}

fn turn(d: vec2f, c: f32, s: f32) -> vec2f {
	return vec2f(c * d.x - s * d.y, s * d.x + c * d.y);
}

// The heading still water's waves run under; a flowing cell's current stands in for it.
const WIND_DIR = vec2f(0.8, 0.6);
// Derivative drag (afl_ext's ocean): each wave shifts the sample point by its own slope, so the
// finer waves bunch up on the crests of the coarser ones, which is what reads as water.
const WAVE_DRAG = 0.38;
// Per wave: the direction's turn off the wind (cos, sin) and a phase offset. Hand-spread
// within about ±40°, the swell (first) dead on the wind, so no two waves share a heading.
const WAVES = array<vec3f, 10>(
	vec3f(1.000, 0.000, 0.0),
	vec3f(0.853, 0.523, 2.1),
	vec3f(0.814, -0.581, 4.4),
	vec3f(0.955, 0.296, 1.3),
	vec3f(0.976, -0.218, 5.6),
	vec3f(0.778, 0.629, 3.0),
	vec3f(0.900, -0.435, 0.7),
	vec3f(0.993, 0.120, 4.9),
	vec3f(0.765, -0.644, 2.6),
	vec3f(0.921, 0.389, 5.9),
);

// Analytic wave slope field: a dense sum of exp(sin) waves (peaked crests, broad troughs) spread
// about the wind, frequencies growing 1.3x per wave, under derivative drag. Pure ALU. `boost`
// scales amplitude (impact turbulence), `gust` is the per-vertex wind field. Each wave fades out
// past ~10 of its own wavelengths, so far water keeps the swell and loses the ripples before they
// can alias into pixel shimmer; the loop ends at the first wave that has faded.
fn ripple_grad(p: vec2f, t: f32, flow: vec2f, boost: f32, dist: f32, gust: f32) -> vec2f {
	let flowing = dot(flow, flow) > 0.5;
	let wind = select(WIND_DIR, flow, flowing);
	// Advect with the top-face surface scroll so ripples ride the current instead of swimming
	// against it.
	var pos = p - flow * (t * 0.4);
	var g = vec2f(0.0);
	var freq = 0.35;
	var spd = 0.6;
	var w = 1.0;
	let amp = 0.04 * boost * gust / (1.0 + dist * 0.008);
	for (var i = 0u; i < 10u; i++) {
		let len = 6.2832 / freq;
		let fade = 1.0 - smoothstep(10.0 * len, 30.0 * len, dist);
		if fade <= 0.0 {
			break;
		}
		let wv = WAVES[i];
		let d = turn(wind, wv.x, wv.y);
		let x = dot(d, pos) * freq + t * spd + wv.z;
		let e = exp(sin(x) - 1.0);
		let dx = e * cos(x);
		g += d * (dx * w * amp * fade);
		pos -= d * (dx * w * WAVE_DRAG / freq);
		w *= 0.8;
		freq *= 1.3;
		spd *= 1.07;
	}
	return g;
}

// Reflected environment along `ray` from `origin`: a screen-space march over the pre-water solid
// depth (desktop, water_misc.y), with the procedural sky (sky_base + stars) as the always-available
// fallback — so shoreline blocks, trees and cliffs mirror where the screen knows them, and open
// water still mirrors the actual sky. The sun/moon discs are deliberately absent: the analytic
// specular lobe plays their role on the rippled surface.
// Schlick's approximation from a head-on reflectance `f0`; `c` is the cosine
// on the AIR side of the interface.
fn schlick_at(f0: f32, c: f32) -> f32 {
	return f0 + (1.0 - f0) * pow(1.0 - c, 5.0);
}

// The interface's reflectance from above, at water's own head-on value.
fn schlick(c: f32) -> f32 {
	return schlick_at(0.02, c);
}

// The underside's head-on reflectance, a LOOK rather than water's number: the
// surface overhead should read as a mirror without a Snell's window to give
// it an edge, and water's F0 makes it nearly invisible head-on.
const UNDERSIDE_F0 = 0.25;

// The mirror's analytic fallback above the surface, UNPROVEN sky — an
// underground pool must not mirror the daytime dome through its ceiling (R32;
// the SSR march is real screen data and needs no fade). Saturates to 1 at the
// ramp's top edge, so open water is bit-identical.
fn mirror_sky(origin: vec3f, ray: vec3f) -> vec3f {
	var sky = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	// The deck the sky pass marches, flat: the mirror agrees with the sky above it in shape.
	var cover = 0.0;
	if frame.water_misc.w > 0.5 {
		cover = sky_cloud_mirror(origin, ray, frame.light_color.w);
	}
	sky = mix(sky, sky_cloud_col(frame.horizon.rgb, frame.light_color.rgb), cover);
	sky += vec3f(sky_stars(ray)) * frame.sun_dir.w * smoothstep(0.0, 0.3, ray.y) * (1.0 - cover);
	return sky * medium_sky_fade(origin.y);
}

fn reflection_color(origin: vec3f, ray: vec3f, dist: f32, weight: f32, sky: vec3f) -> vec3f {
	// `sky` is the fallback where the march finds nothing. `weight` is the
	// Fresnel blend this result is about to be mixed with, and the march is
	// gated on it for the same reason the sun DDA above is gated on its own:
	// it was running at full cost where it cannot show — head-on, Schlick
	// bottoms out at its F0 floor, so a look straight down at calm water was
	// paying the full march for a sliver of a pixel.
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
	o.world = world;
	o.ni = ni;
	o.flow = (w1 >> 15u) & 7u;
	o.gust = 1.0;
	if !stubbed(STUB_WGUST) {
		o.gust = gust_field(world.xz, frame.light_color.w);
	}
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
	let vertical = i.ni == 3u || i.ni == 4u;
	// Blocks fallen at this pixel (the bias tames the lip singularity).
	let y_fallen = i.fall_y + 0.5;
	var base_uv = i.uv;
	if i.falling == 1u && !vertical {
		// Gravity stretch: v follows free-fall time of flight (√y), so
		// the clump pulses below elongate and their world speed grows
		// ∝ √y down the sheet.
		base_uv = vec2f(i.uv.x, sqrt(y_fallen) * 2.2);
	}
	// Aeration: falling water entrains air and whitens with distance
	// fallen — glassy at the lip, diffuse froth at the base. This is
	// scattering AT the surface, not absorption through the body.
	let aer = smoothstep(0.5, 6.0, y_fallen) * f32(i.falling);
	// Froth base: the old water texture was a flat rgb(64,164,223) blended 0.55 toward off-white,
	// which is this constant. The PNG stays an asset for the bucket icon and the GI albedo LUT.
	let froth_tex = vec3f(0.62, 0.82, 0.94);
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
		g = ripple_grad(i.world.xz, t, FLOW_DIRS[min(i.flow, 4u)], boost, i.view_dist, i.gust);
	}
	let nrm = ripple_normal(i.ni, g);
	let vdir = normalize(frame.cam_pos.xyz - i.world);
	let mirror = 1.0 - 0.7 * aer;
	let fres = schlick(abs(dot(vdir, nrm))) * mirror;
	// Glint from the active caster (sun by day, moon by night) on the
	// rippled normal. The exponent widens with distance so the glitter
	// path stays stable instead of aliasing into per-pixel shimmer;
	// (e+2) keeps lobe energy roughly constant as it widens. Fresnel
	// at the half vector: bright at low sun, subtle from above.
	// The lobe also widens and dims where the wind roughens the surface.
	let gust01 = clamp((i.gust - 0.35) / 1.05, 0.0, 1.0);
	let e = mix(240.0, 24.0, smoothstep(8.0, 120.0, i.view_dist)) * mix(1.3, 0.6, gust01);
	let hs = normalize(vdir - frame.light_dir.xyz);
	let fres_h = schlick(max(dot(vdir, hs), 0.0));
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
	// The point behind the pane, when it is a bed the sample was shaded at (see `bed_known`).
	var bed_known = false;
	var bed_dist = 0.0;
	var bed_y = i.world.y;
	if !stubbed(STUB_WREFRACT) {
		let d_scene = textureLoad(scene_depth, vec2i(i.pos.xy), 0).x;
		let ndc = (i.pos.xy / dims * 2.0 - 1.0) * vec2f(1.0, -1.0);
		let wp = frame.inv_view_proj * vec4f(ndc, d_scene, 1.0);
		let bed = wp.xyz / wp.w;
		bed_dist = distance(bed, frame.cam_pos.xyz);
		thick = select(max(bed_dist - i.view_dist, 0.0), 0.0, below);
		bed_known = !below;
		bed_y = bed.y;
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
	let sky_in = sky_base(vray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	// The down-welling source fades with depth (R32), exactly as
	// medium_view's submerged branch does — an underground pool's
	// backdrop must not in-scatter the daytime zenith.
	let sky_down = frame.zenith.rgb * medium_sky_fade(i.world.y);
	// Light leaves the bed, crosses the water, THEN the air. The sample arrives with chunk.wgsl's
	// air mixed in at the BED, and neither keeping that nor adjusting it survives a grazing bay:
	// kept, the column absorbs it and the sea runs navy to a hard chunk edge; keyed on the bed,
	// the sea wears its seabed's silhouette and a bed beyond the streamed world reads as sky at
	// any range. So peel the bed's own air off, transport, and haze at the SURFACE like the shore.
	if bed_known {
		let a = medium_air_tr(bed_dist);
		let bed_tw = max(col - (1.0 - a) * sky_in * medium_sky_fade(bed_y), vec3f(0.0))
			* medium_water_over_air(thick, bed_dist);
		// `bed_tw` is already through the column, so the column contributes its in-scatter alone.
		col = medium_view(
			bed_tw + medium_water_column(vec3f(0.0), sky_down, 0.0, max(-vray.y, 0.0), thick),
			frame.cam_pos.xyz,
			i.world,
			sky_in,
			frame.zenith.rgb,
			frame.water_misc,
			frame.misc.y,
		);
	}
	// How much of this pixel is still the refracted BACKDROP, which has already been through the
	// medium — above, at the surface, or seen from below the leg chunk.wgsl gave it — so fogging
	// it again hazes distant water about twice as hard as the shore beside it, a visible step at
	// the waterline. Only the surface's own light (froth, mirror, glint) has not travelled yet,
	// and every term below is a `mix`, so one scalar tracks the split exactly.
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
	// `reflection_color` and is explicitly staying. Seen from BELOW, the
	// SAME fresnel, and no Snell's window: the underside mirrors the seabed
	// where the screen knows it and the column's own in-scatter where it
	// does not, never the dome. Real water bends the sky into a cone and
	// turns to a mirror outside it, but a mirror of a floor the screen
	// cannot see is a wall, and any cone at all read as a hole overhead;
	// one continuous surface is the answer that plays.
	var rr = reflect(-vdir, nrm);
	rr.y = select(max(rr.y, 0.02), min(rr.y, -0.02), below);
	rr = normalize(rr);
	let mirror_w = select(
		fres * (1.0 - 0.7 * frame.water_misc.x),
		schlick_at(UNDERSIDE_F0, abs(dot(vdir, nrm))) * mirror,
		below,
	);
	let mirror_fallback = select(
		mirror_sky(i.world, rr),
		medium_water_endless(sky_down, 0.0, -rr.y),
		below,
	);
	col = mix(col, reflection_color(i.world, rr, i.view_dist, mirror_w, mirror_fallback), mirror_w);
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
