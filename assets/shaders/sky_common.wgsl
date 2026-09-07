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

// Integer hash (pcg4d): four uniform lanes from a 2-D cell and a seed. Bits, not fract(sin()),
// so it is exact thousands of blocks out, where the water's fields sample it.
fn sky_pcg4(p: vec4u) -> vec4u {
	var v = p * 1664525u + 1013904223u;
	v.x += v.y * v.w;
	v.y += v.z * v.x;
	v.z += v.x * v.y;
	v.w += v.y * v.z;
	v ^= v >> vec4u(16u);
	v.x += v.y * v.w;
	v.y += v.z * v.x;
	v.z += v.x * v.y;
	v.w += v.y * v.z;
	return v;
}

fn sky_cell4(c: vec2u, seed: u32) -> vec4f {
	return vec4f(sky_pcg4(vec4u(c, seed, 0x9e3779b9u)) >> vec4u(8u)) / 16777216.0;
}

// 2-D value noise, four lanes from one hash per corner, smoothstep-blended. floor -> i32 -> u32
// is a bit reinterpretation, so negative coordinates stay consistent across the origin.
fn sky_vnoise2(p: vec2f, seed: u32) -> vec4f {
	let i = floor(p);
	let f = p - i;
	let u = f * f * (3.0 - 2.0 * f);
	let c = vec2u(vec2i(i));
	let a = sky_cell4(c, seed);
	let b = sky_cell4(c + vec2u(1u, 0u), seed);
	let d = sky_cell4(c + vec2u(0u, 1u), seed);
	let e = sky_cell4(c + vec2u(1u, 1u), seed);
	return mix(mix(a, b, u.x), mix(d, e, u.x), u.y);
}

// Two octaves of the above; lanes sit roughly in [0.2, 0.8] about 0.5.
fn sky_fbm2(p: vec2f, seed: u32) -> vec4f {
	return sky_vnoise2(p, seed) * 0.65
		+ sky_vnoise2(p * 2.03 + vec2f(7.3, 3.1), seed + 1u) * 0.35;
}

// One wind for the water's gust field and the sky's clouds, so the mirror agrees with the dome.
// The drift is a circle with the animation clock's own period (600 s, Engine::time_secs):
// constant speed, continuous, and exactly periodic, where a straight scroll would snap back
// 180 blocks at the wrap.
const SKY_WIND_SPEED = 0.3;
const SKY_WIND_OMEGA = 0.010471976;
fn sky_wind_drift(t: f32) -> vec2f {
	let a = t * SKY_WIND_OMEGA;
	return (SKY_WIND_SPEED / SKY_WIND_OMEGA) * vec2f(cos(a), sin(a));
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

// The cloud deck: a slab of world y the sky pass marches through its shape volume (sky.wgsl)
// and the water's mirror projects onto flat. Above the highest terrain, so no summit stands
// inside it.
const SKY_CLOUD_BASE = 260.0;
const SKY_CLOUD_TOP = 330.0;
// The deck's horizontal extent, as a soft edge on cover: whole between the eye and
// SKY_CLOUD_NEAR blocks out, gone at SKY_CLOUD_REACH. Without it a grazing ray crosses cover
// without end, so every pixel below a dozen degrees of elevation saturates to cloud and the
// horizon reads as overcast under any amount of cover. The sky march and the water mirror both
// take it, so the deck ends at the same distance in the dome and in its reflection.
const SKY_CLOUD_NEAR = 600.0;
const SKY_CLOUD_REACH = 1600.0;
fn sky_cloud_far(dist: f32) -> f32 {
	return 1.0 - smoothstep(SKY_CLOUD_NEAR, SKY_CLOUD_REACH, dist);
}

// Cloud cover over a world xz: fbm on the shared wind's circle, run 14x faster than the water's
// gusts (about 4 blocks/s), thresholded. The march extrudes this through the deck with the shape
// volume; the mirror uses it flat. SKY_CLOUD_AMOUNT is the one knob for how much sky is cloud:
// it slides the threshold through the fbm's band, 0 clear and 1 overcast.
const SKY_CLOUD_AMOUNT = 0.45;
fn sky_cloud_cover(xz: vec2f, t: f32) -> f32 {
	let n = sky_fbm2((xz - sky_wind_drift(t) * 14.0) / 150.0, 23u).x;
	let lo = 0.78 - 0.48 * SKY_CLOUD_AMOUNT;
	return smoothstep(lo, lo + 0.22, n);
}

// Cover along `ray` from `origin` as a flat layer at the deck's middle: what the water mirrors.
// Zero at the horizon and for an eye above the deck, thinning toward the horizon.
fn sky_cloud_mirror(origin: vec3f, ray: vec3f, t: f32) -> f32 {
	let rise = 0.5 * (SKY_CLOUD_BASE + SKY_CLOUD_TOP) - origin.y;
	if ray.y < 0.02 || rise <= 0.0 {
		return 0.0;
	}
	let dist = rise / ray.y;
	let xz = origin.xz + ray.xz * dist;
	return sky_cloud_cover(xz, t) * sky_cloud_far(dist) * smoothstep(0.02, 0.15, ray.y);
}

// A cloud's underside as the water sees it: skylit, with a little of the direct light bled
// through, so it sits a touch brighter than the horizon it replaces.
fn sky_cloud_col(horizon: vec3f, light: vec3f) -> vec3f {
	return horizon * 0.85 + light * 0.25;
}

// The sun and moon discs and the night's stars, drawn over the gradient by the sky pass; the
// caller weights them by whatever cloud sits in front.
fn sky_discs(ray: vec3f, sun: vec4f, moon: vec4f) -> vec3f {
	// Sun disc (soft-edged, and oversized for game feel).
	var col = vec3f(1.0, 0.95, 0.85) * smoothstep(0.9994, 0.9998, dot(ray, sun.xyz)) * 4.0;
	// Moon disc: smaller, dim, slightly blue.
	col += vec3f(0.9, 0.93, 1.0) * smoothstep(0.9996, 0.9999, dot(ray, moon.xyz));
	// Stars fade in at night, suppressed in the horizon haze.
	col += vec3f(sky_stars(ray)) * sun.w * smoothstep(0.0, 0.3, ray.y);
	return col;
}

// Horizon→zenith gradient plus sunset/sunrise glow — the disc-free,
// star-free, cloud-free sky base. sun/moon are the frame lanes passed
// through verbatim: moon.w = sunset glow strength.
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
