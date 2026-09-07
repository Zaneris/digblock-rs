// The generic burst rack: block-break debris, splash droplets, sparkle,
// and poof, all one pipeline — confetti.wgsl's stateless arrangement
// (CPU owns {origin, age, seed}; every particle is a pure function of
// those) extended with a KIND lane and an instance color, so a new
// polish effect is a match arm here rather than a sixth module. One
// instance per live burst: [pos.xyz, age][seed, kind, life, size]
// [color.rgb, surface_y + 1 (zero when absent)]; vertex_index picks the particle (vi / 6) and the quad
// corner (vi % 6). Every kind's alpha reaches zero exactly at its
// `life`, so the CPU dropping the burst there can never cut a particle.

// Frame struct + group(0) come from frame_common.wgsl; group(2) GI world
// bindings + the shared NEE from gi_common.wgsl (prepended). Bursts are
// lit as confetti is (R34): camera ambient + traced sun + `sum_lights`
// at the particle, because debris flies wherever somebody is digging —
// beside a torch as often as under the sky.

struct CamLight {
	ambient: vec4f,
	sun: vec4f,
}
@group(1) @binding(0) var<uniform> cam_light: CamLight;

// Cost-stub mirror (DIG_STUB; renderer.rs's `stub` module owns the bit).
const STUB_GINEE = 1024u;

fn stubbed(bit: u32) -> bool {
	return (u32(frame.misc.z) & bit) != 0u;
}

// Particle budget per instance — keep in sync with bursts.rs PARTICLES.
// Kinds that want fewer emit degenerate quads for the surplus indices.
const PARTICLES: u32 = 64u;

const KIND_DEBRIS: u32 = 0u;
const KIND_SPLASH: u32 = 1u;
const KIND_SPARKLE: u32 = 2u;
const KIND_POOF: u32 = 3u;

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

// How many of the budget a kind actually shows.
fn kind_count(kind: u32) -> u32 {
	switch kind {
		case KIND_SPLASH: {
			return 36u;
		}
		case KIND_SPARKLE: {
			return 24u;
		}
		case KIND_POOF: {
			return 12u;
		}
		default: {
			return 28u; // debris
		}
	}
}

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) col: vec4f,
	// x = kind (the fragment picks a square or a round footprint).
	@location(2) kind_lane: vec2f,
	@location(3) phase: vec2f,
}

@vertex
fn vs(
	@builtin(vertex_index) vi: u32,
	@location(0) inst: vec4f,
	@location(1) inst2: vec4f,
	@location(2) inst3: vec4f,
) -> VsOut {
	let pi = vi / 6u;
	let corner = CORNERS[vi % 6u];
	let age = inst.w;
	let kind = u32(inst2.y);
	let life = inst2.z;
	let seed = inst2.x + f32(pi) * 17.311;
	let live = pi < kind_count(kind);
	let ripple = kind == KIND_SPLASH && pi < 2u && inst3.w > 0.0;

	// Launch + arc, per kind. Debris and splash are ballistic (real
	// gravity — stone is not paper); sparkle drifts; poof billows.
	let az = select(hash1(seed) * 6.2832, f32(pi) * 0.785398, kind == KIND_SPLASH && pi < 10u);
	let radial = vec3f(cos(az), 0.0, sin(az));
	var v0 = vec3f(0.0);
	var grav = 0.0;
	switch kind {
		case KIND_SPLASH: {
			v0 = radial * (1.2 + 2.0 * hash1(seed + 1.0))
				+ vec3f(0.0, 2.6 + 1.6 * hash1(seed + 2.0), 0.0);
			if pi < 10u { v0 = radial * 1.7 + vec3f(0.0, 1.8, 0.0); }
			grav = -14.0;
		}
		case KIND_SPARKLE: {
			let el = hash1(seed + 1.0) * 2.0 - 1.0;
			v0 = radial * 0.5 * (1.0 - abs(el)) + vec3f(0.0, 0.3 + 0.5 * el, 0.0);
		}
		case KIND_POOF: {
			v0 = radial * (0.5 + 0.4 * hash1(seed + 1.0))
				+ vec3f(0.0, 0.35 + 0.3 * hash1(seed + 2.0), 0.0);
		}
		default: { // debris
			v0 = radial * (1.5 + 2.0 * hash1(seed + 1.0))
				+ vec3f(0.0, 2.0 + 2.5 * hash1(seed + 2.0), 0.0);
			grav = -9.0;
		}
	}
	var center = inst.xyz + v0 * age + vec3f(0.0, 0.5 * grav * age * age, 0.0);
	if kind == KIND_POOF {
		center += radial * sin(age * 3.0 + az) * age * 0.15;
	}

	// Footprint, per kind, scaled by the instance size lane. Debris
	// tumbles like confetti (one cos on the width); poof grows.
	var size = vec2f(0.0);
	var roll = 0.0;
	switch kind {
		case KIND_SPLASH: {
			let s = 0.03 + 0.03 * hash1(seed + 3.0);
			let speed = length(v0 + vec3f(0.0, grav * age, 0.0));
			size = vec2f(s, s * (1.0 + speed * 0.35));
			let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
			let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));
			let velocity = v0 + vec3f(0.0, grav * age, 0.0);
			roll = -atan2(dot(velocity, right), dot(velocity, up));
		}
		case KIND_SPARKLE: {
			let s = 0.035 + 0.03 * hash1(seed + 3.0);
			size = vec2f(s, s);
		}
		case KIND_POOF: {
			let s = 0.14 + 0.42 * sqrt(clamp(age / life, 0.0, 1.0));
			size = vec2f(s, s);
			roll = hash1(seed + 5.0) * 6.2832;
		}
		default: { // debris
			let spin = 6.0 + 8.0 * hash1(seed + 3.0);
			let tumble = abs(cos(spin * age + hash1(seed + 4.0) * 6.2832));
			let s = 0.035 + 0.095 * pow(hash1(seed + 6.0), 2.0);
			size = vec2f(s * (0.25 + 0.75 * tumble), s);
			roll = spin * 0.4 * age + hash1(seed + 5.0) * 6.2832;
		}
	}
	size *= inst2.w * f32(live);
	let rot = vec2f(cos(roll), sin(roll));
	let local = vec2f(
		corner.x * size.x * rot.x - corner.y * size.y * rot.y,
		corner.x * size.x * rot.y + corner.y * size.y * rot.x,
	);

	// Camera-facing, same basis extraction as spray/confetti.
	let right = normalize(vec3f(frame.view_proj[0].x, frame.view_proj[1].x, frame.view_proj[2].x));
	let up = normalize(vec3f(frame.view_proj[0].y, frame.view_proj[1].y, frame.view_proj[2].y));
	var world = center + right * local.x + up * local.y;
	if ripple {
		let phase = clamp(age / life - f32(pi) * 0.16, 0.0, 1.0);
		center = vec3f(inst.x, inst3.w - 1.0 + 0.012, inst.z);
		world = center + vec3f(corner.x, 0.0, corner.y) * (0.1 + 1.5 * phase);
	}

	// The end fade reaches zero EXACTLY at `life` for every kind; the
	// pop-in keeps the first frame from being a solid disc in a head.
	var alpha = 1.0 - smoothstep(life * 0.7, life, age);
	alpha *= smoothstep(0.0, 0.05, age);
	var tint = inst3.rgb;
	switch kind {
		case KIND_SPLASH: {
			// Droplets read light: lift toward white over the base water tint.
			tint = mix(tint, vec3f(0.85, 0.92, 1.0), 0.55);
			alpha *= 0.9;
		}
		case KIND_SPARKLE: {
			// Twinkle — each mote blinks on its own clock.
			alpha *= 0.35 + 0.65 * abs(sin((8.0 + 8.0 * hash1(seed + 7.0)) * age + az));
			tint *= 1.8;
		}
		case KIND_POOF: {
			alpha *= 0.35;
		}
		default: { // debris: hash-varied brightness mimics texel variation
			tint *= 0.7 + 0.5 * hash1(seed + 7.0);
		}
	}

	// Lit like confetti: probe ambient + traced sun, plus the emitter
	// list at the particle's centre with a zero normal, reach-gated.
	let sh = cam_light.sun.rgb * frame.light_dir.w;
	var nee = vec3f(0.0);
	let lat = center.xz - frame.cam_pos.xz;
	let gmax = gi_lateral_max(frame.gi_misc.x);
	if dot(lat, lat) <= gmax * gmax && !stubbed(STUB_GINEE) {
		nee = sum_lights(
			i32(frame.gi_misc.x), frame.gi_misc.y, frame.cam_pos.xz, center, vec3f(0.0), frame.light_color.w,
		);
	}
	let lighting = cam_light.ambient.rgb + frame.light_color.rgb * 0.8 * sh + nee;
	let col = tint * select(lighting, vec3f(1.0), kind == KIND_SPARKLE);
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
	o.col = vec4f(lit, alpha * f32(live));
	o.kind_lane = vec2f(select(f32(kind), 4.0, ripple), hash1(seed + 9.0));
	o.phase = vec2f(age / life, az);
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let pixel = max(length(fwidth(i.uv)) * 2.0, 0.008);
	let kind = u32(i.kind_lane.x);
	let p = (i.uv - 0.5) * 2.0;
	var mask = 0.0;
	var col = i.col.rgb;
	if kind == 4u {
		mask = fx_ring(p, 0.8, 0.035, pixel) * (1.0 - i.phase.x);
	} else if kind == KIND_DEBRIS {
		let chip = max(max(abs(p.x), abs(p.y)), abs(p.x + p.y * 0.7) * 0.8);
		mask = fx_edge(chip, 0.9, pixel);
		col *= 0.72 + 0.28 * smoothstep(-0.15, 0.05, p.x - p.y * 0.5 + i.kind_lane.y * 0.3);
	} else if kind == KIND_SPARKLE {
		mask = mix(fx_glint(p, pixel), 1.0 - smoothstep(0.1, 1.0, length(p)), step(0.6, i.kind_lane.y));
	} else if kind == KIND_POOF {
		mask = fx_puff(p, i.phase.x * 2.0 + i.phase.y);
		mask *= 1.0 - smoothstep(0.35, 1.0, i.phase.x);
	} else {
		mask = 1.0 - smoothstep(0.65, 1.0, length(p));
		let glint = 1.0 - smoothstep(0.0, 0.25, length(p - vec2f(-0.22, 0.3)));
		col *= 0.8 + glint * 0.7;
	}
	return vec4f(col, i.col.a * mask);
}
