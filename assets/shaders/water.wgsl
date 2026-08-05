// Water shader — fork of chunk.wgsl with the shared two-word vertex:
// word 0 is the solid geometry packing (position/normal/corner; ao and
// tint bits unused), word 1 holds the texture layer in bits 0-9 and the
// per-vertex surface payload above it. Bits 10-13 are the drop code d
// (9 − mean corner amount): the vertex sinks DROPS[d] below its lattice
// height, so a source surface sits at 8/9 and thinner flowing water
// lower along a concave curve, with corner heights shared between
// adjacent cells (no cracks). Bit 14 marks falling water (side faces
// scroll downward), bits 15-17 the horizontal flow dir (0 still, 1 +x,
// 2 −x, 3 +z, 4 −z; top faces scroll along it), bits 18-19 the splash
// code (3 = fall impact cell — continuous churn; 2/1 = pool cells at
// Chebyshev 1/2 — expanding foam rings; both boost ripple turbulence),
// bits 20-23 the vertex fall distance (blocks below the lip, +1 on
// bottom-plane verts so interpolation is continuous; drives gravity
// stretch and aeration whitening).
// The animation clock rides in frame.light_color.w. Lighting matches
// chunk.wgsl minus AO/tint; alpha comes from misc.x (the water frame
// UBO copy).

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

@group(2) @binding(0) var shadow_near: texture_depth_2d;
@group(2) @binding(1) var shadow_wide: texture_depth_2d;
@group(2) @binding(2) var shadow_smp: sampler_comparison;

struct ChunkU {
	origin: vec4f,
}

@group(3) @binding(0) var<uniform> chunk: ChunkU;

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

// Face-axis ambient, matching chunk.wgsl.
const AXIS = array<f32, 6>(0.85, 0.78, 0.78, 0.58, 1.0, 0.85);

// Surface drop per code: 0 pins full height (submerged water and all
// bottom-plane vertices); 1 is the resting source surface (8/9, kept
// so pools and oceans sit where they always have); codes 2-8 fall
// away on a concave curve — steep right below the source, easing off
// as the water thins — so flowing runs read as a real gradient
// instead of the old linear d/9 trickle. Code 9 is the cascade
// plunge: the corner sinks the whole block to meet the surface of
// the water one step down.
const DROPS = array<f32, 10>(
	0.0, 0.111, 0.32, 0.50, 0.63, 0.73, 0.81, 0.87, 0.92, 1.0,
);

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) @interpolate(flat) direct: f32,
	@location(3) sc_near: vec3f,
	@location(4) sc_wide: vec3f,
	@location(5) fog_dist: f32,
	@location(6) @interpolate(flat) axis: f32,
	// Splash code: 3 impact cell, 2/1 ring cells, 0 none.
	@location(7) @interpolate(flat) splash: u32,
	// World position (ripple field domain + view vector).
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

@vertex
fn vs(@location(0) packed: vec2u) -> VsOut {
	let w0 = packed.x;
	let w1 = packed.y;
	// Geometry decode identical to chunk.wgsl: twelfth-block units, with
	// the whole-block part split off in integer space for exactness.
	let x = twelfths(w0 & 0xffu);
	var y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	// Surface drop: shared corner codes through the concave curve. The
	// water payload sits above the 10-bit layer field in w1.
	y -= DROPS[min((w1 >> 10u) & 0xfu, 9u)];
	let ni = (w0 >> 24u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + chunk.origin.xyz;

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
	o.fog_dist = distance(world, frame.cam_pos.xyz);
	o.axis = AXIS[ni];

	let pn = frame.light_near * vec4f(world, 1.0);
	o.sc_near = vec3f(pn.xy * vec2f(0.5, -0.5) + 0.5, pn.z);
	// Wide-cascade normal offset — see chunk.wgsl.
	let pw = frame.light_wide * vec4f(world + n * frame.misc.w, 1.0);
	o.sc_wide = vec3f(pw.xy * vec2f(0.5, -0.5) + 0.5, pw.z);
	return o;
}

// 5-tap cross PCF, identical to chunk.wgsl.
fn pcf5(tex: texture_depth_2d, sc: vec3f) -> f32 {
	let px = frame.misc.z;
	var v = textureSampleCompareLevel(tex, shadow_smp, sc.xy, sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(px, 0.0), sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(-px, 0.0), sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(0.0, px), sc.z);
	v += textureSampleCompareLevel(tex, shadow_smp, sc.xy + vec2f(0.0, -px), sc.z);
	return v / 5.0;
}

fn shadow_vis(sc_near: vec3f, sc_wide: vec3f, direct: f32) -> f32 {
	if direct <= 0.0 {
		return 0.0;
	}
	let wsc = vec3f(sc_wide.xy, sc_wide.z - 0.0001);
	let d = max(abs(sc_near.x - 0.5), abs(sc_near.y - 0.5)) * 2.0;
	if d >= 1.0 {
		return pcf5(shadow_wide, wsc);
	}
	let near = pcf5(shadow_near, sc_near);
	let t = smoothstep(0.85, 1.0, d);
	if t <= 0.0 {
		return near;
	}
	return mix(near, pcf5(shadow_wide, wsc), t);
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
	// fallen — glassy at the lip, diffuse froth at the base. Whitening
	// happens on the albedo (so froth still takes shadow), while the
	// mirror terms (fresnel, reflection, specular) damp out.
	let aer = smoothstep(0.5, 6.0, y_fallen) * f32(i.falling);
	let alb = mix(tex, vec3f(0.93, 0.96, 1.0), aer * 0.55);
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
	var vis = 1.0;
	if frame.misc.y > 0.5 {
		vis = shadow_vis(i.sc_near, i.sc_wide, i.direct);
	}
	let sh = mix(1.0, vis, frame.light_dir.w);
	// Froth light: the same ambient+direct mix the spray billboards use
	// (spray.wgsl), shadowed like the surface it sits on — so foam dims
	// with the night instead of glowing unlit white.
	let froth_light = frame.ambient.rgb + frame.light_color.rgb * 0.8 * sh;
	// In-scattered water color (the body's own diffuse), streak-bright.
	var scatter = alb * (frame.ambient.rgb * i.axis + frame.light_color.rgb * i.direct * sh);
	scatter *= 1.0 + (s - 0.5) * 0.3;
	// Rippled normal + Schlick fresnel. Impact turbulence: splash cells
	// churn harder.
	let boost = 1.0 + 0.5 * f32(i.splash);
	let g = ripple_grad(i.world.xz, t, FLOW_DIRS[min(i.flow, 4u)], boost, i.fog_dist);
	let nrm = ripple_normal(i.ni, g);
	let vdir = normalize(frame.cam_pos.xyz - i.world);
	let mirror = 1.0 - 0.7 * aer;
	let fres = (0.02 + 0.98 * pow(1.0 - abs(dot(vdir, nrm)), 5.0)) * mirror;
	// Glint from the active caster (sun by day, moon by night) on the
	// rippled normal. The exponent widens with distance so the glitter
	// path stays stable instead of aliasing into per-pixel shimmer;
	// (e+2) keeps lobe energy roughly constant as it widens. Fresnel
	// at the half vector: bright at low sun, subtle from above.
	let e = mix(240.0, 24.0, smoothstep(8.0, 120.0, i.fog_dist));
	let hs = normalize(vdir - frame.light_dir.xyz);
	let fres_h = 0.02 + 0.98 * pow(1.0 - max(dot(vdir, hs), 0.0), 5.0);
	let spec = pow(max(dot(nrm, hs), 0.0), e) * (e + 2.0) / 242.0 * fres_h;
	// Water column thickness along the view ray: reconstruct the solid
	// scene point behind this fragment from the pre-water depth copy
	// (exact via inv_view_proj — no projection assumptions).
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
	let thick = select(max(scene_dist - i.fog_dist, 0.0), 0.0, below);
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
	let refracted = textureSampleLevel(scene_tex, scene_smp, suv, 0.0).rgb;
	// Beer-Lambert absorption, red dying first: shallow water stays
	// clear, deep water swallows the bottom into the scatter color.
	// WATER_EXTINCTION lives in frame_common.wgsl because the submerged
	// scene fog (apply_fog) must stay visibly consistent with it.
	let transmit = exp(-WATER_EXTINCTION * thick);
	var col = mix(scatter, refracted, transmit);
	// Snell window: from underwater, rays steeper than the critical
	// angle transmit the world above; shallower rays totally internally
	// reflect. The rippled `nrm` wobbling the boundary IS the sparkle.
	let window = smoothstep(TIR_COS_EDGE.x, TIR_COS_EDGE.y, abs(dot(vdir, nrm)));
	if below {
		// Through the window: the refracted above-surface render,
		// brightened (open air is brighter than the murk-adapted eye).
		// Outside it: a mirror of the water body itself — the underwater
		// fog color, dimmed because the reflected rays head down into
		// deeper murk. This is what makes the surface READ as a plane
		// from below instead of recompositing its own background.
		let thru = refracted * mix(1.0, SNELL_BRIGHT, window);
		col = mix(frame.fog_color.rgb * TIR_MIRROR_DIM, thru, window);
	}
	// Contact foam line where the water meets ground (meaningless on the
	// underside — thick is pinned to 0 there and would foam everything).
	let shore = (1.0 - smoothstep(0.05, 0.45, thick)) * f32(!below);
	col = mix(col, vec3f(0.95, 0.97, 1.0) * froth_light, shore * 0.25);
	// Reflected sky, fresnel-blended: water mirrors the horizon at
	// grazing angles and stays see-through from above. On the underside
	// the TIR mirror above replaces it entirely; half-submerged front
	// faces keep the damped version.
	var rr = reflect(-vdir, nrm);
	rr.y = max(rr.y, 0.02);
	col = mix(
		col,
		reflection_color(i.world, normalize(rr), i.fog_dist),
		fres * select(1.0 - 0.7 * frame.water_misc.x, 0.0, below),
	);
	// Splash foam (after the mirror mix — froth sits on the surface):
	// the impact cell holds a steady churn, spatially de-phased so
	// neighboring cells never pulse in lockstep; ring cells (codes
	// 1-2 only — a ring starting at r = 0 made the whole impact row
	// strobe) light up as an expanding front passes their distance.
	if i.splash > 0u {
		let impact = step(2.5, f32(i.splash));
		let wob = sin(t * 6.0 + i.world.x * 2.1 + i.world.z * 1.7);
		let churn = impact * (0.34 + 0.05 * wob) * (1.0 - 0.5 * i.uv.y);
		let r = (3.0 - f32(i.splash)) / 3.0;
		let ph = fract(t * 0.9);
		let ring = (1.0 - impact) * smoothstep(0.22, 0.0, abs(r - ph * 0.8)) * (1.0 - ph) * 0.35;
		col = mix(col, froth_light, min(churn + ring, 0.6));
	}
	col += frame.light_color.rgb * spec * sh * 2.0 * mirror;
	// Glassy crest sheen where flow tips over a lip: strongest in the
	// first fallen block, keyed to the plunging drop edge.
	let lip = f32(i.falling) * (1.0 - min(i.fall_y, 1.0)) * smoothstep(6.0, 9.0, i.drop);
	col += frame.light_color.rgb * lip * sh * 0.15;
	col = apply_fog(col, i.fog_dist, i.world);
	// Alpha: the fresnel base, pushed toward opaque where absorption ate
	// the background (col already composites the refracted scene there,
	// and the dst pixel holds the untinted scene — letting much of it
	// through would wash the depth tint out). The saturation runs at
	// WATER_ALPHA_K x the color extinction — slower onset — but tops
	// out at the higher WATER_ALPHA_CAP, so shallow ponds stay clear
	// while deep water goes near-opaque with only a hint of bottom.
	// Aeration pins near-opaque; streak gaps (s < 0.5) thin falling
	// sheets back toward translucent. The underside pane gets its own
	// alpha: near-opaque in the TIR mirror ring, translucent through
	// the Snell window — a distinct plane, not a tinted nothing.
	let opaque = min(
		1.0 - dot(exp(-WATER_EXTINCTION * WATER_ALPHA_K * thick), vec3f(1.0 / 3.0)),
		WATER_ALPHA_CAP,
	);
	let base = mix(max(mix(frame.misc.x, 1.0, fres), opaque), 0.92, aer);
	let alpha = select(base, mix(BELOW_ALPHA.x, BELOW_ALPHA.y, window), below);
	return vec4f(col, clamp(alpha * (0.75 + 0.5 * s), 0.0, 1.0));
}
