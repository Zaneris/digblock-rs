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
	// x = alpha (1.0 solid / 0.7 water), y = the streaming radius in
	// blocks (the medium's rim distance — terrain streams as a vertical
	// cylinder, so it bounds HORIZONTAL distance); zw reserved.
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

// INDIRECT_K is gone. It was 0.55 because probe radiance "lands brighter
// than the old tuned ambient did" — a fudge against a term the M1
// demolition deleted, so it had been calibrating the indirect half
// against something that no longer existed. Scaling one half of the
// light transport by a constant does not darken a picture, it breaks
// energy conservation between the direct and indirect halves, and any
// exposure the image needs belongs in one tonemap where it applies to
// both. Its pinned twin in gi_probe.wgsl and the test that existed only
// to hold the two equal are gone with it.

// The water look tunables and `apply_fog` died at the M1 demolition;
// medium_common.wgsl replaced both at M2 with one participating medium
// whose in-scatter is `sky_base` itself. Nothing atmospheric belongs
// here any more — see that file for why the old haze could not have
// been fixed by retuning it.
