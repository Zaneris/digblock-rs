// RTGI resolve (GI-DESIGN.md): the half-res fullscreen pass that turns
// probes into per-pixel lighting. World position from the R32F depth
// copy, face normal from derivatives (exact on a blocky world), then:
// indirect = the shared gi_common cascade fallthrough (8 surrounding
// C0 probes, trilinear x backface x exact short-DDA occlusion through
// the voxel window — the blocky-world replacement for DDGI's
// visibility maps — then C1, then the analytic dome); direct = ONE
// sun-visibility DDA per pixel — the entire replacement for the
// shadow-map system.
//
// M4 made that sun ray STOCHASTIC and gave the pass a memory, which are
// one change and not two. The ray now takes the exact, unquantized sun
// direction jittered over a small cone, and the result accumulates
// across frames through a reprojected history.
//
// Both halves are needed together. Quantization exists so PROBES hold
// still between updates; on a per-pixel ray it is bug 1 — sun.rs pins
// the caster to dir.z == 0 and gi.rs snaps it to 1/38 steps, so every
// shadow ray is an exactly-planar, exactly-rational line through a
// cubic lattice, which aliases identically in every z-slice and
// periodically in x/y. That is the streaking, and it is pinned to the
// WORLD, not the screen. A cone jitter breaks the rational slope per
// pixel per frame; the history is what turns the resulting noise back
// into an image. Without the history, jitter is just noise; without the
// jitter, the history has nothing to average.
//
// MRT, both rgba16f: indirect rgb + view distance, and sun visibility +
// face-normal code + sun-ray water path + history length. The second
// target used to be rgba8, which cannot accumulate — at a history length
// of 512 each frame contributes 1/512 of a difference, and in 8 bits that
// rounds to zero and the average stops moving.
//
// The gather core and the group(2) world bindings come from
// gi_common.wgsl (prepended by digblock-assets); group 1 is empty.

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
	// Last frame's view-projection — the codebase's first temporal state.
	// Solids are static geometry under a moving camera, so reprojecting a
	// world position through this is EXACT and no motion-vector buffer is
	// needed.
	prev_vp: mat4x4f,
	// xyz last frame's camera, w history valid (0 on the first frame
	// after a target rebuild, a renderer rebuild, or a sun jump).
	prev_cam: vec4f,
	// xyz EXACT unquantized dir toward the sun, w above-horizon (0/1).
	sun_exact: vec4f,
	// x frame counter (the sample sequence's rotation), y cos of the sun
	// cone's half-angle, zw free.
	jitter: vec4f,
}

@group(0) @binding(0) var<uniform> rz: GiResolve;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
// The prepass's rasterized face-normal code (chunk.wgsl's `fs_depth`).
// 255 = no solid was drawn here.
@group(0) @binding(2) var normal_tex: texture_2d<u32>;
// Last frame's outputs, ping-ponged (never copied).
@group(0) @binding(3) var hist_indirect: texture_2d<f32>;
@group(0) @binding(4) var hist_sun: texture_2d<f32>;

// How many frames a still camera keeps sharpening for. alpha is
// 1/n with n climbing to here, so this is what "takes a few seconds to
// look accurate" actually means: ~5 s at 100 fps.
const N_MAX = 512.0;

// R2, the 2-D low-discrepancy sequence (Roberts 2018): the plastic
// constant's reciprocal powers. Behind one function so a spatiotemporal
// blue-noise tile can replace it — STBN is measurably better at 1 spp
// under a temporal filter, but it costs an embedded asset, so it is not
// worth paying for until hashed noise is SHOWN to be insufficient.
const R2 = vec2f(0.7548776662466927, 0.5698402909980532);

fn sample2(px: vec2i, frame_i: f32) -> vec2f {
	// Per-pixel offset (Cranley-Patterson rotation) so neighbours do not
	// share a sample, plus the per-frame advance.
	let h = fract(sin(dot(vec2f(px), vec2f(12.9898, 78.233))) * 43758.5453);
	return fract(R2 * frame_i + vec2f(h, fract(h * 1.6180339887)));
}

// A direction inside a cone of half-angle acos(cos_max) about `dir`.
fn sample_cone(dir: vec3f, u: vec2f, cos_max: f32) -> vec3f {
	let cos_t = mix(1.0, cos_max, u.x);
	let sin_t = sqrt(max(1.0 - cos_t * cos_t, 0.0));
	let phi = 6.2831853 * u.y;
	let up = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(dir.y) > 0.9);
	let tx = normalize(cross(up, dir));
	let ty = cross(dir, tx);
	return normalize(tx * (sin_t * cos(phi)) + ty * (sin_t * sin(phi)) + dir * cos_t);
}

// gi_resolve's normal-code order: axis * 2 + sign bit, indexed by the
// code chunk.wgsl's `code_of` produces. One table, two readers.
const CODE_NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
);

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
		// Sky: lit, no water, and a normal code of 255 that matches no
		// real face — so a sky texel never wins a weight in the scene's
		// bilateral upsample, and a history length of 1 so nothing
		// accumulates against it.
		o.indirect = vec4f(0.0, 0.0, 0.0, 1e9);
		o.sun = vec4f(1.0, 255.0, 0.0, 1.0);
		return o;
	}
	let clip = vec4f(i.ndc, d, 1.0);
	let wpos4 = rz.inv_view_proj * clip;
	let p = wpos4.xyz / wpos4.w;
	// EXACT face normal, rasterized by the prepass. This was
	// `normalize(cross(dpdy(p), dpdx(p)))` — a half-res derivative,
	// correct only where the 2x2 quad lies on one face and garbage on
	// every silhouette, where it poisoned the ray origin, the ambient-
	// cube axis weights and the backface term at once.
	var code = textureLoad(normal_tex, clamp(uv, vec2i(0), vec2i(dim) - 1), 0).r;
	if code > 5u {
		// No solid was rasterized at this exact texel even though depth
		// says something is here — a half-res sample landing on the far
		// side of a silhouette. Fall back to the derivative rather than
		// indexing out of the table.
		let g = normalize(cross(dpdy(p), dpdx(p)));
		let a = abs(g);
		if a.x > a.y && a.x > a.z {
			code = select(0u, 1u, g.x < 0.0);
		} else if a.y > a.z {
			code = 2u + select(0u, 1u, g.y < 0.0);
		} else {
			code = 4u + select(0u, 1u, g.z < 0.0);
		}
	}
	let n = CODE_NORMALS[code];
	// The true surface point: each march applies its own GI_SURFACE_EPS,
	// so pushing off here as well is what stacked to 1.2 blocks of
	// geometry that could not shadow anything.
	let sp = p;
	let side = i32(rz.cam.w);

	let indirect =
		gi_indirect(side, sp, n, rz.dims0, rz.dims1, rz.zenith.rgb, rz.horizon.rgb, vec2i(8, 4));
	// One sun ray along the EXACT direction, jittered over a cone about
	// twice the real sun's angular radius. That gives a physical
	// penumbra — sharp at contact, softening with distance to the
	// occluder — for free, since it is the same single ray either way.
	let u = sample2(uv, rz.jitter.x);
	var sdir = rz.sun_exact;
	sdir = vec4f(sample_cone(sdir.xyz, u, rz.jitter.y), sdir.w);
	let sv = gi_sun_vis(side, rz.cam.xz, sdir, sp, 160);
	let view_dist = length(p - rz.cam.xyz);

	// Reproject into last frame. Exact for static geometry, so this needs
	// no motion vectors — only a validity test.
	var n_hist = 0.0;
	var acc = indirect;
	var acc_sun = sv.x;
	var acc_water = sv.y;
	if rz.prev_cam.w > 0.5 {
		let pc = rz.prev_vp * vec4f(p, 1.0);
		if pc.w > 0.0 {
			let nd = pc.xy / pc.w;
			let huv = vec2f(nd.x * 0.5 + 0.5, 0.5 - nd.y * 0.5);
			if all(huv >= vec2f(0.0)) && all(huv <= vec2f(1.0)) {
				let hdim = vec2f(textureDimensions(hist_indirect));
				let ht = clamp(vec2i(huv * hdim), vec2i(0), vec2i(hdim) - 1);
				let hi = textureLoad(hist_indirect, ht, 0);
				let hs = textureLoad(hist_sun, ht, 0);
				// Validate on the exact face code AND on the distance the
				// point WOULD have been at from the old camera — comparing
				// stored view distances directly would reject the whole
				// screen the moment the camera moves.
				let want = length(p - rz.prev_cam.xyz);
				let ok = u32(hs.g) == code
					&& abs(hi.w - want) <= 0.02 * max(want, 1.0);
				if ok {
					n_hist = min(hs.a + 1.0, N_MAX);
					let a = 1.0 / n_hist;
					acc = mix(hi.rgb, indirect, a);
					acc_sun = mix(hs.r, sv.x, a);
					acc_water = mix(hs.b, sv.y, a);
				}
			}
		}
	}
	if n_hist < 1.0 {
		n_hist = 1.0;
	}
	o.indirect = vec4f(acc, view_dist);
	// r = sun visibility, g = normal code, b = the sun ray's water path
	// in blocks, a = history length. The path travels rather than a
	// transmittance so the scene shader can apply medium_common's
	// per-channel WATER_SIGMA — the same water the view ray goes through.
	o.sun = vec4f(acc_sun, f32(code), acc_water, n_hist);
	return o;
}
