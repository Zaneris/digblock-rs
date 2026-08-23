// The essence draw: motes of a vein's own colour prised off the rock and pulled
// into the tool, over a kindle on the face they leave from.
//
// One instance per live draw — a player holds one button — and every mote's
// flight is a pure function of (source, destination, progress, envelope, clock);
// the CPU owns an envelope and nothing else, spray.wgsl's and confetti.wgsl's
// arrangement, so there is nothing to simulate, save on suspend, or rebuild on
// device loss but the pipeline. vertex_index picks the particle (vi / 6) and the
// quad corner (vi % 6); particle 0 is the face kindle and the rest are motes.
//
// The one effect drawn ADDITIVELY, because essence is light. That is also why it
// does not call `medium_view`: the in-scatter half belongs to a SURFACE seen
// through air, and adding sky to an emissive term would make a far mote brighter
// than a near one. It takes the transmittance halves alone, and skips the rim
// term because a draw is always within a raycast's REACH of the eye.

// The Frame struct + group(0) binding come from frame_common.wgsl, the
// extinction from medium_common.wgsl (both prepended by digblock-assets).

// Motes per draw — keep in sync with siphon.rs MOTES, which sizes the
// draw call. `the_mote_count_matches_the_shader` holds the two equal.
const MOTES: u32 = 40u;
// Seconds a mote takes to travel the whole source-to-destination path.
// Must divide the animation clock's wrap exactly, a whole number of
// cycles per wrap, or the entire stream jumps at the seam — spray.wgsl's
// rule, and the same clock.
const FLIGHT: f32 = 0.75;
// Mote side in blocks, at MOTE_REF and beyond; nearer than that the
// angular sizing below takes over. They also shrink as they are drawn in,
// so the stream reads as converging and not merely as approaching.
const MOTE: f32 = 0.08;
// How bright a mote burns against its own vein. Above 1 on purpose: what
// is flying is the essence CONCENTRATED out of a whole face, so it has to
// out-read the flecks it came from — and the flecks are already drawn
// fullbright. There is no tonemap, so the dominant channel clips and the
// mote reads as a hot speck of the tier's hue, which is the intent.
const MOTE_GAIN: f32 = 1.9;
// Where on the face a mote lifts off, as a radius in blocks. The inner
// bound keeps the stream off the exact centre, which reads as a single
// jet rather than as a seam giving way across its whole width; the outer
// one keeps every lift-off point on the face it belongs to.
const LIFT_MIN: f32 = 0.13;
const LIFT_MAX: f32 = 0.44;
// Radians of twist a mote turns through over its flight — around a full
// turn. The spiral is most of what separates "drawn out of the rock" from
// "thrown at the camera", and it costs one sin and one cos.
const TWIST: f32 = 6.0;
// Where a mote stops being drawn, as its DISTANCE from the eye in blocks
// rather than a point in its flight. It has to stop short: the destination
// is under the player's own chin, and a mote that survives to it flies into
// the lens. Distance and not phase is what holds at any range — a near vein
// and one at the end of the reach have paths of very different lengths.
const NEAR_GONE: f32 = 0.45;
const NEAR_FULL: f32 = 1.10;
// The distance, in blocks, at which a mote is its full world size.
// Nearer than this it shrinks in proportion, which holds its ANGULAR size
// roughly fixed — a speck of light has no extent worth preserving, and
// the last stretch of a flight happens within a metre of the eye, where
// leaving it alone inflates every mote into a lamp sailing past.
const MOTE_REF: f32 = 2.5;
// The share of motes already flying at zero progress. The rest join as
// the draw builds, which is how the stream thickens over a hold without
// the vertex count moving.
const THIN: f32 = 0.34;
// The kindle on the face: its side as a fraction of the cell, and how
// hard it burns at full progress.
const KINDLE_SIZE: f32 = 0.86;
const KINDLE_GAIN: f32 = 0.95;
// How far the kindle whitens toward the end of a draw. The tint alone
// only ever restates the flecks it sits between; a seam under strain
// should read as HOTTER than its own colour, and white is the only
// direction left once the hue is already the rock's.
const KINDLE_WHITE: f32 = 0.45;
// Breath rate of the kindle, radians/s: slow enough to read as the seam
// straining and not as a strobe.
const BREATH: f32 = 7.0;
// How far off the face the kindle sits. Small enough to be a decal on
// the rock rather than a plate hovering over it, and far enough that a
// standard depth buffer at these distances never argues about it.
const KINDLE_LIFT: f32 = 0.012;

const CORNERS = array<vec2f, 6>(
	vec2f(-0.5, -0.5),
	vec2f(0.5, -0.5),
	vec2f(-0.5, 0.5),
	vec2f(-0.5, 0.5),
	vec2f(0.5, -0.5),
	vec2f(0.5, 0.5),
);

fn hash1(n: f32) -> f32 {
	return fract(sin(n) * 43758.5453);
}

// The outward normal of a face code (axis * 2 + positive), the twin of
// siphon.rs's `face_code`. Written with `select` rather than by indexing
// a vector with a computed component: dynamic component indexing is one
// of the things Chrome's WGSL front end refuses that naga accepts, and
// it costs a whole pipeline in the browser only.
fn face_normal(code: u32) -> vec3f {
	let axis = code / 2u;
	let s = select(-1.0, 1.0, (code & 1u) == 1u);
	return vec3f(
		select(0.0, s, axis == 0u),
		select(0.0, s, axis == 1u),
		select(0.0, s, axis == 2u),
	);
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	// rgb premixed (tinted, attenuated, scaled by the envelope), a = the
	// additive weight this quad carries.
	@location(1) col: vec4f,
	// Inner edge of the radial falloff: 0 is the kindle's soft bloom, and
	// the motes take a harder one so they read as specks.
	@location(2) soft: f32,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) i0: vec4f,
	@location(1) i1: vec4f,
	@location(2) i2: vec4f,
) -> VsOut {
	let pi = vi / 6u;
	let corner = CORNERS[vi % 6u];
	let t = frame.light_color.w;
	let source = i0.xyz;
	let progress = i0.w;
	let dest = i1.xyz;
	let envelope = i1.w;
	let tint = i2.rgb;

	// A tangent basis for the face. The `a` pick is what keeps the cross
	// product from degenerating on the two horizontal faces, where the
	// normal is the up vector itself.
	let n = face_normal(u32(i2.w));
	let a = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(n.y) > 0.5);
	let fu = normalize(cross(a, n));
	let fv = cross(n, fu);
	// Camera-facing basis, the same rows spray.wgsl extracts.
	let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
	let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));

	var center: vec3f;
	var bx: vec3f;
	var by: vec3f;
	var col = tint;
	var size = 0.0;
	var weight = 0.0;
	var soft = 0.0;
	if pi == 0u {
		// The kindle lies ON the face, so it billboards against the face
		// basis rather than the camera's — it is a decal on the rock, and
		// a camera-facing one would swing off the block at any angle.
		center = source + n * KINDLE_LIFT;
		bx = fu;
		by = fv;
		size = KINDLE_SIZE;
		col = mix(tint, vec3f(1.0), KINDLE_WHITE * progress);
		weight = KINDLE_GAIN * progress * (0.78 + 0.22 * sin(t * BREATH));
		soft = 0.0;
	} else {
		let seed = source.x * 12.9898 + source.z * 78.233 + f32(pi) * 37.719;
		// Motes join the stream as the draw builds: THIN of them fly from
		// the first instant and the rest fall in as progress rises. A
		// per-mote gate rather than a count, because the vertex count is
		// fixed and a stream that thickens is the point.
		let joined = hash1(seed + 8.0) <= mix(THIN, 1.0, progress);
		let phase = fract(t / FLIGHT + hash1(seed + 3.0));
		// Prised loose slowly, then taken. The square is the whole
		// difference between a mote drifting off a wall and one being
		// pulled off it.
		let along = phase * phase;
		// The funnel: a mote lifts off somewhere on the face and spirals
		// inward as it travels, so the stream converges on the tool
		// instead of running at the camera in parallel lines.
		let ang = hash1(seed) * 6.2832 + TWIST * phase;
		let rad = mix(LIFT_MIN, LIFT_MAX, hash1(seed + 1.0)) * (1.0 - along);
		center = mix(source, dest, along) + fu * (cos(ang) * rad) + fv * (sin(ang) * rad);
		let cam_d = distance(frame.cam_pos.xyz, center);
		bx = right;
		by = up;
		col = tint * MOTE_GAIN;
		size = MOTE * (1.0 - 0.4 * along) * min(cam_d / MOTE_REF, 1.0);
		// Harder-edged than the kindle, so a mote reads as a speck rather
		// than as a second bloom that happens to be moving.
		soft = 0.45;
		weight = f32(joined)
			* smoothstep(0.0, 0.09, phase)
			* smoothstep(NEAR_GONE, NEAR_FULL, cam_d)
			* (0.45 + 0.55 * progress);
	}

	// A quad carrying no weight collapses to a point instead of drawing
	// transparent: an additive pass still pays for every pixel it covers,
	// and early in a draw most of the motes are gated off (THIN).
	let live = select(0.0, 1.0, weight > 1e-4);
	let world = center + (bx * (corner.x * size) + by * (corner.y * size)) * live;

	// Transmittance only — see the header for why the in-scatter half is
	// wrong for an emissive term. The submerged split is medium_view's,
	// minus the scattering: the segment is water until the ray leaves
	// through the surface, air after.
	let d = distance(frame.cam_pos.xyz, world);
	var air = d;
	var tr = vec3f(1.0);
	if frame.water_misc.x > 0.5 {
		let ray = (world - frame.cam_pos.xyz) / max(d, 1e-4);
		var dw = d;
		if ray.y > 1e-4 {
			dw = min(d, frame.water_misc.z / ray.y);
		}
		tr = medium_water_tr(dw);
		air = d - dw;
	}
	tr *= medium_air_tr(air);

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = corner + 0.5;
	// The envelope multiplies the COLOUR and not the weight, so the flare
	// a completed draw ends on can push past white where a wider quad
	// would only have got flatter.
	o.col = vec4f(col * tr * envelope, weight);
	o.soft = soft;
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	// Procedural soft disc — no texture, spray.wgsl's shape with the
	// falloff's inner edge left to the vertex stage.
	let d = length(i.uv - 0.5) * 2.0;
	return vec4f(i.col.rgb, i.col.a * smoothstep(1.0, i.soft, d));
}
