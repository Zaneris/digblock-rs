// Shared procedural-sky core, prepended to sky.wgsl and water.wgsl by
// digblock-assets (concat! at include time — naga error line numbers in
// those files are offset by this prelude). Pure functions of their
// arguments only, so the sky pass and water reflections can never
// drift apart. Callers add the sun/moon discs themselves: the sky pass
// draws real discs, while water represents them with an analytic
// specular lobe instead (discs alias badly on rippled normals).

fn sky_hash13(p: vec3f) -> f32 {
	return fract(sin(dot(p, vec3f(127.1, 311.7, 74.7))) * 43758.5453);
}

// Direction-space starfield: hash a cell grid over the ray, place one
// potential star per cell, threshold most of them away. Callers weight
// by star intensity (sun_dir.w) and their own horizon suppression.
fn sky_stars(ray: vec3f) -> f32 {
	let scaled = ray * 64.0;
	let cell = floor(scaled);
	let h = sky_hash13(cell);
	if h < 0.92 {
		return 0.0;
	}
	// Star point inside the cell, from further hashes.
	let sp = cell
		+ vec3f(sky_hash13(cell + 17.0), sky_hash13(cell + 31.0), sky_hash13(cell + 47.0));
	let d = length(scaled - sp);
	let brightness = (h - 0.92) / 0.08;
	return smoothstep(0.35, 0.0, d) * brightness;
}

// Horizon→zenith gradient plus sunset/sunrise glow — the disc-free,
// star-free sky base. sun/moon are the frame lanes passed through
// verbatim: moon.w = sunset glow strength.
// The dome as the sky pass draws it: the gradient plus the sun and moon discs
// and the night's stars. Snell's window samples this along the BENT ray.
fn sky_dome(ray: vec3f, horizon: vec3f, zenith: vec3f, sun: vec4f, moon: vec4f) -> vec3f {
	var col = sky_base(ray, horizon, zenith, sun, moon);
	// Sun disc (soft-edged, and oversized for game feel).
	col += vec3f(1.0, 0.95, 0.85) * smoothstep(0.9994, 0.9998, dot(ray, sun.xyz)) * 4.0;
	// Moon disc: smaller, dim, slightly blue.
	col += vec3f(0.9, 0.93, 1.0) * smoothstep(0.9996, 0.9999, dot(ray, moon.xyz));
	// Stars fade in at night, suppressed in the horizon haze.
	col += vec3f(sky_stars(ray)) * sun.w * smoothstep(0.0, 0.3, ray.y);
	return col;
}

fn sky_base(ray: vec3f, horizon: vec3f, zenith: vec3f, sun: vec4f, moon: vec4f) -> vec3f {
	let up = max(ray.y, 0.0);
	var col = mix(horizon, zenith, pow(up, 0.45));
	// Sunset/sunrise glow: hugs the horizon, azimuthally centered on
	// the sun (glow strength already fades on the night side).
	let flat_ray = normalize(vec3f(ray.x, 0.0, ray.z));
	let flat_sun = normalize(vec3f(sun.x, 0.0, sun.z));
	let sa = max(dot(flat_ray, flat_sun), 0.0);
	col += moon.w * horizon * pow(sa, 6.0) * pow(1.0 - abs(ray.y), 3.0);
	return col;
}
