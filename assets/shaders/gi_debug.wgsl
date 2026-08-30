// GI voxel-window debug raymarch (`DIG_GI=window`).
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
@group(0) @binding(3) var<storage, read> lut: array<vec4u>;
// C0's front probe atlas (six axis-major Z slabs); misc.yzw = dims.
@group(0) @binding(4) var atlas_tex: texture_3d<f32>;

// C0's probe spacing in blocks. Pinned twin of the value gi.rs builds
// cascade 0 with; this view reads the atlas directly rather than through
// gi_common's parameterised gather, so it has to be told.
const C0_SPACING = 2.0;

// Nearest-probe 3-axis gather of the C0 field. A THIRD copy of the axis
// gather: it bypasses gi_common's `gi_cascade_gather`, so a fix made there
// does not reach `DIG_GI=probes`, and the capture reads as though it never
// landed. It cannot share the real one while its bindings are its own, but
// it must track the alive flag and where C0's probes are.
fn probe_light(p: vec3f, n: vec3f) -> vec3f {
	let dims = vec3i(vec3f(dbg.misc.y, dbg.misc.z, dbg.misc.w));
	let g = vec3i(round(p / C0_SPACING));
	let t = ((g % dims) + dims) % dims;
	if textureLoad(atlas_tex, t, 0).a <= 0.5 {
		return vec3f(0.0);
	}
	let w = n * n;
	let ax = select(1, 0, n.x > 0.0);
	let ay = select(3, 2, n.y > 0.0);
	let az = select(5, 4, n.z > 0.0);
	var acc = vec3f(0.0);
	acc += w.x * textureLoad(atlas_tex, t + vec3i(0, 0, ax * dims.z), 0).rgb;
	acc += w.y * textureLoad(atlas_tex, t + vec3i(0, 0, ay * dims.z), 0).rgb;
	acc += w.z * textureLoad(atlas_tex, t + vec3i(0, 0, az * dims.z), 0).rgb;
	return acc;
}

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
	let p = dbg.cam.xyz;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(p, dir, inv);
	var water = 0.0;
	for (var it = 0; it < 512; it++) {
		let cell = r.cell;
		let axis = r.axis;
		// Outside the lateral window or the vertical world going away:
		// sky.
		let lateral = length(vec2f(cell.xz) + 0.5 - dbg.cam.xz);
		if lateral > gi_lateral_max(dbg.cam.w) || (cell.y > 255 && dir.y > 0.0) || cell.y < 0 {
			break;
		}
		// Coarse gate, now the real two-level skip (gi_dda.wgsl): an
		// empty coarse brick is left in one jump. This view exists to
		// prove the window, the toroidal addressing and the coarse texture
		// agree with what the probe kernel sees, so it has to march the
		// same way the probe kernel does.
		if coarse_cell(cell >> vec3u(3u)) == 0u {
			r = gi_ray_skip_brick(r, dir, step_dir, inv);
			r = gi_ray_step(r, step_dir, inv);
			continue;
		}
		let v = window_cell(cell);
		if v != 0u {
			let m = lut[v];
			let op = m.x >> 24u;
			let albedo = vec3f(
				f32(m.x & 0xffu),
				f32((m.x >> 8u) & 0xffu),
				f32((m.x >> 16u) & 0xffu),
			) / 255.0;
			// Partial cells draw as their whole cell here on
			// purpose: this view proves the WINDOW's contents, and the
			// cell-granular picture is what the window actually stores.
			if op >= 2u {
				let shade = select(select(0.66, 1.0, axis == 1u), 0.80, axis == 0u);
				var col = albedo * shade;
				if m.y > 0u {
					col = albedo * (1.0 + f32(m.y) * 0.15);
				}
				// Probe mode: light the hit with the C0 field instead —
				// the whole P2 chain (trace, bounce, hysteresis, toroidal
				// checksums) on screen at once.
				if dbg.misc.x > 0.5 {
					var fn_ = vec3f(0.0);
					if axis == 0u {
						fn_.x = -sign(dir.x);
					} else if axis == 1u {
						fn_.y = -sign(dir.y);
					} else {
						fn_.z = -sign(dir.z);
					}
					let hp = vec3f(cell) + 0.5 + fn_ * 0.6;
					return vec4f(albedo * probe_light(hp, fn_), 1.0);
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
		r = gi_ray_step(r, step_dir, inv);
	}
	let sky = mix(vec3f(0.75, 0.85, 1.0), vec3f(0.25, 0.45, 0.9), max(dir.y, 0.0));
	return vec4f(mix(sky, vec3f(0.1, 0.3, 0.5), min(water * 0.12, 0.8)), 1.0);
}
