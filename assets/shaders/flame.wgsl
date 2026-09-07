// Torch-local crossed tongues and six drifting embers. No particle simulation.
// The instance transform also lets the hand use its own projection and depth pass.
const QUADS: u32 = 8u;
const CORNERS = array<vec2f, 6>(vec2f(-0.5, 0.0), vec2f(0.5, 0.0), vec2f(-0.5, 1.0),
	vec2f(-0.5, 1.0), vec2f(0.5, 0.0), vec2f(0.5, 1.0));
struct FlameOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) phase: f32,
	@location(2) ember: f32,
	@location(3) tr: vec3f,
	@location(4) weight: f32,
}
@vertex
fn vs(@builtin(vertex_index) vi: u32,
	@location(0) m0: vec4f, @location(1) m1: vec4f,
	@location(2) m2: vec4f, @location(3) m3: vec4f,
	@location(4) site: vec4f) -> FlameOut {
	let pi = vi / 6u;
	let uv = CORNERS[vi % 6u];
	let clock = frame.light_color.w;
	let phase = clock * 6.2831853 + site.w;
	let flicker = fire_gain(clock, site.w);
	var local = vec3f(0.0);
	var weight = 1.0;
	let dist = distance(site.xyz, frame.cam_pos.xyz);
	if pi < 2u {
		let bend = sin(phase + uv.y * 3.0 + f32(pi)) * 0.07 * uv.y * uv.y;
		let x = uv.x * 0.36 + bend;
		local = vec3f(x, uv.y * 0.55 * flicker - 0.04, 0.0);
		if pi == 1u { local = vec3f(0.0, local.y, x); }
		weight = 0.72;
	} else {
		let seed = f32(pi) * 2.39996 + site.w;
		let age = fract(clock / 2.0 + f32(pi) / 6.0 + site.w * 0.01);
		local = vec3f(sin(seed + age * 4.0) * age * 0.22, 0.25 + age * 0.9,
			cos(seed + age * 3.0) * age * 0.22);
		let angle = f32(pi) * 1.0472;
		local += vec3f(cos(angle) * uv.x, uv.y - 0.5, sin(angle) * uv.x) * 0.028;
		weight = sin(age * 3.14159265) * (1.0 - smoothstep(12.0, 28.0, dist));
	}
	weight *= 1.0 - smoothstep(48.0, 64.0, dist);
	if weight < 0.001 { local = vec3f(0.0); }
	var water = 0.0;
	if frame.water_misc.x > 0.5 {
		let ray = (site.xyz - frame.cam_pos.xyz) / max(dist, 0.0001);
		water = dist;
		if ray.y > 0.0001 { water = min(dist, frame.water_misc.z / ray.y); }
	}
	let tr = medium_water_tr(water) * medium_air_tr(dist - water);
	var o: FlameOut;
	o.pos = mat4x4f(m0, m1, m2, m3) * vec4f(local, 1.0);
	o.uv = vec2f(uv.x * 2.0, uv.y);
	o.phase = phase + f32(pi) * 1.8;
	o.ember = f32(pi >= 2u);
	o.tr = tr;
	o.weight = weight * flicker;
	return o;
}
@fragment
fn fs(i: FlameOut) -> @location(0) vec4f {
	let pixel = max(length(fwidth(i.uv)) * 2.0, 0.008);
	let y = i.uv.y;
	let bend = sin(y * 9.0 - i.phase * 2.0) * y * 0.22;
	let width = max(0.02, (1.0 - y) * (0.75 + 0.15 * sin(y * 15.0 - i.phase)));
	let edge = abs(i.uv.x + bend) / width;
	let body = (1.0 - smoothstep(0.45, 1.0, edge)) * smoothstep(0.0, 0.10, y)
		* (1.0 - smoothstep(0.85, 1.0, y));
	let hot = (1.0 - smoothstep(0.1, 0.65, y)) * (1.0 - smoothstep(0.0, 0.65, edge));
	let color = mix(vec3f(1.0, 0.20, 0.015), vec3f(1.0, 0.88, 0.36), hot);
	let ember = fx_glint(vec2f(i.uv.x, y * 2.0 - 1.0), pixel);
	return vec4f(color * i.tr, mix(body, ember, i.ember) * i.weight);
}
