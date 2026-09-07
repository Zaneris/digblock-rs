// A restrained, periodic variation: placed torches have spatial phases,
// carried torches keep a stable phase while their owner moves.
fn fire_seed(origin: vec3f) -> f32 {
	return dot(origin, vec3f(12.9898, 4.1414, 78.233));
}
fn fire_gain(clock: f32, seed: f32) -> f32 {
	let phase = clock * 6.2831853 + seed;
	return 1.0 + 0.06 * sin(phase) + 0.035 * sin(phase * 2.0 + 1.7);
}
