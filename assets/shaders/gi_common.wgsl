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
@group(2) @binding(2) var<storage, read> gi_lut: array<vec4u>;
@group(2) @binding(3) var gi_atlas0: texture_3d<f32>;
@group(2) @binding(4) var gi_atlas1: texture_3d<f32>;

// How far off a surface a ray starts. It exists only to put the origin
// on the AIR side of a face the point lies exactly on — nothing more.
//
// Every ray used to start a good fraction of a block out, and
// `gi_cascade_gather` added its own offset on top of the one its callers
// had already applied, so the occlusion probe began more than a whole
// cell off the surface: that much geometry could not shadow anything at
// all. Both marches below step BEFORE they test, so the origin cell is
// skipped regardless and the offset never needed to be large enough to
// leave it.
const GI_SURFACE_EPS = 0.01;

// The toroidal wrap, ONE integer modulo per axis instead of two.
//
// This is the frame's single hottest arithmetic, and it was hiding in
// plain sight as address math. `((c % s) + s) % s` is the textbook
// positive-modulo, and on a GPU with no integer divide unit each `%` is
// an emulated sequence, not an instruction. The marches below ran it
// twice per iteration — once for the coarse brick, once for the fine
// cell — over two wrapping axes, so a whole stack of emulated divisions
// stood between every DDA step and its texture load, in the loop that
// dominates the frame. The second modulo exists only to fix a negative
// first result, which a compare and an add do for free.
//
// Measured on the standard bench, repeated, well outside the run-to-run
// spread: a large frame-time win for this half alone, capture
// pixel-identical.
//
// The lateral pair is wrapped together because y never wraps (the window
// is world-height) and a vec2 modulo is one instruction sequence, not
// two.
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

// The coarse brick from the SAME pre-wrapped lateral coordinate, which
// is what lets one wrap serve both levels of the skip.
//
// The window side is a multiple of the brick size, so for c = q*side + w
// the identity wrap(c >> s, side >> s) == wrap(c, side) >> s holds
// exactly (both sides are floor(w / brick), and w >= 0 makes the shift
// the division). That is the whole trick: the brick address is a shift
// off the fine one, with no modulo of its own. That halves the wrap
// again — the two changes together take a large bite out of the bench's
// mean frame time and its p95 alike.
//
// Measured and REJECTED here, so it is not tried a third time: caching
// the last brick index and skipping the coarse load while the march
// stays inside it. The load it saves is a hit in a texture small enough
// to stay resident; the extra live register and the divergent branch cost
// more than that, and it measured slower.
//
// The y guard is the FINE bound now, because the caller passes the fine
// y; `y >> 3u` lands it inside the coarse texture's own vertical range.
fn gi_coarse_empty_w(w: vec2i, y: i32) -> bool {
	if y < 0 || y > 255 {
		return true;
	}
	return textureLoad(gi_coarse, vec3i(w.x >> 3u, y >> 3u, w.y >> 3u), 0).r == 0u;
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

// Sun visibility: march toward the sun until sky or a hit, counting the
// water crossed on the way. `sun` = quantized dir toward the sun
// (w = above-horizon 0/1); `iters` caps the march (the resolve affords
// the full hard bound below, water/players a far shorter one). Returns:
//
//   .x = 1 if the sun is reached, 0 if an opaque cell blocks it or the
//        caster is below the horizon
//   .y = water cells crossed — the submerged PATH LENGTH, not an
//        attenuation
//   .z = the distance (blocks, `sun` is unit) at which the blocking cell
//        was entered, 0 when nothing blocked. The resolve turns this into
//        a penumbra width — occluder distance x tan(sun radius) is the
//        physical penumbra, reconstructed spatially instead of waited for
//        temporally — and the DDA already carries it as `r.t`, so a
//        blocked ray answers it for free.
//
// Returning the path instead of a transmittance is what leaves exactly
// one water extinction constant in the engine. This used to end in a
// grey per-cell exponential: a fudge with no relationship to the view
// ray's water tint, and the two disagreed outright, with nothing tying
// them together. The caller now applies
// medium_common's per-channel WATER_SIGMA, so both rays through the same
// water agree by construction and sunlight arrives underwater correctly
// red-starved rather than uniformly dimmed.
//
// Cells, not exact chord lengths: a shallow ray crosses a cell over a
// longer path than a steep one. That approximation predates this and is
// left alone; M4's trace carries real segment lengths.
fn gi_sun_vis(side: i32, sky_y: f32, cam_xz: vec2f, sun: vec4f, p: vec3f, iters: i32) -> vec3f {
	if sun.w < 0.5 {
		return vec3f(0.0, 0.0, 0.0);
	}
	let dir = sun.xyz;
	let o = p + dir * GI_SURFACE_EPS;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(o, dir, inv);
	var water = 0.0;
	let lateral_max = gi_lateral_max(f32(side));
	// The hard bound tracks the window, not a literal: with the brick skip
	// an empty brick costs one iteration, so a ray crossing the whole
	// window needs far fewer steps than blocks — but a ray grazing dense
	// geometry does not, and truncating it silently un-shadows whatever is
	// past the cut. That bug shipped once already, with an iteration cap
	// set to half what the window could demand.
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
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - cam_xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		// Empty coarse brick: leave it in one jump instead of crawling
		// its cells and re-asking the coarse texture at each. (A third,
		// coarser skip mip was built and MEASURED OUT here, both
		// orderings: checked first it taxed every dense iteration of
		// this latency-bound loop, nested it taxed every empty one, and
		// at terrain altitudes its bricks almost always hold something —
		// the sky ceiling above is what actually ends the climb.)
		//
		// ONE wrap serves both levels — see gi_coarse_empty_w.
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
				return vec3f(0.0, water, r.t);
			}
			if op == 3u {
				// A shaped cell blocks only where its boxes actually
				// stand — this is what lets a low sun clear the slab
				// beside the one you placed, and a torch shade as a
				// post instead of a pillar.
				let t_exit = min(r.tmax.x, min(r.tmax.y, r.tmax.z));
				let bh = gi_boxes_hit(o, dir, r.cell, r.t, t_exit, gi_lut[v].zw, r.axis);
				if bh.x >= 0.0 {
					return vec3f(0.0, water, bh.x);
				}
			}
			if op == 1u {
				water += 1.0;
			}
		}
	}
	return vec3f(1.0, water, 0.0);
}

// A general march, for the final gather's ray — the one thing `gi_occluded`
// (a bool) and `gi_sun_vis` (a visibility) could not answer: WHAT was hit.
// Same shape as `gi_sun_vis` deliberately, including stepping before
// testing, so a shaped block never occludes its own surface here either.
struct GiHit {
	hit: bool,
	cell: vec3i,
	// Axis last crossed — the hit face's axis.
	axis: u32,
	// Water cells crossed on the way, as a PATH LENGTH (see gi_sun_vis).
	water: f32,
}

fn gi_march(side: i32, sky_y: f32, cam_xz: vec2f, p: vec3f, dir: vec3f, max_t: f32, iters: i32) -> GiHit {
	var out: GiHit;
	out.hit = false;
	out.cell = vec3i(0);
	out.axis = 1u;
	out.water = 0.0;
	let o = p + dir * GI_SURFACE_EPS;
	let step_dir = vec3i(sign(dir));
	let inv = 1.0 / max(abs(dir), vec3f(1e-6));
	var r = gi_ray_init(o, dir, inv);
	let lateral_max = gi_lateral_max(f32(side));
	for (var i = 0; i < 128; i++) {
		if i >= iters {
			break;
		}
		r = gi_ray_step(r, step_dir, inv);
		// Sky ceiling (R18): a non-descending ray above the window's
		// highest occupied cell is a proven miss — see gi_sun_vis.
		if r.t > max_t || r.cell.y > 255 || r.cell.y < 0
			|| (dir.y >= 0.0 && f32(r.cell.y) > sky_y) {
			break;
		}
		let lat = vec2f(r.cell.xz) + 0.5 - cam_xz;
		if dot(lat, lat) > lateral_max * lateral_max {
			break;
		}
		let w = gi_wrap2(r.cell.xz, side);
		if gi_coarse_empty_w(w, r.cell.y) {
			// Lands on the last cell inside the empty region; the step at
			// the top of the loop is what crosses out of it.
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
				return out;
			}
			if op == 3u {
				// An emitting partial cell resolves its boxes like every
				// other partial (R27). It used to be a hit WHOLE, and that
				// rule was never about occlusion — it was there so a gather
				// ray would reliably strike the torch's cell and collect
				// `alb * emit * LIGHT_GAIN`, because letting rays pass
				// through collapsed the vault's pool to a fraction of
				// itself. R27 removed the emission from this path entirely
				// (`path_radiance`), which left the rule doing only the one
				// thing its own comment said it must not do: casting a
				// whole-cell shadow from a torch that is a thin post.
				//
				// It matters most exactly where R27 aims. With the direct
				// term now resampled over EVERY in-range emitter, a
				// clustered set of torches was shadowing each other's NEE
				// rays as solid cubes.
				let t_exit = min(r.tmax.x, min(r.tmax.y, r.tmax.z));
				let bh = gi_boxes_hit(o, dir, r.cell, r.t, t_exit, gi_lut[v].zw, r.axis);
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
		}
	}
	return out;
}

// Material LUT accessors — the twins of gi_probe.wgsl's, which reads the
// same packed words out of its own binding.
fn gi_albedo(v: u32) -> vec3f {
	let m = gi_lut[v].x;
	return vec3f(f32(m & 0xffu), f32((m >> 8u) & 0xffu), f32((m >> 16u) & 0xffu)) / 255.0;
}

fn gi_emit(v: u32) -> f32 {
	return f32(gi_lut[v].y & 0xffu);
}

// The colour a class CASTS, which is usually the colour it is — but not
// always, and the exception is the reason this is a second accessor
// rather than a call to `gi_albedo`.
//
// An emitter's tint is normally the mean of its texture, and that works
// for a torch because a torch's texture is mostly flame. An ore face is a
// wide grey field with a few bright flecks on it, so its mean is STONE:
// measured, every tier comes out within a few percent of colourless, and
// no weighting of that image recovers the hue because the problem is the
// composition and not the average. Left derived, an ember seam would
// light a tunnel white. So the ore rows state their cast colour — the
// same fleck palette texgen paints them with — and it rides the LUT word
// above the level (GI-DESIGN.md R31).
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

// Analytic fallback beyond/starved probes: the same dome, taken along the
// surface normal and dimmed — it stands in for an IRRADIANCE the cascades
// could not supply, not for the radiance arriving from one direction,
// which is why it carries the extra factor and `gi_sky_radiance` does not.
fn gi_sky_fallback(n: vec3f, zenith: vec3f, horizon: vec3f) -> vec3f {
	return gi_sky_radiance(n, zenith, horizon) * 0.35;
}

// Is `p` actually inside a cascade's coverage? The atlases are toroidal,
// so `g % dims` ALWAYS yields a texel — just one serving a different
// world cell once you leave the window. Every wrapped read outside
// coverage is silently another place's light.
fn gi_in_cascade(p: vec3f, cam: vec3f, dims4: vec4f) -> bool {
	let half = vec3f(dims4.xyz) * dims4.w * 0.5;
	return all(abs(p - cam) < half - vec3f(dims4.w));
}

// The cheap cache lookup that TERMINATES a gather path: nearest probe, no
// relocation walk, no occlusion DDA. gi_probe.wgsl's own multi-bounce
// (`gather_prev`) has always done exactly this, and for the same reason —
// the value fetched is one bounce further out, already dim, and about to
// be averaged over a hemisphere of directions and hundreds of frames.
//
// The difference is not small. `gi_cascade_gather` blends 8 probes, each
// with a relocation walk and an occlusion march, which is dozens of
// texture loads; this is a handful. Running the full version at the end
// of the resolve's gather ray measured a large frame-rate drop, and
// refining a secondary bounce is not what that budget should buy.
//
// Unlike `gather_prev` it checks coverage first, so a hit beyond a
// cascade's reach falls through to the next one instead of reading a
// wrapped texel from somewhere else entirely.
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
	return gi_sky_fallback(n, zenith, horizon);
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
	return gi_sky_fallback(n, zenith, horizon);
}
