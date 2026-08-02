// Fullscreen composite of the offscreen scene color into the swapchain
// — the first draw of the water pass, after the scene pass (solids +
// sky) rendered offscreen and its depth was copied for water to
// sample. One triangle, exact 1:1 texel fetch (no sampler), depth
// compare Always with writes off so water then tests against the
// preserved scene depth.

// Must match blit_bgl in renderer.rs: binding 0 is the scene color,
// binding 1 the linear sampler (declared in the layout, unused here —
// the composite is an exact textureLoad).
@group(0) @binding(0) var scene_tex: texture_2d<f32>;

struct VsOut {
	@builtin(position) pos: vec4f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	// One triangle covering the screen at NDC depth 1.
	let xy = vec2f(f32(vi / 2u) * 4.0 - 1.0, f32(vi & 1u) * 4.0 - 1.0);
	var o: VsOut;
	o.pos = vec4f(xy, 1.0, 1.0);
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	return vec4f(textureLoad(scene_tex, vec2i(i.pos.xy), 0).rgb, 1.0);
}
