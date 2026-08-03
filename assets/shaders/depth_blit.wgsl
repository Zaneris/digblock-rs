// Resolve the live solid-pass depth into a sampleable R32Float copy —
// WebGPU forbids copy_texture_to_texture into a depth32float
// destination, so the "copy" is one fullscreen draw between the scene
// pass and the water pass. Water samples the result for column
// thickness (absorption, soft shores) while the live attachment goes
// on to take the water prepass's writes.

// Must match depth_blit_bgl in renderer.rs.
@group(0) @binding(0) var src_depth: texture_depth_2d;

struct VsOut {
	@builtin(position) pos: vec4f,
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
	// One triangle covering the screen.
	let xy = vec2f(f32(vi / 2u) * 4.0 - 1.0, f32(vi & 1u) * 4.0 - 1.0);
	var o: VsOut;
	o.pos = vec4f(xy, 1.0, 1.0);
	return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) f32 {
	return textureLoad(src_depth, vec2i(i.pos.xy), 0);
}
