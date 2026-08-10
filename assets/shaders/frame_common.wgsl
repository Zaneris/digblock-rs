// Shared per-frame uniform block, prepended to chunk.wgsl, water.wgsl,
// sky.wgsl, spray.wgsl, and players.wgsl by digblock-assets (concat! at
// include time — naga error line numbers in those files are offset by
// this prelude). One definition means the five passes can never drift
// apart; the Rust-side layout match against FrameUniforms in
// renderer.rs is enforced by a naga Layouter test there.
struct Frame {
	view_proj: mat4x4f,
	// Inverse of view_proj — sky-pass view-ray reconstruction.
	inv_view_proj: mat4x4f,
	// xyz = active caster dir (sun or moon), toward the ground
	// w = caster strength 0..1 (twilight fade)
	light_dir: vec4f,
	// rgb = direct light color (sun by day, moonlight by night)
	// w = animation clock in seconds (wraps at 600; water scroll/foam)
	light_color: vec4f,
	// rgb = the scene-pass clear color (sky horizon tint); w free
	clear_color: vec4f,
	// xyz = camera position; w free
	cam_pos: vec4f,
	// xyz = direction toward the sun in the sky; w = star intensity
	sun_dir: vec4f,
	// xyz = direction toward the moon; w = sunset glow strength
	moon_dir: vec4f,
	// rgb = sky zenith color
	zenith: vec4f,
	// rgb = sky horizon color
	horizon: vec4f,
	// x = alpha (1.0 solid / 0.7 water); yzw reserved — the shadow-map
	// lanes died with the cascades.
	misc: vec4f,
	// x = camera underwater (0/1), y = SSR enabled (0/1),
	// z = water-surface height above the camera (underwater path-length
	// clamp; only meaningful while x = 1), w reserved
	water_misc: vec4f,
	// RTGI probe-field parameters (patched by the renderer each frame
	// from GiDraw — the engine writes zeros):
	// xyz = quantized dir toward the sun (the GI rays' caster — moon at
	// night), w = caster above horizon (0/1).
	gi_sun: vec4f,
	// C0 probe grid dims xyz, w = spacing in blocks (2).
	gi0: vec4f,
	// C1 far-cascade dims xyz, w = spacing (8).
	gi1: vec4f,
	// x = voxel-window side in blocks (the toroidal modulus).
	gi_misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

// Indirect calibration: probe radiance arrives in sky-color units,
// which lands brighter than the old tuned ambient did — one knob scales
// the whole indirect term into the look the direct term was tuned
// against (P3; re-derive at P5 if the probe payload gets renormalized).
// Lives here because chunk.wgsl AND every direct probe gatherer (water,
// players) must agree; gi_probe.wgsl's cam_light carries a pinned twin
// (`indirect_k_is_one_constant` in digblock-assets).
const INDIRECT_K = 0.55;

// The water look tunables and `apply_fog` both died at the M1
// demolition (GI-DESIGN.md). `apply_fog` was one function with two
// hand-tuned models bolted together — a submerged Beer-Lambert
// single-scatter and an above-water smoothstep toward one flat horizon
// colour — and neither is coming back in that form. M2 replaces both
// with a single participating medium whose in-scatter is the same
// directional sky radiance the GI rays already sample, so haze and
// lighting cannot drift apart the way two constant blocks guaranteed
// they would.
//
// Until then there is deliberately NO distance falloff: the draw
// distance ends in a hard rim against the sky. That is the honest
// intermediate state, not an oversight.
