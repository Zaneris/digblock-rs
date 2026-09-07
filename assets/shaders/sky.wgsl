// Fullscreen procedural sky: horizon→zenith gradient, sunset glow, a volumetric cloud deck,
// sun + moon discs, hash-based stars at night. Drawn as one triangle at NDC depth 1.0 after the
// solids (depth test kills occluded fragments), before water. Everything is a pure function of
// the frame uniforms and a baked shape volume — deterministic under a frozen --tod (stars don't
// twinkle; the clouds ride the animation clock, as the water does). The gradient/glow/star math
// and the deck's cover live in sky_common.wgsl (prepended by digblock-assets, so line numbers
// here are offset), shared with the water shader's reflections.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets, before sky_common).

// The tiling Perlin-Worley shape volume (cloud_noise.rs), R8, repeat-sampled.
@group(1) @binding(0) var cloud_tex: texture_3d<f32>;
@group(1) @binding(1) var cloud_smp: sampler;

const CLOUD_STEPS = 14u;
// Extinction per block of full density: ~30 blocks of solid cloud is opaque to the eye. The
// light march folds three multi-scatter octaves in, so the interior still glows (see `cloud_light`).
const CLOUD_SIGMA = 0.10;
// Single-scatter albedo: clouds are very nearly white.
const CLOUD_ALBEDO = 0.94;
// Blocks per repeat of the shape volume, and its straight scroll: each component's 600 s travel
// is a whole number of repeats (3.2*600 = 12*160, 2.4*600 = 9*160), so the tiling volume lands on
// itself at the clock's wrap. It runs slower than the cover, and the difference is what makes the
// shapes evolve instead of sliding by as a stamp.
const CLOUD_REPEAT = 160.0;
const CLOUD_VEL = vec2f(3.2, 2.4);

struct Cloud {
	scatter: vec3f,
	trans: f32,
}

// Density at a point in the deck. Cover is read HERE, per sample, so a grazing ray crossing many
// cells is cut by each cell's own cover rather than one read stretched along it. The shape
// volume, remapped by the cover and shaped by height (flat base, a top that climbs with cover so
// wisps stay low and thick cover towers), then eroded at its edges by the same volume read five
// times finer: wispy where it thins, and the erosion flips sense low to high so bases stay flat
// while tops crumble (Schneider's Nubis). The erosion is skipped on shadow samples. `dist` is the
// sample's distance from the eye, which ends the deck (sky_cloud_far) per sample: a grazing ray
// is thinned along its whole path rather than saturating and being painted over afterwards.
fn cloud_density(p: vec3f, dist: f32, t: f32, detail: bool) -> f32 {
	let cover = sky_cloud_cover(p.xz, t) * sky_cloud_far(dist);
	if cover <= 0.0 {
		return 0.0;
	}
	let h = clamp((p.y - SKY_CLOUD_BASE) / (SKY_CLOUD_TOP - SKY_CLOUD_BASE), 0.0, 1.0);
	let top = mix(0.35, 1.0, cover);
	let hgrad = smoothstep(0.0, 0.08, h) * (1.0 - smoothstep(top * 0.55, top, h));
	let q = (p - vec3f(CLOUD_VEL.x, 0.0, CLOUD_VEL.y) * t) / CLOUD_REPEAT;
	let base = textureSampleLevel(cloud_tex, cloud_smp, q, 0.0).x * hgrad;
	var d = clamp((base - (1.0 - cover)) / cover, 0.0, 1.0);
	if detail && d > 0.0 {
		let e = textureSampleLevel(cloud_tex, cloud_smp, q * 5.3 + vec3f(0.31, 0.47, 0.11), 0.0).x;
		let erode = mix(e, 1.0 - e, clamp(h * 6.0, 0.0, 1.0)) * 0.3;
		d = clamp((d - erode) / (1.0 - erode), 0.0, 1.0);
	}
	return d;
}

// Henyey-Greenstein scaled so isotropic is 1: a forward lobe that peaks looking into the sun.
fn cloud_hg(mu: f32, g: f32) -> f32 {
	return (1.0 - g * g) / pow(1.0 + g * g - 2.0 * g * mu, 1.5);
}

// The sun reaching a sample through `od` blocks-of-density toward it: three octaves of Wrenninge's
// multi-scatter approximation (contribution, extinction and eccentricity halve per octave), which
// is what keeps a cloud's shadowed body luminous instead of the near-black single scattering gives.
fn cloud_light(od: f32, mu: f32) -> f32 {
	var lit = 0.0;
	var a = 1.0;
	var b = 1.0;
	var c = 1.0;
	for (var k = 0u; k < 3u; k++) {
		lit += a * exp(-b * CLOUD_SIGMA * od) * cloud_hg(mu, c * 0.55);
		a *= 0.5;
		b *= 0.5;
		c *= 0.5;
	}
	return lit;
}

// March the deck along `ray`: three shadow samples toward the caster per lit step, Beer-Lambert
// extinction, energy-conserving in-scatter (Hillaire), skylight ambient that brightens toward the
// tops. `jitter` slides the start to hide the steps. `clear` is the cloudless sky along the ray:
// aerial perspective sinks the far deck into it, glow and all, rather than into a flat horizon
// tint, so a sunset still reads through the last of the deck.
fn cloud_march(origin: vec3f, ray: vec3f, jitter: f32, t: f32, clear: vec3f) -> Cloud {
	var c: Cloud;
	c.scatter = vec3f(0.0);
	c.trans = 1.0;
	if abs(ray.y) < 1e-3 {
		return c;
	}
	let ta = (SKY_CLOUD_BASE - origin.y) / ray.y;
	let tb = (SKY_CLOUD_TOP - origin.y) / ray.y;
	let t0 = max(min(ta, tb), 0.0);
	let t1 = min(max(ta, tb), SKY_CLOUD_REACH);
	if t1 <= t0 {
		return c;
	}
	let ds = (t1 - t0) / f32(CLOUD_STEPS);
	let to_light = -frame.light_dir.xyz;
	let mu = dot(ray, to_light);
	let sun = frame.light_color.rgb * frame.light_dir.w;
	var s = t0 + ds * jitter;
	for (var i = 0u; i < CLOUD_STEPS; i++) {
		let p = origin + ray * s;
		let d = cloud_density(p, s, t, true);
		if d > 0.003 {
			var od = cloud_density(p + to_light * 5.0, s, t, false) * 5.0;
			od += cloud_density(p + to_light * 14.0, s, t, false) * 9.0;
			od += cloud_density(p + to_light * 30.0, s, t, false) * 16.0;
			let h = clamp((p.y - SKY_CLOUD_BASE) / (SKY_CLOUD_TOP - SKY_CLOUD_BASE), 0.0, 1.0);
			let ambient = mix(frame.horizon.rgb, frame.zenith.rgb, h) * (0.5 + 0.5 * h);
			let radiance = CLOUD_ALBEDO * (sun * cloud_light(od, mu) + ambient);
			let ext = exp(-CLOUD_SIGMA * d * ds);
			c.scatter += c.trans * (1.0 - ext) * radiance;
			c.trans *= ext;
			if c.trans < 0.02 {
				break;
			}
		}
		s += ds;
	}
	let haze = smoothstep(0.15, 1.0, t0 / SKY_CLOUD_REACH);
	c.scatter = mix(c.scatter, clear * (1.0 - c.trans), haze);
	return c;
}

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

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let ray = view_ray(i.ndc);
	var col = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	var trans = 1.0;
	// The deck rides water_misc.w: on unless DIG_NO_CLOUDS / ?noclouds drops it.
	if frame.water_misc.w > 0.5 {
		let jitter = sky_hash13(vec3f(floor(i.pos.xy), 0.0));
		let cl = cloud_march(frame.cam_pos.xyz, ray, jitter, frame.light_color.w, col);
		col = col * cl.trans + cl.scatter;
		trans = cl.trans;
	}
	col += sky_discs(ray, frame.sun_dir, frame.moon_dir) * trans;
	// Submerged, the dome is seen through the water: the column up to where the ray surfaces,
	// or an endless one where it never does — which is what lies past the last streamed chunk,
	// and was a raw sky beside a seabed the medium had faded into its own in-scatter.
	if frame.water_misc.x > 0.5 {
		let z0 = frame.water_misc.z;
		let sky_down = frame.zenith.rgb * medium_sky_fade(frame.cam_pos.y + z0);
		if ray.y > 1e-4 {
			col = medium_water_column(col, sky_down, z0, -ray.y, z0 / ray.y);
		} else {
			col = medium_water_endless(sky_down, z0, -ray.y);
		}
	}
	return vec4f(col, 1.0);
}
