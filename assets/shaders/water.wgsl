// Water shader — fork of chunk.wgsl with a two-word vertex: word 0 is
// the solid packing (position/normal/corner/texture; ao and tint bits
// unused), word 1 carries per-vertex surface data. Bits 0-3 of word 1
// are the drop code d (9 − mean corner amount): the vertex sinks
// DROPS[d] below its lattice height, so a source surface sits at 8/9
// and thinner flowing water lower along a concave curve, with corner
// heights shared between adjacent cells (no cracks). Bit 4 marks
// falling water (side faces scroll downward), bits 5-7 the horizontal
// flow dir (0 still, 1 +x, 2 −x, 3 +z, 4 −z; top faces scroll along
// it), bit 8 foam (a fall striking ground: brightened splash pulse).
// The animation clock rides in frame.light_color.w. Lighting matches
// chunk.wgsl minus AO/tint; alpha comes from misc.x (the water frame
// UBO copy).

// Must match FrameUniforms in renderer.rs and the Frame struct in
// sky.wgsl — field order and packing are load-bearing.
struct Frame {
	view_proj: mat4x4f,
	inv_view_proj: mat4x4f,
	light_near: mat4x4f,
	light_wide: mat4x4f,
	// xyz = active caster dir (sun or moon), toward the ground
	// w = shadow strength 0..1 (twilight fade)
	light_dir: vec4f,
	// rgb = direct light color (sun by day, moonlight by night)
	// w = animation clock in seconds (wraps at 600; water scroll/foam)
	light_color: vec4f,
	// rgb = sky-tinted ambient floor
	ambient: vec4f,
	// rgb = fog color (sky horizon); w = fog start distance (units)
	fog_color: vec4f,
	// xyz = camera position; w = 1 / (fog_end − fog_start)
	cam_pos: vec4f,
	// xyz = direction toward the sun in the sky; w = star intensity
	sun_dir: vec4f,
	// xyz = direction toward the moon; w = sunset glow strength
	moon_dir: vec4f,
	// rgb = sky zenith color
	zenith: vec4f,
	// rgb = sky horizon color
	horizon: vec4f,
	// x = alpha (0.7 for water)
	// y = shadows enabled (0.0 until the shadow passes exist)
	// z = shadow map texel size (1.0 / resolution, both cascades)
	// w = wide-cascade texel size in world units (normal-offset bias)
	misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;
@group(1) @binding(2) var shadow_near: texture_depth_2d;
@group(1) @binding(3) var shadow_wide: texture_depth_2d;
@group(1) @binding(4) var shadow_smp: sampler_comparison;

struct ChunkU {
	origin: vec4f,
}

@group(2) @binding(0) var<uniform> chunk: ChunkU;

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
	// Foam strength (flat: whole splash cell brightens).
	@location(7) @interpolate(flat) foam: f32,
	// World position (ripple field domain + view vector).
	@location(8) world: vec3f,
	@location(9) @interpolate(flat) ni: u32,
	@location(10) @interpolate(flat) flow: u32,
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

// Reflected environment along `ray`: the same procedural sky the sky
// pass draws (sky_base from the shared prelude), plus stars at night.
// The sun/moon discs are deliberately absent — the analytic specular
// lobe plays their role on the rippled surface. This function is the
// SSR seam: a screen-space march can replace its body, sampling the
// scene on hit and falling back to sky_base on miss.
fn reflection_color(ray: vec3f) -> vec3f {
	var col = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	col += vec3f(sky_stars(ray)) * frame.sun_dir.w * smoothstep(0.0, 0.3, ray.y);
	return col;
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

@vertex
fn vs(@location(0) packed: vec2u) -> VsOut {
	let w0 = packed.x;
	let w1 = packed.y;
	let x = f32(w0 & 0x1fu);
	var y = f32((w0 >> 5u) & 0x1fu);
	let z = f32((w0 >> 10u) & 0x1fu);
	// Surface drop: shared corner codes through the concave curve.
	y -= DROPS[min(w1 & 0xfu, 9u)];
	let ni = (w0 >> 15u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + chunk.origin.xyz;

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	var uv = vec2f(f32((w0 >> 20u) & 1u), f32((w0 >> 21u) & 1u));
	// Scroll: falling side faces run downward (v grows toward the face
	// bottom, so subtracting time moves features down-screen); flowing
	// top faces run along the mesher's flow direction. The offset is
	// linear in uv, so interpolation stays exact; fs wraps with fract.
	let t = frame.light_color.w;
	let falling = ((w1 >> 4u) & 1u) == 1u;
	let flow = (w1 >> 5u) & 7u;
	let vertical = ni == 3u || ni == 4u;
	if falling && !vertical {
		uv.y -= t * 1.5;
	} else if ni == 4u && flow != 0u {
		let s = t * 0.4;
		if flow == 1u {
			uv.x -= s;
		} else if flow == 2u {
			uv.x += s;
		} else if flow == 3u {
			uv.y -= s;
		} else {
			uv.y += s;
		}
	}
	o.uv = uv;
	o.foam = f32((w1 >> 8u) & 1u);
	o.layer = (w0 >> 23u) & 63u;
	o.world = world;
	o.ni = ni;
	o.flow = flow;

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
	var v = textureSampleCompare(tex, shadow_smp, sc.xy, sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(px, 0.0), sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(-px, 0.0), sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(0.0, px), sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(0.0, -px), sc.z);
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
	let uv = fract(i.uv);
	let tex = textureSample(blocks_tex, blocks_smp, uv, i.layer).rgb;
	var vis = 1.0;
	if frame.misc.y > 0.5 {
		vis = shadow_vis(i.sc_near, i.sc_wide, i.direct);
	}
	let sh = mix(1.0, vis, frame.light_dir.w);
	var col = tex * (frame.ambient.rgb * i.axis + frame.light_color.rgb * i.direct * sh);
	// Rippled normal + Schlick fresnel: alpha rises from the base
	// (misc.x) toward opaque at grazing angles, so distant/low-angle
	// water reads as a surface instead of tinted glass.
	let g = ripple_grad(i.world.xz, t, FLOW_DIRS[min(i.flow, 4u)], 1.0, i.fog_dist);
	let nrm = ripple_normal(i.ni, g);
	let vdir = normalize(frame.cam_pos.xyz - i.world);
	let fres = 0.02 + 0.98 * pow(1.0 - abs(dot(vdir, nrm)), 5.0);
	// Glint from the active caster (sun by day, moon by night) on the
	// rippled normal. The exponent widens with distance so the glitter
	// path stays stable instead of aliasing into per-pixel shimmer;
	// (e+2) keeps lobe energy roughly constant as it widens. Fresnel
	// at the half vector: bright at low sun, subtle from above.
	let e = mix(240.0, 24.0, smoothstep(8.0, 120.0, i.fog_dist));
	let hs = normalize(vdir - frame.light_dir.xyz);
	let fres_h = 0.02 + 0.98 * pow(1.0 - max(dot(vdir, hs), 0.0), 5.0);
	let spec = pow(max(dot(nrm, hs), 0.0), e) * (e + 2.0) / 242.0 * fres_h;
	// Splash foam where a fall strikes ground: brighten toward white,
	// strongest at the face top, with a slow pulse.
	if i.foam > 0.0 {
		let pulse = 0.20 + 0.06 * sin(t * 6.0);
		col = mix(col, vec3f(1.0), i.foam * pulse * (1.0 - uv.y));
	}
	// Reflected sky, fresnel-blended: water mirrors the horizon at
	// grazing angles and stays see-through from above.
	var rr = reflect(-vdir, nrm);
	rr.y = max(rr.y, 0.02);
	col = mix(col, reflection_color(normalize(rr)), fres);
	col += frame.light_color.rgb * spec * sh * 2.0;
	let f = clamp((i.fog_dist - frame.fog_color.w) * frame.cam_pos.w, 0.0, 1.0);
	col = mix(col, frame.fog_color.rgb, f * f * (3.0 - 2.0 * f));
	return vec4f(col, mix(frame.misc.x, 1.0, fres));
}
