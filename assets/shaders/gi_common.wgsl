// Shared RTGI sampling core (GI-DESIGN.md), prepended by
// digblock-assets to every FRAGMENT shader that gathers the probe
// field directly: the GI resolve, water, and players. One definition
// is the point — the probe-relocation nudge, the occlusion rule, and
// the cascade fallthrough must agree everywhere or lighting differs
// between a surface and the water beside it. The probe COMPUTE kernel
// (gi_probe.wgsl) keeps its own twins of the small helpers — its
// bindings live in a compute-visibility group of their own — with a
// comment pinning them to these.
//
// Bindings sit at group(2) (the water/players material group is 1 and
// their frame group is 0; the resolve leaves group 1 empty). All
// cascade/window parameters arrive as plain arguments, because the
// consumers carry them in different uniforms (the resolve's own block
// vs the shared Frame's gi fields).

@group(2) @binding(0) var gi_window: texture_3d<u32>;
@group(2) @binding(1) var gi_coarse: texture_3d<u32>;
@group(2) @binding(2) var<storage, read> gi_lut: array<vec2u>;
@group(2) @binding(3) var gi_atlas0: texture_3d<f32>;
@group(2) @binding(4) var gi_atlas1: texture_3d<f32>;

// How far off a surface a ray starts. It exists only to put the origin
// on the AIR side of a face the point lies exactly on — nothing more.
//
// Every ray used to start 0.6 blocks out, and `gi_cascade_gather` added
// its own 0.6 on top of the 0.6 its callers had already applied, so the
// occlusion probe began 1.2 blocks off the surface: more than a whole
// cell of geometry that could not shadow anything. Both marches below
// step BEFORE they test, so the origin cell is skipped regardless and
// the offset never needed to be large enough to leave it.
const GI_SURFACE_EPS = 0.01;

fn gi_cell(side: i32, c: vec3i) -> u32 {
	if c.y < 0 || c.y > 255 {
		return 0u;
	}
	let t = vec3i(((c.x % side) + side) % side, c.y, ((c.z % side) + side) % side);
	return textureLoad(gi_window, t, 0).r;
}

fn gi_coarse_empty(side: i32, c: vec3i) -> bool {
	let cs = side / 8;
	if c.y < 0 || c.y > 31 {
		return true;
	}
	let t = vec3i(((c.x % cs) + cs) % cs, c.y, ((c.z % cs) + cs) % cs);
	return textureLoad(gi_coarse, t, 0).r == 0u;
}

fn gi_solid(side: i32, c: vec3i) -> bool {
	let v = gi_cell(side, c);
	return v != 0u && (gi_lut[v].x >> 24u) == 2u;
}

// Deterministic probe relocation — the EXACT twin of gi_probe.wgsl's
// (same fixed neighbor order over the same voxel window), so every
// gather looks for each probe where the trace actually ran.
fn gi_relocate(side: i32, cell: vec3i) -> vec3i {
	if !gi_solid(side, cell) {
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
		if !gi_solid(side, cell + offs[k]) {
			return cell + offs[k];
		}
	}
	return cell;
}

// Short exact occlusion: DDA from `a` to `b`, true if any opaque cell
// strictly between them. Capped steps — probes are near.
fn gi_occluded(side: i32, a: vec3f, b: vec3f, cap: i32) -> bool {
	let d = b - a;
	let len = length(d);
	if len < 0.5 {
		return false;
	}
	let dir = d / len;
	var cell = vec3i(floor(a));
	let goal = vec3i(floor(b));
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var tmax = abs(select(floor(a) + 1.0, floor(a), dir < vec3f(0.0)) - a) * inv;
	for (var i = 0; i < 24; i++) {
		if i >= cap || all(cell == goal) {
			return false;
		}
		if tmax.x < tmax.y && tmax.x < tmax.z {
			cell.x += step_dir.x;
			tmax.x += inv.x;
		} else if tmax.y < tmax.z {
			cell.y += step_dir.y;
			tmax.y += inv.y;
		} else {
			cell.z += step_dir.z;
			tmax.z += inv.z;
		}
		if all(cell == goal) {
			return false;
		}
		if gi_solid(side, cell) {
			return true;
		}
	}
	return false;
}

// Sun visibility: march toward the sun until sky or a hit, counting the
// water crossed on the way. `sun` = quantized dir toward the sun
// (w = above-horizon 0/1); `iters` caps the march (the resolve affords
// 160, water/players ~12). Returns:
//
//   .x = 1 if the sun is reached, 0 if an opaque cell blocks it or the
//        caster is below the horizon
//   .y = water cells crossed — the submerged PATH LENGTH, not an
//        attenuation
//
// Returning the path instead of a transmittance is what leaves exactly
// one water extinction constant in the engine. This used to end in
// `exp(-water * 0.35)`: a grey per-cell fudge with no relationship to
// the view ray's water tint, and the two disagreed by a factor of ~2.3
// with nothing tying them together. The caller now applies
// medium_common's per-channel WATER_SIGMA, so both rays through the same
// water agree by construction and sunlight arrives underwater correctly
// red-starved rather than uniformly dimmed.
//
// Cells, not exact chord lengths: a shallow ray crosses a cell over a
// longer path than a steep one. That approximation predates this and is
// left alone; M4's trace carries real segment lengths.
fn gi_sun_vis(side: i32, cam_xz: vec2f, sun: vec4f, p: vec3f, iters: i32) -> vec2f {
	if sun.w < 0.5 {
		return vec2f(0.0, 0.0);
	}
	let dir = sun.xyz;
	let o = p + dir * GI_SURFACE_EPS;
	var cell = vec3i(floor(o));
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var tmax = abs(select(floor(o) + 1.0, floor(o), dir < vec3f(0.0)) - o) * inv;
	var water = 0.0;
	let lateral_max = f32(side) * 0.5 - 1.0;
	for (var i = 0; i < 160; i++) {
		if i >= iters {
			break;
		}
		// STEP FIRST, THEN TEST — this loop is where every slab, stair
		// and door top went solid black.
		//
		// It used to test `cell` at the top, and on the first iteration
		// `cell` is the shading point's OWN cell. For a full cube the
		// point sits on the face, so that cell is the air outside and the
		// test is harmless. A shaped block is a third of a cell tall, so
		// its top surface lies INSIDE its own cell, which GI classifies
		// opaque (occupancy is per cell — there are no sub-cell masks in
		// the voxel window) — the very first test returned "blocked" and
		// the surface shadowed itself. `gi_occluded` directly above has
		// always stepped first; this march simply never did.
		if tmax.x < tmax.y && tmax.x < tmax.z {
			cell.x += step_dir.x;
			tmax.x += inv.x;
		} else if tmax.y < tmax.z {
			cell.y += step_dir.y;
			tmax.y += inv.y;
		} else {
			cell.z += step_dir.z;
			tmax.z += inv.z;
		}
		if cell.y > 255 {
			break;
		}
		let lat = vec2f(cell.xz) + 0.5 - cam_xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		if !gi_coarse_empty(side, cell >> vec3u(3u)) {
			let v = gi_cell(side, cell);
			if v != 0u {
				let op = gi_lut[v].x >> 24u;
				if op == 2u {
					return vec2f(0.0, water);
				}
				if op == 1u {
					water += 1.0;
				}
			}
		}
	}
	return vec2f(1.0, water);
}

// Is the probe at texel `t` carrying real radiance?
//
// A probe whose cell is solid — and whose six relocation neighbours are
// solid too, so `gi_relocate` had nowhere to step — traces nothing and
// stores BLACK. The update kernel flags exactly that in the atlas alpha
// (0 = dead, gi_probe.wgsl's textureStore), and until now nothing read
// the flag: every gather blended those zeros in as though they were
// measured radiance, at full trilinear weight. That is where the dark
// patches around trees, ridge interiors and any dense geometry came
// from, and the bounce gather spread them further each sweep.
//
// A dead probe holds no information, so it must not vote at all — drop
// it from the blend and let `wsum` fall, which is what makes the
// cascade fall through to C1 and then to the analytic dome instead of
// averaging in a lie. Axis 0's slab sits at z offset 0, and all six
// axes of a probe are written with the same flag, so one load answers.
fn gi_probe_alive(t: vec3i, c1: bool) -> bool {
	if c1 {
		return textureLoad(gi_atlas1, t, 0).a > 0.5;
	}
	return textureLoad(gi_atlas0, t, 0).a > 0.5;
}

// 3-axis gather from one cascade's atlas at a probe texel.
fn gi_axis_gather(t: vec3i, nz: i32, n: vec3f, c1: bool) -> vec3f {
	let w = n * n;
	let ax = select(1, 0, n.x > 0.0);
	let ay = select(3, 2, n.y > 0.0);
	let az = select(5, 4, n.z > 0.0);
	var acc = vec3f(0.0);
	if c1 {
		acc += w.x * textureLoad(gi_atlas1, t + vec3i(0, 0, ax * nz), 0).rgb;
		acc += w.y * textureLoad(gi_atlas1, t + vec3i(0, 0, ay * nz), 0).rgb;
		acc += w.z * textureLoad(gi_atlas1, t + vec3i(0, 0, az * nz), 0).rgb;
	} else {
		acc += w.x * textureLoad(gi_atlas0, t + vec3i(0, 0, ax * nz), 0).rgb;
		acc += w.y * textureLoad(gi_atlas0, t + vec3i(0, 0, ay * nz), 0).rgb;
		acc += w.z * textureLoad(gi_atlas0, t + vec3i(0, 0, az * nz), 0).rgb;
	}
	return acc;
}

// 8-probe blend from one cascade; w accumulates weight, returns rgb.
// `dims4` = cascade dims xyz + spacing w; `occ_cap` bounds the
// per-probe occlusion DDA.
fn gi_cascade_gather(side: i32, p: vec3f, n: vec3f, dims4: vec4f, c1: bool, occ_cap: i32) -> vec4f {
	let dims = vec3i(dims4.xyz);
	let spacing = dims4.w;
	let base = floor(p / spacing - 0.5);
	let frac_p = p / spacing - 0.5 - base;
	var acc = vec3f(0.0);
	var wsum = 0.0;
	for (var i = 0; i < 8; i++) {
		let o = vec3i(i & 1, (i >> 1) & 1, (i >> 2) & 1);
		let g = vec3i(base) + o;
		// The probe's cell center, after the same relocation nudge the
		// trace applied (a probe buried in a wall steps into the open).
		let pw = vec3f(gi_relocate(side, g * i32(spacing))) + 0.5;
		// Trilinear x backface (probe behind the surface contributes
		// nothing) x exact occlusion.
		let tw = abs(vec3f(vec3i(1) - o) - frac_p);
		var w = tw.x * tw.y * tw.z;
		let to_probe = pw - p;
		w *= clamp(dot(normalize(to_probe + n * 0.01), n) * 0.5 + 0.5, 0.05, 1.0);
		if w < 1e-4 {
			continue;
		}
		if gi_occluded(side, p + n * GI_SURFACE_EPS, pw, occ_cap) {
			continue;
		}
		let t = ((g % dims) + dims) % dims;
		// A probe buried in solid stores black; blending it in is what
		// painted the dark patches. Skipping it costs its weight too, so
		// a pixel surrounded by dead probes falls through to C1 and then
		// to the analytic dome rather than averaging toward zero.
		if !gi_probe_alive(t, c1) {
			continue;
		}
		acc += w * gi_axis_gather(t, dims.z, n, c1);
		wsum += w;
	}
	return vec4f(acc, wsum);
}

// Analytic fallback beyond/starved probes: dim sky dome, desaturated
// the same way the probe kernel's sky_radiance is.
fn gi_sky_fallback(n: vec3f, zenith: vec3f, horizon: vec3f) -> vec3f {
	let c = mix(horizon, zenith, clamp(n.y * 0.5 + 0.5, 0.0, 1.0));
	let lum = dot(c, vec3f(0.299, 0.587, 0.114));
	return mix(c, vec3f(lum), 0.35) * 0.35;
}

// The full C0 → C1 → analytic fallthrough, exactly the resolve's rule,
// shared so water/players light like the terrain beside them. `caps` =
// per-cascade occlusion DDA caps (the resolve affords (8,4); the
// direct-gather consumers halve them).
fn gi_indirect(
	side: i32,
	p: vec3f,
	n: vec3f,
	dims0: vec4f,
	dims1: vec4f,
	zenith: vec3f,
	horizon: vec3f,
	caps: vec2i,
) -> vec3f {
	var g = gi_cascade_gather(side, p, n, dims0, false, caps.x);
	if g.w < 0.05 {
		let g1 = gi_cascade_gather(side, p, n, dims1, true, caps.y);
		if g1.w >= 0.05 {
			g = g1;
		}
	}
	if g.w >= 0.05 {
		return g.rgb / g.w;
	}
	return gi_sky_fallback(n, zenith, horizon);
}
