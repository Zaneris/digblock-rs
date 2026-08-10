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

// RTGI camera light (gi_probe.wgsl's cam_light entry): [0] = omni
// probe ambient at the camera (already INDIRECT_K-scaled), [1].x =
// sun visibility there. Billboards have no meaningful surface to
// gather at, and spray lives within ~64 blocks of the eye — the
// camera's light is theirs.
struct CamLight {
	ambient: vec4f,
	sun: vec4f,
}
@group(1) @binding(0) var<uniform> cam_light: CamLight;

// Droplet loop length. Must divide the 600s animation clock wrap
// exactly or the whole bank jumps at the seam: 600 / 1.0 = 600 cycles.
// Long enough that a droplet thrown at the new higher speeds completes
// its arc — apex lands at 0.33..0.58s, touchdown by ~1.16s.
const LIFE: f32 = 1.0;
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
		// Mist: soft slabs jittered around the splash. Their alpha
		// varies per slab but NOT over time — they used to breathe on
		// offset clocks, which was part of the impact reading as a
		// pulse. The droplets carry the motion now.
		// Stacked to span the droplets' apex range (~0.5..1.5) rather
		// than hugging the surface, so the mist reads as a plume rising
		// out of the impact instead of a flat bank lying on the pool.
		let stack = f32(pi);
		center = inst.xyz
			+ vec3f(
				(hash1(seed + 11.0) - 0.5) * 1.6,
				0.4 + 0.55 * stack,
				(hash1(seed + 12.0) - 0.5) * 1.6,
			);
		size = vec2f(2.2, 1.6);
		alpha = 0.10 + 0.06 * hash1(seed + 13.0);
	} else {
		// Droplet: velocity cone out of the impact, ballistic arc,
		// growing and fading over its looped lifetime. The cone is
		// deliberately steep — horizontal speed stays under a block a
		// second while the throw is 3.0..5.2, so the spray reads as
		// going UP out of the impact rather than spreading sideways
		// across the pool.
		let az = hash1(seed) * 6.2832;
		let v0 = vec3f(cos(az), 0.0, sin(az)) * (0.3 + 0.9 * hash1(seed + 1.0))
			+ vec3f(0.0, 3.0 + 2.2 * hash1(seed + 2.0), 0.0);
		let t01 = fract(t / LIFE + hash1(seed + 3.0));
		let tau = t01 * LIFE;
		center = inst.xyz + v0 * tau + vec3f(0.0, -9.0, 0.0) * (0.5 * tau * tau);
		size = vec2f(mix(0.06, 0.17, t01));
		alpha = (1.0 - t01) * smoothstep(0.0, 0.12, t01) * 0.7;
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
	// White-ish froth in the camera's probe ambient + traced direct
	// (twilight-faded like every surface), fogged like everything else.
	let sh = mix(1.0, cam_light.sun.x, frame.light_dir.w);
	var col = vec3f(0.9, 0.94, 1.0) * (cam_light.ambient.rgb + frame.light_color.rgb * 0.8 * sh);
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
