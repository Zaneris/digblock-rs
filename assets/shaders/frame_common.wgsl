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
	// x = camera underwater (0/1), y = SSR enabled (0/1),
	// z = water-surface height above the camera (underwater path-length
	// clamp; only meaningful while x = 1), w reserved
	water_misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

// ---- Water look tunables (ONE place; the engine-side partners are
// the UW_* consts by frame_uniforms() in engine.rs) -------------------
// Per-channel Beer-Lambert extinction, 1/blocks — the looking-into-
// water color absorption in water.wgsl's fs().
const WATER_EXTINCTION: vec3f = vec3f(0.45, 0.16, 0.09);
// The submerged scene fog uses the same K scaled by eye adaptation —
// the eye opens up underwater, so you see FARTHER than the from-above
// absorption implies. Keep <= 1.0.
const WATER_EYE_ADAPT: f32 = 0.45;
// Surface alpha saturates SLOWER than the color extinction (0.5x) but
// runs all the way to opaque: deep water fully hides the bottom while
// shallow ponds stay clear (water.wgsl `opaque`).
const WATER_ALPHA_K: f32 = 0.5;
const WATER_ALPHA_CAP: f32 = 1.0;
// Underside surface look (water.wgsl `below` branch): Snell-window
// edges around cos(48.6°) ≈ 0.66 for n = 1.33, through-window
// brightening, mirror-ring dimming vs the underwater fog color, and
// the pane alpha at (mirror, window).
const TIR_COS_EDGE: vec2f = vec2f(0.58, 0.76);
const SNELL_BRIGHT: f32 = 1.6;
const TIR_MIRROR_DIM: f32 = 0.55;
const BELOW_ALPHA: vec2f = vec2f(0.9, 0.6);

// Distance fog, shared by every scene pass so they cannot drift apart.
// Above water: the classic smoothstepped band toward the horizon color.
// Submerged (water_misc.x): the standard real-time single-scattering
// water model — per-channel Beer-Lambert attenuation with in-scatter,
// applied over the UNDERWATER PORTION of the view ray only. The water
// surface height above the camera rides in water_misc.z, so geometry
// above the surface attenuates only through the short column of water
// between the eye and the surface — looking up at the world stays
// bright instead of fogging over the full distance. In-scatter targets
// a view-elevation gradient (engine-derived lit-water horizon → the
// brighter zenith), so the murk shades toward the light overhead
// instead of converging to a flat wall; WATER_EYE_ADAPT is the one
// sanctioned divergence from the from-above absorption.
fn apply_fog(col: vec3f, dist: f32, world: vec3f) -> vec3f {
	if frame.water_misc.x > 0.5 {
		let cam_y = frame.cam_pos.y;
		let surf = frame.water_misc.z;
		var path = dist;
		if world.y > surf {
			path = dist * clamp((surf - cam_y) / max(world.y - cam_y, 1e-3), 0.0, 1.0);
		}
		let transmit = exp(-WATER_EXTINCTION * WATER_EYE_ADAPT * path);
		let up = clamp((world.y - cam_y) / max(dist, 1e-3), 0.0, 1.0);
		let murk = mix(frame.horizon.rgb, frame.zenith.rgb, up);
		return mix(murk, col, transmit);
	}
	let f = clamp((dist - frame.fog_color.w) * frame.cam_pos.w, 0.0, 1.0);
	return mix(col, frame.fog_color.rgb, f * f * (3.0 - 2.0 * f));
}
