// The voxel DDA's arithmetic — and nothing else. No bindings at all, by
// design: gi_common.wgsl's marches run in fragment shaders against the
// group(2) world data, while gi_probe.wgsl carries its own uniform group
// and cannot take that prelude. Three hand-copied steppers is how the
// system got here, and it is why the same bug had to be found three
// times. The parts that touch textures still live in each file, because
// the bindings genuinely differ; the parts that are pure maths live here
// once.
//
// `t` is the ray parameter for `origin + t*dir` with `dir` AS GIVEN — so
// with a unit dir it is a distance in blocks, and `tmax` holds absolute
// t values, not deltas.

// How far from the camera the window still holds real data, in blocks.
//
// The window is `side` blocks across and centred on the camera, so cells
// within this radius are always inside the uploaded band — a CIRCLE of
// this radius fits inside the +/-side/2 square with a cell to spare. Every
// march in the system breaks here and answers "nothing further" rather
// than reading a texel that the toroidal wrap would alias onto another
// part of the world.
//
// It lives here because all four marches need it and none of them can
// share bindings: gi_common's two (fragment, group(2) world data),
// gi_probe's (its own uniform group) and gi_debug's. Three hand-copied
// steppers is how the same bug had to be found three times; the bounds
// they march to are the same kind of shared fact.
fn gi_lateral_max(side: f32) -> f32 {
	return side * 0.5 - 1.0;
}

// Ray vs the box cover of a PARTIAL cell, over the ray's span
// [t0, t1] inside `cell`. `bx` is the LUT entry's zw pair, one word per
// box, each packing that box's min and max corner in twelfths
// (core::gi::pack_box is the packer and
// `box_pack_matches_the_shader_decode` pins the two together; an empty
// word means no box). `def_axis` answers for a ray already inside
// the box's lateral bounds when it enters the cell, where no box plane
// was crossed and the cell-entry face is the hit face. Returns
// (hit t, axis) with t < 0 for a miss.
//
// It lives here, not in gi_common.wgsl, for the same reason the
// steppers do: gi_probe.wgsl cannot take gi_common's bindings, and a
// partial cell must block a probe ray and a sun ray identically or a
// slab's shadow differs between the cache and the per-pixel answer.
fn gi_boxes_hit(o: vec3f, dir: vec3f, cell: vec3i, t0: f32, t1: f32, bx: vec2u, def_axis: u32) -> vec2f {
	var best = vec2f(-1.0, 0.0);
	// An axis with no motion cannot bound t. The epsilon's sign is
	// arbitrary: an origin inside the slab gets planes at ±huge (no
	// constraint), one outside gets both huge with one sign (empty
	// interval) — containment falls out either way, with no NaN from
	// the 0 × inf a plain 1/dir would produce. The sun ray lives at
	// dir.z == 0 permanently, so this is the common case, not a corner.
	let d = select(dir, vec3f(1e-6), abs(dir) < vec3f(1e-6));
	for (var i = 0; i < 2; i++) {
		let p = select(bx.x, bx.y, i == 1);
		if p == 0u {
			continue;
		}
		let bmin = vec3u(p, p >> 4u, p >> 8u) & vec3u(0xfu);
		let bmax = vec3u(p >> 12u, p >> 16u, p >> 20u) & vec3u(0xfu);
		let lo = vec3f(cell) + vec3f(bmin) / 12.0;
		let hi = vec3f(cell) + vec3f(bmax) / 12.0;
		let ta = (lo - o) / d;
		let tb = (hi - o) / d;
		let near = min(ta, tb);
		let far = max(ta, tb);
		let tmin = max(max(near.x, near.y), max(near.z, t0));
		let tmax = min(min(far.x, far.y), min(far.z, t1));
		if tmin <= tmax && (best.x < 0.0 || tmin < best.x) {
			var axis = def_axis;
			if tmin == near.x {
				axis = 0u;
			} else if tmin == near.y {
				axis = 1u;
			} else if tmin == near.z {
				axis = 2u;
			}
			best = vec2f(tmin, f32(axis));
		}
	}
	return best;
}

struct GiRay {
	cell: vec3i,
	// Absolute t at which the ray crosses the next boundary on each axis.
	tmax: vec3f,
	// t at which the ray entered `cell`.
	t: f32,
	// Axis last crossed (0/1/2) — the hit face's axis.
	axis: u32,
}

// Seed a DDA at `origin`. `inv` = 1/|dir| per axis, passed in because
// callers reuse it every step.
fn gi_ray_init(origin: vec3f, dir: vec3f, inv: vec3f) -> GiRay {
	var r: GiRay;
	r.cell = vec3i(floor(origin));
	r.tmax = abs(select(floor(origin) + 1.0, floor(origin), dir < vec3f(0.0)) - origin) * inv;
	r.t = 0.0;
	r.axis = 1u;
	return r;
}

// One step across the nearest cell boundary.
fn gi_ray_step(r: GiRay, step_dir: vec3i, inv: vec3f) -> GiRay {
	var o = r;
	if r.tmax.x < r.tmax.y && r.tmax.x < r.tmax.z {
		o.cell.x += step_dir.x;
		o.t = r.tmax.x;
		o.tmax.x += inv.x;
		o.axis = 0u;
	} else if r.tmax.y < r.tmax.z {
		o.cell.y += step_dir.y;
		o.t = r.tmax.y;
		o.tmax.y += inv.y;
		o.axis = 1u;
	} else {
		o.cell.z += step_dir.z;
		o.t = r.tmax.z;
		o.tmax.z += inv.z;
		o.axis = 2u;
	}
	return o;
}

// Advance to the LAST cell of the empty coarse brick containing
// `r.cell`, leaving the boundary crossing itself to the ordinary step.
//
// THIS is the two-level skip GI-DESIGN.md describes and that was never
// written. The coarse texture existed and was consulted, but an empty
// brick only ever saved the fine texture load — the march still crawled
// through every cell of it, and asked the coarse texture again at every
// one of them. Air is most of a voxel world, so most of the DDA's life
// was spent stepping through nothing.
//
// It advances the DDA's own state arithmetically rather than
// reconstructing a position as `origin + dir*t`. That is not fussiness:
// the position form was measurably wrong. It has to land past the exit
// plane, so where a ray leaves through a brick EDGE it jumps to the
// diagonal cell and skips the one the stepper would have entered first —
// and with the sun direction quantized to a small rational and pinned to
// dir.z == 0, edge exits are periodic rather than rare. It moved a
// measurable slice of the frame, well clear of the capture-noise floor,
// all of it at shadow boundaries. Reconstructing from the origin also
// accumulates f32 error over a long march, which the incremental form
// does not have.
//
// The counts below are structural. From a cell inside the brick there
// are exactly `n` boundary crossings left on each axis before that axis
// leaves the brick, so clamping each axis to `n - 1` keeps the walk
// strictly inside with no epsilon anywhere, and the exit plane is then
// crossed by `gi_ray_step` under its own tie-breaking rule — which is
// what makes this bit-identical to stepping.
fn gi_ray_skip_brick(r: GiRay, dir: vec3f, step_dir: vec3i, inv: vec3f) -> GiRay {
	let lo = (r.cell >> vec3u(3u)) << vec3u(3u);
	// Crossings remaining to this brick's face, per axis. An axis with
	// no motion never leaves, so it gets a count nothing can beat.
	let up = lo + vec3i(8) - r.cell;
	let dn = r.cell - lo + vec3i(1);
	let moving = abs(dir) > vec3f(1e-6);
	let n = select(
		vec3i(1 << 20),
		select(dn, up, step_dir > vec3i(0)),
		moving,
	);
	// t at which each axis reaches the brick face, in the same
	// accumulating units tmax already carries.
	let tface = r.tmax + vec3f(n - vec3i(1)) * inv;
	let t_exit = min(tface.x, min(tface.y, tface.z));
	let raw = floor((vec3f(t_exit) - r.tmax) / inv) + vec3f(1.0);
	let m = select(
		vec3i(0),
		clamp(vec3i(raw), vec3i(0), n - vec3i(1)),
		moving,
	);
	var o: GiRay;
	o.cell = r.cell + step_dir * m;
	o.tmax = r.tmax + vec3f(m) * inv;
	o.axis = r.axis;
	// Entry t of the cell we landed in: the latest boundary actually
	// crossed.
	let entry = o.tmax - inv;
	o.t = max(
		max(select(r.t, entry.x, m.x > 0), select(r.t, entry.y, m.y > 0)),
		select(r.t, entry.z, m.z > 0),
	);
	return o;
}

// How far short of an emitter a shadow ray must stop so that the
// emitter's own cell can never occlude it.
//
// An emitting cell is a hit WHOLE for every march but the sun ray (the
// voxel is the emission model, and LIGHT_GAIN is calibrated against the
// direct-hit term), so NEE guards against self-occlusion by stopping
// short of the light rather than by teaching the DDA a skip cell. The
// distance that guarantees it is the light's distance to the FARTHEST
// CORNER of its own cell — the half diagonal, for a light at the cell
// center, which is where the hardcoded margin this replaced came from,
// slack included.
//
// That constant was only ever correct while the origin WAS the center.
// `gi::light_origin` puts a shaped emitter's point source where its
// geometry actually glows — a torch's flame, partway up its cell — and
// from there the cell's lower corners sit further off than the old
// margin reached: rays arriving from below re-entered the emitting cell
// through them and the torch shadowed itself in cell-aligned wedges on
// the floor around its base. Deriving the margin keeps that impossible for
// whatever origin a future wall-mounted or off-center emitter has.
//
// It is deliberately the whole cell and not the shape's own boxes: the
// window classifies an emitter as a hit whole, so the boxes are not
// what a gather march would test against anyway.
fn gi_light_margin(p: vec3f) -> f32 {
	let f = p - floor(p);
	// The addend is the slack the old fixed margin carried over the bare
	// half diagonal.
	return length(max(f, vec3f(1.0) - f)) + 0.035;
}

// A light list entry's w word: the GI class byte, plus a flag set when a
// player is CARRYING it. Every reader masks, exactly as the resolve's
// history length rides its face code.
//
// The flag exists because `gi_light_margin` above is a property of the
// light's CELL, and a carried light has no cell of its own. Its bound
// stays modest only because a placed emitter sits at a KNOWN offset
// inside one — a torch's flame is the worst any of them can do. A
// carried light hangs above its carrier's feet, so a carrier standing on
// a block top lands on frac(y) == 0.0 — the degenerate maximum of
// `max(f, 1 - f)` — and the margin grows past a whole block, further
// still depending where in x/z they stand. That is wider than the
// player's own capsule, so a wall they are brushing falls entirely
// inside that skip: the floor on its far side sees the light through
// solid stone.
const GI_LIGHT_CARRIED = 256u;

// A per-ENTRY cast level, riding above the flag, or 0 for "take the
// class's own" — which is every placed emitter, so their word is what it
// always was. It exists because one light in the game answers to whoever
// is holding it (the Resonant Pick brightens at the rank that widens its
// swing), and a class byte cannot say that: two players holding the same
// tool share a LUT row and must be free to cast differently.
const GI_LIGHT_LEVEL_SHIFT = 9u;
const GI_LIGHT_LEVEL_MASK = 15u;

// The entry's second per-holder property: draw its tint pulled toward
// white. The Resonant Pick wears it at its resonant rank, where the tool
// stops being a blue signal and becomes a light to work by. It cannot be
// a class property for the same reason the level cannot — every copy of
// the tool shares one LUT row.
const GI_LIGHT_WHITENED = 8192u;
const GI_LIGHT_WHITEN = 0.35;

fn gi_light_class(w: f32) -> u32 {
	return bitcast<u32>(w) & 255u;
}

// The entry's own level, 0 when it defers to its class. Bit math only —
// the LUT read stays at the call site, because this prelude is
// dependency-free so the compute kernels can share it.
fn gi_light_level(w: f32) -> u32 {
	return (bitcast<u32>(w) >> GI_LIGHT_LEVEL_SHIFT) & GI_LIGHT_LEVEL_MASK;
}

// The tint an entry actually casts, given the one its class carries.
// Takes the albedo as an argument rather than reading the LUT, which is
// what keeps this prelude dependency-free — and is why the resolve and
// the probes cannot disagree about a whitened light.
fn gi_light_tint(w: f32, albedo: vec3f) -> vec3f {
	if (bitcast<u32>(w) & GI_LIGHT_WHITENED) != 0u {
		return mix(albedo, vec3f(1.0), GI_LIGHT_WHITEN);
	}
	return albedo;
}

// The margin for one light list entry, and the ONE place the carried
// case is decided so the two NEE passes cannot disagree about it.
//
// Zero for a carried light, and that is not slack: the origin is on the
// axis of the carrier's own capsule, inside the space that capsule
// already occupies, so its cell is non-solid by construction and there
// is nothing of its own for it to be occluded by. A wall right beside it
// SHOULD block it, which is the whole point.
fn gi_light_margin_of(l: vec4f) -> f32 {
	if (bitcast<u32>(l.w) & GI_LIGHT_CARRIED) != 0u {
		return 0.0;
	}
	return gi_light_margin(l.xyz);
}
