// RTGI probe update (GI-DESIGN.md). One workgroup = one probe × 64
// rays DDA-marched through the voxel window; radiance folds into a
// 6-axis ambient cube blended over time. The atlas is six axis-major
// Z slabs: probe texel (x,y,z), axis a → (x, y, z + a*dims.z).
//
// Toroidal self-healing: a probe texel serves whatever world grid
// cell is congruent to it around the camera; flags carry a checksum
// of that cell, and a mismatch (the window scrolled) restarts the
// probe at full alpha instead of blending into another place's light.
// Sun direction arrives quantized (~1.5°) and steps do NOT reset ages
// — shadows glide across a step on steady-state hysteresis.

struct GiUniforms {
	// C0/C1 grid dims (xyz) — w: this cascade's probe spacing in blocks.
	dims: vec4u,
	// xyz: camera position in world blocks; w: window side in blocks.
	cam: vec4f,
	// xyz: quantized dir TOWARD the sun; w: sun above horizon (0/1).
	sun: vec4f,
	sun_color: vec4f,
	zenith: vec4f,
	horizon: vec4f,
	// x: first probe index this dispatch, y: frame counter,
	// z: light count cap, w: unused.
	sched: vec4u,
	// Up to 4 edit-invalidation spheres (xyz center, w radius; unused
	// entries have radius 0), applied by the `inval` entry point.
	inval0: vec4f,
	inval1: vec4f,
	inval2: vec4f,
	inval3: vec4f,
}

struct Light {
	pos: vec3f,
	kind: u32,
}

@group(0) @binding(0) var<uniform> gi: GiUniforms;
@group(0) @binding(1) var window_tex: texture_3d<u32>;
@group(0) @binding(2) var coarse_tex: texture_3d<u32>;
@group(0) @binding(3) var<storage, read> lut: array<vec2u>;
@group(0) @binding(4) var<storage, read> lights: array<vec4f>;
@group(0) @binding(5) var prev_atlas: texture_3d<f32>;
@group(0) @binding(6) var out_atlas: texture_storage_3d<rgba16float, write>;
@group(0) @binding(7) var flags_tex: texture_storage_3d<r32uint, read_write>;
// The camera-light sample the `cam_light` entry writes: [0] = omni
// probe ambient at the camera (premultiplied by INDIRECT_K), [1].x =
// sun visibility there. Spray and the viewmodel bind this as their
// light rig — no CPU probe reads.
@group(0) @binding(8) var<storage, read_write> cam_out: array<vec4f, 2>;

// Pinned twin of frame_common.wgsl's INDIRECT_K (this module can't
// take the frame prelude — its group(0) is the GI uniform). The
// digblock-assets test `indirect_k_is_one_constant` holds them equal.
const INDIRECT_K = 0.55;

const RAYS: u32 = 64u;
const MAX_T: f32 = 160.0;

// The world grid cell this probe texel currently serves: congruent to
// the texel index around the camera's grid position.
fn probe_grid_cell(t: vec3i, n: vec3i, spacing: i32) -> vec3i {
	let c = vec3i(floor(gi.cam.xyz / f32(spacing)));
	let half = n / 2;
	// Smallest g >= c - half with g ≡ t (mod n), per axis.
	let lo = c - half;
	let d = lo - t;
	return t + ((d + select(vec3i(0), n - vec3i(1), d > vec3i(0))) / n) * n;
}

fn checksum(g: vec3i) -> u32 {
	let u = vec3u(g + vec3i(0x4000));
	return ((u.x * 3u + u.y * 977u + u.z * 6151u) & 0xffffu) << 16u;
}

fn window_cell(c: vec3i) -> u32 {
	let side = i32(gi.cam.w);
	if c.y < 0 || c.y > 255 {
		return 0u;
	}
	let t = vec3i(((c.x % side) + side) % side, c.y, ((c.z % side) + side) % side);
	return textureLoad(window_tex, t, 0).r;
}

fn coarse_empty(c: vec3i) -> bool {
	let side = i32(gi.cam.w) / 8;
	if c.y < 0 || c.y > 31 {
		return true;
	}
	let t = vec3i(((c.x % side) + side) % side, c.y, ((c.z % side) + side) % side);
	return textureLoad(coarse_tex, t, 0).r == 0u;
}

// Sky as a GI source: the dome colors, pulled toward their luminance —
// the raw zenith blue is tuned for LOOKING at the sky, and bouncing it
// at full saturation tints every shadow cold (the old ambient was a
// separately tuned neutral for the same reason).
fn sky_radiance(dir: vec3f) -> vec3f {
	let c = mix(gi.horizon.rgb, gi.zenith.rgb, clamp(dir.y * 0.5 + 0.5, 0.0, 1.0));
	let lum = dot(c, vec3f(0.299, 0.587, 0.114));
	return mix(c, vec3f(lum), 0.35);
}

fn solid_cell(c: vec3i) -> bool {
	let v = window_cell(c);
	return v != 0u && (lut[v].x >> 24u) == 2u;
}

// Deterministic probe relocation (DDGI's, made stateless): a probe
// whose lattice cell is solid nudges ≤1 block into the first air
// neighbor in a FIXED order — up first (cavities open upward), then
// the sides, down last. The resolve recomputes the identical offset
// from the same voxel window, so no flags bits carry it.
fn relocate(cell: vec3i) -> vec3i {
	if !solid_cell(cell) {
		return cell;
	}
	var offs = array<vec3i, 6>(
		vec3i(0, 1, 0),
		vec3i(1, 0, 0),
		vec3i(-1, 0, 0),
		vec3i(0, 0, 1),
		vec3i(0, 0, -1),
		vec3i(0, -1, 0),
	);
	for (var k = 0; k < 6; k++) {
		if !solid_cell(cell + offs[k]) {
			return cell + offs[k];
		}
	}
	return cell;
}

struct Hit {
	hit: bool,
	cell: vec3i,
	axis: u32,
	water: f32,
}

// March a ray through the window. `max_t` bounds the march; water
// cells accumulate for transmittance.
fn march(from_p: vec3f, dir: vec3f, max_t: f32) -> Hit {
	var out: Hit;
	out.hit = false;
	out.water = 0.0;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(from_p, dir, inv);
	let lateral_max = gi.cam.w * 0.5 - 1.0;
	for (var it = 0; it < 256; it++) {
		if r.t > max_t || (r.cell.y > 255 && dir.y > 0.0) || r.cell.y < 0 {
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - gi.cam.xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		// The two-level skip (gi_dda.wgsl). Probe rays run to MAX_T = 160
		// blocks through mostly-air, so this is where it pays most.
		if coarse_empty(r.cell >> vec3u(3u)) {
			// Skip lands on the last cell inside the empty brick, then
			// steps out under the ordinary tie rule. Both are needed:
			// this loop steps at the BOTTOM, so `continue` alone would
			// not advance and the march would stall on its iteration cap.
			r = gi_ray_skip_brick(r, dir, step_dir, inv);
			r = gi_ray_step(r, step_dir, inv);
			continue;
		}
		let v = window_cell(r.cell);
		if v != 0u {
			let op = lut[v].x >> 24u;
			if op == 2u {
				out.hit = true;
				out.cell = r.cell;
				out.axis = r.axis;
				return out;
			}
			if op == 1u {
				out.water += 1.0;
			}
		}
		r = gi_ray_step(r, step_dir, inv);
	}
	return out;
}

fn albedo_of(v: u32) -> vec3f {
	let m = lut[v].x;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

// Last frame's 6-axis irradiance at a world position along a normal.
fn gather_prev(p: vec3f, n: vec3f) -> vec3f {
	let dims = vec3i(gi.dims.xyz);
	let spacing = f32(gi.dims.w);
	let g = vec3i(round(p / spacing));
	let t = ((g % dims) + dims) % dims;
	// Pinned twin of gi_common's `gi_probe_alive`. A probe buried in
	// solid stored black, and this is the site that SPREAD it: a live
	// probe would sample its buried neighbour, fold the zero into its
	// own bounce term, store the dimmed result, and hand that to its
	// neighbours next sweep. Returning no bounce is honest; returning a
	// neighbour's black as bounce is a feedback loop.
	if textureLoad(prev_atlas, t, 0).a <= 0.5 {
		return vec3f(0.0);
	}
	let w = n * n;
	var acc = vec3f(0.0);
	// Axis slabs: 0/1 ±x, 2/3 ±y, 4/5 ±z.
	let ax = select(1, 0, n.x > 0.0);
	let ay = select(3, 2, n.y > 0.0);
	let az = select(5, 4, n.z > 0.0);
	acc += w.x * textureLoad(prev_atlas, t + vec3i(0, 0, ax * dims.z), 0).rgb;
	acc += w.y * textureLoad(prev_atlas, t + vec3i(0, 0, ay * dims.z), 0).rgb;
	acc += w.z * textureLoad(prev_atlas, t + vec3i(0, 0, az * dims.z), 0).rgb;
	return acc;
}

// Radiance arriving at the probe from one ray direction.
fn trace_radiance(p: vec3f, dir: vec3f) -> vec3f {
	let h = march(p, dir, MAX_T);
	// Per-channel now, and from medium_common's measured sigma — the
	// same constant the view ray uses. This was a grey exp(-water*0.35)
	// with no relationship to the water the eye looks through.
	let water_att = medium_water_tr(h.water);
	if !h.hit {
		var sky = sky_radiance(dir);
		// The sun disc itself: a bright narrow lobe so probes carry
		// direct sun where it reaches them.
		if gi.sun.w > 0.5 {
			let s = max(dot(dir, gi.sun.xyz), 0.0);
			sky += gi.sun_color.rgb * pow(s, 32.0) * 6.0;
		}
		return sky * water_att;
	}
	// Hit: emitted + reflected (sun NEE from the hit + last frame's
	// probe field = the infinite bounce).
	let v = window_cell(h.cell);
	let alb = albedo_of(v);
	let emit = f32(lut[v].y);
	if emit > 0.0 {
		return alb * emit * 0.6 * water_att;
	}
	var n = vec3f(0.0);
	if h.axis == 0u {
		n.x = -sign(dir.x);
	} else if h.axis == 1u {
		n.y = -sign(dir.y);
	} else {
		n.z = -sign(dir.z);
	}
	let hp = vec3f(h.cell) + 0.5 + n * 0.6;
	var incident = gather_prev(hp, n);
	if gi.sun.w > 0.5 {
		let ndl = max(dot(n, gi.sun.xyz), 0.0);
		if ndl > 0.0 {
			let sh = march(hp, gi.sun.xyz, MAX_T);
			if !sh.hit {
				incident += gi.sun_color.rgb * ndl * medium_water_tr(sh.water);
			}
		}
	}
	return alb * incident * water_att;
}

var<workgroup> ray_rad: array<vec3f, 64>;
var<workgroup> ray_dir: array<vec3f, 64>;

@compute @workgroup_size(64)
fn update_probes(@builtin(workgroup_id) wg: vec3u, @builtin(local_invocation_index) li: u32) {
	let n = vec3i(gi.dims.xyz);
	let count = u32(n.x * n.y * n.z);
	let pi = (gi.sched.x + wg.x) % count;
	let t = vec3i(vec3u(pi % gi.dims.x, (pi / gi.dims.x) % gi.dims.y, pi / (gi.dims.x * gi.dims.y)));
	let spacing = i32(gi.dims.w);
	let g = probe_grid_cell(t, n, spacing);
	let cell = relocate(g * spacing);
	let p = vec3f(cell) + 0.5;

	// Spherical Fibonacci direction, rotated per update by the frame
	// counter folded into the golden-angle phase.
	let i = f32(li);
	let phase = f32(gi.sched.y % 64u) / 64.0;
	let ga = 2.399963;
	let z = 1.0 - (2.0 * i + 1.0) / f32(RAYS);
	let r = sqrt(max(1.0 - z * z, 0.0));
	let th = ga * i + phase * 6.2831853;
	let dir = normalize(vec3f(r * cos(th), z, r * sin(th)));

	// A probe still inside solid after relocation traces nothing
	// useful: dead (zero radiance).
	if solid_cell(cell) {
		ray_rad[li] = vec3f(0.0);
	} else {
		ray_rad[li] = trace_radiance(p, dir);
	}
	ray_dir[li] = dir;
	workgroupBarrier();

	if li >= 6u {
		return;
	}
	// Thread a (0..6) folds all rays into its axis: cosine-weighted.
	var axis = vec3f(0.0);
	if li == 0u {
		axis.x = 1.0;
	} else if li == 1u {
		axis.x = -1.0;
	} else if li == 2u {
		axis.y = 1.0;
	} else if li == 3u {
		axis.y = -1.0;
	} else if li == 4u {
		axis.z = 1.0;
	} else {
		axis.z = -1.0;
	}
	var acc = vec3f(0.0);
	var wsum = 0.0;
	for (var k = 0u; k < RAYS; k++) {
		let w = max(dot(ray_dir[k], axis), 0.0);
		acc += ray_rad[k] * w;
		wsum += w;
	}
	let result = acc / max(wsum, 1e-4);

	let texel = t + vec3i(0, 0, i32(li) * n.z);
	// Age/checksum ride the flags texture; thread 0 owns the update.
	let sum = checksum(g);
	let f = textureLoad(flags_tex, t).r;
	var age = f & 0xffu;
	if (f & 0xffff0000u) != sum {
		age = 0u;
	}
	var alpha = 0.10;
	if age == 0u {
		alpha = 1.0;
	} else if age < 4u {
		alpha = 0.5;
	} else if age < 8u {
		alpha = 0.25;
	}
	var prev = textureLoad(prev_atlas, texel, 0).rgb;
	if age == 0u {
		prev = result;
	}
	let blended = mix(prev, result, alpha);
	textureStore(out_atlas, texel, vec4f(blended, select(1.0, 0.0, solid_cell(cell))));
	if li == 0u {
		textureStore(flags_tex, t, vec4u(sum | min(age + 1u, 255u), 0u, 0u, 0u));
	}
}

// Camera light: one invocation samples the probe field and sun
// visibility AT THE CAMERA into cam_out, for the passes with no world
// position to gather at — spray billboards and the first-person
// viewmodel. Dispatched with the C0 cascade's bind group (prev_atlas =
// the front atlas, gi.dims = C0), after the probe update.
@compute @workgroup_size(1)
fn cam_light() {
	let spacing = i32(gi.dims.w);
	let g = vec3i(round(gi.cam.xyz / f32(spacing)));
	let cell = relocate(g * spacing);
	let p = vec3f(cell) + 0.5;
	// Omnidirectional ambient: the mean of the 6 axis irradiances.
	var amb = vec3f(0.0);
	amb += gather_prev(p, vec3f(1.0, 0.0, 0.0));
	amb += gather_prev(p, vec3f(-1.0, 0.0, 0.0));
	amb += gather_prev(p, vec3f(0.0, 1.0, 0.0));
	amb += gather_prev(p, vec3f(0.0, -1.0, 0.0));
	amb += gather_prev(p, vec3f(0.0, 0.0, 1.0));
	amb += gather_prev(p, vec3f(0.0, 0.0, -1.0));
	amb /= 6.0;
	var sv = vec3f(0.0);
	if gi.sun.w > 0.5 {
		let h = march(p, gi.sun.xyz, MAX_T);
		if !h.hit {
			sv = medium_water_tr(h.water);
		}
	}
	cam_out[0] = vec4f(amb * INDIRECT_K, 0.0);
	// rgb, not one grey lane: sunlight reaching a submerged camera is
	// red-starved, and the hand and the spray must agree with the world.
	cam_out[1] = vec4f(sv, 0.0);
}

// Edit invalidation: reset ages inside the spheres (both halves of the
// flags word survive except age).
@compute @workgroup_size(64)
fn inval(@builtin(global_invocation_id) id: vec3u) {
	let n = vec3i(gi.dims.xyz);
	let count = u32(n.x * n.y * n.z);
	if id.x >= count {
		return;
	}
	let t = vec3i(vec3u(id.x % gi.dims.x, (id.x / gi.dims.x) % gi.dims.y, id.x / (gi.dims.x * gi.dims.y)));
	let spacing = i32(gi.dims.w);
	let g = probe_grid_cell(t, n, spacing);
	let p = vec3f(g * spacing) + 0.5;
	var hitcount = 0u;
	if gi.inval0.w > 0.0 && distance(p, gi.inval0.xyz) < gi.inval0.w {
		hitcount++;
	}
	if gi.inval1.w > 0.0 && distance(p, gi.inval1.xyz) < gi.inval1.w {
		hitcount++;
	}
	if gi.inval2.w > 0.0 && distance(p, gi.inval2.xyz) < gi.inval2.w {
		hitcount++;
	}
	if gi.inval3.w > 0.0 && distance(p, gi.inval3.xyz) < gi.inval3.w {
		hitcount++;
	}
	if hitcount == 0u {
		return;
	}
	let f = textureLoad(flags_tex, t).r;
	textureStore(flags_tex, t, vec4u(f & 0xffff0000u, 0u, 0u, 0u));
}
