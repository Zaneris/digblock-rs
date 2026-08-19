// Fullbright ink: how a block with the `glows` builder reads, wherever it is
// drawn. The ONE definition, shared by the chunk mesh, your own hand
// (viewmodel.wgsl) and somebody else's (players.wgsl) — a torch that reads
// one way in the world and another in a hand is exactly the bug a shared
// prelude exists to make impossible. Dependency-free on purpose, which is
// what makes it admissible in viewmodel.wgsl, a pass that carries no frame
// prelude.
//
// A light SOURCE has to out-read what it casts or the picture inverts, and
// the flame is an ordinary albedo texture that at unity gain renders no
// brighter than a well-lit wall. The gain rides the ink's OWN luminance
// because a torch is one block but not one material: head and wooden post
// are both fullbright, and a flat multiplier lifts the post into glowing
// wood. Weighting by value puts the light where the light is, with no
// texture-id special case — which is also why ONE per-draw flag is enough
// for an item made of two materials.
//
// **The corollary is a rule for the TEXTURE: ink is authored for
// FULLBRIGHT, not for lit.** A near-white material has no headroom left,
// clips on all three channels and reads as flat white (R29). Raising this
// gain further was tried and barely moved the sprite, because the ink is
// already clamped by the scene target's SURFACE format — that ceiling is
// what caps LIGHT_GAIN too, and both are stopgaps for the missing tonemap.
const EMISSIVE_GAIN = 1.6;

fn emissive_ink(tex: vec3f) -> vec3f {
	let ink = dot(tex, vec3f(0.299, 0.587, 0.114));
	return tex * (1.0 + EMISSIVE_GAIN * ink * ink);
}
