// The hold-to-dig fracture — the crack half of the highlight (shared
// stage in highlight_common.wgsl; the module split is argued in
// highlight.wgsl). Bound only while a hold is live, so the branch
// frontier's dynamically-indexed arrays are never handed to the driver
// for a player who is merely aiming.

const TAU = 6.2831853;

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

// The crack: a TREE of jagged polylines, not a radial web — a spoke web reads as a pinwheel.
// Growth is by ARC LENGTH from the origin, so the break spreads as you dig and RETREATS as a
// released hold decays (`skills::dig_decayed`). Walked ONCE per pixel into FIXED slots: a
// next-free-slot counter would reshuffle the per-branch hashes mid-hold. Roots fork ONCE, wide
// and shallow. Every `per cell` knob is a RANGE off the CELL hash — six faces, one failure WAY.
const CR_BRANCH = 16u; // slots; the ceiling on roots × 2 + their forks
const CR_STEP = 4u;    // loop bound; deeper generations stop earlier
const CR_ROOT_LO = 4u; // per cell: fewest roots...
const CR_ROOT_N = 4u;  // ...and how many counts above that it may roll
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
// Raggedness. The polyline walk gives the crack its LARGE-scale wander, but a segment is a clean
// straight line between joints and on its own reads as a twig, so the sample point is warped by a
// little noise before any distance is measured — every segment frayed along its whole length for
// one fixed cost instead of one per extra joint. Amplitude stays under the line's half-width, or
// the fraying pinches the crack in two.
const CR_RAG = 16.0;   // warp cells across the face
const CR_RAG_AMP = 0.018; // warp amplitude, face units
// Head start withheld from a root, as a fraction of the full arc. Roots all leaving at once put a
// symmetrical asterisk on the block through the opening of the hold — the pinwheel this exists to
// be rid of. Staggered, the break opens as one or two cracks and the rest join in as it bites.
// Root 0 is never delayed, so the very first frame of a hold has a mark.
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
// get once the slots run out.
fn cr_child(i: u32, nroot: u32) -> u32 {
	if i < nroot * 2u {
		return min(i + nroot, CR_BRANCH);
	}
	return CR_BRANCH;
}

// One mix, two disjoint fields out — a 2D gradient costs four of these,
// not eight scalar hashes.
fn cr_h2(c: vec2i) -> vec2f {
	// The parentheses are MANDATORY, not style: Tint rejects a bare `a * b ^ c * d` while naga
	// accepts it, so an unparenthesized version compiles natively, passes the asset validation
	// test, and then invalidates this whole pipeline in Chrome — no outline, no crack, a wall of
	// uncaptured errors. `gi_resolve.wgsl`'s hash is written the same way.
	var x =(u32(c.x) * 0x9e3779b9u) ^ (u32(c.y) * 0x85ebca6bu);
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
	let base = hl_base(in);
	let p = base.p;
	let px = base.px;
	var alpha = base.alpha;
	let progress = in.data.x;
	// TWO seeds. `cseed` is the cell — it picks HOW this block fails, and
	// all six faces share it. `seed` adds the face, and places the actual
	// lines, so a corner never shows one pattern mirrored into itself.
	let cseed = u32(in.data.y);
	let seed = cseed * 6u + u32(in.data.z);

	if progress > 0.0 {
		// How THIS BLOCK fails — see the `per cell` note above the
		// constants. All six faces roll the same way.
		let nroot = CR_ROOT_LO + u32(hash11(cseed * 61u + 5u) * f32(CR_ROOT_N));
		let jag = CR_JAG_LO + CR_JAG_N * hash11(cseed * 61u + 17u);
		let fork = CR_FORK_LO + CR_FORK_N * hash11(cseed * 61u + 23u);
		let seg0 = CR_SEG_LO + CR_SEG_N * hash11(cseed * 61u + 41u);
		let wide = CR_WIDE_LO + CR_WIDE_N * hash11(cseed * 61u + 53u);
		// The arc a full hold propagates, derived from this block's own segment length rather
		// than fixed: a block that cracks in long strides has further to run, and both finish at
		// progress 1, where a shared constant would leave the coarse blocks half-drawn. `span` is
		// one root's own reach; `arc` adds room for the last root's head start, so a staggered
		// root still finishes on time.
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

		// The branch frontier. Slots 0..nroot are the roots; the rest are written by their
		// parent's walk below and stay off until growth reaches the joint they hang from —
		// `b_on` left at the zero WGSL gives an uninitialized function-scope var, so an
		// unreached slot is skipped rather than walked from garbage.
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
