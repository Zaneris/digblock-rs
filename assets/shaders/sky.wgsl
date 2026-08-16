// Fullscreen procedural sky: horizon→zenith gradient, sunset glow,
// sun + moon discs, hash-based stars at night. Drawn as one triangle
// at NDC depth 1.0 after the solids (depth test kills occluded
// fragments), before water. Everything is a pure function of the frame
// uniforms — deterministic under a frozen --tod (stars don't twinkle).
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
	var col = sky_base(ray, frame.horizon.rgb, frame.zenith.rgb, frame.sun_dir, frame.moon_dir);
	// Sun disc (soft-edged, and oversized for game feel).
	let ds = dot(ray, frame.sun_dir.xyz);
	col += vec3f(1.0, 0.95, 0.85) * smoothstep(0.9994, 0.9998, ds) * 4.0;
	// Moon disc: smaller, dim, slightly blue.
	let dm = dot(ray, frame.moon_dir.xyz);
	col += vec3f(0.9, 0.93, 1.0) * smoothstep(0.9996, 0.9999, dm);
	// Stars fade in at night, suppressed in the horizon haze.
	col += vec3f(sky_stars(ray)) * frame.sun_dir.w * smoothstep(0.0, 0.3, ray.y);
	return vec4f(col, 1.0);
}
