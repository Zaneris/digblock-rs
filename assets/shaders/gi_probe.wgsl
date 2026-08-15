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
	// x: this dispatch's PERMUTED base probe index, y: frame counter,
	// z: light count cap, w: the sweep's permutation stride.
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
@group(0) @binding(3) var<storage, read> lut: array<vec4u>;
@group(0) @binding(4) var<storage, read> lights: array<vec4f>;
@group(0) @binding(5) var prev_atlas: texture_3d<f32>;
@group(0) @binding(6) var out_atlas: texture_storage_3d<rgba16float, write>;
@group(0) @binding(7) var flags_tex: texture_storage_3d<r32uint, read_write>;
// The camera-light sample the `cam_light` entry writes: [0] = omni
// probe ambient at the camera, [1].rgb =
// sun visibility there. Spray and the viewmodel bind this as their
// light rig — no CPU probe reads.
@group(0) @binding(8) var<storage, read_write> cam_out: array<vec4f, 2>;

const RAYS: u32 = 32u;
const MAX_T: f32 = 160.0;

// How far an emitter is allowed to matter. `light` is a 0-15 level and
// the 1/d^2 falloff has already buried it well before this; the bound
// exists so a shadow ray is never traced across the whole window for a
// contribution that rounds to nothing.
const LIGHT_RANGE: f32 = 24.0;
// Emitter radiance per unit of `light` level. One calibration constant,
// and it is chosen rather than derived: `light` is a gameplay nibble,
// not a photometric quantity. 0.6 set the pool at the brightness the
// direct-hit term already gave the emitting cell itself, which is what
// keeps a lit torch and the wall beside it looking like one source.
//
// 0.75 buys a torch that lights a room rather than a puddle, and it is
// a CEILING rather than a preference: the flame's own ink cannot follow
// the pool up. chunk.wgsl's EMISSIVE_GAIN was tried at 2.4 against 1.6
// and moved the sprite 3 luminance levels out of 255 — the ink is
// already clamped by the surface-format scene target, so there is no
// headroom above it to buy the contrast back with. Every step of pool
// brightness therefore comes straight out of the margin by which the
// source out-reads what it casts. Measured in DIG_VAULT at the standard
// capture pose (--tod 90 --pitch -20, 1200 frames):
//
//   gain   clip(R>=254)   flame   pool   flame/pool
//   0.60         0.12%    211.4  118.8         1.78
//   0.75         0.65%    212.8  165.2         1.29
//   0.90         2.45%    219.6  190.5         1.15
//   1.20         5.32%    216.7  216.6         1.00
//
// At 1.00 the wall behind the torch reads as bright as the flame and
// the picture inverts. 0.75 is where the pool grows most for the least
// of that margin; past it the clip fraction turns over into the
// hard-edged-ellipse regime LIGHT_NEAR was introduced to remove (R22
// measured 2.96% there). The real headroom is an HDR scene target and
// one tonemap on the composite — the same backlog item LIGHT_NEAR and
// EMISSIVE_GAIN are both stopgaps for.
const LIGHT_GAIN: f32 = 0.75;
// Where the point model's near field stops being taken literally, in
// blocks — the pinned twin of gi_resolve's, and the same reasoning: a
// voxel emitter is not a point, 1/d^2 diverges into it, and the support
// block half a block away took the whole of `emit * LIGHT_GAIN` against
// a display whose white is 1.0. Beyond it the falloff is exactly 1/d^2,
// so no probe outside this radius changes at all.
const LIGHT_NEAR: f32 = 2.0;

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

// The wrap helpers, twins of gi_common's — one modulo per axis, shared
// between the coarse and fine lookups. The reasoning (and the bench
// numbers, which were taken on the resolve rather than here) is over
// there; this kernel's own march measures inside the noise either way,
// but the two steppers have to stay the same code or the cache and the
// per-pixel answer drift.
fn wrap2(v: vec2i, m: i32) -> vec2i {
	let r = v % vec2i(m);
	return select(r, r + vec2i(m), r < vec2i(0));
}

fn window_cell_w(w: vec2i, y: i32) -> u32 {
	if y < 0 || y > 255 {
		return 0u;
	}
	return textureLoad(window_tex, vec3i(w.x, y, w.y), 0).r;
}

fn coarse_empty_w(w: vec2i, y: i32) -> bool {
	if y < 0 || y > 255 {
		return true;
	}
	return textureLoad(coarse_tex, vec3i(w.x >> 3u, y >> 3u, w.y >> 3u), 0).r == 0u;
}

fn window_cell(c: vec3i) -> u32 {
	return window_cell_w(wrap2(c.xz, i32(gi.cam.w)), c.y);
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
	// Partial cells (opacity 3) stay "solid" for probe placement — the
	// pinned twin of gi_common's `gi_solid`, and the old conservative
	// behavior. Only `march` below resolves their sub-boxes.
	return v != 0u && (lut[v].x >> 24u) >= 2u;
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
	let lateral_max = gi_lateral_max(gi.cam.w);
	for (var it = 0; it < 256; it++) {
		// gi.zenith.w carries the sky ceiling (R18): a non-descending ray
		// above the window's highest occupied cell can hit nothing — the
		// gi_common twin comment has the measurement.
		if r.t > max_t || (dir.y >= 0.0 && f32(r.cell.y) > gi.zenith.w) || r.cell.y < 0 {
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - gi.cam.xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		// The two-level skip (gi_dda.wgsl). Probe rays run to MAX_T = 160
		// blocks through mostly-air, so this is where it pays most. (A
		// third 64-cube level was measured out — see gi_common's marches.)
		let w = wrap2(r.cell.xz, i32(gi.cam.w));
		if coarse_empty_w(w, r.cell.y) {
			// Skip lands on the last cell inside the empty region, then
			// steps out under the ordinary tie rule. Both are needed:
			// this loop steps at the BOTTOM, so `continue` alone would
			// not advance and the march would stall on its iteration cap.
			r = gi_ray_skip_brick(r, dir, step_dir, inv);
			r = gi_ray_step(r, step_dir, inv);
			continue;
		}
		let v = window_cell_w(w, r.cell.y);
		if v != 0u {
			// Word 0 only on the hot path — the pinned twin of
			// gi_common's marches (16 B per occupied cell measured
			// +0.4 ms on the bench).
			let op = lut[v].x >> 24u;
			if op == 2u {
				out.hit = true;
				out.cell = r.cell;
				out.axis = r.axis;
				return out;
			}
			if op == 3u {
				// Every partial cell blocks only where its boxes stand,
				// emitters included since R27 — the pinned twin of
				// gi_common's `gi_march` rule. See there for why the
				// emitting special case existed and why taking the
				// emission out of these marches is what retired it.
				let t_exit = min(r.tmax.x, min(r.tmax.y, r.tmax.z));
				let bh = gi_boxes_hit(from_p, dir, r.cell, r.t, t_exit, lut[v].zw, r.axis);
				if bh.x >= 0.0 {
					out.hit = true;
					out.cell = r.cell;
					out.axis = u32(bh.y);
					return out;
				}
			}
			if op == 1u {
				out.water += 1.0;
			}
		}
		r = gi_ray_step(r, step_dir, inv);
	}
	return out;
}

// Next-event estimation against the emitter list — no longer the only
// NEE in the system: since R20 the resolve fires a DETERMINISTIC ray at
// each pixel's dominant light from the same buffer, with const twins of
// LIGHT_RANGE/LIGHT_GAIN pinned to these by
// `resolve_and_probe_agree_on_light_calibration`.
//
// Before P4 the `lights` buffer had been uploaded and bound but read by
// NO shader. Emitters reached the picture only when one of a probe's 64
// Fibonacci rays happened to strike the single voxel a torch occupies —
// exactly the firefly a light list exists to prevent. A 1-voxel emitter
// subtends under 1% of the sphere at 3 blocks, so a given probe had
// better than even odds of finding it not at all: large walls caught it
// across many probes that bleed into one another, small isolated objects
// sat in the few cells that all missed. That is why DIG_VAULT has a
// warm pool on its walls and a pitch-black wood cube standing in it.
//
// One sample per ray, uniform over the list, weighted by 1/pdf = count.
// With 64 rays a probe takes 64 NEE samples per update and the
// hysteresis ladder filters what is left.
fn nee(p: vec3f, n: vec3f, rnd: f32) -> vec3f {
	let count = bitcast<u32>(lights[0].x);
	if count == 0u {
		return vec3f(0.0);
	}
	let li = min(u32(rnd * f32(count)), count - 1u);
	let l = lights[li + 1u];
	let d = l.xyz - p;
	let dist2 = dot(d, d);
	if dist2 > LIGHT_RANGE * LIGHT_RANGE || dist2 < 1e-4 {
		return vec3f(0.0);
	}
	let dist = sqrt(dist2);
	let dir = d / dist;
	let ndl = dot(n, dir);
	if ndl <= 0.0 {
		return vec3f(0.0);
	}
	let cls = gi_light_class(l.w);
	let emit = f32(lut[cls].y);
	if emit <= 0.0 {
		return vec3f(0.0);
	}
	// Stop short of the emitter's own cell. The torch classifies as a
	// PARTIAL cell now (its post boxes), so its own geometry rarely
	// blocks — but the margin stays: a ray run to the exact emitter
	// point can still clip the box it stands in, and the light must
	// never shadow itself. `gi_light_margin` derives it from where in
	// its cell the light actually sits, so it holds for an origin that
	// is not the cell center — see its comment for what a flame-height
	// torch did to a flat 0.9, and why a CARRIED light takes zero.
	let sh = march(p, dir, max(dist - gi_light_margin_of(l), 0.0));
	if sh.hit {
		return vec3f(0.0);
	}
	// A voxel emitter as a point source: intensity falls as 1/d^2 outside
	// LIGHT_NEAR and holds inside it — the clamp started as "don't divide
	// by nothing" for a probe sharing a torch's cell and is now also what
	// keeps the near field under the white point (see the constant).
	let radiance = albedo_of(cls) * emit * LIGHT_GAIN;
	return radiance * (ndl / max(dist2, LIGHT_NEAR * LIGHT_NEAR))
		* medium_water_tr(sh.water) * f32(count);
}

fn albedo_of(v: u32) -> vec3f {
	let m = lut[v].x;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

// Last frame's 6-axis irradiance at a world position along a normal.
// Is `p` inside this cascade's window at all? The atlas is toroidal, so
// `g % dims` ALWAYS yields a texel — just one serving a different world
// cell once you leave the window. Rays reach MAX_T while C0 covers only
// dims * spacing, so without this a distant hit folds a different part of
// the world into this probe's bounce.
//
// The fragment-side gathers got this at R3 (`gi_in_cascade`). This is the
// same bug on the compute side, and it is the worse half: a fragment that
// reads a wrapped probe is wrong for one pixel for one frame, whereas a
// PROBE that reads one folds the wrong radiance into its own stored value
// and hands it on as bounce, every sweep, to everything that gathers it.
fn prev_in_cascade(p: vec3f) -> bool {
	let spacing = f32(gi.dims.w);
	let half = vec3f(gi.dims.xyz) * spacing * 0.5;
	return all(abs(p - gi.cam.xyz) < half - vec3f(spacing));
}

fn gather_prev(p: vec3f, n: vec3f) -> vec3f {
	let dims = vec3i(gi.dims.xyz);
	let spacing = f32(gi.dims.w);
	if !prev_in_cascade(p) {
		// Beyond coverage there is no probe for `p`. The analytic dome is
		// what the fragment side falls through to, and the same 0.35 —
		// answering with a wrapped read instead is what this exists to
		// stop, and answering with zero would darken every distant bounce.
		return sky_radiance(n) * 0.35;
	}
	let g = vec3i(round(p / spacing));
	let t = ((g % dims) + dims) % dims;
	// Is this texel still serving the world cell we are asking about? The
	// window is toroidal, so a texel is REMAPPED to a new world cell as
	// the camera travels, and it keeps the old cell's radiance until the
	// sweep reaches it. `update_probes` already computes this checksum and
	// resets the probe's age on a mismatch — but only when it gets there.
	// Reading it here is what stops the interval between the remap and the
	// re-sweep from feeding another place's light into this probe's bounce
	// and STORING it.
	//
	// This matters more since the cadence cut: a full sweep is 48 frames
	// now rather than 12, so the stale window is four times longer.
	if (textureLoad(flags_tex, t).r & 0xffff0000u) != checksum(g) {
		return sky_radiance(n) * 0.35;
	}
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

// A probe grid cell's texel in the toroidal atlas, and the two reads the
// camera light needs off it. Indexing by the grid cell directly is not a
// convenience: `gather_prev` takes a world POINT and snaps it with
// `round(p / spacing)`, so handing it a RELOCATED probe centre — nudged a
// whole block at a spacing of 2 — rounds to the neighbouring probe's
// texel and silently reads the wrong one.
fn prev_texel(g: vec3i) -> vec3i {
	let dims = vec3i(gi.dims.xyz);
	return ((g % dims) + dims) % dims;
}

fn prev_alive(g: vec3i) -> bool {
	return textureLoad(prev_atlas, prev_texel(g), 0).a > 0.5;
}

// The mean of a probe's six axis irradiances — an omnidirectional
// ambient, for the billboards and the viewmodel that have no normal to
// gather about.
fn prev_omni(g: vec3i) -> vec3f {
	let dims = vec3i(gi.dims.xyz);
	let t = prev_texel(g);
	var acc = vec3f(0.0);
	for (var a = 0; a < 6; a++) {
		acc += textureLoad(prev_atlas, t + vec3i(0, 0, a * dims.z), 0).rgb;
	}
	return acc / 6.0;
}

// Radiance arriving at the probe from one ray direction. `rnd` selects
// this ray's NEE sample.
fn trace_radiance(p: vec3f, dir: vec3f, rnd: f32) -> vec3f {
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
	if f32(lut[v].y) > 0.0 {
		// An emitter reaches the cache through `nee` below and nowhere
		// else — the twin of gi_resolve's `path_radiance` rule (R27), for
		// the same reason: this term is radiance, so it does not fall off,
		// and a probe that catches a torch on one of 32 rays folds a 6.0
		// into a 6-axis average that then hysteresis-blends for 48 frames.
		// The cell still occludes.
		//
		// It also had a stale calibration. This read `emit * 0.6` — the
		// pre-R26 LIGHT_GAIN, left behind when the constant went to 0.75,
		// and invisible to `resolve_and_probe_agree_on_light_calibration`
		// because that pin compares the two shaders' NAMED consts and a
		// bare literal has no name to compare.
		return vec3f(0.0);
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
	incident += nee(hp, n, rnd);
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

var<workgroup> ray_rad: array<vec3f, RAYS>;
var<workgroup> ray_dir: array<vec3f, RAYS>;

@compute @workgroup_size(32)
fn update_probes(@builtin(workgroup_id) wg: vec3u, @builtin(local_invocation_index) li: u32) {
	let n = vec3i(gi.dims.xyz);
	let count = u32(n.x * n.y * n.z);
	// Permuted sweep: sweep step k maps to probe (k * stride) % count,
	// which is a bijection because the stride is coprime with the count.
	// The base arrives pre-reduced (the CPU does `cursor * stride` in 64
	// bits); only `wg.x * stride` happens here, and the stride is clamped
	// so that cannot overflow. A contiguous cursor made convergence sweep
	// the volume as a visible band.
	let pi = (gi.sched.x + wg.x * gi.sched.w) % count;
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
		// Per-ray, per-update decorrelated NEE pick: the ray index and
		// the frame counter, hashed. Reusing one draw for all 64 rays
		// would sample one emitter 64 times and miss the rest entirely.
		let rnd = fract(sin(f32(li) * 12.9898 + f32(gi.sched.y) * 78.233) * 43758.5453);
		ray_rad[li] = trace_radiance(p, dir, rnd);
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
// This used to snap the camera to the C0 lattice — `round(cam/2)*2`,
// relocate, and read that ONE probe six times. Two things then went wrong
// together, and both were quantized to that 2-block snap, which is why
// they appeared and vanished with a step rather than fading. `gather_prev`
// returns literal black for a probe buried in solid (rightly — see its
// comment), so a single dead probe zeroed all six axes at once; and the
// sun ray started from the same buried point, hit immediately, and
// returned no sun. Against a cliff — which is exactly where a waterfall
// is — `relocate` tries six neighbours and can find every one of them
// solid, so the pair produced FULLY BLACK spray from some standing
// positions and bright spray from a step away.
//
// The fragment-side gathers never had this: `gi_cascade_gather` blends
// eight probes, drops the dead ones OUT of the weight instead of folding
// their zero in, and falls back to the sky dome when nothing survives.
// This is that same discipline, which is all it ever needed.
@compute @workgroup_size(1)
fn cam_light() {
	let spacing = f32(gi.dims.w);
	let cam = gi.cam.xyz;
	// Trilinear over the eight probes around the camera ITSELF, on
	// gi_cascade_gather's grid convention so the hand and the world agree
	// about where a probe is. No snap: the camera is a real position, and
	// quantizing it was only ever a way to name one texel to read.
	let base = floor(cam / spacing - 0.5);
	let frac = cam / spacing - 0.5 - base;
	var amb = vec3f(0.0);
	var wsum = 0.0;
	for (var k = 0; k < 8; k++) {
		let o = vec3i(k & 1, (k >> 1) & 1, (k >> 2) & 1);
		let tw = abs(vec3f(vec3i(1) - o) - frac);
		let w = tw.x * tw.y * tw.z;
		if w < 1e-4 {
			continue;
		}
		let g = vec3i(base) + o;
		// Buried probes leave the average instead of voting black into
		// it. This is the whole fix: one dead probe used to be the only
		// probe, so its zero WAS the answer.
		if !prev_alive(g) {
			continue;
		}
		amb += prev_omni(g) * w;
		wsum += w;
	}
	if wsum > 0.0 {
		amb /= wsum;
	} else {
		// Every candidate was buried. The camera is inside geometry (or
		// in a pocket smaller than the probe spacing), and the honest
		// answer is the dome rather than black — black is what made the
		// spray disappear.
		amb = sky_radiance(vec3f(0.0, 1.0, 0.0)) * 0.35;
	}
	var sv = vec3f(0.0);
	if gi.sun.w > 0.5 {
		// From the camera, not from a snapped lattice point that may sit
		// on the far side of a wall from it.
		let h = march(cam, gi.sun.xyz, MAX_T);
		if !h.hit {
			sv = medium_water_tr(h.water);
		}
	}
	cam_out[0] = vec4f(amb, 0.0);
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
