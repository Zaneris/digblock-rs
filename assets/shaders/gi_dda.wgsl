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

// Advance to the LAST cell of the empty 8-cube containing `r.cell`,
// leaving the boundary crossing itself to the ordinary step.
//
// THIS is the two-level skip GI-DESIGN.md describes and that was never
// written. The coarse texture existed and was consulted, but an empty
// brick only ever saved the fine texture load — the march still crawled
// through all eight cells, and asked the coarse texture again at every
// one of them. Air is most of a voxel world, so most of the DDA's life
// was spent stepping through nothing.
//
// It advances the DDA's own state arithmetically rather than
// reconstructing a position as `origin + dir*t`. That is not fussiness:
// the position form was measurably wrong. It has to land past the exit
// plane, so where a ray leaves through a brick EDGE it jumps to the
// diagonal cell and skips the one the stepper would have entered first —
// and with the sun direction quantized to a small rational and pinned to
// dir.z == 0, edge exits are periodic rather than rare. It changed 0.83%
// of pixels against a 0.11% capture-noise floor, at shadow boundaries.
// Reconstructing from the origin also accumulates f32 error over a
// 160-block march, which the incremental form does not have.
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
