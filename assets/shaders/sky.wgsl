// Fullscreen procedural sky: horizon→zenith gradient, sunset glow,
// sun + moon discs, hash-based stars at night. Drawn as one triangle
// at NDC depth 1.0 after the solids (depth test kills occluded
// fragments), before water. Everything is a pure function of the frame
// uniforms — deterministic under a frozen --tod (stars don't twinkle).

// Must match FrameUniforms in renderer.rs and the Frame struct in
// chunk.wgsl — field order and packing are load-bearing.
struct Frame {
	view_proj: mat4x4f,
	inv_view_proj: mat4x4f,
	light_near: mat4x4f,
	light_wide: mat4x4f,
	light_dir: vec4f,
	light_color: vec4f,
	ambient: vec4f,
	// rgb = fog color (sky horizon); w = fog start distance
	fog_color: vec4f,
	// xyz = camera position; w = 1 / (fog_end − fog_start)
	cam_pos: vec4f,
	// xyz = direction toward the sun in the sky; w = star intensity
	sun_dir: vec4f,
	// xyz = direction toward the moon; w = sunset glow strength
	moon_dir: vec4f,
	// rgb = sky zenith color
	zenith: vec4f,
	// rgb = sky horizon color
	horizon: vec4f,
	misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) ndc: vec2f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	// One triangle covering the screen; z = w = 1 → NDC depth exactly 1.
	let xy = vec2f(f32(vi / 2u) * 4.0 - 1.0, f32(vi & 1u) * 4.0 - 1.0);
	var o: VsOut;
	o.pos = vec4f(xy, 1.0, 1.0);
	o.ndc = xy;
	return o;
}

fn view_ray(ndc: vec2f) -> vec3f {
	let w = frame.inv_view_proj * vec4f(ndc, 1.0, 1.0);
	return normalize(w.xyz / w.w - frame.cam_pos.xyz);
}

fn hash13(p: vec3f) -> f32 {
	return fract(sin(dot(p, vec3f(127.1, 311.7, 74.7))) * 43758.5453);
}

// Direction-space starfield: hash a cell grid over the ray, place one
// potential star per cell, threshold most of them away.
fn stars(ray: vec3f) -> f32 {
	let scaled = ray * 64.0;
	let cell = floor(scaled);
	let h = hash13(cell);
	if h < 0.92 {
		return 0.0;
	}
	// Star point inside the cell, from further hashes.
	let sp = cell + vec3f(hash13(cell + 17.0), hash13(cell + 31.0), hash13(cell + 47.0));
	let d = length(scaled - sp);
	let brightness = (h - 0.92) / 0.08;
	return smoothstep(0.35, 0.0, d) * brightness;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let ray = view_ray(i.ndc);
	let up = max(ray.y, 0.0);
	var col = mix(frame.horizon.rgb, frame.zenith.rgb, pow(up, 0.45));
	// Sunset/sunrise glow: hugs the horizon, azimuthally centered on
	// the sun (glow strength already fades on the night side).
	let flat_ray = normalize(vec3f(ray.x, 0.0, ray.z));
	let flat_sun = normalize(vec3f(frame.sun_dir.x, 0.0, frame.sun_dir.z));
	let sa = max(dot(flat_ray, flat_sun), 0.0);
	col += frame.moon_dir.w * frame.horizon.rgb * pow(sa, 6.0) * pow(1.0 - abs(ray.y), 3.0);
	// Sun disc (~2° with a soft edge, oversized for game feel).
	let ds = dot(ray, frame.sun_dir.xyz);
	col += vec3f(1.0, 0.95, 0.85) * smoothstep(0.9994, 0.9998, ds) * 4.0;
	// Moon disc: smaller, dim, slightly blue.
	let dm = dot(ray, frame.moon_dir.xyz);
	col += vec3f(0.9, 0.93, 1.0) * smoothstep(0.9996, 0.9999, dm);
	// Stars fade in at night, suppressed in the horizon haze.
	col += vec3f(stars(ray)) * frame.sun_dir.w * smoothstep(0.0, 0.3, ray.y);
	return vec4f(col, 1.0);
}
