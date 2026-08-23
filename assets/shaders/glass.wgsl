// Glass shader — the Translucent MESH BUCKET (glass cubes, crystal, translucent shaped covers),
// drawn inside the water pass after the water draw, over the composited scene, in two halves:
//
// - `fs_tint` MULTIPLIES the backdrop by the texel's transmittance: a pane over bright sky GLOWS
//   with what it passes, which alpha blending can never say — coverage averages, a filter filters.
//   T is the LUT rule's per-texel twin (`material_lut`), so what the eye sees through a pane and
//   what the GI carries through its cell are one formula.
// - `fs_shine` ADDS the surface's own light: the covered fraction shades as a diffuse surface,
//   and `glows` rows add their ink.
//
// The halves do NOT commute across panes, so `GlassDraw::draw` walks its chunks BACK TO FRONT and
// runs a chunk's tint and shine together. The vertex words are the SOLID stream's — the decode
// below must agree with mesher::pack / solid_word1, exactly as chunk.wgsl and water.wgsl must. A
// blended surface is absent from the depth prepass, so the raw probe field is the only lighting
// the shine half can read: water's R17 split, probe gather per VERTEX, sun DDA per fragment.

// The Frame struct + group(0) binding(0) come from frame_common.wgsl. The
// pipeline shares water's layout, whose group 0 carries the scene targets
// too — declared there, unused and undeclared here.

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;

// Group 2 is the shared GI world data, declared by gi_common.wgsl.

// Java normal() table order (VertTex.glsl), which is also the face code
// order the mesher packs: W(+x), N(+z), S(-z), B(-y), T(+y), E(-x).
const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
);

// The sun DDA's step cap — water.wgsl's reasoning verbatim: a blended
// surface affords a shorter march than the resolve's, and the probes'
// indirect term carries what is beyond it.
const SUN_STEPS = 12;

// The importance gate on the sun march, in the pixel's own units — see
// water.wgsl's SUN_MIN_L for why the weight multiplies the light colour.
const SUN_MIN_L = 0.002;

// How much of the light falling on a pane scatters back off its surface — the ADDITIVE half's
// weight. Small on purpose: a pane's appearance is its transmission, and most of what does not
// pass is absorbed rather than re-emitted. Without this the shine half treated the covered
// fraction as a full diffuse wall and a stained pane in daylight bloomed additive white over its
// own tinted backdrop.
const GLASS_REFLECT = 0.22;

// The shine half's share of one FACE, under the same pairing convention the tint half's sqrt
// encodes: a closed volume shows a ray two faces, so each carries half of what the cell
// contributes. Ink needs it stated because `emissive_ink` is shared with the lit passes and
// pinned against them, and a crystal would otherwise glow twice as bright free-standing as set
// into a wall. GLASS_REFLECT is NOT scaled by this — it was tuned against the doubled term.
const INK_PER_FACE = 0.5;

// See chunk.wgsl: exact twelfths → blocks decode.
fn twelfths(v: u32) -> f32 {
	return f32(v / 12u) + f32(v % 12u) / 12.0;
}

struct VsOut {
	// @invariant on BOTH entries, and load-bearing: the tint half and the shine half rasterize
	// the same triangles from the same stream, and a one-ulp difference would leave a fragment
	// taking the multiply without the add (a dark seam) or the add without the multiply (a
	// bright fringe). It also settles the coplanar tie with the water prepass that `LessEqual`
	// depends on. Same reasoning as chunk.wgsl and water.wgsl — build the position identically.
	@invariant @builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	// Lambert against the caster, flat per face.
	@location(2) @interpolate(flat) direct: f32,
	// Probe-field indirect, gathered per vertex (header) and interpolated.
	@location(3) indirect: vec3f,
	// Stage-A emissive (the w1 flag): fullbright ink, alpha still blends.
	@location(4) @interpolate(flat) emissive: u32,
	// World position (sun DDA origin + the medium's segment endpoint).
	@location(5) world: vec3f,
}

// Location 1 is the instance-step chunk origin — see chunk.wgsl.
@vertex
fn vs(@location(0) packed: vec2u, @location(1) origin: vec4f) -> VsOut {
	let w0 = packed.x;
	let w1 = packed.y;
	let x = twelfths(w0 & 0xffu);
	let y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	let ni = (w0 >> 24u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + origin.xyz;

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = vec2f(twelfths((w1 >> 10u) & 0xfu), twelfths((w1 >> 14u) & 0xfu));
	o.layer = w1 & 0x3ffu;
	o.direct = max(dot(n, -frame.light_dir.xyz), 0.0);
	o.emissive = (w1 >> 18u) & 1u;
	o.world = world;
	// The probe gather, hoisted out of the fragment shader (R17). Ink is
	// its own light, so an emissive vertex skips the texel walk outright.
	o.indirect = vec3f(0.25);
	if o.emissive == 0u {
		o.indirect = gi_indirect(
			i32(frame.gi_misc.x),
			world,
			n,
			frame.cam_pos.xyz,
			frame.gi0,
			frame.gi1,
			frame.zenith.rgb,
			frame.horizon.rgb,
			vec2i(4, 2),
		);
	}
	return o;
}

struct VsTintOut {
	@invariant @builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
}

// The tint half's vertex entry: position and texture only — the filter
// is a material, so this pipeline never pays the probe gather.
@vertex
fn vs_tint(@location(0) packed: vec2u, @location(1) origin: vec4f) -> VsTintOut {
	let w0 = packed.x;
	let w1 = packed.y;
	let x = twelfths(w0 & 0xffu);
	let y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	// Built exactly as `vs` builds it — same expression, same order — or
	// @invariant guarantees nothing between the two pipelines.
	let world = vec3f(x, y, z) + origin.xyz;
	var o: VsTintOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = vec2f(twelfths((w1 >> 10u) & 0xfu), twelfths((w1 >> 14u) & 0xfu));
	o.layer = w1 & 0x3ffu;
	return o;
}

// What this texel lets through, per channel — the blend multiplies the backdrop by exactly this.
// The SQUARE ROOT of the texel's T: a view ray crosses a closed glass volume's faces in PAIRS
// (same-type interior faces cull), so per-pair the backdrop takes T, which is what the GI charges
// a one-cell wall. A face culled by an opaque neighbour and a wall n cells thick both fall outside
// the pair — v1 edges recorded in GI-DESIGN R35, wanting a mesher "has a partner" vertex bit.
@fragment
fn fs_tint(i: VsTintOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer);
	let t = vec3f(1.0) - tex.a * (vec3f(1.0) - tex.rgb);
	return vec4f(sqrt(t), 1.0);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer);
	// Fullbright ink through ink_common's one definition — chunk.wgsl's
	// rule, so crystal reads the same lit or blended. The branch narrows
	// to the lighting; the medium below applies to both alike.
	var col = emissive_ink(tex.rgb) * INK_PER_FACE;
	if i.emissive == 0u {
		// The sun march, gated on its own contribution exactly as
		// water.wgsl gates its (SUN_MIN_L there): a face turned from the
		// caster, or lit by nothing, pays no DDA.
		let lc = max(frame.light_color.r, max(frame.light_color.g, frame.light_color.b));
		var sv: GiSunVis;
		sv.vis = 1.0;
		sv.water = 0.0;
		sv.dist = 0.0;
		sv.thru = vec3f(1.0);
		if lc * i.direct > SUN_MIN_L {
			sv = gi_sun_vis(
				i32(frame.gi_misc.x),
				frame.gi_misc.y,
				frame.cam_pos.xz,
				frame.gi_sun,
				i.world,
				SUN_STEPS,
			);
		}
		// Strength fades the LIGHT, never the shadow — see chunk.wgsl.
		// The sun ray's water column shares the view ray's sigma, and
		// `thru` is any FURTHER glass between this pane and the sun.
		let sh = sv.vis * medium_water_tr(sv.water) * sv.thru * frame.light_dir.w;
		col = tex.rgb * (frame.light_color.rgb * i.direct * sh + i.indirect) * GLASS_REFLECT;
	}
	// The surface's own light rides the covered fraction — an open texel
	// contributes nothing here, its whole story is the tint half. The
	// medium's TRANSMITTANCE alone applies (an additive term takes no
	// in-scatter — the siphon's rule, and its submerged split): the
	// backdrop behind the pane carried the full medium already.
	let d = distance(frame.cam_pos.xyz, i.world);
	var air = d;
	var tr = vec3f(1.0);
	if frame.water_misc.x > 0.5 {
		let ray = (i.world - frame.cam_pos.xyz) / max(d, 1e-4);
		var dw = d;
		if ray.y > 1e-4 {
			dw = min(d, frame.water_misc.z / ray.y);
		}
		tr = medium_water_tr(dw);
		air = d - dw;
	}
	tr *= medium_air_tr(air);
	return vec4f(col * tex.a * tr, 1.0);
}
