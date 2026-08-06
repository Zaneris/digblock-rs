// Depth-only shadow pass. Depth bias is set on the pipeline, not here.

struct Light {
	view_proj: mat4x4f,
}

@group(0) @binding(0) var<uniform> light: Light;

// See chunk.wgsl: exact twelfths → blocks decode.
fn twelfths(v: u32) -> f32 {
	return f32(v / 12u) + f32(v % 12u) / 12.0;
}

// Location 1 is the instance-step chunk origin — see chunk.wgsl.
@vertex
fn vs(@location(0) packed: vec2u, @location(1) origin: vec4f) -> @builtin(position) vec4f {
	// Solid meshes only — water never casts, so only w0 geometry matters
	// (twelfth-block units, see chunk.wgsl).
	let w0 = packed.x;
	let x = twelfths(w0 & 0xffu);
	let y = twelfths((w0 >> 8u) & 0xffu);
	let z = twelfths((w0 >> 16u) & 0xffu);
	let world = vec3f(x, y, z) + origin.xyz;
	return light.view_proj * vec4f(world, 1.0);
}
