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
	light_near: mat4x4f,
	light_wide: mat4x4f,
	// xyz = active caster dir (sun or moon), toward the ground
	// w = shadow strength 0..1 (twilight fade)
	light_dir: vec4f,
	// rgb = direct light color (sun by day, moonlight by night)
	// w = animation clock in seconds (wraps at 600; water scroll/foam)
	light_color: vec4f,
	// rgb = sky-tinted ambient floor
	ambient: vec4f,
	// rgb = fog color (sky horizon); w = fog start distance (units)
	fog_color: vec4f,
	// xyz = camera position; w = 1 / (fog_end − fog_start)
	cam_pos: vec4f,
	// xyz = direction toward the sun in the sky; w = star intensity
	sun_dir: vec4f,
	// xyz = direction toward the moon; w = sunset glow strength
	moon_dir: vec4f,
	// rgb = sky zenith color
	zenith: vec4f,
	// rgb = sky horizon color
	horizon: vec4f,
	// x = alpha (1.0 solid / 0.7 water)
	// y = shadows enabled (0.0 until the shadow passes exist)
	// z = shadow map texel size (1.0 / resolution, both cascades)
	// w = wide-cascade texel size in world units (normal-offset bias)
	misc: vec4f,
	// x = camera underwater (0/1), y = SSR enabled (0/1), z/w reserved
	water_misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;
