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
fn vs(@location(0) packed: u32) -> @builtin(position) vec4f {
	let x = f32(packed & 0x1fu);
	var y = f32((packed >> 5u) & 0x1fu);
	let z = f32((packed >> 10u) & 0x1fu);
	if ((packed >> 22u) & 1u) == 1u {
		y -= 0.3;
	}
	let world = vec3f(x, y, z) + chunk.origin.xyz;
	return light.view_proj * vec4f(world, 1.0);
}
