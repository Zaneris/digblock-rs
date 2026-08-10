// GI voxel-window debug raymarch (`DIG_GI=window`; GI-DESIGN.md).
// Fullscreen DDA over the R8Uint window: proves the classification,
// the toroidal addressing, the coarse skip, and the material LUT are
// all correct before any probe traces them — hits shade as LUT albedo
// with a per-axis weight, emissive classes glow, water tints, misses
// fall to a plain sky gradient. This march is the one the P2 probe
// kernel reuses.

struct GiDebug {
	inv_view_proj: mat4x4f,
	// xyz camera position, w = window side in blocks (the toroidal
	// modulus; also the lateral march limit).
	cam: vec4f,
	misc: vec4f,
}

@group(0) @binding(0) var<uniform> dbg: GiDebug;
@group(0) @binding(1) var window_tex: texture_3d<u32>;
@group(0) @binding(2) var coarse_tex: texture_3d<u32>;
@group(0) @binding(3) var<storage, read> lut: array<vec2u>;

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) ndc: vec2f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	// The engine-standard fullscreen triangle.
	var o: VsOut;
	let x = f32(i32(vi & 1u) * 4 - 1);
	let y = f32(i32(vi >> 1u) * 4 - 1);
	o.pos = vec4f(x, y, 0.0, 1.0);
	o.ndc = vec2f(x, y);
	return o;
}

// The window texel for a world cell (toroidal XZ, direct Y).
fn window_cell(c: vec3i) -> u32 {
	let side = i32(dbg.cam.w);
	if c.y < 0 || c.y > 255 {
		return 0u;
	}
	let t = vec3i(
		((c.x % side) + side) % side,
		c.y,
		((c.z % side) + side) % side,
	);
	return textureLoad(window_tex, t, 0).r;
}

fn coarse_cell(c: vec3i) -> u32 {
	let side = i32(dbg.cam.w) / 8;
	if c.y < 0 || c.y > 31 {
		return 0u;
	}
	let t = vec3i(
		((c.x % side) + side) % side,
		c.y,
		((c.z % side) + side) % side,
	);
	return textureLoad(coarse_tex, t, 0).r;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let far = dbg.inv_view_proj * vec4f(i.ndc, 1.0, 1.0);
	let dir = normalize(far.xyz / far.w - dbg.cam.xyz);
	var p = dbg.cam.xyz;
	var cell = vec3i(floor(p));
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	// Distance along the ray to each next cell boundary.
	var tmax = abs(select(floor(p) + 1.0, floor(p), dir < vec3f(0.0)) - p) * inv;
	var water = 0.0;
	var axis = 1u; // entry axis of the last step (0 x, 1 y, 2 z)
	for (var it = 0; it < 512; it++) {
		// Outside the lateral window or the vertical world going away:
		// sky.
		let lateral = length(vec2f(cell.xz) + 0.5 - dbg.cam.xz);
		if lateral > dbg.cam.w * 0.5 - 1.0 || (cell.y > 255 && dir.y > 0.0) || cell.y < 0 {
			break;
		}
		// Coarse gate: an empty 8-cube spares the fine load entirely
		// (the DDA still steps cell by cell — the P2 kernel adds the
		// true 8-block skip; the debug view only needs the correctness
		// half of the coarse texture proven).
		let cc = coarse_cell(cell >> vec3u(3u));
		let v = select(0u, window_cell(cell), cc != 0u);
		if v != 0u {
			let m = lut[v];
			let op = m.x >> 24u;
			let albedo = vec3f(
				f32(m.x & 0xffu),
				f32((m.x >> 8u) & 0xffu),
				f32((m.x >> 16u) & 0xffu),
			) / 255.0;
			if op == 2u {
				let shade = select(select(0.66, 1.0, axis == 1u), 0.80, axis == 0u);
				var col = albedo * shade;
				if m.y > 0u {
					col = albedo * (1.0 + f32(m.y) * 0.15);
				}
				// Depth cue + water tint.
				let d = length(vec3f(cell) + 0.5 - dbg.cam.xyz);
				col *= 1.0 - min(d / (dbg.cam.w * 0.75), 0.85);
				col = mix(col, vec3f(0.1, 0.3, 0.5), min(water * 0.12, 0.8));
				return vec4f(col, 1.0);
			}
			if op == 1u {
				water += 1.0;
			}
		}
		// Step to the next cell across the nearest boundary.
		if tmax.x < tmax.y && tmax.x < tmax.z {
			cell.x += step_dir.x;
			tmax.x += inv.x;
			axis = 0u;
		} else if tmax.y < tmax.z {
			cell.y += step_dir.y;
			tmax.y += inv.y;
			axis = 1u;
		} else {
			cell.z += step_dir.z;
			tmax.z += inv.z;
			axis = 2u;
		}
	}
	let sky = mix(vec3f(0.75, 0.85, 1.0), vec3f(0.25, 0.45, 0.9), max(dir.y, 0.0));
	return vec4f(mix(sky, vec3f(0.1, 0.3, 0.5), min(water * 0.12, 0.8)), 1.0);
}
