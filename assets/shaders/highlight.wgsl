// Block highlight: an idle outline on the crosshair's target cell, plus
// a procedural crack web that grows with hold-to-dig progress. One
// instance = the target cell, as a vertex_index-generated box (the
// players.wgsl trick) inflated a hair past the block so it never
// z-fights the face it decorates. Alpha-blended darkening decal — no
// textures, no lighting.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

// Drawn in the WATER pass (after the water surfaces), so the box on a
// targeted water source — or on a submerged block — blends over the
// water instead of vanishing under its depth tint. The pass's live
// depth buffer contains water-surface depth by then, so the pipeline's
// depth test is Always and occlusion is done here by hand against the
// pre-water SOLID depth copy: terrain still hides the box, water never
// does. (R32Float resolve, textureLoad only — see water.wgsl.)
@group(0) @binding(1) var scene_depth: texture_2d<f32>;

const QUAD = array<vec2f, 6>(
	vec2f(0.0, 0.0),
	vec2f(1.0, 0.0),
	vec2f(0.0, 1.0),
	vec2f(0.0, 1.0),
	vec2f(1.0, 0.0),
	vec2f(1.0, 1.0),
);

const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
);
const TANGENTS = array<vec3f, 6>(
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(1.0, 0.0, 0.0),
);

const TAU = 6.2831853;

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	// x = dig progress (0 = idle outline only), y = per-cell seed.
	@location(1) @interpolate(flat) data: vec2f,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) i0: vec4f, // cell min xyz, progress
	@location(1) i1: vec4f, // seed, spare
) -> VsOut {
	let face = vi / 6u;
	let corner = vi % 6u;
	let n = NORMALS[face];
	let t = TANGENTS[face];
	let b = cross(n, t);
	let uv = QUAD[corner];
	// Unit box around the cell center, inflated 0.4% — deterministic
	// across backends, unlike depth-bias units.
	let local = (n * 0.5 + (uv.x - 0.5) * t + (uv.y - 0.5) * b) * 1.004 + vec3f(0.5);
	let world = i0.xyz + local;
	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.uv = uv;
	out.data = vec2f(i0.w, i1.x);
	return out;
}

// Small integer hash → 0..1 (lowbias32-style mixer).
fn hash11(x0: u32) -> f32 {
	var x = x0;
	x ^= x >> 16u;
	x *= 0x7feb352du;
	x ^= x >> 15u;
	x *= 0x846ca68bu;
	x ^= x >> 16u;
	return f32(x & 0xffffffu) / 16777216.0;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	// Manual occlusion against the solid scene (see header). The box is
	// inflated past its block, so its own face sits strictly in front of
	// the surface it decorates — no bias needed.
	let d_scene = textureLoad(scene_depth, vec2i(in.pos.xy), 0).x;
	if in.pos.z > d_scene {
		discard;
	}
	let progress = in.data.x;
	let seed = u32(in.data.y);
	// Thin dark border along every face edge (both modes — the outline
	// stays visible while digging).
	let e = min(min(in.uv.x, 1.0 - in.uv.x), min(in.uv.y, 1.0 - in.uv.y));
	var alpha = (1.0 - smoothstep(0.015, 0.035, e)) * 0.45;

	if progress > 0.0 {
		// Radial crack web from the face center: hash-jittered spokes
		// whose count, reach, and width all grow with progress. Purely
		// procedural — a few short hairlines early, a dense web near 1.
		let p = in.uv - vec2f(0.5);
		let r = max(length(p), 1e-4);
		let ang = atan2(p.y, p.x);
		let spokes = 3u + u32(progress * 6.0);
		var crack = 0.0;
		for (var i = 0u; i < 9u; i++) {
			if i >= spokes {
				break;
			}
			let h = hash11(seed * 16u + i);
			let a0 = (f32(i) + h) * TAU / f32(spokes);
			var d = abs(ang - a0);
			d = min(d, TAU - d);
			let reach = (0.30 + 0.70 * hash11(seed * 16u + i + 977u)) * (0.15 + 0.85 * progress);
			if r < reach {
				// Jagged: wobble the spoke's angle with radius.
				let wobble = sin(r * 34.0 + h * 41.0) * 0.22;
				let arc = abs(d + wobble) * r;
				let w = 0.004 + 0.014 * progress * (1.0 - r / reach);
				crack = max(crack, 1.0 - smoothstep(w * 0.5, w, arc));
			}
		}
		alpha = max(alpha, crack * 0.65);
	}
	return vec4f(0.0, 0.0, 0.0, alpha);
}
