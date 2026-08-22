// The standing spiral: the glowing marker over an arrival pad — spawn, and
// the waystones. A slow helix of arcane light rises out of a soft ring on
// the ground, phase-scrolled by the shared animation clock so every marker
// breathes on its own beat. Persistent and stateless: the CPU owns one
// instance per site and nothing ages — the whole motion is a pure function
// of (site, phase, clock), so there is nothing to save on suspend and
// nothing to rebuild on device loss but the pipeline.
//
// Additive, like every light the engine draws as light. vertex_index picks
// the particle (vi / 6) and the quad corner (vi % 6); particle 0 is the
// ground ring, the rest are motes on the helix.

// Frame struct + group(0) come from frame_common.wgsl; the transmittance
// halves from medium_common.wgsl (both prepended by digblock-assets).

// Motes per marker — keep in sync with waystone_fx.rs MOTES, which sizes
// the draw call (`the_marker_mote_count_matches_the_shader`).
const MOTES: u32 = 24u;
// Seconds one mote takes to climb the whole spiral.
const RISE: f32 = 3.4;
// Radians/s the spiral turns at — slow enough to read as standing.
const ORBIT: f32 = 0.9;
// How much of the orbit radius the helix keeps at its top: the spiral
// narrows as it rises, which is what separates "rising out of the pad"
// from "hovering over it".
const TAPER: f32 = 0.35;
// Mote side in blocks, and brightness against the marker tint.
const MOTE: f32 = 0.11;
const MOTE_GAIN: f32 = 1.5;
// The ground ring's brightness, and its breath rate (radians/s).
const RING_GAIN: f32 = 0.5;
const BREATH: f32 = 1.7;
// The marker's own colour: arcane violet-white. A constant rather than an
// instance lane on purpose — every arrival pad speaks one identity, the
// way every torch burns one flame.
const TINT = vec3f(0.62, 0.55, 1.0);

const CORNERS = array<vec2f, 6>(
	vec2f(-0.5, -0.5),
	vec2f(0.5, -0.5),
	vec2f(-0.5, 0.5),
	vec2f(-0.5, 0.5),
	vec2f(0.5, -0.5),
	vec2f(0.5, 0.5),
);

fn hash1(n: f32) -> f32 {
	return fract(sin(n) * 43758.5453);
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) col: vec4f,
	// 1 = the ground ring's annulus falloff, 0 = a mote's soft disc.
	@location(2) ring: f32,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) i0: vec4f,
	@location(1) i1: vec4f,
) -> VsOut {
	let pi = vi / 6u;
	let corner = CORNERS[vi % 6u];
	let clock = frame.light_color.w;
	let site = i0.xyz; // pad floor centre, at ground level
	let phase = i0.w; // per-site beat offset
	let radius = i1.x;
	let height = i1.y;

	let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
	let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));

	var center: vec3f;
	var bx: vec3f;
	var by: vec3f;
	var size = 0.0;
	var weight = 0.0;
	var ring = 0.0;
	var col = TINT;
	if pi == 0u {
		// The ground ring: flat on the pad, breathing on the site's beat.
		center = site + vec3f(0.0, 0.02, 0.0);
		bx = vec3f(1.0, 0.0, 0.0);
		by = vec3f(0.0, 0.0, 1.0);
		size = radius * 2.2;
		weight = RING_GAIN * (0.75 + 0.25 * sin(clock * BREATH + phase * 6.2832));
		ring = 1.0;
	} else {
		let seed = phase * 91.17 + f32(pi) * 37.719;
		let mph = fract(clock / RISE + hash1(seed) + phase);
		let ang = hash1(seed + 1.0) * 6.2832 + clock * ORBIT + mph * 2.4;
		let rad = radius * mix(1.0, TAPER, mph);
		center = site + vec3f(cos(ang) * rad, height * mph, sin(ang) * rad);
		bx = right;
		by = up;
		size = MOTE;
		col = mix(TINT, vec3f(1.0), 0.3 * mph) * MOTE_GAIN;
		// In and out along the climb, so the top of the spiral dissolves
		// instead of being cut.
		weight = sin(3.1416 * mph);
		ring = 0.0;
	}

	let live = select(0.0, 1.0, weight > 1e-4);
	let world = center + (bx * (corner.x * size) + by * (corner.y * size)) * live;

	// Transmittance only — siphon's rule for anything drawn as light.
	let d = distance(frame.cam_pos.xyz, world);
	var air = d;
	var tr = vec3f(1.0);
	if frame.water_misc.x > 0.5 {
		let ray = (world - frame.cam_pos.xyz) / max(d, 1e-4);
		var dw = d;
		if ray.y > 1e-4 {
			dw = min(d, frame.water_misc.z / ray.y);
		}
		tr = medium_water_tr(dw);
		air = d - dw;
	}
	tr *= medium_air_tr(air);

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = corner + 0.5;
	o.col = vec4f(col * tr, weight);
	o.ring = ring;
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let d = length(i.uv - 0.5) * 2.0;
	// A mote is a soft disc; the ring is an annulus — bright at its rim,
	// open in the middle so the pad itself stays readable.
	let disc = smoothstep(1.0, 0.45, d);
	let annulus = smoothstep(0.55, 0.8, d) * smoothstep(1.0, 0.88, d);
	return vec4f(i.col.rgb, i.col.a * mix(disc, annulus, i.ring));
}
