// Block highlight, shared half: the instance-box vertex stage and the
// fragment base (occlusion + face-edge outline) behind BOTH highlight
// pipelines — the idle outline (highlight.wgsl) and the hold-to-dig
// fracture (highlight_crack.wgsl). One instance = the target cell, as a
// vertex_index-generated box (the players.wgsl trick) inflated a hair past
// the block so it never z-fights the face it decorates. Alpha-blended
// darkening decal — no textures, no lighting.
//
// The box is the TARGET'S OWN bounds, not the unit cell: a shaped block
// hands over `shapes::object_bounds`, so a slab outlines at its own size
// and a door as one two-cell leaf. That is why the two scalar look
// constants below are absolute world distances rather than fractions of a
// face — a third-slab scaled the same way would wear a border a third as
// thick. Nothing here assumes the box fits inside its cell.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

// Drawn in the WATER pass, after the water surfaces, so the box on a targeted water source — or
// on a submerged block — blends over the water instead of vanishing under its depth tint. The
// live depth buffer holds water-surface depth by then, so the pipeline's depth test is Always
// and occlusion is done here by hand against the pre-water SOLID depth copy: terrain still hides
// the box, water never does. (R32Float resolve, textureLoad only — see water.wgsl.)
@group(0) @binding(1) var scene_depth: texture_2d<f32>;

const QUAD = array<vec2f, 6>(
	vec2f(0.0, 0.0),
	vec2f(1.0, 0.0),
	vec2f(0.0, 1.0),
	vec2f(0.0, 1.0),
	vec2f(1.0, 0.0),
	vec2f(1.0, 1.0),
);

const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
);
const TANGENTS = array<vec3f, 6>(
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
	vec3f(1.0, 0.0, 0.0),
);

// Clearance past the decorated surface, in blocks — deterministic across
// backends, unlike depth-bias units. (Per side, it is what the old
// scale-the-unit-cube-up-a-hair came to.)
const INFLATE = 0.002;
// Border half-width ramp, in blocks, for the outline's dark edge.
const EDGE0 = 0.015;
const EDGE1 = 0.035;

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	// x = dig progress (0 = idle outline only), y = per-cell seed,
	// z = which of the six faces this is.
	@location(1) @interpolate(flat) data: vec3f,
	// This face's size in blocks, for world-space border and crack aspect.
	@location(2) @interpolate(flat) size: vec2f,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) i0: vec4f, // cell min xyz, progress
	@location(1) i1: vec4f, // box lo (block-local), seed
	@location(2) i2: vec4f, // box hi (block-local), spare
) -> VsOut {
	let face = vi / 6u;
	let corner = vi % 6u;
	let n = NORMALS[face];
	let t = TANGENTS[face];
	let b = cross(n, t);
	let uv = QUAD[corner];
	// The target's own box, inflated by an absolute margin on every side.
	// n/t/b are axis-aligned units, so the componentwise half-extent
	// products below pick out one axis each.
	let lo = i1.xyz - INFLATE;
	let hi = i2.xyz + INFLATE;
	let center = (lo + hi) * 0.5;
	let half = (hi - lo) * 0.5;
	let local = center + half * (n + t * ((uv.x - 0.5) * 2.0) + b * ((uv.y - 0.5) * 2.0));
	let world = i0.xyz + local;
	var out: VsOut;
	out.pos = frame.view_proj * vec4f(world, 1.0);
	out.uv = uv;
	// i1.w is the seed — i1.xyz is the box. (This read was i1.x until
	// R28, i.e. the box's own min x, which is 0.0 on every full cube: the
	// per-cell seed the comment promised was a constant 0 and every block
	// in the world cracked with the same pattern.)
	out.data = vec3f(i0.w, i1.w, f32(face));
	out.size = vec2f(dot(abs(t), hi - lo), dot(abs(b), hi - lo));
	return out;
}

// What both fragment entries start from: the aspect-corrected face-local
// point, its pixel footprint, and the edge-outline alpha.
struct HlBase {
	p: vec2f,
	px: f32,
	alpha: f32,
}

// Occlude, then outline. Called FIRST in every fragment entry, because
// `px` is a derivative: it has to be taken in uniform control flow
// (Tint's rule), and everything past it is either a discard or, in the
// crack module, a loop with data-dependent breaks.
fn hl_base(in: VsOut) -> HlBase {
	// Face-local coordinates, aspect-corrected about the face's long
	// axis so the crack keeps its shape on an oblong face instead of
	// squashing into it. Exactly (1, 1) on a square face, so a full cube
	// works in plain block units.
	let p = (in.uv - vec2f(0.5)) * (in.size / max(in.size.x, in.size.y));
	// Half a pixel's footprint in face units — the crack's analytic
	// antialiasing, and the floor on its width so a hairline tip stays a
	// visible thread instead of a shimmering dotted line.
	let px = max(length(fwidth(p)) * 0.5, 1e-6);

	// Manual occlusion against the solid scene (see header). The box is
	// inflated past its block, so its own face sits strictly in front of
	// the surface it decorates — no bias needed.
	let d_scene = textureLoad(scene_depth, vec2i(in.pos.xy), 0).x;
	if in.pos.z > d_scene {
		discard;
	}
	// Thin dark border along every face edge (both modes — the outline
	// stays visible while digging). Distance to the nearest edge is
	// measured in BLOCKS, so a slab's border matches a cube's.
	let e = min(
		min(in.uv.x, 1.0 - in.uv.x) * in.size.x,
		min(in.uv.y, 1.0 - in.uv.y) * in.size.y,
	);
	return HlBase(p, px, (1.0 - smoothstep(EDGE0, EDGE1, e)) * 0.45);
}
