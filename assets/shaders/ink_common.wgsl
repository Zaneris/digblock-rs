// Fullbright ink: how a block with the `glows` builder reads, wherever
// it is drawn.
//
// The ONE definition, shared by every pass that can draw an emissive
// block — the chunk mesh, your own hand (viewmodel.wgsl), and somebody
// else's hand (players.wgsl). Prepended rather than copied because
// nothing else can hold those three together, and a torch that reads one
// way in the world and another way in a hand is exactly the bug a shared
// prelude exists to make impossible.
//
// Dependency-free on purpose, which is what makes it admissible in
// viewmodel.wgsl — a pass that deliberately carries no frame prelude
// because it is authored in view space and must not declare bindings it
// does not use.
//
// Fullbright ink is a light SOURCE, and a source has to out-read what it
// casts or the picture inverts: the flame is authored as an ordinary
// albedo texture, so at unity gain it renders no brighter than a well-lit
// wall — and once the torch's near field stopped clipping (LIGHT_NEAR,
// gi_resolve.wgsl) the pool it threw was brighter than the flame
// throwing it.
//
// The gain rides the ink's OWN luminance, because a torch is one block
// but not one material: the head and the wooden post are both
// fullbright, and a flat multiplier lifts the post into glowing wood.
// Weighting by value puts the light where the light is — the flame takes
// nearly the whole gain, the post barely moves — and it needs no texture
// id special case in a shader to do it. That is also why ONE per-draw
// flag is enough for an item made of two materials.
//
// The corollary is a rule for the TEXTURE rather than for this function:
// an ink texture is authored for FULLBRIGHT, not for lit. A material
// painted near white has no headroom left to take any gain at all — it
// clips on all three channels and reads as flat white — and the
// weighting cannot separate it from a flame of the same luminance, which
// is what made the torch's post out-read its own fire until the post was
// cut from the dark timber (R29).
//
// This gain rather than a larger one: raising it further was tried and
// barely moved the sprite at all, because the ink is already clamped by
// the scene target's SURFACE format and cannot be raised to follow a
// brighter pool. That ceiling is what caps LIGHT_GAIN, and both are
// stopgaps for the missing tonemap (R22).
const EMISSIVE_GAIN = 1.6;

fn emissive_ink(tex: vec3f) -> vec3f {
	let ink = dot(tex, vec3f(0.299, 0.587, 0.114));
	return tex * (1.0 + EMISSIVE_GAIN * ink * ink);
}
