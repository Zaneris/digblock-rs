// RTGI probe update (GI-DESIGN.md). P0 SCAFFOLDING: a real compute
// pipeline writing the real C0 probe atlas, with a placeholder kernel —
// P2 replaces the body with the DDA trace. What must survive P2 is the
// shape: one dispatch of `dims.x*y*z` probe threads, the atlas laid out
// as six axis-major Z slabs (probe (x,y,z), axis a → texel
// (x, y, z + a*dims.z)), rgba16float write-only storage.

struct GiUniforms {
	// xyz = probe grid dims, w = frame counter (the amortization cursor
	// and per-update ray-rotation seed once P2 traces).
	dims: vec4u,
}

@group(0) @binding(0) var<uniform> gi: GiUniforms;
@group(0) @binding(1) var atlas: texture_storage_3d<rgba16float, write>;

@compute @workgroup_size(64)
fn update_probes(@builtin(global_invocation_id) id: vec3u) {
	let n = gi.dims.xyz;
	if id.x >= n.x * n.y * n.z {
		return;
	}
	let p = vec3u(id.x % n.x, (id.x / n.x) % n.y, id.x / (n.x * n.y));
	// Placeholder payload: a per-axis-scaled position ramp, so a debug
	// view can tell probes AND axes apart at a glance.
	let ramp = vec3f(p) / vec3f(max(n - vec3u(1u), vec3u(1u)));
	for (var a = 0u; a < 6u; a++) {
		let texel = vec3u(p.x, p.y, p.z + a * n.z);
		textureStore(atlas, texel, vec4f(ramp * (f32(a) + 1.0) / 6.0, 1.0));
	}
}
