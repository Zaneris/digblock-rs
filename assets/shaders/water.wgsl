// Water shader — fork of chunk.wgsl with the shared two-word vertex:
// word 0 is the solid geometry packing (position/normal/corner; ao and
// tint bits unused), word 1 holds the texture layer in bits 0-9 and the
// per-vertex surface payload above it. Bits 10-13 are the drop code d
// (9 − strongest corner amount): the vertex sinks d/9 below its lattice
// height, so a surface corner sits at exactly the water level the sim
// computed there — a source at 8/9, its spread at 4/9, then 2/9 and
// 1/9 — with corner heights shared between adjacent cells (no cracks).
// Bit 14 marks falling water (side faces
// scroll downward), bits 15-17 the horizontal flow dir (0 still, 1 +x,
// 2 −x, 3 +z, 4 −z; top faces scroll along it), bits 18-19 the splash
// code (3 = fall impact cell; 2/1 = pool cells at Chebyshev 1/2 — a
// static foam wash fading outward; all three boost ripple turbulence),
// bits 20-23 the vertex fall distance (blocks below the lip, +1 on
// bottom-plane verts so interpolation is continuous; drives gravity
// stretch and aeration whitening).
// The animation clock rides in frame.light_color.w. Lighting is RTGI
// (GI-DESIGN.md): a direct probe gather at the surface world position
// through gi_common's shared cascade fallthrough, plus a short
// per-fragment sun DDA — water never reads the half-res resolve
// buffers, whose depth belongs to the solids BEHIND it. Alpha comes
// from misc.x (the water frame UBO copy).

// The Frame struct + group(0) binding(0) come from frame_common.wgsl
// (prepended by digblock-assets, before sky_common). The water pipelines
// extend group 0 with the scene targets: the pre-water solid depth copy
// and the offscreen scene color — water column thickness, absorption,
// soft shores, refraction, and the SSR march read these.
// R32Float resolve of the pre-water solid depth (see depth_blit.wgsl —
// WebGPU forbids copying into a depth texture, so it arrives as a
// color format and loads return vec4).
@group(0) @binding(1) var scene_depth: texture_2d<f32>;
@group(0) @binding(2) var scene_tex: texture_2d<f32>;
@group(0) @binding(3) var scene_smp: sampler;

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

// Group 2 is the shared GI world data, declared by gi_common.wgsl
// (prepended between sky_common and this file).

// The sun DDA's step cap: water covers big screen areas, so it affords
// a shorter march than the resolve's 160 — shore/cliff shadows within
// this many blocks of the surface; beyond, the probes' indirect term
// carries what's left (P5 revisits the number against the bench).
const SUN_STEPS = 12;

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

// Surface drop per code, in ninths — LINEAR, and that is load-bearing:
// d is 9 − the strongest water amount at the corner, so d/9 puts the
// vertex at exactly the level the sim computed and nothing else. This
// was a hand-tuned concave curve (0.32/0.50/0.63/0.73/…) from back when
// corners took the MEAN amount and every code bunched up near the top;
// once corners take the max, the codes land straight on the sim's
// ladder and the curve just falsified them — a Flow4's lip is d = 5,
// which the curve sank to 0.27 of a block instead of its own 4/9.
// 0 pins full height (submerged water and all bottom-plane vertices);
// 9 is the cascade plunge, sinking the corner a whole block to meet
// the water one step down. Divided, not tabulated, so the mapping
// cannot drift from the ninths the mesher packs.
const DROP_LEVELS = 9.0;

struct VsOut {
	// Invariant: the depth prepass and color pass run this vs in two
	// different pipelines, and the color pass depth-tests LessEqual
	// against the prepass result. Without @invariant the compiler may
	// contract the position math differently per pipeline (the depth
	// pipeline strips every varying, changing the optimization context),
	// and an ulp of divergence fails the depth test — water full of
	// view-dependent holes.
	@invariant @builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) @interpolate(flat) direct: f32,
	@location(5) view_dist: f32,
	// Splash code: 3 impact cell, 2/1 surrounding cells, 0 none.
	@location(7) @interpolate(flat) splash: u32,
	// World position (ripple field domain + view vector + GI gather).
	@location(8) world: vec3f,
	@location(9) @interpolate(flat) ni: u32,
	@location(10) @interpolate(flat) flow: u32,
	@location(11) @interpolate(flat) falling: u32,
	// Blocks fallen below the lip, continuous down a sheet (top-plane
	// verts carry the cell's count, bottom-plane +1). Saturates at 15.
	@location(12) fall_y: f32,
	// Interpolated drop code (0 full .. 9 plunge) — the lip highlight
	// keys on the plunging edge of a fall's crest.
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

// Flow-dir codes (word 1 bits 5-7) as xz vectors.
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
	// Wave directions: aligned to the current (±40° spread) when the
	// cell flows, fixed spread directions on still water.
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
	// Advect phase with the 0.4 blocks/s surface scroll so ripples
	// ride the current instead of swimming against it.
	let q = p - flow * (t * 0.4);
	var g = d0 * (s.x * fade.x * cos(dot(d0, q) * f.x + t * spd.x));
	g += d1 * (s.y * fade.y * cos(dot(d1, q) * f.y + t * spd.y));
	g += d2 * (s.z * fade.z * cos(dot(d2, q) * f.z + t * spd.z));
	return g;
}

// Reflected environment along `ray` from `origin`: a screen-space
// march over the pre-water solid depth (desktop, water_misc.y), with
// the procedural sky (sky_base + stars) as the always-available
// fallback — so shoreline blocks, trees, and cliffs mirror where the
// screen knows them, and open water still mirrors the actual sky. The
// sun/moon discs are deliberately absent — the analytic specular lobe
// plays their role on the rippled surface.
fn reflection_color(origin: vec3f, ray: vec3f, dist: f32) -> vec3f {
	var sky = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	sky += vec3f(sky_stars(ray)) * frame.sun_dir.w * smoothstep(0.0, 0.3, ray.y);
	if frame.water_misc.y < 0.5 {
		return sky;
	}
	// World-space steps with a growing stride, projected per step; a
	// depth crossing within a thickness bound is a hit. The stride
	// scales with the fragment's distance (its pixel footprint), so
	// the fixed tap count resolves fine shoreline detail up close AND
	// reaches the mountains across a lake from far water — where the
	// reflection is low-frequency and coarse hits look right.
	// Explicit-LOD fetches are legal in this varying branch; the march
	// itself is gated by a uniform flag.
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

// Location 1 is the instance-step chunk origin — see chunk.wgsl.
@vertex
fn vs(@location(0) packed: vec2u, @location(1) origin: vec4f) -> VsOut {
	let w0 = packed.x;
	let w1 = packed.y;
	// Geometry decode identical to chunk.wgsl: twelfth-block units, with
	// the whole-block part split off in integer space for exactness.
	let x = twelfths(w0 & 0xffu);
	var y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	// Surface drop: the shared corner code, straight into ninths. The
	// water payload sits above the 10-bit layer field in w1.
	y -= f32(min((w1 >> 10u) & 0xfu, 9u)) / DROP_LEVELS;
	let ni = (w0 >> 24u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + origin.xyz;

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
	return o;
}

// Depth-prepass entry: rasterizes the same vs geometry with color
// writes masked off, recording only the nearest water depth so the
// color pass blends a single layer.
@fragment
fn fs_depth() -> @location(0) vec4f {
	return vec4f(0.0);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let t = frame.light_color.w;
	// Motion per face: falling sides run downward at 1.5 uv/s, flowing
	// tops at 0.4 along the flow dir (sample offset opposes feature
	// motion). rate × len = speed; len bounds how far a layer slides.
	let vertical = i.ni == 3u || i.ni == 4u;
	// Blocks fallen at this pixel (+0.5 tames the lip singularity).
	let y_fallen = i.fall_y + 0.5;
	var base_uv = i.uv;
	var adv = vec2f(0.0);
	var rate = 0.0;
	var len = 0.0;
	if i.falling == 1u && !vertical {
		// Gravity stretch: v follows free-fall time of flight (√y), so
		// features elongate and their world speed grows ∝ √y down the
		// sheet. Past the 15-block cap the texture smears, hidden under
		// full aeration whitening by then.
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
	// the sheet depth. s ~ 0.5 is neutral.
	var s = 0.5;
	if i.falling == 1u && !vertical {
		let h = select(i.world.x, i.world.z, i.ni == 0u || i.ni == 5u);
		let front = 0.6 * vnoise1(h * 3.0) + 0.4 * vnoise1(h * 7.0 + 13.7);
		let back = 0.6 * vnoise1(h * 3.0 + 31.1) + 0.4 * vnoise1(h * 7.0 + 47.3);
		let cf = smoothstep(0.6, 1.0, sin(base_uv.y * 2.5 - t * 5.0 + front * 6.28));
		let cb = smoothstep(0.6, 1.0, sin(base_uv.y * 1.5 - t * 3.0 + back * 6.28));
		s = mix((back + cb * 0.5) * 0.6, front + cf * 0.5, clamp(front, 0.0, 1.0));
	}
	// RTGI: probe-field indirect at the surface (sky, bounce, torches —
	// all one gathered term) + a short per-fragment sun DDA. Same
	// combine as chunk.wgsl, minus AO. `sh` folds twilight exactly as
	// mapped shadows did; it multiplies the diffuse terms AND the
	// specular below, never the indirect (a torch has no sun glint lobe
	// to borrow).
	let face_n = NORMALS[i.ni];
	let side = i32(frame.gi_misc.x);
	let gp = i.world;
	let indirect = INDIRECT_K
		* gi_indirect(side, gp, face_n, frame.gi0, frame.gi1, frame.zenith.rgb, frame.horizon.rgb, vec2i(4, 2));
	let sv = gi_sun_vis(side, frame.cam_pos.xz, frame.gi_sun, gp, SUN_STEPS);
	let sh = mix(vec3f(1.0), sv.x * medium_water_tr(sv.y), frame.light_dir.w);
	// Froth light: the same direct+indirect mix the spray billboards
	// use (spray.wgsl), shadowed like the surface it sits on — so foam
	// dims with the night instead of glowing unlit white.
	let froth_light = frame.light_color.rgb * 0.8 * sh + indirect;
	// Rippled normal + Schlick fresnel. Impact turbulence: splash cells
	// churn harder.
	let boost = 1.0 + 0.5 * f32(i.splash);
	let g = ripple_grad(i.world.xz, t, FLOW_DIRS[min(i.flow, 4u)], boost, i.view_dist);
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
	let d_scene = textureLoad(scene_depth, vec2i(i.pos.xy), 0).x;
	let ndc = (i.pos.xy / dims * 2.0 - 1.0) * vec2f(1.0, -1.0);
	let wp = frame.inv_view_proj * vec4f(ndc, d_scene, 1.0);
	let scene_dist = distance(wp.xyz / wp.w, frame.cam_pos.xyz);
	// Underside view (submerged camera, back of the face): the "scene
	// behind" is the sky above the surface, so the reconstructed
	// thickness is bogus (~far plane). The water column is behind the
	// camera, not behind the surface — treat the pane as thin.
	let below = frame.water_misc.x > 0.5 && dot(vdir, NORMALS[i.ni]) < 0.0;
	let thick = select(max(scene_dist - i.view_dist, 0.0), 0.0, below);
	// Refraction: sample the scene through a ripple-perturbed offset
	// that grows with thickness (clamped so shores don't smear), and
	// falls back to the straight sample when the offset ray would pull
	// something in FRONT of the water across the surface.
	var suv = i.pos.xy / dims;
	let off = g * (0.10 * min(thick, 1.5));
	let suv2 = clamp(suv + off, vec2f(0.0), vec2f(1.0));
	let d_off = textureLoad(scene_depth, vec2i(suv2 * (dims - 1.0)), 0).x;
	if d_off >= i.pos.z {
		suv = suv2;
	}
	// What is behind the surface, seen through the water body. The old
	// hand-tuned Beer-Lambert depth tint, the Snell window and the TIR
	// mirror ring came out at the M1 demolition; this is the same
	// transport computed rather than tuned, over the reconstructed
	// column. Water tints by DEPTH because red is absorbed an order of
	// magnitude harder than blue (medium_common's measured sigma), not
	// because a constant said which way to lean.
	var col = textureSampleLevel(scene_tex, scene_smp, suv, 0.0).rgb;
	let vray = -vdir;
	col = medium_water_seg(
		col,
		medium_water_light(frame.zenith.rgb, max(-vray.y, 0.0) * thick * 0.5),
		thick,
	);
	// How much of this pixel is still the refracted BACKDROP. That part
	// already carries chunk.wgsl's medium the whole way from the camera,
	// so fogging it again below would make distant water haze about
	// twice as hard as the shore beside it — a visible step at the
	// waterline. Only the surface's own light (froth, mirror, glint) has
	// not travelled yet, and every term below is a `mix`, so one scalar
	// tracks the split exactly.
	var backdrop = 1.0;
	let backdrop_col = col;
	// Churned water is full of bubbles and reads white whatever the
	// depth behind it; the mirror terms damp out over the same ramp.
	col = mix(col, froth_tex * froth_light, aer);
	backdrop *= 1.0 - aer;
	// Contact foam line where the water meets ground (meaningless on the
	// underside — thick is pinned to 0 there and would foam everything).
	let shore = (1.0 - smoothstep(0.05, 0.45, thick)) * f32(!below);
	col = mix(col, vec3f(0.95, 0.97, 1.0) * froth_light, shore * 0.25);
	backdrop *= 1.0 - shore * 0.25;
	// Reflected sky, fresnel-blended: water mirrors the horizon at
	// grazing angles and stays see-through head-on. SSR rides inside
	// `reflection_color` and is explicitly staying.
	var rr = reflect(-vdir, nrm);
	rr.y = max(rr.y, 0.02);
	let mirror_w = fres * select(1.0 - 0.7 * frame.water_misc.x, 0.0, below);
	col = mix(col, reflection_color(i.world, normalize(rr), i.view_dist), mirror_w);
	backdrop *= 1.0 - mirror_w;
	// Splash foam (after the mirror mix — froth sits on the surface):
	// a STATIC wash, brightest on the impact cell and fading with the
	// splash code out to Chebyshev 2. Deliberately timeless. This was an
	// expanding ring keyed to fract(t * 0.9) plus a wobble on the impact
	// churn, and the two together read as the impact strobing white
	// once a second — the splash's motion belongs in the droplets
	// (spray.wgsl), which throw upward, not in the pool surface
	// brightening and dimming under them. The ripple normals still
	// churn harder here (`boost` above), so the foam has movement in it
	// without the whole cell pulsing.
	if i.splash > 0u {
		let foam_w = 0.09 * f32(i.splash) * (1.0 - 0.5 * i.uv.y);
		col = mix(col, froth_light, foam_w);
		backdrop *= 1.0 - foam_w;
	}
	col += frame.light_color.rgb * spec * sh * 2.0 * mirror;
	// Glassy crest sheen where flow tips over a lip: strongest in the
	// first fallen block, keyed to the plunging drop edge.
	let lip = f32(i.falling) * (1.0 - min(i.fall_y, 1.0)) * smoothstep(6.0, 9.0, i.drop);
	col += frame.light_color.rgb * lip * sh * 0.15;
	// Alpha: the Fresnel base only. The depth-driven opacity ramp went
	// with the absorption model that justified it — a clear pane cannot
	// also claim to hide the bottom. Aeration still pins near-opaque and
	// streak gaps still thin falling sheets.
	let alpha = mix(max(mix(frame.misc.x, 1.0, fres), 0.0), 0.92, aer);
	// The camera-to-surface medium, applied to the surface's own light
	// only (see `backdrop` above). Every term above is a linear mix, so
	// after n of them the pixel is exactly
	//     col = B*backdrop + (surface light)
	// with backdrop = prod(1 - w_k) and B the pre-mix backdrop. Peel the
	// backdrop off, normalise what remains to a full-coverage radiance,
	// fog THAT, and re-weight — which puts the in-scatter in proportional
	// to how much of the pixel is actually surface. Fogging the composite
	// instead would in-scatter a full pixel's worth of haze behind a
	// pane that is mostly see-through.
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
	return vec4f(back + surf, clamp(alpha * (0.75 + 0.5 * s), 0.0, 1.0));
}
