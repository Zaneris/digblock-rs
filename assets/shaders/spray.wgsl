// Stateless waterfall spray: droplet + mist billboards whose position,
// size, and alpha are pure functions of (impact cell, particle index,
// animation clock) — no CPU simulation, no per-frame particle uploads,
// nothing to rebuild on device loss but the pipeline. One instance per
// fall-impact cell (xyz center, w spare); vertex_index picks the
// particle (vi / 6) and quad corner (vi % 6). Particles 0..MIST_N are
// large slow mist slabs jittered around the base; the rest are
// ballistic droplets on a 0.75s loop (800 cycles per 600s clock wrap
// — seamless).

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

const LIFE: f32 = 0.75;
// Mist slabs per instance — keep in sync with spray.rs MIST_N.
const MIST_N: u32 = 3u;

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
	// rgb premixed (lit + fogged), a = billboard alpha.
	@location(1) col: vec4f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) inst: vec4f) -> VsOut {
	let pi = vi / 6u;
	let corner = CORNERS[vi % 6u];
	let t = frame.light_color.w;
	// Per-particle randomness seeded by cell position + index.
	let seed = inst.x * 12.9898 + inst.z * 78.233 + f32(pi) * 37.719;
	var center: vec3f;
	var size = vec2f(0.0);
	var alpha = 0.0;
	if pi < MIST_N {
		// Mist: soft slabs jittered around the splash, breathing on
		// offset clocks so the bank never pulses as one.
		let stack = f32(pi);
		center = inst.xyz
			+ vec3f(
				(hash1(seed + 11.0) - 0.5) * 1.8,
				0.35 + 0.3 * stack,
				(hash1(seed + 12.0) - 0.5) * 1.8,
			);
		size = vec2f(2.6, 1.5);
		alpha = 0.13 + 0.05 * sin(t * (1.3 + 0.3 * stack) + hash1(seed) * 6.28);
	} else {
		// Droplet: velocity cone out of the impact, ballistic arc,
		// growing and fading over its looped lifetime.
		let az = hash1(seed) * 6.2832;
		let v0 = vec3f(cos(az), 0.0, sin(az)) * (0.5 + 1.3 * hash1(seed + 1.0))
			+ vec3f(0.0, 2.2 + 1.6 * hash1(seed + 2.0), 0.0);
		let t01 = fract(t / LIFE + hash1(seed + 3.0));
		let tau = t01 * LIFE;
		center = inst.xyz + v0 * tau + vec3f(0.0, -9.0, 0.0) * (0.5 * tau * tau);
		size = vec2f(mix(0.07, 0.20, t01));
		alpha = (1.0 - t01) * smoothstep(0.0, 0.15, t01) * 0.7;
	}
	// Camera-facing: rows 0/1 of P·V are the scaled view basis rows —
	// safe to renormalize for a standard perspective projection.
	let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
	let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));
	let world = center + right * (corner.x * size.x) + up * (corner.y * size.y);
	// Distance fade matches the CPU cull radius (spray.rs SPRAY_RADIUS)
	// so emitters never pop.
	let dist = distance(world, frame.cam_pos.xyz);
	alpha *= 1.0 - smoothstep(56.0, 64.0, dist);
	// White-ish froth in the ambient + direct light (no shadow taps),
	// fogged like everything else.
	var col = vec3f(0.9, 0.94, 1.0) * (frame.ambient.rgb + frame.light_color.rgb * 0.8);
	col = apply_fog(col, dist, world);
	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = corner + 0.5;
	o.col = vec4f(col, alpha);
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	// Procedural soft circle — no texture.
	let d = length(i.uv - 0.5) * 2.0;
	return vec4f(i.col.rgb, i.col.a * smoothstep(1.0, 0.55, d));
}
