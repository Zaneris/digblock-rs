// Fullscreen procedural sky: horizon→zenith gradient, sunset glow,
// sun + moon discs, hash-based stars at night. Drawn as one triangle
// at NDC depth 1.0 after the solids (depth test kills occluded
// fragments), before water. Everything is a pure function of the frame
// uniforms — deterministic under a frozen --tod (stars don't twinkle;
// the clouds ride the animation clock, as the water does).
// The gradient/glow/star math lives in sky_common.wgsl (prepended by
// digblock-assets, so line numbers here are offset), shared with the
// water shader's reflections.

// The Frame struct + group(0) binding come from frame_common.wgsl
// (prepended by digblock-assets, before sky_common).

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
	var col = sky_dome(
		ray,
		frame.horizon.rgb,
		frame.zenith.rgb,
		frame.sun_dir,
		frame.moon_dir,
		frame.light_color.rgb,
		frame.light_color.w,
	);
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
