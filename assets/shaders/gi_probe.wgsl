// RTGI probe update (GI-DESIGN.md). One workgroup = one probe, its RAYS
// directions DDA-marched through the voxel window; radiance folds into a
// 6-axis ambient cube blended over time. The atlas is six axis-major
// Z slabs: probe texel (x,y,z), axis a → (x, y, z + a*dims.z).
//
// Toroidal self-healing: a probe texel serves whatever world grid
// cell is congruent to it around the camera; flags carry a checksum
// of that cell, and a mismatch (the window scrolled) restarts the
// probe at full alpha instead of blending into another place's light.
// Sun direction arrives quantized and steps do NOT reset ages —
// shadows glide across a step on steady-state hysteresis.

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

// The LUT's tinted-transmission op — mirrored from the CPU's
// `LUT_OP_TINTED` and pinned by `the_lut_ops_match_the_shaders`.
const GI_LUT_OP_TINTED = 4u;
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

// How far an emitter is allowed to matter. `light` is a gameplay level
// and the 1/d^2 falloff has already buried it well before this; the bound
// exists so a shadow ray is never traced across the whole window for a
// contribution that rounds to nothing.
const LIGHT_RANGE: f32 = 24.0;
// Emitter radiance per unit of `light` level, CHOSEN rather than derived: `light`
// is a gameplay nibble, not a photometric quantity. It is a CEILING rather than a
// preference — chunk.wgsl's EMISSIVE_GAIN is already clamped by the scene target, so
// every step of pool brightness eats the margin by which a source out-reads what it
// casts, and past here the clip fraction turns over. The sweep is in GI-DESIGN.md R26.
const LIGHT_GAIN: f32 = 0.75;
// Where the point model's near field stops being taken literally, in blocks — the
// pinned twin of gi_resolve's, and the same reasoning: a voxel emitter is not a point,
// 1/d^2 diverges into it, and the support block beside it took the whole of
// `emit * LIGHT_GAIN` against a display whose white is 1.0. Beyond it the falloff is
// exactly 1/d^2, so no probe outside this radius changes at all.
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

// The wrap helpers, twins of gi_common's — one modulo per axis, shared between the
// coarse and fine lookups. The two steppers have to stay the same code or the cache
// and the per-pixel answer drift; this kernel's own march measures inside the noise
// either way, and the numbers (taken on the resolve) are over there.
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
	// Partial cells stay "solid" for probe placement — the pinned
	// twin of gi_common's `gi_solid`, and the old conservative
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
	// On a hit, the cell struck; on a miss, the cell where the march
	// ended — the point an unproven answer is evaluated at (R32).
	cell: vec3i,
	axis: u32,
	water: f32,
	// Glass throughput crossed on the way (op 4) — the pinned twin of
	// gi_common's `GiSunVis.thru`.
	thru: vec3f,
	// Miss quality (R32) — the pinned twin of gi_common's `GiHit.sky`:
	// true only on the PROVEN sky-ceiling exit, false when the ray merely
	// ran out (max_t, lateral bound, iteration cap, the world's floor).
	sky: bool,
}

// March a ray through the window. `max_t` bounds the march; water
// cells accumulate for transmittance.
fn march(from_p: vec3f, dir: vec3f, max_t: f32) -> Hit {
	var out: Hit;
	out.hit = false;
	out.water = 0.0;
	out.thru = vec3f(1.0);
	out.sky = false;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(from_p, dir, inv);
	let lateral_max = gi_lateral_max(gi.cam.w);
	for (var it = 0; it < 256; it++) {
		// gi.zenith.w carries the sky ceiling (R18): a non-descending ray
		// above the window's highest occupied cell can hit nothing — the
		// gi_common twin comment has the measurement.
		if dir.y >= 0.0 && f32(r.cell.y) > gi.zenith.w {
			out.sky = true;
			break;
		}
		if r.t > max_t || r.cell.y < 0 {
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - gi.cam.xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		// The two-level skip (gi_dda.wgsl). Probe rays run all the way to
		// MAX_T through mostly-air, so this is where it pays most. (A
		// third, coarser level was measured out — see gi_common's marches.)
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
			// gi_common's marches (the whole entry per occupied cell
			// measured a clear cost on the bench).
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
			if op == GI_LUT_OP_TINTED {
				out.thru *= glass_tr(v);
			}
		}
		r = gi_ray_step(r, step_dir, inv);
	}
	// Every miss path lands here; record where the march ended.
	out.cell = r.cell;
	return out;
}

// Next-event estimation against the emitter list, no longer the only NEE in the
// system: since R20 the resolve fires a DETERMINISTIC ray at each pixel's dominant
// light from the same buffer, with const twins of LIGHT_RANGE and LIGHT_GAIN pinned by
// `resolve_and_probe_agree_on_light_calibration`. Without it an emitter reaches the
// picture only when a Fibonacci ray strikes the one voxel a torch occupies — a firefly.
fn nee(p: vec3f, n: vec3f, rnd: f32) -> vec3f {
	let count = bitcast<u32>(lights[0].x);
	if count == 0u {
		return vec3f(0.0);
	}
	let li = min(u32(rnd * f32(count)), count - 1u);
	// Two vec4s an entry, as in the resolve: position and class word,
	// then the rectangle a surface emitter radiates from.
	let l = lights[li * 2u + 1u];
	let q = lights[li * 2u + 2u];
	// A point on the drawn light. The probe has one uniform to spend and
	// has already spent it choosing WHICH light, so the point on the face
	// is taken from a hash of the two it does have — a probe's rays are
	// averaged over a long history and a fixed point on every rectangle
	// would bake the face's centre line into the cache.
	let s = gi_light_sample(l, q, p, fract(rnd * 37.0), fract(rnd * 91.0));
	let d = s.p - p;
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
	// The entry's own level when it has one (a tool whose wielder has
	// earned a brighter glow), else the class's — see gi_dda.wgsl. The
	// resolve's reservoir reads it the same way, so the two NEE passes
	// price the same light identically.
	let ov = gi_light_level(l.w);
	let emit = select(f32(lut[cls].y & 0xffu), f32(ov), ov != 0u);
	if emit <= 0.0 {
		return vec3f(0.0);
	}
	// Stop short of the emitter's own cell: a ray to the exact emitter point can
	// clip the box it stands in, and a light must never shadow itself.
	// `gi_light_margin` derives the margin from where in its cell the light sits,
	// so it holds for an origin that is not the cell centre — see its comment for
	// the flame-height torch, and for why a CARRIED light takes zero.
	let sh = march(p, dir, max(dist - gi_light_margin_of(l), 0.0));
	if sh.hit {
		return vec3f(0.0);
	}
	// A voxel emitter as a point source: intensity falls as 1/d^2 outside
	// LIGHT_NEAR and holds inside it — the clamp started as "don't divide
	// by nothing" for a probe sharing a torch's cell and is now also what
	// keeps the near field under the white point (see the constant).
	let radiance = gi_light_tint(l.w, emit_tint_of(cls)) * emit * LIGHT_GAIN;
	// `s.w` is 1 for a point and cos(theta_l) * area for a rectangle —
	// the factors that make an area source an area source, from the one
	// shared sampler, so the probes and the resolve cannot price the same
	// seam differently.
	return radiance * (ndl * s.w / max(dist2, LIGHT_NEAR * LIGHT_NEAR))
		* medium_water_tr(sh.water) * sh.thru * f32(count);
}

// The tinted class's rgb filter (op 4) — gi_common's `gi_glass_tr` twin
// over this pass's own `lut` binding.
fn glass_tr(v: u32) -> vec3f {
	let m = lut[v].z;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

fn albedo_of(v: u32) -> vec3f {
	let m = lut[v].x;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

// The colour a class CASTS — gi_common's `gi_emit_tint` against this
// shader's own binding. Usually the albedo, but an ore seam's mean is
// stone and its light is not, so the cast colour rides the LUT word above
// the level. See the twin for the measurement.
fn emit_tint_of(v: u32) -> vec3f {
	let m = lut[v].y >> 8u;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

// Last frame's 6-axis irradiance at a world position along a normal. Is `p` inside
// this cascade's window at all? The atlas is toroidal, so `g % dims` ALWAYS yields a
// texel — just one serving a different world cell once you leave the window, while rays
// reach MAX_T and C0 covers only dims * spacing. The fragment side got this at R3
// (`gi_in_cascade`); here a PROBE that folds STORES the wrong radiance and bounces it on.
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
		// what the fragment side falls through to, dimmed the same way —
		// answering with a wrapped read instead is what this exists to
		// stop, and answering with zero would darken every distant bounce.
		// Depth-faded exactly as gi_common's `gi_sky_fallback` (R32): the
		// compute-side twin must fade with it, or the cache's bounce
		// re-imports the very leak the fragment side removed.
		return sky_radiance(n) * 0.35 * medium_sky_fade(p.y);
	}
	let g = vec3i(round(p / spacing));
	let t = ((g % dims) + dims) % dims;
	// Is this texel still serving the world cell we are asking about? The window
	// is toroidal, so a texel is REMAPPED to a new world cell as the camera
	// travels and keeps the old cell's radiance until the sweep reaches it.
	// `update_probes` resets the age on a mismatch, but only when it gets there;
	// reading it here is what stops the gap feeding another place's light in.
	if (textureLoad(flags_tex, t).r & 0xffff0000u) != checksum(g) {
		return sky_radiance(n) * 0.35 * medium_sky_fade(p.y);
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

// A probe grid cell's texel in the toroidal atlas, and the two reads the camera light
// needs off it. Indexing by the grid cell directly is not a convenience: `gather_prev`
// takes a world POINT and snaps it with `round(p / spacing)`, so handing it a RELOCATED
// probe centre — nudged a whole block, most of the way to the next probe at C0's
// spacing — rounds to the neighbouring texel and silently reads the wrong one.
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
	// same constant the view ray uses. This was a grey exponential with
	// no relationship to the water the eye looks through.
	let water_att = medium_water_tr(h.water) * h.thru;
	if !h.hit {
		var sky = sky_radiance(dir);
		// The sun disc itself: a bright narrow lobe so probes carry
		// direct sun where it reaches them.
		if gi.sun.w > 0.5 {
			let s = max(dot(dir, gi.sun.xyz), 0.0);
			sky += gi.sun_color.rgb * pow(s, 32.0) * 6.0;
		}
		// A miss that PROVED sky (the ceiling test) is daylight at any
		// depth; one that merely ran out of march is faded at the exhaust
		// point (R32) — an exhausted deep ray must not carry the sun lobe
		// into a probe fifty blocks down a tunnel.
		return sky * water_att
			* select(medium_sky_fade(f32(h.cell.y) + 0.5), 1.0, h.sky);
	}
	// Hit: emitted + reflected (sun NEE from the hit + last frame's
	// probe field = the infinite bounce).
	let v = window_cell(h.cell);
	let alb = albedo_of(v);
	if f32(lut[v].y) > 0.0 {
		// An emitter reaches the cache through `nee` below and nowhere else —
		// the twin of gi_resolve's `path_radiance` rule (R27), for the same
		// reason: this term is radiance, so it does not fall off, and a probe
		// that catches a torch on one of its rays folds a huge value into a
		// 6-axis average that then hysteresis-blends for a whole sweep. The
		// cell still occludes.
		//
		// Never write the gain as a bare literal here: the pin
		// `resolve_and_probe_agree_on_light_calibration` compares the two
		// shaders' NAMED consts, and a literal has no name to compare.
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
				// Unproven sun (the ray ran out rather than clearing the
				// ceiling) fades at the receiving surface — the same rule
				// gi_sun_vis applies at its shading point (R32).
				incident += gi.sun_color.rgb * ndl * medium_water_tr(sh.water) * sh.thru
					* select(medium_sky_fade(hp.y), 1.0, sh.sky);
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
	// Permuted sweep: step k maps to probe (k * stride) % count, a bijection
	// because the stride is coprime with the count. The base arrives pre-reduced
	// and only `wg.x * stride` happens here, with the stride clamped so that
	// cannot overflow. A contiguous cursor made convergence sweep the volume as a
	// visible band.
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
		// the frame counter, hashed. Reusing one draw for every ray
		// would sample one emitter over and over and miss the rest
		// entirely.
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

// Camera light: one invocation samples the probe field and sun visibility AT THE CAMERA
// into cam_out, for the passes with no world position to gather at — spray billboards
// and the viewmodel. It must NOT snap the camera to the C0 lattice and read one probe
// six times: `gather_prev` returns literal black for a buried probe, and against a cliff
// `relocate` finds all six solid, so spray went FULLY BLACK from some standing spots.
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
		// spray disappear. Depth-faded like every other unproven dome
		// answer (R32), so a hand deep underground agrees with the walls
		// around it.
		amb = sky_radiance(vec3f(0.0, 1.0, 0.0)) * 0.35 * medium_sky_fade(cam.y);
	}
	var sv = vec3f(0.0);
	if gi.sun.w > 0.5 {
		// From the camera, not from a snapped lattice point that may sit
		// on the far side of a wall from it.
		let h = march(cam, gi.sun.xyz, MAX_T);
		if !h.hit {
			sv = medium_water_tr(h.water) * h.thru
				* select(medium_sky_fade(cam.y), 1.0, h.sky);
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
