// Small procedural footprints shared by physical and emissive effects.
// Derivatives keep fine details quiet when their projected size shrinks.
fn fx_edge(distance: f32, radius: f32, pixel: f32) -> f32 {
	let aa = max(pixel, 0.008);
	return 1.0 - smoothstep(radius - aa, radius + aa, distance);
}

fn fx_glint(p: vec2f, pixel: f32) -> f32 {
	let q = abs(p);
	let core = 1.0 - smoothstep(0.0, 0.36, length(p));
	let star = fx_edge(sqrt(q.x) + sqrt(q.y), 1.0, pixel);
	return max(core, star * 0.8);
}

fn fx_ring(p: vec2f, radius: f32, width: f32, pixel: f32) -> f32 {
	return fx_edge(abs(length(p) - radius), width, pixel);
}

fn fx_puff(p: vec2f, phase: f32) -> f32 {
	let warp = vec2f(sin(p.y * 7.0 + phase), cos(p.x * 8.0 - phase)) * 0.12;
	let d = length(p + warp);
	return (1.0 - smoothstep(0.2, 1.0, d)) * (0.8 + 0.2 * sin(p.x * 9.0 + p.y * 6.0 + phase));
}
