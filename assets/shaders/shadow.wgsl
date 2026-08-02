// Depth-only shadow pass. Depth bias is set on the pipeline, not here.

struct Light {
	view_proj: mat4x4f,
}

@group(0) @binding(0) var<uniform> light: Light;

struct ChunkU {
	origin: vec4f,
}

@group(1) @binding(0) var<uniform> chunk: ChunkU;

@vertex
fn vs(@location(0) packed: vec2u) -> @builtin(position) vec4f {
	// Solid meshes only — water never casts, so only w0 geometry matters
	// (half-block units, see chunk.wgsl).
	let w0 = packed.x;
	let x = f32(w0 & 0x3fu) * 0.5;
	let y = f32((w0 >> 6u) & 0x3fu) * 0.5;
	let z = f32((w0 >> 12u) & 0x3fu) * 0.5;
	let world = vec3f(x, y, z) + chunk.origin.xyz;
	return light.view_proj * vec4f(world, 1.0);
}
