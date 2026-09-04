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

// Mirrors renderer/frame_types.rs `stub::NOCLOUD`.
const STUB_NOCLOUD = 2097152u;
const CLOUD_STEPS = 12u;
// Extinction per block of full density: ~40 blocks of solid cloud is opaque.
const CLOUD_SIGMA = 0.12;
// The march's length cap along a grazing ray, past which the deck is horizon haze anyway.
const CLOUD_REACH = 1400.0;
// Blocks per repeat of the shape volume.
const CLOUD_REPEAT = 140.0;

struct Cloud {
	scatter: vec3f,
	trans: f32,
}

// Density at a point in the deck: the shape volume remapped by the column's cover, shaped by
// height so bases are flat and tops crumble away.
fn cloud_density(p: vec3f, cover: f32, drift: vec2f) -> f32 {
	let h = (p.y - SKY_CLOUD_BASE) / (SKY_CLOUD_TOP - SKY_CLOUD_BASE);
	let hgrad = smoothstep(0.0, 0.12, h) * (1.0 - smoothstep(0.5, 1.0, h));
	let q = (p - vec3f(drift.x, 0.0, drift.y)) / CLOUD_REPEAT;
	let shape = textureSampleLevel(cloud_tex, cloud_smp, q, 0.0).x;
	return clamp((shape - (1.0 - cover)) / max(cover, 0.05), 0.0, 1.0) * hgrad;
}

// March the deck along `ray`: single scattering with two shadow samples toward the caster per
// step, Beer-Lambert extinction, a forward Henyey-Greenstein lobe over a skylight floor. The
// column's cover is read once at the deck's middle; `jitter` slides the start to hide the steps.
fn cloud_march(origin: vec3f, ray: vec3f, jitter: f32, t: f32) -> Cloud {
	var c: Cloud;
	c.scatter = vec3f(0.0);
	c.trans = 1.0;
	if abs(ray.y) < 1e-3 {
		return c;
	}
	let ta = (SKY_CLOUD_BASE - origin.y) / ray.y;
	let tb = (SKY_CLOUD_TOP - origin.y) / ray.y;
	let t0 = max(min(ta, tb), 0.0);
	let t1 = min(max(ta, tb), t0 + CLOUD_REACH);
	if t1 <= t0 {
		return c;
	}
	let tm = clamp((0.5 * (SKY_CLOUD_BASE + SKY_CLOUD_TOP) - origin.y) / ray.y, t0, t1);
	let cover = sky_cloud_cover((origin + ray * tm).xz, t);
	if cover <= 0.0 {
		return c;
	}
	let drift = sky_wind_drift(t) * 6.0;
	let ds = (t1 - t0) / f32(CLOUD_STEPS);
	let to_light = -frame.light_dir.xyz;
	let mu = dot(ray, to_light);
	let g = 0.45;
	let phase = 0.2 + (1.0 - g * g) / (12.566 * pow(1.0 + g * g - 2.0 * g * mu, 1.5));
	let ambient = mix(frame.horizon.rgb, frame.zenith.rgb, 0.5) * 0.6;
	let sun = frame.light_color.rgb * frame.light_dir.w;
	var s = t0 + ds * jitter;
	for (var i = 0u; i < CLOUD_STEPS; i++) {
		let p = origin + ray * s;
		let d = cloud_density(p, cover, drift);
		if d > 0.005 {
			let dl = 12.0;
			var od = cloud_density(p + to_light * dl, cover, drift) * dl;
			od += cloud_density(p + to_light * (dl * 2.5), cover, drift) * (dl * 1.5);
			let lit = exp(-CLOUD_SIGMA * od);
			let ext = exp(-CLOUD_SIGMA * d * ds);
			c.scatter += c.trans * (1.0 - ext) * (sun * lit * phase + ambient);
			c.trans *= ext;
			if c.trans < 0.02 {
				break;
			}
		}
		s += ds;
	}
	// Far along the deck the clouds sink into the horizon haze.
	let haze = smoothstep(0.15, 1.0, t0 / CLOUD_REACH);
	c.scatter = mix(c.scatter, frame.horizon.rgb * (1.0 - c.trans), haze);
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
	if (u32(frame.misc.z) & STUB_NOCLOUD) == 0u {
		let jitter = sky_hash13(vec3f(floor(i.pos.xy), 0.0));
		let cl = cloud_march(frame.cam_pos.xyz, ray, jitter, frame.light_color.w);
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
