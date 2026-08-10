// RTGI resolve (GI-DESIGN.md): the half-res fullscreen pass that turns
// probes into per-pixel lighting. World position from the R32F depth
// copy, face normal from derivatives (exact on a blocky world), then:
// indirect = 8 surrounding C0 probes, each weighted by trilinear
// distance x backface x an exact short DDA occlusion through the voxel
// window (the blocky-world replacement for DDGI's visibility maps),
// falling through to C1 and then an analytic ambient; direct =
// ONE sun-visibility DDA per pixel — the entire replacement for the
// shadow-map system. MRT: indirect rgb + view depth (rgba16f), sun
// visibility + face-normal code (rgba8) for the scene's bilateral
// upsample.

struct GiResolve {
	inv_view_proj: mat4x4f,
	// xyz cam pos, w window side in blocks.
	cam: vec4f,
	// C0 dims xyz, w spacing (2).
	dims0: vec4f,
	// C1 dims xyz, w spacing (8).
	dims1: vec4f,
	// xyz quantized dir toward the sun, w sun-above-horizon (0/1).
	sun: vec4f,
	sun_color: vec4f,
	zenith: vec4f,
	horizon: vec4f,
}

@group(0) @binding(0) var<uniform> rz: GiResolve;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
@group(0) @binding(2) var window_tex: texture_3d<u32>;
@group(0) @binding(3) var coarse_tex: texture_3d<u32>;
@group(0) @binding(4) var<storage, read> lut: array<vec2u>;
@group(0) @binding(5) var atlas0: texture_3d<f32>;
@group(0) @binding(6) var atlas1: texture_3d<f32>;

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) ndc: vec2f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	var o: VsOut;
	let x = f32(i32(vi & 1u) * 4 - 1);
	let y = f32(i32(vi >> 1u) * 4 - 1);
	o.pos = vec4f(x, y, 0.0, 1.0);
	o.ndc = vec2f(x, y);
	return o;
}

fn window_cell(c: vec3i) -> u32 {
	let side = i32(rz.cam.w);
	if c.y < 0 || c.y > 255 {
		return 0u;
	}
	let t = vec3i(((c.x % side) + side) % side, c.y, ((c.z % side) + side) % side);
	return textureLoad(window_tex, t, 0).r;
}

fn coarse_empty(c: vec3i) -> bool {
	let side = i32(rz.cam.w) / 8;
	if c.y < 0 || c.y > 31 {
		return true;
	}
	let t = vec3i(((c.x % side) + side) % side, c.y, ((c.z % side) + side) % side);
	return textureLoad(coarse_tex, t, 0).r == 0u;
}

fn solid(c: vec3i) -> bool {
	let v = window_cell(c);
	return v != 0u && (lut[v].x >> 24u) == 2u;
}

// Deterministic probe relocation — the EXACT twin of gi_probe.wgsl's
// (same fixed neighbor order over the same voxel window), so the
// gather looks for each probe where the trace actually ran.
fn relocate(cell: vec3i) -> vec3i {
	if !solid(cell) {
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
		if !solid(cell + offs[k]) {
			return cell + offs[k];
		}
	}
	return cell;
}

// Short exact occlusion: DDA from `a` to `b`, true if any opaque cell
// strictly between them. Capped steps — probes are near.
fn occluded(a: vec3f, b: vec3f, cap: i32) -> bool {
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
		if solid(cell) {
			return true;
		}
	}
	return false;
}

// Sun visibility: march toward the sun until sky or a hit; water thins.
fn sun_vis(p: vec3f) -> f32 {
	if rz.sun.w < 0.5 {
		return 0.0;
	}
	let dir = rz.sun.xyz;
	var cell = vec3i(floor(p));
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var tmax = abs(select(floor(p) + 1.0, floor(p), dir < vec3f(0.0)) - p) * inv;
	var water = 0.0;
	let lateral_max = rz.cam.w * 0.5 - 1.0;
	for (var i = 0; i < 160; i++) {
		if cell.y > 255 {
			break;
		}
		let lat = vec2f(cell.xz) + 0.5 - rz.cam.xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		if !coarse_empty(cell >> vec3u(3u)) {
			let v = window_cell(cell);
			if v != 0u {
				let op = lut[v].x >> 24u;
				if op == 2u {
					return 0.0;
				}
				if op == 1u {
					water += 1.0;
				}
			}
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
	}
	return exp(-water * 0.35);
}

// 3-axis gather from one cascade's atlas at a probe texel.
fn axis_gather(t: vec3i, nz: i32, n: vec3f, c1: bool) -> vec3f {
	let w = n * n;
	let ax = select(1, 0, n.x > 0.0);
	let ay = select(3, 2, n.y > 0.0);
	let az = select(5, 4, n.z > 0.0);
	var acc = vec3f(0.0);
	if c1 {
		acc += w.x * textureLoad(atlas1, t + vec3i(0, 0, ax * nz), 0).rgb;
		acc += w.y * textureLoad(atlas1, t + vec3i(0, 0, ay * nz), 0).rgb;
		acc += w.z * textureLoad(atlas1, t + vec3i(0, 0, az * nz), 0).rgb;
	} else {
		acc += w.x * textureLoad(atlas0, t + vec3i(0, 0, ax * nz), 0).rgb;
		acc += w.y * textureLoad(atlas0, t + vec3i(0, 0, ay * nz), 0).rgb;
		acc += w.z * textureLoad(atlas0, t + vec3i(0, 0, az * nz), 0).rgb;
	}
	return acc;
}

// 8-probe blend from one cascade; w accumulates weight, returns rgb.
fn cascade_gather(p: vec3f, n: vec3f, dims4: vec4f, c1: bool, occ_cap: i32) -> vec4f {
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
		let pw = vec3f(relocate(g * i32(spacing))) + 0.5;
		// Trilinear x backface (probe behind the surface contributes
		// nothing) x exact occlusion.
		let tw = abs(vec3f(vec3i(1) - o) - frac_p);
		var w = tw.x * tw.y * tw.z;
		let to_probe = pw - p;
		w *= clamp(dot(normalize(to_probe + n * 0.01), n) * 0.5 + 0.5, 0.05, 1.0);
		if w < 1e-4 {
			continue;
		}
		if occluded(p + n * 0.6, pw, occ_cap) {
			continue;
		}
		let t = ((g % dims) + dims) % dims;
		acc += w * axis_gather(t, dims.z, n, c1);
		wsum += w;
	}
	return vec4f(acc, wsum);
}

struct FsOut {
	@location(0) indirect: vec4f,
	@location(1) sun: vec4f,
}

@fragment
fn fs(i: VsOut) -> FsOut {
	var o: FsOut;
	let dim = vec2f(textureDimensions(depth_tex));
	let uv = vec2i(vec2f(i.ndc.x * 0.5 + 0.5, 0.5 - i.ndc.y * 0.5) * dim);
	let d = textureLoad(depth_tex, clamp(uv, vec2i(0), vec2i(dim) - 1), 0).r;
	if d >= 1.0 {
		o.indirect = vec4f(0.0, 0.0, 0.0, 1e9);
		o.sun = vec4f(1.0);
		return o;
	}
	let clip = vec4f(i.ndc, d, 1.0);
	let wpos4 = rz.inv_view_proj * clip;
	let p = wpos4.xyz / wpos4.w;
	// Face normal from position derivatives — flat on block faces.
	let n = normalize(cross(dpdy(p), dpdx(p)));
	let sp = p + n * 0.6;

	var g = cascade_gather(sp, n, rz.dims0, false, 8);
	if g.w < 0.05 {
		let g1 = cascade_gather(sp, n, rz.dims1, true, 4);
		if g1.w >= 0.05 {
			g = g1;
		}
	}
	var indirect: vec3f;
	if g.w >= 0.05 {
		indirect = g.rgb / g.w;
	} else {
		// Analytic fallback beyond/starved: dim sky dome, desaturated
		// the same way the probe kernel's sky_radiance is.
		let c = mix(rz.horizon.rgb, rz.zenith.rgb, clamp(n.y * 0.5 + 0.5, 0.0, 1.0));
		let lum = dot(c, vec3f(0.299, 0.587, 0.114));
		indirect = mix(c, vec3f(lum), 0.35) * 0.35;
	}
	let sv = sun_vis(sp);
	// Normal code: axis (0/1/2) * 2 + sign bit, packed to unorm8.
	var code = 0u;
	let an = abs(n);
	if an.x > an.y && an.x > an.z {
		code = select(0u, 1u, n.x < 0.0);
	} else if an.y > an.z {
		code = 2u + select(0u, 1u, n.y < 0.0);
	} else {
		code = 4u + select(0u, 1u, n.z < 0.0);
	}
	o.indirect = vec4f(indirect, length(p - rz.cam.xyz));
	o.sun = vec4f(sv, f32(code) / 255.0, 0.0, 1.0);
	return o;
}
