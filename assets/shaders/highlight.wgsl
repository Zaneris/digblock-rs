// Block highlight: an idle outline on the crosshair's target cell, plus
// a procedural fracture that spreads across it with hold-to-dig
// progress (the tree is documented at CR_BRANCH, below). One instance =
// the target cell, as a vertex_index-generated box (the players.wgsl
// trick) inflated a hair past the block so it never z-fights the face
// it decorates. Alpha-blended darkening decal — no textures, no
// lighting.
//
// Photograph it with DIG_CRACK=<percent>, which pins the progress and
// stands a wall in front of the camera to hold it; a hold is input-
// driven and lasts a second, so a sweep of those runs is the only way
// to see whether the break SPREADS or merely fades in.
//
// The box is the TARGET'S OWN bounds, not the unit cell: a shaped block
// hands over `shapes::object_bounds`, so a slab outlines at its own size
// and a door outlines as one two-cell leaf. That is why the two scalar
// look constants below (inflation and border width) are absolute world
// distances rather than fractions of a face — a third-slab scaled the
// same way would wear a border a third as thick, and a door leaf's would
// all but vanish. Both still evaluate to exactly their old values on a
// full cube.
//
// Nothing here assumes the box fits inside its cell: `local` is built
// from the box's own centre and half-extents and added to the instance's
// cell origin, so a door's 0..2 y range spans both halves for free, and
// `size` hands the fracture a 1×2 face to spread across.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets).

// Drawn in the WATER pass (after the water surfaces), so the box on a
// targeted water source — or on a submerged block — blends over the
// water instead of vanishing under its depth tint. The pass's live
// depth buffer contains water-surface depth by then, so the pipeline's
// depth test is Always and occlusion is done here by hand against the
// pre-water SOLID depth copy: terrain still hides the box, water never
// does. (R32Float resolve, textureLoad only — see water.wgsl.)
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

const TAU = 6.2831853;

// Clearance past the decorated surface, in blocks — deterministic across
// backends, unlike depth-bias units. (0.002 per side is what the old
// unit-cube-scaled-by-1.004 came to.)
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

// Small integer hash → 0..1 (lowbias32-style mixer).
fn hash11(x0: u32) -> f32 {
	var x = x0;
	x ^= x >> 16u;
	x *= 0x7feb352du;
	x ^= x >> 15u;
	x *= 0x846ca68bu;
	x ^= x >> 16u;
	return f32(x & 0xffffffu) / 16777216.0;
}

// ---------------------------------------------------------------------
// The crack: a TREE of jagged polylines, not a radial web.
//
// Six root branches leave a hash-placed origin; each root throws off one
// branch partway along, and four of those throw off one more. Sixteen
// polylines whose heading random-walks at every joint, so no part of the
// pattern is a rotation of any other part. That is the whole difference
// from the spoke web this replaced: a spoke is a ray from the centre and
// enough of them read as a pinwheel however hard you wobble them,
// whereas a line that wanders and splits reads as something giving way.
//
// Growth is by ARC LENGTH from the origin, not by radius: a point is
// drawn only once the hold has propagated past it. So the break spreads
// outward as you dig and retreats back into the origin as a released
// hold decays (`skills::dig_decayed`), which is the same motion run
// backwards rather than a fade.
//
// The tree is walked ONCE per pixel and children are written into FIXED
// slots — `nroot + i` for root i, `2·nroot + j` for j's child. A
// next-free-slot counter would reshuffle the slots, and therefore the
// per-branch hashes, and therefore the whole pattern, as growth reached
// spawn points in a different order. That pops mid-hold.
//
// Roots each fork ONCE, rather than a few of them forking twice. Both
// spend the same sixteen polylines, but a fork lands beside its parent,
// so the wide-and-shallow tree covers the face while the narrow-and-deep
// one piles into one quadrant and leaves the opposite one bare — which
// is what made the first two cuts read as a twig stuck to a block rather
// than a block coming apart.
//
// EVERY KNOB BELOW MARKED `per cell` IS A RANGE, NOT A VALUE, and is
// drawn from the cell's own hash. That is the difference between blocks
// whose cracks differ and blocks that crack DIFFERENTLY: with the shape
// of the tree fixed and only its angles jittered, sixteen lines from a
// point with one fork each is a signature, and the eye reads the
// signature, not the angles. Rolling the root count, the jag, the fork
// angle, the segment length and the weight per cell is what makes one
// block split into four long heavy pieces and its neighbour craze into a
// dense fine web.
//
// They come off the CELL hash, not the (cell, face) one that places the
// individual lines — so all six faces of a block fail the same WAY with
// different lines, which is what a single block coming apart looks like.
const CR_BRANCH = 16u; // slots; the ceiling on roots × 2 + their forks
const CR_STEP = 4u;    // loop bound; deeper generations stop earlier
const CR_ROOT_LO = 4u; // per cell: fewest roots...
const CR_ROOT_N = 4u;  // ...and how many counts above that (so 4..7)
const CR_JAG_LO = 0.32;   // per cell: heading turn per segment, radians
const CR_JAG_N = 0.46;
const CR_FORK_LO = 0.42;  // per cell: a fork's turn off its parent
const CR_FORK_N = 0.62;
const CR_SEG_LO = 0.150;  // per cell: gen-0 segment length, face units
const CR_SEG_N = 0.065;
const CR_WIDE_LO = 0.0065; // per cell: half-width at the origin
const CR_WIDE_N = 0.0060;
// Each generation's segments shorten by this fraction — proportional,
// not absolute, so a long-segmented block's fine cracks stay in
// proportion to its coarse ones.
const CR_SEG_FALL = 0.26;
// Raggedness. The polyline walk gives the crack its LARGE-scale wander;
// on its own that reads as a twig, because a segment is a clean straight
// line between joints. So the sample point is warped by a little noise
// before any distance is measured, which frays every segment along its
// whole length for one fixed cost instead of one per extra joint.
// Amplitude stays under the line's half-width, or the fraying pinches
// the crack in two.
const CR_RAG = 16.0;   // warp cells across the face
const CR_RAG_AMP = 0.018; // warp amplitude, face units
// Head start withheld from a root, as a fraction of the full arc. Roots
// all leaving at once put a symmetrical asterisk on the block for the
// first quarter of the hold — the exact pinwheel this was meant to be
// rid of. Staggered, the break opens as one or two cracks and the rest
// join in as it bites. Root 0 is never delayed, so the very first frame
// of a hold has a mark.
const CR_STAGGER = 0.16;
// Origin jitter, as a fraction of the face's own half-extent — a break
// that always started dead centre was half of why the old web read as a
// rotating stamp.
const CR_OFF = 0.55;

// Generation of branch i, under the slot map above.
fn cr_gen(i: u32, nroot: u32) -> u32 {
	if i < nroot {
		return 0u;
	}
	if i < nroot * 2u {
		return 1u;
	}
	return 2u;
}

// Slot for branch i's one child, or CR_BRANCH for "has none" — which is
// also what the last generation gets, and what a wide tree's later roots
// get once the sixteen slots run out.
fn cr_child(i: u32, nroot: u32) -> u32 {
	if i < nroot * 2u {
		return min(i + nroot, CR_BRANCH);
	}
	return CR_BRANCH;
}

// One mix, two 12-bit fields out — a 2D gradient costs four of these,
// not eight scalar hashes.
fn cr_h2(c: vec2i) -> vec2f {
	var x = (u32(c.x) * 0x9e3779b9u) ^ (u32(c.y) * 0x85ebca6bu);
	x ^= x >> 16u;
	x *= 0x7feb352du;
	x ^= x >> 15u;
	x *= 0x846ca68bu;
	x ^= x >> 16u;
	return vec2f(f32(x & 0xfffu), f32((x >> 12u) & 0xfffu)) / 4095.0 - 0.5;
}

// Smoothly interpolated value noise, ±0.5 per component. Smooth, not
// the cheaper piecewise-constant kind: a warp that jumps at cell edges
// tears the crack into disconnected pieces.
fn cr_warp(v: vec2f) -> vec2f {
	let i = floor(v);
	let f = v - i;
	let u = f * f * (3.0 - 2.0 * f);
	let c = vec2i(i);
	return mix(
		mix(cr_h2(c), cr_h2(c + vec2i(1, 0)), u.x),
		mix(cr_h2(c + vec2i(0, 1)), cr_h2(c + vec2i(1, 1)), u.x),
		u.y,
	);
}

fn cr_rot(v: vec2f, a: f32) -> vec2f {
	let c = cos(a);
	let s = sin(a);
	return vec2f(v.x * c - v.y * s, v.x * s + v.y * c);
}

// Distance from p to segment a→b.
fn cr_seg_d(p: vec2f, a: vec2f, b: vec2f) -> f32 {
	let pa = p - a;
	let ba = b - a;
	let h = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-9), 0.0, 1.0);
	return length(pa - ba * h);
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	// Face-local coordinates, aspect-corrected about the face's long
	// axis so the crack keeps its shape on an oblong face instead of
	// squashing into it. Exactly (1, 1) on a square face, so a full cube
	// works in plain block units.
	//
	// Computed FIRST, above the discard, because `px` below is a
	// derivative: it has to be taken in uniform control flow (Tint's
	// rule), and everything past this point is either a discard or a
	// loop with data-dependent breaks.
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
	let progress = in.data.x;
	// TWO seeds. `cseed` is the cell — it picks HOW this block fails, and
	// all six faces share it. `seed` adds the face, and places the actual
	// lines, so a corner never shows one pattern mirrored into itself.
	let cseed = u32(in.data.y);
	let seed = cseed * 6u + u32(in.data.z);
	// Thin dark border along every face edge (both modes — the outline
	// stays visible while digging). Distance to the nearest edge is
	// measured in BLOCKS, so a slab's border matches a cube's.
	let e = min(
		min(in.uv.x, 1.0 - in.uv.x) * in.size.x,
		min(in.uv.y, 1.0 - in.uv.y) * in.size.y,
	);
	var alpha = (1.0 - smoothstep(EDGE0, EDGE1, e)) * 0.45;

	if progress > 0.0 {
		// How THIS BLOCK fails — see the `per cell` note above the
		// constants. All six faces roll the same way.
		let nroot = CR_ROOT_LO + u32(hash11(cseed * 61u + 5u) * f32(CR_ROOT_N));
		let jag = CR_JAG_LO + CR_JAG_N * hash11(cseed * 61u + 17u);
		let fork = CR_FORK_LO + CR_FORK_N * hash11(cseed * 61u + 23u);
		let seg0 = CR_SEG_LO + CR_SEG_N * hash11(cseed * 61u + 41u);
		let wide = CR_WIDE_LO + CR_WIDE_N * hash11(cseed * 61u + 53u);
		// The arc a full hold propagates, derived from this block's own
		// segment length rather than fixed. A block that cracks in long
		// strides has further to run, and both finish at progress 1 —
		// a shared constant would leave the coarse blocks half-drawn.
		// `span` is one root's own reach; `arc` adds room for the last
		// root's head start, so a staggered root still finishes on time.
		let span = f32(CR_STEP) * seg0 * 1.05;
		let arc = span * (1.0 + CR_STAGGER);

		// Where the break started: off-centre, stable per face.
		let he = vec2f(0.5) * in.size / max(in.size.x, in.size.y);
		let org = vec2f(
			(hash11(seed * 3u + 11u) - 0.5) * CR_OFF * he.x * 2.0,
			(hash11(seed * 3u + 29u) - 0.5) * CR_OFF * he.y * 2.0,
		);
		let grow = progress * arc;
		// Fray the sample point, not the walk: growth is still measured
		// in clean arc length, so the frontier advances smoothly.
		let pw = p + cr_warp(p * CR_RAG + f32(seed & 63u)) * CR_RAG_AMP;

		// The branch frontier. Slots 0..nroot are the roots; the rest
		// are written by their parent's walk, below, and stay off until
		// growth actually reaches the joint they hang from — which is
		// `b_on` staying at the zero WGSL gives an uninitialized
		// function-scope var, so an unreached slot is skipped rather
		// than walked from garbage.
		var b_pos: array<vec2f, CR_BRANCH>;
		var b_dir: array<vec2f, CR_BRANCH>;
		var b_t0: array<f32, CR_BRANCH>;
		var b_on: array<f32, CR_BRANCH>;
		let spin = hash11(seed * 7u + 3u) * TAU;
		for (var i = 0u; i < nroot; i++) {
			let a = spin + (f32(i) + 0.6 * hash11(seed * 13u + i)) * TAU / f32(nroot);
			b_pos[i] = org;
			b_dir[i] = vec2f(cos(a), sin(a));
			b_t0[i] = select(hash11(seed * 29u + i) * CR_STAGGER * span, 0.0, i == 0u);
			b_on[i] = 1.0;
		}

		var crack = 0.0;
		for (var i = 0u; i < CR_BRANCH; i++) {
			if b_on[i] == 0.0 {
				continue;
			}
			let gen = cr_gen(i, nroot);
			let steps = CR_STEP - gen;
			let seg = seg0 * (1.0 - CR_SEG_FALL * f32(gen));
			var pos = b_pos[i];
			var dir = b_dir[i];
			var t = b_t0[i];
			for (var s = 0u; s < CR_STEP; s++) {
				if s >= steps || t >= grow {
					break;
				}
				let salt = seed * 8191u + i * 61u + s;
				dir = cr_rot(dir, (hash11(salt) - 0.5) * 2.0 * jag);
				let len = seg * (0.65 + 0.7 * hash11(salt + 4099u));
				let nxt = pos + dir * len;
				// Clip the segment to the grown arc, so the tip advances
				// smoothly instead of a whole segment blinking on.
				let tip = pos + (nxt - pos) * clamp((grow - t) / len, 0.0, 1.0);
				let d = cr_seg_d(pw, pos, tip);
				// A fracture is widest where it started and tapers to
				// nothing at the tip; deeper generations are finer.
				let taper = 1.0 - 0.20 * clamp(t / arc, 0.0, 1.0);
				let w = wide * taper / (1.0 + 0.55 * f32(gen));
				let hw = max(w * (0.5 + 0.5 * progress), px);
				crack = max(crack, 1.0 - smoothstep(max(hw - px, 0.0), hw + px, d));
				// Fork, once growth has actually reached this joint. The
				// child's `t0` is the joint's arc, so a joint beyond the
				// frontier spawns a branch that draws nothing yet.
				let c = cr_child(i, nroot);
				// Never off the last segment: a fork at the very tip is
				// just a bent line.
				let sp = min(u32(hash11(salt + 271u) * 2.0), steps - 2u);
				if c < CR_BRANCH && s == sp {
					let side = select(-1.0, 1.0, hash11(salt + 15013u) < 0.5);
					b_pos[c] = nxt;
					b_dir[c] = cr_rot(dir, side * (fork + 0.5 * hash11(salt + 7919u)));
					b_t0[c] = t + len;
					b_on[c] = 1.0;
				}
				pos = nxt;
				t += len;
			}
		}
		alpha = max(alpha, crack * (0.45 + 0.35 * progress));
	}
	return vec4f(0.0, 0.0, 0.0, alpha);
}
