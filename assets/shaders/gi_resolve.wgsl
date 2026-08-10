// RTGI resolve (GI-DESIGN.md): the half-res fullscreen pass that turns
// probes into per-pixel lighting. World position from the R32F depth
// copy, face normal from derivatives (exact on a blocky world), then:
// indirect = the shared gi_common cascade fallthrough (8 surrounding
// C0 probes, trilinear x backface x exact short-DDA occlusion through
// the voxel window — the blocky-world replacement for DDGI's
// visibility maps — then C1, then the analytic dome); direct = ONE
// sun-visibility DDA per pixel — the entire replacement for the
// shadow-map system. MRT: indirect rgb + view depth (rgba16f), sun
// visibility + face-normal code (rgba8) for the scene's bilateral
// upsample.
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
}

@group(0) @binding(0) var<uniform> rz: GiResolve;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
// The prepass's rasterized face-normal code (chunk.wgsl's `fs_depth`).
// 255 = no solid was drawn here.
@group(0) @binding(2) var normal_tex: texture_2d<u32>;

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
		// bilateral upsample. (`vec4f(1.0)` here used to claim a full
		// WATER_DEPTH_SCALE of water in the blue lane.)
		o.indirect = vec4f(0.0, 0.0, 0.0, 1e9);
		o.sun = vec4f(1.0, 1.0, 0.0, 1.0);
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
	let sv = gi_sun_vis(side, rz.cam.xz, rz.sun, sp, 160);
	o.indirect = vec4f(indirect, length(p - rz.cam.xyz));
	// r = sun reached, g = normal code, b = the sun ray's water path
	// length. The path travels rather than a transmittance so the scene
	// shader can apply medium_common's per-channel WATER_SIGMA — the same
	// water the view ray goes through.
	o.sun = vec4f(sv.x, f32(code) / 255.0, min(sv.y / WATER_DEPTH_SCALE, 1.0), 1.0);
	return o;
}
