// Teleport light: the channel aura wound around a caster, the departure
// collapse, and the arrival bloom. Deliberately NOT confetti — nothing here
// falls or flutters; everything is made of light and moves along a helix.
//
// One instance per effect. Three modes share one parametrization: a mote
// orbits the caster's axis at a radius and height driven by its phase, and
// the mode decides which way the phase drives them — departure tightens the
// helix and lifts it into a column, arrival is the same flight reversed
// (a ring blooming outward as the light settles down into the player), and
// the channel winds in place at a rate the cast's progress sets. The CPU
// owns an age/progress and an envelope; every mote's position is a pure
// function of the instance and the shared animation clock — spray.wgsl's
// arrangement, siphon.wgsl's additive light.
//
// vertex_index picks the particle (vi / 6) and the quad corner (vi % 6);
// particle 0 is the column of light, the rest are motes.

// Frame struct + group(0) come from frame_common.wgsl; the transmittance
// halves from medium_common.wgsl (both prepended by digblock-assets).

// Motes per effect — keep in sync with teleport.rs MOTES, which sizes the
// draw call (`the_teleport_mote_count_matches_the_shader`).
const MOTES: u32 = 48u;
// Seconds a burst (departure or arrival) plays for. The weight curves
// below reach zero exactly here, so dropping the instance at this age can
// never cut a visible mote — confetti's LIFE rule.
const LIFE: f32 = 1.15;
// The helix: where the orbit starts (departure) or ends (arrival), in
// blocks off the caster's axis, and how tall the flight is.
const RADIUS: f32 = 0.85;
const HEIGHT: f32 = 2.5;
// Radians of orbit a mote turns through over a burst.
const TWIST: f32 = 7.5;
// The channel's standing helix: its radius, its height, and the seconds
// one mote takes to climb it.
const CH_RADIUS: f32 = 0.55;
const CH_HEIGHT: f32 = 1.9;
const CH_RISE: f32 = 1.6;
// Mote side in blocks, and its brightness against the tint. Above 1 for
// the same reason siphon's motes are: this is concentrated light with no
// tonemap, and the dominant channel is allowed to clip.
const MOTE: f32 = 0.09;
const MOTE_GAIN: f32 = 1.8;
// The column of light: width, height and brightness.
const COLUMN_W: f32 = 0.55;
const COLUMN_H: f32 = 2.7;
const COLUMN_GAIN: f32 = 0.85;
// How far a mote whitens as its flight completes — a jump ENDS hot.
const WHITE: f32 = 0.55;

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

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	// rgb premixed (tinted, attenuated, enveloped), a = additive weight.
	@location(1) col: vec4f,
	// Inner edge of the radial falloff (0 = the column's soft bloom).
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
	let clock = frame.light_color.w;
	let origin = i0.xyz; // the caster's feet
	let mode = u32(i0.w); // 0 departure, 1 arrival, 2 channel
	let t = i1.x; // burst age (secs) or channel progress (0..1)
	let envelope = i1.y;
	let seed0 = i1.z;
	let tint = i2.rgb;

	// Camera basis, and its horizontal half for the cylindrical column.
	let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
	let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));
	let right_h = normalize(vec3f(right.x, 0.0, right.z));

	var center: vec3f;
	var bx: vec3f;
	var by: vec3f;
	var col = tint;
	var size_x = 0.0;
	var size_y = 0.0;
	var weight = 0.0;
	var soft = 0.0;
	let ph = clamp(t / LIFE, 0.0, 1.0);
	if pi == 0u {
		// The column: a cylindrical billboard standing on the feet. Its
		// life differs per mode — a departure swells into it and dies on
		// it, an arrival opens on it, a channel earns it near completion.
		center = origin + vec3f(0.0, COLUMN_H * 0.5, 0.0);
		bx = right_h;
		by = vec3f(0.0, 1.0, 0.0);
		size_x = COLUMN_W;
		size_y = COLUMN_H;
		var strength = 0.0;
		if mode == 0u {
			strength = smoothstep(0.3, 0.7, ph) * (1.0 - smoothstep(0.7, 1.0, ph));
		} else if mode == 1u {
			strength = 1.0 - smoothstep(0.0, 0.55, ph);
		} else {
			strength = 0.8 * smoothstep(0.7, 1.0, t);
		}
		col = mix(tint, vec3f(1.0), 0.35);
		weight = COLUMN_GAIN * strength;
		soft = 0.0;
	} else {
		let seed = seed0 + f32(pi) * 37.719;
		let base_ang = hash1(seed) * 6.2832;
		var ang = 0.0;
		var rad = 0.0;
		var h = 0.0;
		var strength = 0.0;
		if mode == 2u {
			// The standing wind: motes climb a fixed helix forever, the
			// wind rate and brightness riding the cast's progress.
			let mph = fract(clock / CH_RISE + hash1(seed + 3.0));
			ang = base_ang + clock * (1.5 + 2.5 * t);
			rad = CH_RADIUS * (1.0 - 0.12 * sin(clock * 2.0 + seed));
			h = CH_HEIGHT * mph;
			strength = sin(3.1416 * mph) * (0.3 + 0.7 * t);
			col = mix(tint, vec3f(1.0), WHITE * t);
		} else {
			// A burst: every mote flies the same journey, staggered a
			// little so the swarm has depth. Departure tightens and
			// rises; arrival is the identical flight backwards.
			let stag = clamp(ph * 1.2 - hash1(seed + 5.0) * 0.2, 0.0, 1.0);
			var mph = stag;
			if mode == 1u {
				mph = 1.0 - stag;
			}
			ang = base_ang + TWIST * mph;
			rad = RADIUS * (1.0 - mph);
			h = HEIGHT * mph * mph;
			// In and out inside the burst's own life, reaching zero AT
			// the end (the drop rule).
			strength = smoothstep(0.0, 0.08, ph) * (1.0 - smoothstep(0.82, 1.0, ph));
			col = mix(tint, vec3f(1.0), WHITE * mph);
		}
		center = origin + vec3f(cos(ang) * rad, h, sin(ang) * rad);
		bx = right;
		by = up;
		size_x = MOTE;
		size_y = MOTE;
		col *= MOTE_GAIN;
		weight = strength;
		soft = 0.45;
	}
	weight *= envelope;

	// A quad carrying no weight collapses to a point instead of drawing
	// transparent — an additive pass pays for every pixel it covers.
	let live = select(0.0, 1.0, weight > 1e-4);
	let world = center + (bx * (corner.x * size_x) + by * (corner.y * size_y)) * live;

	// Transmittance only, siphon's rule: adding in-scatter to an emissive
	// term would brighten far motes over near ones.
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
	o.col = vec4f(col * tr, weight);
	o.soft = soft;
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	// Procedural soft disc for motes; the column takes the softest edge
	// and fades along its own height through the same falloff.
	let d = length(i.uv - 0.5) * 2.0;
	return vec4f(i.col.rgb, i.col.a * smoothstep(1.0, i.soft, d));
}
