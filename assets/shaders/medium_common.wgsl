// One participating medium — the whole atmosphere in one function. Radiative
// transfer along a segment of length d through extinction sigma:
//     L = L_surface * exp(-sigma*d)  +  L_in * (1 - exp(-sigma*d))
//
// **The entire design is in where L_in comes from: `sky_base`, the SAME function
// the sky pass draws.** That makes the medium self-consistent at infinity — as d
// grows a surface fades to exactly the sky pixel that would be behind it, sunset
// glow included — so haze and sky cannot drift apart and the draw-distance rim
// disappears. Fogging toward one flat colour is the wrong QUANTITY, not a curve
// to retune.
//
// Water is the same equation with a different sigma and a down-welling
// in-scatter DIRECTION, so it tints by depth while air tints by distance.
// Dependency-free — every input an argument — so it can be prepended to the GI
// compute kernels, and the view and sun rays share ONE water extinction.

// Air extinction, expressed as a VISIBILITY DISTANCE rather than a raw
// coefficient: Koschmieder's relation, sigma = ln(1/0.02)/V, at the standard 2%
// contrast threshold, so the knob means "how far can you see?" The visibility IS
// a weather choice — clear air is INVISIBLE at this world's scale, so a value
// that shows aerial perspective declares hazy air; `medium_rim` owns the edge.
const AIR_VISIBILITY = 1600.0;
const AIR_SIGMA = 3.912 / AIR_VISIBILITY;

// Water extinction per block, per channel. MEASURED, not chosen: the beam
// attenuation of clear natural water sampled near the RGB primaries (Smith &
// Baker 1981 / Pope & Fry 1997; a block is a metre). This is the ONLY water
// extinction in the codebase — `gi_sun_vis` returns a water PATH LENGTH rather
// than baking its own exponential, so view ray and sun ray cannot disagree.
const WATER_SIGMA = vec3f(0.36, 0.068, 0.030);

// Water's single-scatter albedo, folded with the multiple-scattering
// gain a single-scatter term cannot represent. This is the one number
// here that is chosen rather than measured: it sets how bright the
// blue-green haze between the eye and a submerged surface reads.
const WATER_ALBEDO = 0.35;

// The depth fade for UNPROVEN daylight (GI-DESIGN.md R32): full at the top edge,
// gone at the bottom. Every place the lighting ANSWERS sky without proving a
// path to it multiplies by this, so a deep tunnel goes dark; a PROVEN sky escape
// never takes it. `smoothstep` saturates at the top edge, so the surface world
// cannot regress — that edge is `SEA_LEVEL`, per `the_sky_fade_tops_out_at_sea_level`.
const SKY_FADE_FULL_Y = 128.0;
const SKY_FADE_ZERO_Y = 64.0;

fn medium_sky_fade(y: f32) -> f32 {
	return smoothstep(SKY_FADE_ZERO_Y, SKY_FADE_FULL_Y, y);
}

fn medium_air_tr(d: f32) -> f32 {
	return exp(-AIR_SIGMA * max(d, 0.0));
}

fn medium_water_tr(d: f32) -> vec3f {
	return exp(-WATER_SIGMA * max(d, 0.0));
}

// Down-welling light surviving to `depth` below the surface — the source term
// for anything in-scattered underwater, and the reason deep water goes dark AND
// blue rather than merely dark. Attenuating irradiance by the BEAM coefficient
// slightly overstates the darkening (diffuse K_d is smaller, because forward-
// scattered light keeps going down); the error is in the safe direction.
fn medium_water_light(sky_down: vec3f, depth: f32) -> vec3f {
	return sky_down * WATER_ALBEDO * medium_water_tr(depth);
}

// One submerged segment: `d` blocks of water between `col` and the eye,
// lit by `l_in`.
fn medium_water_seg(col: vec3f, l_in: vec3f, d: f32) -> vec3f {
	let tr = medium_water_tr(d);
	return col * tr + l_in * (vec3f(1.0) - tr);
}

// The streaming boundary, a separate fact from the air: the medium hides no edge
// at any honest sigma, so it gets its own term. Terrain streams in a vertical
// CYLINDER of draw_dist chunks, so the world's edge sits at a fixed HORIZONTAL
// distance and keying this on view distance would fade the ground under a
// downward look. It fades to the same in-scattered sky, so the two agree.
fn medium_rim(horiz_dist: f32, far: f32) -> f32 {
	return smoothstep(far * 0.82, far, horiz_dist);
}

// The whole primary-ray medium: surface colour `col` at `world`, seen from
// `cam`. `sky` = sky_base along the view ray (the caller owns sky_common);
// `sky_down` = the down-welling source for a submerged segment; `water_misc` =
// frame.water_misc (x camera underwater 0/1, z water-surface height above the
// camera); `far` = the streaming radius in blocks (frame.misc.y).
fn medium_view(
	col: vec3f,
	cam: vec3f,
	world: vec3f,
	sky: vec3f,
	sky_down: vec3f,
	water_misc: vec4f,
	far: f32,
) -> vec3f {
	let d = distance(cam, world);
	var out = col;
	var air = d;
	// In-scatter is SKY light, and underground there is none: the medium has no
	// occlusion term, so without this a 50-block tunnel view in-scattered a
	// distance-proportional slice of full daylight at any depth — the dominant
	// leak R32 removes. Faded at the FRAGMENT's altitude, so a viewer looking out
	// of a tunnel mouth still sees full haze on the surface beyond it.
	let sky_f = sky * medium_sky_fade(world.y);
	if water_misc.x > 0.5 {
		let ray = (world - cam) / max(d, 1e-4);
		// The submerged segment ends where the ray leaves the water —
		// at the surface if it rises through it, otherwise at the
		// surface being shaded.
		var dw = d;
		if ray.y > 1e-4 {
			dw = min(d, water_misc.z / ray.y);
		}
		// Depth of the segment's MIDPOINT below the surface: one
		// evaluation point for the whole segment, which is what makes
		// this a single-scatter approximation rather than a march.
		let depth = max(water_misc.z - ray.y * dw * 0.5, 0.0);
		// The down-welling source fades with the same ramp — deep water
		// darkens with world depth on top of its own extinction, which is
		// the correct direction and costs nothing extra.
		out = medium_water_seg(
			out,
			medium_water_light(sky_down * medium_sky_fade(world.y), depth),
			dw,
		);
		air = d - dw;
	}
	let tr = medium_air_tr(air);
	out = out * tr + sky_f * (1.0 - tr);
	// The rim fades to the same (depth-faded) in-scatter, so a deep
	// fragment at the streaming edge fades to dark rather than to sky —
	// consistent by construction, and invisible in practice: the rim
	// lives at the horizon, which is surface terrain.
	return mix(out, sky_f, medium_rim(distance(cam.xz, world.xz), far));
}
