// Level-up confetti: a burst of colored flakes thrown out of one world
// point, whose whole motion is a pure function of (burst origin, burst
// seed, particle index, burst AGE) — the CPU only ages the burst and
// drops it when it expires, exactly the stateless arrangement spray.wgsl
// uses for waterfall mist. One instance per live burst (xyz origin +
// age; seed in the second attribute); vertex_index picks the particle
// (vi / 6) and the quad corner (vi % 6).
//
// Unlike spray, this is NOT a loop: a burst has a beginning, so every
// particle runs its arc exactly once and the whole thing fades out.

// The Frame struct + group(0) binding come from frame_common.wgsl, and
// the group(2) GI world bindings + the shared NEE from gi_common.wgsl
// (both prepended by digblock-assets). Those bindings are read in the
// VERTEX stage here, which the gather layout has allowed since water's
// per-vertex probe gather (R17).

// RTGI camera light (gi_probe.wgsl's cam_light entry) — the same
// argument spray.wgsl makes: a flake has no surface worth gathering at,
// and confetti is thrown within a few blocks of somebody's head, so the
// camera's probe ambient + traced sun is theirs.
//
// That argument covers BOUNCE and the sun, and it does not stretch to
// local lights: the camera's own distance to a torch is not the flake's,
// and a level-up is celebrated indoors as often as out. So the direct
// term is the one thing this pass computes at the flake instead of
// inheriting — the shared `sum_lights` over group(2)'s emitter list,
// exactly as players do (R34).
struct CamLight {
	ambient: vec4f,
	sun: vec4f,
}
@group(1) @binding(0) var<uniform> cam_light: CamLight;

// Cost-stub mirror (DIG_STUB; renderer.rs's `stub` module owns the names
// and the bit numbers — water.wgsl, gi_resolve.wgsl and players.wgsl are
// the others). One bit, and it must be THIS one: an attribution run that
// removes the NEE has to remove all of it, or what is left of the term
// is measured as if it were the whole.
const STUB_GINEE = 1024u;

fn stubbed(bit: u32) -> bool {
	return (u32(frame.misc.z) & bit) != 0u;
}

// Flakes per burst — keep in sync with confetti.rs PARTICLES.
const PARTICLES: u32 = 96u;
// Seconds a burst lives; the CPU drops it at the same age (confetti.rs
// LIFE), so a flake is never cut off mid-arc.
const LIFE: f32 = 2.6;
// How long the fade at the end of a burst takes.
const FADE: f32 = 0.7;
// Flake half-extents, blocks. Deliberately oblong — a square reads as a
// spark, and the tumble below is only legible on a rectangle.
const FLAKE: vec2f = vec2f(0.085, 0.13);

const CORNERS = array<vec2f, 6>(
	vec2f(-0.5, -0.5),
	vec2f(0.5, -0.5),
	vec2f(-0.5, 0.5),
	vec2f(-0.5, 0.5),
	vec2f(0.5, -0.5),
	vec2f(0.5, 0.5),
);

// Saturated party colors. Picked per flake, never blended, so a burst
// reads as confetti rather than as a colored cloud.
const PALETTE = array<vec3f, 6>(
	vec3f(1.00, 0.28, 0.32),
	vec3f(1.00, 0.78, 0.20),
	vec3f(0.36, 0.86, 0.42),
	vec3f(0.30, 0.66, 1.00),
	vec3f(0.80, 0.42, 1.00),
	vec3f(1.00, 1.00, 1.00),
);

fn hash1(n: f32) -> f32 {
	return fract(sin(n) * 43758.5453);
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) col: vec4f,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) inst: vec4f,
	@location(1) inst2: vec4f,
) -> VsOut {
	let pi = vi / 6u;
	let corner = CORNERS[vi % 6u];
	let age = inst.w;
	let seed = inst2.x + f32(pi) * 17.311;

	// Launch: a wide upward cone. The vertical spread is what makes a
	// burst read as thrown rather than as a ring — some flakes barely
	// clear the head while others go well over it.
	let az = hash1(seed) * 6.2832;
	let speed = 1.6 + 2.6 * hash1(seed + 1.0);
	let rise = 2.6 + 2.8 * hash1(seed + 2.0);
	let v0 = vec3f(cos(az), 0.0, sin(az)) * speed + vec3f(0.0, rise, 0.0);
	// Paper falls under drag, not in a vacuum: an exponential approach to
	// a slow terminal fall is the whole difference between confetti and
	// gravel. Closed form of v' = -k(v - v_term) so there is still nothing
	// to integrate on the CPU.
	let k = 1.9;
	let term = vec3f(0.0, -1.15, 0.0);
	let decay = (1.0 - exp(-k * age)) / k;
	let center = inst.xyz + term * age + (v0 - term) * decay;

	// Tumble: the flake spins about its own long axis, so it periodically
	// turns edge-on and vanishes for an instant. Cheap (one cos on the
	// width) and it is most of what sells paper.
	let spin = 7.0 + 9.0 * hash1(seed + 3.0);
	let tumble = abs(cos(spin * age + hash1(seed + 4.0) * 6.2832));
	let size = vec2f(FLAKE.x * (0.15 + 0.85 * tumble), FLAKE.y);
	// Roll in screen space so the flakes aren't all axis-aligned.
	let roll = spin * 0.35 * age + hash1(seed + 5.0) * 6.2832;
	let rot = vec2f(cos(roll), sin(roll));
	let local = vec2f(
		corner.x * size.x * rot.x - corner.y * size.y * rot.y,
		corner.x * size.x * rot.y + corner.y * size.y * rot.x,
	);

	// Camera-facing, same basis extraction as spray.wgsl.
	let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
	let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));
	let world = center + right * local.x + up * local.y;

	// One fade at the end of the burst; no distance fade — the CPU culls
	// bursts outright, and a flake is small enough to vanish on its own.
	var alpha = 1.0 - smoothstep(LIFE - FADE, LIFE, age);
	// And a short pop-in, so the first frame isn't a solid disc of paper
	// sitting inside somebody's head.
	alpha *= smoothstep(0.0, 0.06, age);

	let base = PALETTE[u32(hash1(seed + 6.0) * 6.0) % 6u];
	// Lit like spray: probe ambient + traced sun, twilight fading the
	// LIGHT and never the shadow (chunk.wgsl's rule).
	let sh = cam_light.sun.rgb * frame.light_dir.w;
	// Plus the emitter list, because bounce and sun are the two things a cave
	// has none of: a burst thrown beside a torch used to be a cloud of BLACK
	// paper over a lit floor, which is R33's bug in the billboard family.
	// Priced at the flake's CENTRE — the whole quad sits inside a fraction of
	// a block, so one answer serves its six corners — and with the ZERO
	// normal, since a tumbling flake has no normal that holds still and paper
	// this thin takes light from either side.
	//
	// The reach gate is players': past `gi_lateral_max` a shadow ray has no
	// window to march, and a burst is thrown over a REMOTE player's head too.
	var nee = vec3f(0.0);
	let lat = center.xz - frame.cam_pos.xz;
	let gmax = gi_lateral_max(frame.gi_misc.x);
	if dot(lat, lat) <= gmax * gmax && !stubbed(STUB_GINEE) {
		nee = sum_lights(
			i32(frame.gi_misc.x), frame.gi_misc.y, frame.cam_pos.xz, center, vec3f(0.0),
		);
	}
	let col = base * (cam_light.ambient.rgb + frame.light_color.rgb * 0.8 * sh + nee);
	// The participating medium, per vertex — a flake is far smaller than
	// the segment varies over.
	let ray = normalize(world - frame.cam_pos.xyz);
	let sky = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	let lit = medium_view(
		col,
		frame.cam_pos.xyz,
		world,
		sky,
		frame.zenith.rgb,
		frame.water_misc,
		frame.misc.y,
	);
	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = corner + 0.5;
	o.col = vec4f(lit, alpha);
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	// A flake is a rectangle with barely-softened edges — sharp enough to
	// read as paper rather than as the round droplets spray draws.
	let d = max(abs(i.uv.x - 0.5), abs(i.uv.y - 0.5)) * 2.0;
	return vec4f(i.col.rgb, i.col.a * smoothstep(1.0, 0.88, d));
}
