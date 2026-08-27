// Shared RTGI sampling core (GI-DESIGN.md), prepended by digblock-assets to every shader that
// gathers the probe field directly: the GI resolve, water, players and the confetti. Not all from
// the fragment stage — water gathers per vertex (R17) and the confetti prices a whole flake there
// (R34), which is why group(2) is bound VERTEX | FRAGMENT.
//
// One definition is the point: the probe-relocation nudge, the occlusion rule and the cascade
// fallthrough must agree everywhere, or lighting differs between a surface and the water beside it.
// The probe COMPUTE kernel (gi_probe.wgsl) keeps its own twins of the small helpers, since its
// bindings live in a compute-visibility group of their own, with a comment pinning them to these.
//
// Bindings sit at group(2). All cascade/window parameters arrive as plain arguments, because the
// consumers carry them in different uniforms. The group also carries the emitter list and the NEE
// estimators over it (R33) — one price (`gi_light_score`), two ways to spend a shadow ray on it —
// so the resolve and players share the declaration rather than twins.

@group(2) @binding(0) var gi_window: texture_3d<u32>;
@group(2) @binding(1) var gi_coarse: texture_3d<u32>;
@group(2) @binding(2) var<storage, read> gi_lut: array<vec4u>;

// The LUT's tinted-transmission op — mirrored from the CPU's
// `LUT_OP_TINTED` and pinned by `the_lut_ops_match_the_shaders`.
const GI_LUT_OP_TINTED = 4u;
@group(2) @binding(3) var gi_atlas0: texture_3d<f32>;
@group(2) @binding(4) var gi_atlas1: texture_3d<f32>;
// The emitter list (R20) — the same count-prefixed buffer the probe pass NEE-samples, moved into
// this shared group at R33 so every consumer of the gather core can run an estimator over it.
// lights[0].x is the count as a bitcast u32; entries are PAIRS of vec4s — (xyz world center, w
// class word), then the rectangle a surface emitter radiates from (zero for a point).
@group(2) @binding(5) var<storage, read> lights: array<vec4f>;

// How far off a surface a ray starts. It exists only to put the origin on the AIR side of a face
// the point lies exactly on — nothing more. Both marches below step BEFORE they test, so the
// origin cell is skipped regardless and the offset never needs to be large enough to leave it; a
// larger one put the occlusion probe more than a whole cell off the surface, where that much
// geometry could not shadow anything at all.
const GI_SURFACE_EPS = 0.01;

// The toroidal wrap, ONE integer modulo per axis instead of two — the frame's single hottest
// arithmetic. `((c % s) + s) % s` is the textbook positive-modulo, and on a GPU each `%` is an
// emulated sequence, so the marches below stood a stack of emulated divisions between every DDA
// step and its texture load. The second modulo only fixes a negative first result, which a compare
// and an add do for free. The lateral pair wraps together because y never wraps.
fn gi_wrap2(v: vec2i, m: i32) -> vec2i {
	let r = v % vec2i(m);
	return select(r, r + vec2i(m), r < vec2i(0));
}

// The fine cell at a PRE-WRAPPED lateral coordinate, so a march that
// already knows where it is does not pay for the wrap again.
fn gi_cell_w(w: vec2i, y: i32) -> u32 {
	if y < 0 || y > 255 {
		return 0u;
	}
	return textureLoad(gi_window, vec3i(w.x, y, w.y), 0).r;
}

// The coarse brick from the SAME pre-wrapped lateral coordinate, which is what lets one wrap serve
// both levels of the skip: the window side is a multiple of the brick size, so
// wrap(c >> s, side >> s) == wrap(c, side) >> s holds exactly and the brick address is a shift off
// the fine one. Caching the last brick index was measured and REJECTED — the extra live register
// plus the divergent branch beat the load it saves. The y guard is the FINE bound.
fn gi_coarse_w(w: vec2i, y: i32) -> u32 {
	if y < 0 || y > 255 {
		return 0u;
	}
	return textureLoad(gi_coarse, vec3i(w.x >> 3u, y >> 3u, w.y >> 3u), 0).r;
}

fn gi_coarse_empty_w(w: vec2i, y: i32) -> bool {
	return gi_coarse_w(w, y) == 0u;
}

// The brick word's summary (`core::gi::pack_coarse` packs it, `Brick` names the fields): the
// class most of its blockers wear, and the heightfield-with-a-density `gi_brick_open` reads.
fn gi_coarse_class(word: u32) -> u32 {
	return (word >> 16u) & 0xffu;
}

// The length of [t0, t1] over which a ray at height `oy + dy*t` lies in [a, b).
fn gi_span_len(oy: f32, dy: f32, t0: f32, t1: f32, a: f32, b: f32) -> f32 {
	if abs(dy) < 1e-6 {
		return select(0.0, t1 - t0, oy >= a && oy < b);
	}
	let ta = (a - oy) / dy;
	let tb = (b - oy) / dy;
	return max(min(max(ta, tb), t1) - max(min(ta, tb), t0), 0.0);
}

// The probability a ray crossing a brick over [t0, t1] comes out the other side (R41). The
// brick is three layers by height: open above its highest column top, the CORE below its lowest,
// and the band between — each a homogeneous medium at its stored density, crossed for the length
// the ray spends inside it. The layering is what keeps a ray skimming flat ground open inside
// the ground's own brick; one density over the whole brick darkened every horizon.
fn gi_brick_open(word: u32, oy: f32, dy: f32, ybase: f32, t0: f32, t1: f32) -> f32 {
	let lo = ybase + f32((word >> 24u) & 0xfu);
	let hi = ybase + f32(word >> 28u);
	let core = f32((word >> 2u) & 0x3fu) / 63.0;
	let band = f32((word >> 8u) & 0xffu) / 255.0;
	let l_core = gi_span_len(oy, dy, t0, t1, ybase, lo);
	let l_band = gi_span_len(oy, dy, t0, t1, lo, hi);
	return pow(1.0 - core, l_core) * pow(1.0 - band, l_band);
}

// The wrapping form, for the callers that hold a world cell rather than
// a march's running position (probe relocation, the occlusion DDA).
fn gi_cell(side: i32, c: vec3i) -> u32 {
	return gi_cell_w(gi_wrap2(c.xz, side), c.y);
}

fn gi_solid(side: i32, c: vec3i) -> bool {
	let v = gi_cell(side, c);
	// Partial cells count as solid here on purpose: this predicate
	// places probes and gates the short probe-visibility DDA,
	// where treating a slab as its whole cell is exactly the old — and
	// conservative — behavior. Only the long marches below resolve the
	// sub-boxes.
	return v != 0u && (gi_lut[v].x >> 24u) >= 2u;
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

// Sun visibility: march toward the sun until sky or a hit, counting the water crossed on the way.
// `sun` = quantized dir toward the sun (w = above-horizon 0/1); `iters` caps the march. Returning
// the PATH rather than a transmittance is what leaves exactly one water extinction constant in the
// engine — the caller applies medium_common's per-channel WATER_SIGMA, so sun and view rays
// through the same water agree. Cells, not exact chord lengths: that approximation is left alone.
struct GiSunVis {
	// Occluder visibility: 1 lit, 0 blocked, the depth fade on an
	// unproven miss (R32).
	vis: f32,
	// Water cells crossed, as a PATH LENGTH — the caller applies
	// medium_common's sigma so sun and view rays share one water.
	water: f32,
	// Distance to the occluder on a blocked ray (the penumbra input);
	// 0 on a lit one.
	dist: f32,
	// Glass throughput: the product of every tinted cell's rgb filter
	// (op 4) crossed on the way. 1 where no glass intervenes. A
	// FILTER product, not a medium — it has no path state, so the two
	// ray directions cannot disagree about being "inside" it.
	thru: vec3f,
}

fn gi_sun_vis(side: i32, sky_y: f32, cam_xz: vec2f, sun: vec4f, p: vec3f, iters: i32) -> GiSunVis {
	var out: GiSunVis;
	out.vis = 0.0;
	out.water = 0.0;
	out.dist = 0.0;
	out.thru = vec3f(1.0);
	if sun.w < 0.5 {
		return out;
	}
	let dir = sun.xyz;
	let o = p + dir * GI_SURFACE_EPS;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(o, dir, inv);
	// Did the march PROVE open sky (the R18 ceiling), or merely run out — of iterations, or of
	// window? An unproven exit used to answer "fully lit" too, which deep underground is daylight
	// through fifty blocks of rock; now it answers the depth fade at the shading point (R32).
	// Above the ramp's top edge the two answers are identical, so the accepted lateral-exit
	// tradeoff (GI-DESIGN.md "What is NOT built") is preserved verbatim where it was accepted.
	var proven = false;
	let lateral_max = gi_lateral_max(f32(side));
	// The hard bound tracks the window, not a literal: with the brick skip an empty brick costs
	// one iteration, so a ray crossing the whole window needs far fewer steps than blocks — but a
	// ray grazing dense geometry does not, and truncating it silently un-shadows whatever is past
	// the cut.
	for (var i = 0; i < 320; i++) {
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
		r = gi_ray_step(r, step_dir, inv);
		// The sky ceiling (R18): a non-descending ray above the highest
		// occupied cell in the whole window can never be blocked — one
		// float compare ends the climb at the tallest local hill instead
		// of at the top of the world. This is most of a screen full of
		// water's sun cost: proving open sky one coarse brick at a time
		// was a whole string of dependent texture loads per fragment.
		if r.cell.y > 255 || (dir.y >= 0.0 && f32(r.cell.y) > sky_y) {
			proven = true;
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - cam_xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		// Empty coarse brick: leave it in one jump instead of crawling its cells. (A
		// third, coarser skip mip was MEASURED OUT both orderings: checked first it taxed
		// every dense iteration of this latency-bound loop, nested it taxed every empty
		// one, and at terrain altitudes its bricks almost always hold something.) ONE
		// wrap serves both levels — see gi_coarse_empty_w.
		let w = gi_wrap2(r.cell.xz, side);
		if gi_coarse_empty_w(w, r.cell.y) {
			// Skip lands on the LAST cell inside the empty region; this
			// loop steps at the top, so `continue` is what crosses out of
			// it. Every iteration therefore advances at least one cell.
			r = gi_ray_skip_brick(r, dir, step_dir, inv);
			continue;
		}
		let v = gi_cell_w(w, r.cell.y);
		if v != 0u {
			// Word 0 only on the hot path: this loop is the frame's
			// innermost, and loading the whole LUT entry per occupied
			// cell measured a clear cost on the bench. The box words are
			// fetched only inside the partial branch.
			let op = gi_lut[v].x >> 24u;
			if op == 2u {
				out.dist = r.t;
				return out;
			}
			if op == 3u {
				// A shaped cell blocks only where its boxes actually
				// stand — this is what lets a low sun clear the slab
				// beside the one you placed, and a torch shade as a
				// post instead of a pillar.
				let t_exit = min(r.tmax.x, min(r.tmax.y, r.tmax.z));
				let bh = gi_boxes_hit(o, dir, r.cell, r.t, t_exit, gi_lut[v].zw, r.axis);
				if bh.x >= 0.0 {
					out.dist = bh.x;
					return out;
				}
			}
			if op == 1u {
				out.water += 1.0;
			}
			if op == GI_LUT_OP_TINTED {
				out.thru *= gi_glass_tr(v);
			}
		}
	}
	out.vis = select(medium_sky_fade(p.y), 1.0, proven);
	return out;
}

// A general march, for the final gather's ray — the one thing `gi_occluded`
// (a bool) and `gi_sun_vis` (a visibility) could not answer: WHAT was hit.
// Same shape as `gi_sun_vis` deliberately, including stepping before
// testing, so a shaped block never occludes its own surface here either.
struct GiHit {
	hit: bool,
	// On a hit, the cell struck; on a miss, the cell where the march
	// ENDED — which a caller needs to answer an unproven miss honestly
	// (the probe cache at the exhaust point, R32) instead of with sky.
	cell: vec3i,
	// Axis last crossed — the hit face's axis.
	axis: u32,
	// On a hit, the class struck: the cell's own, or — past `coarse_from` — the brick's modal
	// one, which is why a caller reads this rather than the window at `cell`.
	cls: u32,
	// Water cells crossed on the way, as a PATH LENGTH (see gi_sun_vis).
	water: f32,
	// Glass throughput crossed on the way — see GiSunVis.thru.
	thru: vec3f,
	// Miss quality (R32): true only when the ray PROVED open sky — the
	// R18 ceiling test, or climbing out of the world. False on a hit, and
	// false for the unproven exits (lateral bound, `max_t`, iteration
	// cap, falling out of the world's floor), which used to be answered
	// with sky radiance indistinguishably.
	sky: bool,
}

// `coarse_from` is the t past which a brick is no longer entered but crossed by its summary
// (R41, `gi_brick_open`): the ray survives with the probability the summary gives, `seed` rolls
// that, and a stop is a hit in the brick's modal class at its entry cell. The far gather passes
// the brick edge — one cosine sample of a hemisphere has a footprint the size of its travel, so
// past a brick's length the brick IS its resolution — and every shadow ray passes `max_t`,
// because a torch's occluder is a cell, not a density.
fn gi_march(
	side: i32, sky_y: f32, cam_xz: vec2f, p: vec3f, dir: vec3f,
	max_t: f32, iters: i32, coarse_from: f32, seed: u32,
) -> GiHit {
	var out: GiHit;
	out.hit = false;
	out.cell = vec3i(0);
	out.axis = 1u;
	out.cls = 0u;
	out.water = 0.0;
	out.thru = vec3f(1.0);
	out.sky = false;
	let o = p + dir * GI_SURFACE_EPS;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(o, dir, inv);
	let lateral_max = gi_lateral_max(f32(side));
	var rng = seed;
	for (var i = 0; i < 128; i++) {
		if i >= iters {
			break;
		}
		r = gi_ray_step(r, step_dir, inv);
		// Sky ceiling (R18): a non-descending ray above the window's
		// highest occupied cell is a proven miss — see gi_sun_vis.
		if r.cell.y > 255 || (dir.y >= 0.0 && f32(r.cell.y) > sky_y) {
			out.sky = true;
			break;
		}
		if r.t > max_t || r.cell.y < 0 {
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - cam_xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		let w = gi_wrap2(r.cell.xz, side);
		let cw = gi_coarse_w(w, r.cell.y);
		if cw == 0u {
			// Lands on the last cell inside the empty region; the step at
			// the top of the loop is what crosses out of it.
			r = gi_ray_skip_brick(r, dir, step_dir, inv);
			continue;
		}
		if r.t >= coarse_from {
			// The brick by its summary. Water and glass inside it go uncounted: a far bounce's
			// tint is below what one sample of a hemisphere resolves.
			let t1 = min(gi_brick_exit(r, dir, step_dir, inv), max_t);
			let ybase = f32((r.cell.y >> 3u) << 3u);
			let open = gi_brick_open(cw, o.y, dir.y, ybase, r.t, t1);
			if lcg_next(&rng) >= open {
				out.hit = true;
				out.cell = r.cell;
				out.axis = r.axis;
				out.cls = gi_coarse_class(cw);
				return out;
			}
			r = gi_ray_skip_brick(r, dir, step_dir, inv);
			continue;
		}
		let v = gi_cell_w(w, r.cell.y);
		if v != 0u {
			// Word 0 only on the hot path — see gi_sun_vis.
			let op = gi_lut[v].x >> 24u;
			if op == 2u {
				out.hit = true;
				out.cell = r.cell;
				out.axis = r.axis;
				out.cls = v;
				return out;
			}
			if op == 3u {
				// An emitting partial cell resolves its boxes like every other partial (R27).
				// It used to be a hit WHOLE so a gather ray would reliably strike a torch's
				// cell; R27 removed emission from this path (`path_radiance`), leaving the rule
				// casting a whole-cell shadow from a thin post — and with the direct term
				// resampled over every in-range emitter, clustered torches shadowed each other.
				let t_exit = min(r.tmax.x, min(r.tmax.y, r.tmax.z));
				let bh = gi_boxes_hit(o, dir, r.cell, r.t, t_exit, gi_lut[v].zw, r.axis);
				if bh.x >= 0.0 {
					out.hit = true;
					out.cell = r.cell;
					out.axis = u32(bh.y);
					out.cls = v;
					return out;
				}
			}
			if op == 1u {
				out.water += 1.0;
			}
			if op == GI_LUT_OP_TINTED {
				out.thru *= gi_glass_tr(v);
			}
		}
	}
	// Every miss path lands here; record where the march ended.
	out.cell = r.cell;
	return out;
}

// Material LUT accessors — the twins of gi_probe.wgsl's, which reads the
// same packed words out of its own binding.
fn gi_albedo(v: u32) -> vec3f {
	let m = gi_lut[v].x;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

// A tinted class's per-cell rgb filter (op 4). Word .z is free for every
// full-cube class — only the synthetic shape classes carry boxes there —
// and the CPU packs it only for op-4 rows (material_lut).
fn gi_glass_tr(v: u32) -> vec3f {
	let m = gi_lut[v].z;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

fn gi_emit(v: u32) -> f32 {
	return f32(gi_lut[v].y & 0xffu);
}

// The colour a class CASTS, which is usually the colour it is — but not always, and the exception
// is why this is a second accessor rather than a call to `gi_albedo`. An ore face is a wide grey
// field with a few bright flecks, so its mean is STONE and no weighting recovers the hue; left
// derived, an ember seam would light a tunnel white. So the ore rows STATE their cast colour and
// it rides the LUT word above the level (GI-DESIGN.md R31).
fn gi_emit_tint(v: u32) -> vec3f {
	let m = gi_lut[v].y >> 8u;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

// Sky radiance along a direction, as a GI source: the dome colors pulled
// toward their luminance, because the raw zenith blue is tuned for LOOKING
// at the sky and bouncing it at full saturation tints every shadow cold.
// The pinned twin of gi_probe.wgsl's `sky_radiance`.
fn gi_sky_radiance(dir: vec3f, zenith: vec3f, horizon: vec3f) -> vec3f {
	let c = mix(horizon, zenith, clamp(dir.y * 0.5 + 0.5, 0.0, 1.0));
	let lum = dot(c, vec3f(0.299, 0.587, 0.114));
	return mix(c, vec3f(lum), 0.35);
}

// Is the probe at texel `t` carrying real radiance?
//
// A probe whose cell is solid, with all six relocation neighbours solid too, traces nothing and
// stores BLACK, which the atlas alpha flags. A dead probe holds no information and must not vote:
// dropping it lets `wsum` fall, so the cascade falls through instead of averaging in a lie.
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
fn gi_cascade_gather(
	side: i32,
	p: vec3f,
	n: vec3f,
	cam: vec3f,
	dims4: vec4f,
	c1: bool,
	occ_cap: i32,
) -> vec4f {
	// Outside this cascade's window there is no probe for `p` — only a
	// texel congruent to it, serving somewhere else entirely. Returning
	// zero weight is what makes `gi_indirect` fall through to the next
	// cascade and then to the analytic dome, instead of blending in a
	// wrapped read from across the world at full trilinear weight. That
	// wrapped read was live for every surface past C1's coverage, which
	// is most of the visible terrain at ordinary draw distances.
	if !gi_in_cascade(p, cam, dims4) {
		return vec4f(0.0);
	}
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

// Analytic fallback beyond/starved probes: the same dome along the surface normal and dimmed — it
// stands in for an IRRADIANCE the cascades could not supply, not for radiance from one direction,
// which is why it carries the extra factor and `gi_sky_radiance` does not. Depth-faded (R32),
// because this answer is UNPROVEN: at full strength the daytime dome was the light leaking into
// every tunnel past the cascades' reach. Above the ramp's top edge the fade is exactly 1.
fn gi_sky_fallback(y: f32, n: vec3f, zenith: vec3f, horizon: vec3f) -> vec3f {
	return gi_sky_radiance(n, zenith, horizon) * 0.35 * medium_sky_fade(y);
}

// Is `p` actually inside a cascade's coverage? The atlases are toroidal,
// so `g % dims` ALWAYS yields a texel — just one serving a different
// world cell once you leave the window. Every wrapped read outside
// coverage is silently another place's light.
fn gi_in_cascade(p: vec3f, cam: vec3f, dims4: vec4f) -> bool {
	let half = vec3f(dims4.xyz) * dims4.w * 0.5;
	return all(abs(p - cam) < half - vec3f(dims4.w));
}

// The cheap cache lookup that TERMINATES a gather path: nearest probe, no relocation walk, no
// occlusion DDA. gi_probe.wgsl's `gather_prev` does the same, for the same reason: the value is
// one bounce further out, already dim, and about to be averaged over a hemisphere and hundreds of
// frames; running the full 8-probe gather here measured a large frame-rate drop. Unlike
// `gather_prev` it checks coverage first, so a hit beyond a cascade falls through.
fn gi_cache_at(
	side: i32,
	p: vec3f,
	n: vec3f,
	cam: vec3f,
	dims0: vec4f,
	dims1: vec4f,
	zenith: vec3f,
	horizon: vec3f,
) -> vec3f {
	if gi_in_cascade(p, cam, dims0) {
		let d0 = vec3i(dims0.xyz);
		let t0 = ((vec3i(round(p / dims0.w)) % d0) + d0) % d0;
		if gi_probe_alive(t0, false) {
			return gi_axis_gather(t0, d0.z, n, false);
		}
	}
	if gi_in_cascade(p, cam, dims1) {
		let d1 = vec3i(dims1.xyz);
		let t1 = ((vec3i(round(p / dims1.w)) % d1) + d1) % d1;
		if gi_probe_alive(t1, true) {
			return gi_axis_gather(t1, d1.z, n, true);
		}
	}
	return gi_sky_fallback(p.y, n, zenith, horizon);
}

// The full C0 → C1 → analytic fallthrough, exactly the resolve's rule,
// shared so water/players light like the terrain beside them. `caps` =
// per-cascade occlusion DDA caps (the resolve affords the deeper pair;
// the direct-gather consumers halve them).
fn gi_indirect(
	side: i32,
	p: vec3f,
	n: vec3f,
	cam: vec3f,
	dims0: vec4f,
	dims1: vec4f,
	zenith: vec3f,
	horizon: vec3f,
	caps: vec2i,
) -> vec3f {
	var g = gi_cascade_gather(side, p, n, cam, dims0, false, caps.x);
	if g.w < 0.05 {
		let g1 = gi_cascade_gather(side, p, n, cam, dims1, true, caps.y);
		if g1.w >= 0.05 {
			g = g1;
		}
	}
	if g.w >= 0.05 {
		return g.rgb / g.w;
	}
	return gi_sky_fallback(p.y, n, zenith, horizon);
}

// Local-light calibration, the pinned twins of gi_probe.wgsl's — the per-pixel deterministic NEE
// ray and the probes' stochastic one must price the same torch identically or the direct pool and
// its bounce drift apart (`resolve_and_probe_agree_on_light_calibration`). They live HERE so every
// fragment consumer of this prelude prices a light the same way — the resolve and players share
// these very declarations, not twins.
const LIGHT_RANGE = 24.0;
// Retuned at R26 — the reasoning and the sweep live on gi_probe's twin.
const LIGHT_GAIN = 0.75;
// Where the point model's near field stops being taken literally, in blocks. Beyond it the falloff
// is exactly 1/d^2 and nothing about the pool changes; inside, irradiance holds at 1/LIGHT_NEAR^2.
//
// A point's 1/d^2 diverges, and the support block's top face sits a fraction of a block from the
// torch's centre. A DISPLAY fact and a stopgap: the honest fix is an HDR target and one tonemap.
const LIGHT_NEAR = 2.0;
// The NEE shadow ray's iteration cap. Bounded by max_t <= LIGHT_RANGE
// anyway; this covers the worst dense diagonal with margin, because a
// tight cap silently UN-shadows whatever lies past it — the bug
// gi_sun_vis shipped once, its cap set to half what its window demanded.
const GI_NEE_ITERS = 64;

// The per-pixel direct term for local lights: ONE shadow ray, at a light RESAMPLED over the whole
// in-range list rather than at the brightest one (R27). The estimator is Talbot-style RIS in its
// exact discrete form, targeting `gi_light_score`; a temporal reservoir (ReSTIR DI) was built and
// REFUTED (R37), so do not rebuild one in front of the accumulator and a-trous. **The R20 property
// that must not regress**: this is still what lights the block a torch STANDS ON.
struct NeeLight {
	valid: bool,
	dir: vec3f,
	dist: f32,
	// The WHOLE in-range pool's unshadowed irradiance, carried by the one
	// drawn sample (f_sel / p_sel x sum(p)) — already through the falloff
	// and the cosine, so the use site only gates it on the shadow ray.
	// The surface albedo multiplies in the scene shader, like the rest of
	// indirect.
	irradiance: vec3f,
	// How far short of the drawn light the shadow ray must stop
	// (gi_light_margin).
	margin: f32,
}

// One uniform per reservoir update, which is what makes the selection correct. Reusing a single
// draw for every candidate is the tempting shortcut and it is WRONG: the tests `u < p_i / sum_i`
// become nested events on one variable, so the last candidate keeps its exact marginal while
// everything before it is squeezed — equal weights do not come out equal. An LCG advanced per
// ACCEPTED candidate costs a few ALU on the in-range lights only.
fn lcg_next(state: ptr<function, u32>) -> f32 {
	*state = *state * 1664525u + 1013904223u;
	return f32(*state >> 8u) * (1.0 / 16777216.0);
}

// The cosine a receiver with no usable normal is priced at: exactly the
// mean of |cos| over a flat, two-sided flake in a uniformly random
// orientation, which is what a tumbling confetti billboard is. Not a
// fudge and not a knob — it is what the integral comes to, and it is
// also why a flake is lit from either side: paper this thin has no back.
const GI_OMNI_COS = 0.5;

// What one candidate is worth before its shadow ray: the unshadowed irradiance up to the emitter's
// tint, zero for anything out of range or behind the surface. The ONE place a light is priced, so
// the reservoir that SELECTS on it and the `sum_lights` that PAYS it cannot disagree about what a
// torch is worth. Only the level is read per candidate; the tint is fetched per WINNER. `n` is the
// receiver's normal, and a receiver with none passes the ZERO vector (see `GI_OMNI_COS`).
fn gi_light_score(l: vec4f, q: vec4f, p: vec3f, n: vec3f) -> f32 {
	let d = l.xyz - p;
	let dist2 = dot(d, d);
	if dist2 > LIGHT_RANGE * LIGHT_RANGE || dist2 < 1e-4 {
		return 0.0;
	}
	let facing = dot(n, d) * inverseSqrt(dist2);
	let ndl = select(facing, GI_OMNI_COS, all(n == vec3f(0.0)));
	if ndl <= 0.0 {
		return 0.0;
	}
	// The entry may carry its own level (a tool whose wielder has earned
	// a brighter one); zero defers to the class, which is every placed
	// emitter.
	let ov = gi_light_level(l.w);
	let emit = select(gi_emit(gi_light_class(l.w)), f32(ov), ov != 0u);
	// Scored from the light's CENTRE — for a point source that is the point,
	// for a rectangle a cheap stand-in for the integral over it. A random
	// point here would put two more RNG steps on every candidate rather than
	// on the winner, and a RIS target may be approximate: it decides which
	// light is picked, never what the pick is worth.
	let ctr = gi_light_sample(l, q, p, 0.5, 0.5);
	return emit * ndl * ctr.w / max(dist2, LIGHT_NEAR * LIGHT_NEAR);
}

fn resample_light(p: vec3f, n: vec3f, seed: u32) -> NeeLight {
	var out: NeeLight;
	out.valid = false;
	let count = min(bitcast<u32>(lights[0].x), 256u);
	var rng = seed;
	// sum(p) over every candidate, which IS the pool's total irradiance
	// divided by the emitter tint — so it is both the reservoir's
	// normalizer and the answer.
	var wsum = 0.0;
	// Only the ENTRY is kept per update; the distance, the direction and `gi_light_margin`'s
	// sqrt are computed once after the loop rather than on every candidate that wins the
	// reservoir. The whole vec4 rather than just the position, because the margin is a
	// question about the w word too.
	var sel = vec4f(0.0);
	var sel_q = vec4f(0.0);
	for (var i = 1u; i <= count; i++) {
		// Two vec4s an entry: position and class word, then the rectangle
		// a surface emitter radiates from (zero for a point source).
		let l = lights[i * 2u - 1u];
		let q = lights[i * 2u];
		let score = gi_light_score(l, q, p, n);
		if score <= 0.0 {
			continue;
		}
		wsum += score;
		if lcg_next(&rng) * wsum < score {
			out.valid = true;
			sel = l;
			sel_q = q;
		}
	}
	if out.valid {
		// The winner gets the actual draw. A point source lands back on
		// its own position and this is the identity; a rectangle gets a
		// uniform point across its face, which is what turns a seam from a
		// hotspot into a wash and gives the shadow ray somewhere honest to
		// aim.
		let s = gi_light_sample(sel, sel_q, p, lcg_next(&rng), lcg_next(&rng));
		let d = s.p - p;
		out.dist = length(d);
		out.dir = d / out.dist;
		out.margin = gi_light_margin_of(sel);
		// f_sel / p_sel x sum(p), with the division in closed form: the
		// target IS the contribution up to the tint, so `score_sel` cancels
		// exactly and never appears. That cancellation is why this reads as
		// one deterministic multiply rather than a ratio of two sampled
		// quantities.
		//
		// For a RECTANGLE the cancellation is approximate, knowingly: the
		// target uses the centre and the contribution the drawn point, so
		// they differ by however much the light-side cosine and the distance
		// vary across the face. At the seam that is a little extra variance,
		// which the accumulator and the a-trous pass are there for. The
		// estimator stays unbiased either way.
		out.irradiance =
			gi_light_tint(sel.w, gi_emit_tint(gi_light_class(sel.w))) * LIGHT_GAIN * wsum;
	}
	return out;
}

// How many emitters `sum_lights` may ask about, and the cost knob of the
// whole estimator: every tap is one more march, paid on player pixels
// only. Enough to cover a lit room, which is what a body normally stands
// in; a torch-dense hall or a wall of ore seams runs past it and takes
// the bias below.
const GI_NEE_TAPS = 4u;

// The direct term for a consumer with NO accumulator and no a-trous behind it: players and the
// item in their hand (R34). Same candidates and pricing as `resample_light`, but it sums the
// brightest `GI_NEE_TAPS` candidates with a ray EACH — one binary visibility sample on the whole
// pool is right in front of a temporal filter and wrong in front of nothing. Deterministic, and
// selected on SCORE rather than list order; the bias it trades a dither for is the dimmest tail.
fn sum_lights(side: i32, sky_y: f32, cam_xz: vec2f, p: vec3f, n: vec3f) -> vec3f {
	let count = min(bitcast<u32>(lights[0].x), 256u);
	// The ladder: scores in descending order and the entries they came
	// from. An insertion this short beats sorting the list, and unlike a
	// running argmax it keeps the runners-up the budget can still afford.
	var best = array<f32, GI_NEE_TAPS>();
	var pick = array<u32, GI_NEE_TAPS>();
	for (var i = 1u; i <= count; i++) {
		let score = gi_light_score(lights[i * 2u - 1u], lights[i * 2u], p, n);
		if score <= 0.0 {
			continue;
		}
		for (var k = 0u; k < GI_NEE_TAPS; k++) {
			if score > best[k] {
				for (var j = GI_NEE_TAPS - 1u; j > k; j--) {
					best[j] = best[j - 1u];
					pick[j] = pick[j - 1u];
				}
				best[k] = score;
				pick[k] = i;
				break;
			}
		}
	}
	var sum = vec3f(0.0);
	for (var k = 0u; k < GI_NEE_TAPS; k++) {
		// Descending, so the first empty slot ends the list.
		if best[k] <= 0.0 {
			break;
		}
		let l = lights[pick[k] * 2u - 1u];
		let q = lights[pick[k] * 2u];
		// The ray goes to the point the score was priced from — the same
		// centre `gi_light_score` used. A rectangle's stochastic area draw
		// belongs to an estimator with a filter behind it; here it would
		// just be one more thing to dither.
		let s = gi_light_sample(l, q, p, 0.5, 0.5);
		let d = s.p - p;
		let dist = length(d);
		// Stop short of the light's own cell (gi_light_margin, shared with
		// both other NEE passes).
		let reach = max(dist - gi_light_margin_of(l), 0.0);
		let sh = gi_march(side, sky_y, cam_xz, p, d / dist, reach, GI_NEE_ITERS, reach, 0u);
		if !sh.hit {
			// Glass on the path, exactly as both other NEE passes charge
			// it — nothing filters a body (R33/R34), so without this a
			// player standing in a pane's coloured pool is lit white.
			sum += gi_light_tint(l.w, gi_emit_tint(gi_light_class(l.w)))
				* LIGHT_GAIN
				* best[k]
				* medium_water_tr(sh.water)
				* sh.thru;
		}
	}
	return sum;
}
