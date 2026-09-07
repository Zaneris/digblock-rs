// The lamp half of a glossy finish, for every pass that draws lit geometry against the light
// list: the chunk mesh and other players' hands. ONE definition, so a metal block in somebody's
// hand gleams by the rule the same block on the floor does. Needs the frame prelude, gi_common
// (the list and its scorer) and finish_common (the lobe) ahead of it; the viewmodel carries no
// frame prelude and no list, which is why this is not in finish_common itself.

// Lamp highlights off the nearest few emitters, unshadowed: the list leads with the lamps
// nearest the eye, and a highlight is where a lamp's image lands, so the scan and the shadow
// marches the old term paid per fragment are not needed to show the lantern in the floor.
fn finish_lamps(p: vec3f, n: vec3f, v: vec3f, finish: u32) -> vec3f {
	let count = min(bitcast<u32>(lights[0].x), FINISH_LAMPS);
	var sum = vec3f(0.0);
	for (var j = 1u; j <= count; j++) {
		let light = lights[j * 2u - 1u];
		let shape = lights[j * 2u];
		let diffuse = gi_light_score(light, shape, p, n, frame.light_color.w);
		if diffuse <= 0.0 { continue; }
		let l = normalize(gi_light_sample(light, shape, p, 0.5, 0.5).p - p);
		let lobe = finish_lobe(n, v, l, finish) / max(dot(n, l), 0.01);
		sum += gi_light_tint(light.w, gi_emit_tint(gi_light_class(light.w))) * LIGHT_GAIN * diffuse * lobe;
	}
	return sum;
}
