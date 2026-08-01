// Stateless waterfall spray: droplet + mist billboards whose position,
// size, and alpha are pure functions of (impact cell, particle index,
// animation clock) — no CPU simulation, no per-frame particle uploads,
// nothing to rebuild on device loss but the pipeline. One instance per
// fall-impact cell (xyz center, w spare); vertex_index picks the
// particle (vi / 6) and quad corner (vi % 6). Particle 0 is a large
// slow mist slab at the base; 1..N are ballistic droplets on a 0.75s
// loop (800 cycles per 600s clock wrap — seamless).

// Must match FrameUniforms in renderer.rs — field order and packing
// are load-bearing (see water.wgsl).
struct Frame {
	view_proj: mat4x4f,
	inv_view_proj: mat4x4f,
	light_near: mat4x4f,
	light_wide: mat4x4f,
	light_dir: vec4f,
	// rgb = direct light color; w = animation clock in seconds
	light_color: vec4f,
	ambient: vec4f,
	// rgb = fog color; w = fog start distance
	fog_color: vec4f,
	// xyz = camera position; w = 1 / (fog_end − fog_start)
	cam_pos: vec4f,
	sun_dir: vec4f,
	moon_dir: vec4f,
	zenith: vec4f,
	horizon: vec4f,
	misc: vec4f,
	water_misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

const LIFE: f32 = 0.75;

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
	if pi == 0u {
		// Mist: one soft slab hovering over the splash.
		center = inst.xyz + vec3f(0.0, 0.5, 0.0);
		size = vec2f(2.2, 1.4);
		alpha = 0.12 + 0.05 * sin(t * 1.7 + hash1(seed) * 6.28);
	} else {
		// Droplet: velocity cone out of the impact, ballistic arc,
		// growing and fading over its looped lifetime.
		let az = hash1(seed) * 6.2832;
		let v0 = vec3f(cos(az), 0.0, sin(az)) * (0.6 + 0.8 * hash1(seed + 1.0))
			+ vec3f(0.0, 2.0 + hash1(seed + 2.0), 0.0);
		let t01 = fract(t / LIFE + hash1(seed + 3.0));
		let tau = t01 * LIFE;
		center = inst.xyz + v0 * tau + vec3f(0.0, -9.0, 0.0) * (0.5 * tau * tau);
		size = vec2f(mix(0.06, 0.16, t01));
		alpha = (1.0 - t01) * smoothstep(0.0, 0.15, t01) * 0.8;
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
	let f = clamp((dist - frame.fog_color.w) * frame.cam_pos.w, 0.0, 1.0);
	col = mix(col, frame.fog_color.rgb, f * f * (3.0 - 2.0 * f));
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
