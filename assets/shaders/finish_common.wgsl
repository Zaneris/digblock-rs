// Glossy finishes: how polished stone, crystal and metal respond to light, wherever they are
// drawn. The ONE definition, shared by the chunk mesh, your own hand (viewmodel.wgsl) and
// somebody else's (players.wgsl), so a metal block cannot gleam on the floor and go flat in a
// hand. Dependency-free on purpose, which is what makes it admissible in viewmodel.wgsl, a pass
// that carries no frame prelude. The finish code is `block::tex::finish`: 0 matte, 1 polished,
// 2 metal. Each pass supplies its own sun visibility and its own reflected environment; this
// file owns only the lobe, the Fresnel and the way the two terms fold into the diffuse colour.

// How many of the light list's nearest lamps a glossy face mirrors as highlights: the list is
// flushed nearest-the-eye first (`select_lights`), so the head of it is the lamps a player is
// standing by, and a highlight is a Blinn lobe with no shadow ray, a few dozen ALU per lamp.
const FINISH_LAMPS = 8u;

// Schlick Fresnel per finish: the metal is far more reflective at normal incidence than the
// polished stone, and both climb to a full reflection at grazing. The stone sits well above a
// bare dielectric's 0.04: the probe reflection it mirrors is irradiance, blurred and dim, and
// at a physical f0 it vanished into the diffuse. A wet-marble sheen needs the boost.
fn finish_fresnel(finish: u32, cos_theta: f32) -> f32 {
	let f0 = select(0.25, 0.5, finish == 2u);
	return f0 + (1.0 - f0) * pow(1.0 - max(cos_theta, 0.0), 5.0);
}

// Normalized Blinn lobe with Schlick Fresnel. The metal is tighter and more reflective;
// stone carries a broad, restrained polish. No response below the surface or behind the eye.
fn finish_lobe(n: vec3f, v: vec3f, l: vec3f, finish: u32) -> f32 {
	let nl = max(dot(n, l), 0.0);
	let nv = max(dot(n, v), 0.0);
	if nl <= 0.0 || nv <= 0.0 { return 0.0; }
	let h = normalize(v + l);
	let exponent = select(28.0, 96.0, finish == 2u);
	let fresnel = finish_fresnel(finish, dot(v, h));
	let distribution = (exponent + 8.0) * (1.0 / 25.132741) * pow(max(dot(n, h), 0.0), exponent);
	return fresnel * distribution * nl / max(4.0 * nv * nl, 0.15);
}

// Fold the sun lobe and the reflected environment into the diffuse colour. The diffuse gives
// up what the surface reflects instead, and the metal tints its highlight with its own albedo.
fn finish_combine(col: vec3f, tex: vec3f, finish: u32, sun: vec3f, local: vec3f) -> vec3f {
	let tint = select(vec3f(1.0), mix(vec3f(1.0), tex, 0.35), finish == 2u);
	return col * select(0.80, 0.70, finish == 2u) + (sun + local) * tint;
}
