//! What a caller hands the renderer for one frame. `FrameUniforms` is
//! pinned to the WGSL struct by `frame_layout_matches_wgsl`, so the two
//! are changed together or the test fails.

use super::*;

/// Everything `Renderer::new` needs beyond the device and surface
/// format — derived from `EngineConfig` (see `render_config`), so the
/// renderer itself never infers platform tiers from other knobs.
#[derive(Clone)]
pub(crate) struct RenderConfig {
	/// Draw distance in chunks (sizes the chunk-origin buffer).
	pub draw_dist: i32,
	/// Spray droplets per waterfall-impact instance (on top of the mist
	/// slabs); the mobile config asks for fewer.
	pub spray_droplets: u32,
	/// Force the initial chunk-origin capacity in slots (`DIG_CHUNK_SLOTS`
	/// via the launcher). Tiny values exercise `grow_chunk_buf` in a
	/// normal run. None = sized from `draw_dist`.
	pub chunk_slots_override: Option<u32>,
	/// Replace the scene with the voxel-window debug raymarch
	/// (`DIG_GI=window`).
	pub debug_gi: bool,
	/// Debug raymarch shades hits with the C0 probe field instead of
	/// raw albedo (`DIG_GI=probes`) — the P2 gate view.
	pub debug_gi_probes: bool,
	/// Cost-stub mask (`DIG_STUB`) — see `EngineConfig::stub`. The shader
	/// halves ride `frame.misc.z` (water) and the resolve's `jitter.w`;
	/// the pass-level bits are honoured on this side, because a term you
	/// can only stub INSIDE a shader can never tell you what its geometry
	/// costs.
	pub stub: u32,
}

/// Cost-stub bits (`DIG_STUB=<name>[,<name>…]`). Each removes one term
/// so a single build can attribute a frame, which is the machinery two
/// water perf rounds went without: R17 and R19 were both reasoned from
/// code and measured on a pose that could not show the term they moved.
///
/// The shader-side bits are read in water.wgsl (through `frame.misc.z`)
/// and gi_resolve.wgsl (through its own `jitter.w`, since the resolve does
/// not bind the frame UBO) as a UNIFORM branch, so an un-stubbed run pays
/// nothing and a stubbed one never diverges within a wave. The pass-level
/// bits are honoured in `Renderer::render`.
pub mod stub {
	/// Water's per-VERTEX `gi_indirect` → a constant. The gather is a long
	/// chain of dependent texture loads per water vertex inside C1, which
	/// is a cost bounded by GEOMETRY rather than by pixels — the only
	/// hypothesis that survives "it is just as slow below the surface".
	pub const WVERT: u32 = 1 << 0;
	/// Water's per-fragment `gi_sun_vis` (a short march) → lit.
	pub const WSUN: u32 = 1 << 1;
	/// `reflection_color`'s screen-space march → sky only.
	pub const WSSR: u32 = 1 << 2;
	/// Ripple gradient + specular → flat face normal.
	pub const WRIPPLE: u32 = 1 << 3;
	/// Thickness/refraction/medium → the straight scene sample.
	pub const WREFRACT: u32 = 1 << 4;
	/// Water's whole fragment shader → a flat colour on the first line:
	/// the raster + vertex floor under everything above.
	pub const WFLAT: u32 = 1 << 5;
	/// Skip the spray billboards. A flat-walled pool has no falling
	/// water and therefore NO impact cells, so this term has never once
	/// appeared in a measurement of this engine.
	pub const SPRAY: u32 = 1 << 6;
	/// Skip the water pass entirely (depth prepass, colour, and all).
	/// Bounds everything else: whatever this saves is the whole budget.
	pub const NOWATER: u32 = 1 << 7;

	// The GI half. Water had stubs and the GI did not, which is why two
	// rounds of water tuning have measured numbers behind them and the
	// resolve — the frame's most expensive pass — has never had one term
	// attributed. These ride the resolve's own uniform (`jitter.w`), not
	// `frame.misc.z`: the resolve does not bind the frame UBO.
	/// The resolve's per-pixel sun ray → lit. The single largest ray budget
	/// in the frame.
	pub const GISUN: u32 = 1 << 8;
	/// The resolve's cosine-hemisphere final gather → the cheap
	/// `gi_cache_at` lookup, i.e. the cascade-blend answer the final gather
	/// replaced.
	pub const GIGATHER: u32 = 1 << 9;
	/// The resolve's per-pixel NEE shadow ray → skipped.
	/// Zero in any scene with no emitter in range, by construction.
	pub const GINEE: u32 = 1 << 10;
	/// The whole resolve fragment → a constant on the first line: the
	/// raster + history floor under every term above, the `wflat` twin.
	pub const GIFLAT: u32 = 1 << 11;
	/// Skip both à-trous iterations. Pass-level — the scene then reads the
	/// raw resolve out of `[0]` and history goes stale, which is exactly
	/// the point: this measures the filter, not the picture.
	pub const GIATROUS: u32 = 1 << 12;
	/// Skip the chunk draws in the SCENE pass only. The prepass still runs,
	/// so depth and face codes are intact and the GI is untouched — this is
	/// the one clean raster stub.
	pub const NOSCENE: u32 = 1 << 13;
	/// Skip the chunk draws in BOTH the prepass and the scene pass.
	///
	/// Read this as an UPPER BOUND on chunk-geometry cost, never as an
	/// estimate, and never subtract it from `noscene` to "get" the prepass.
	/// With no prepass geometry the depth buffer stays cleared, so every
	/// resolve pixel takes the sky early-out and the frame's most expensive
	/// pass goes trivially cheap too. A prepass-only stub would carry the
	/// same confound with none of the honesty, which is why there isn't one.
	pub const NORASTER: u32 = 1 << 14;

	const NAMES: [(&str, u32); 15] = [
		("wvert", WVERT),
		("wsun", WSUN),
		("wssr", WSSR),
		("wripple", WRIPPLE),
		("wrefract", WREFRACT),
		("wflat", WFLAT),
		("spray", SPRAY),
		("nowater", NOWATER),
		("gisun", GISUN),
		("gigather", GIGATHER),
		("ginee", GINEE),
		("giflat", GIFLAT),
		("giatrous", GIATROUS),
		("noscene", NOSCENE),
		("noraster", NORASTER),
	];

	/// Comma-separated names → mask. Unknown names are returned so the
	/// launcher can complain: a typo that silently measures the
	/// UN-stubbed build is exactly the failure this facility exists to
	/// prevent.
	pub fn parse(s: &str) -> (u32, Vec<String>) {
		let mut mask = 0;
		let mut bad = Vec::new();
		for tok in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
			match NAMES.iter().find(|(n, _)| *n == tok) {
				Some((_, bit)) => mask |= bit,
				None => bad.push(tok.to_string()),
			}
		}
		(mask, bad)
	}

	/// Every name, for the launcher's usage line.
	pub fn names() -> impl Iterator<Item = &'static str> {
		NAMES.iter().map(|(n, _)| *n)
	}
}

/// Everything `Renderer::render` consumes for one frame, built by the
/// engine — one argument instead of a parameter list that grew with
/// every feature.
pub(crate) struct FrameScene<'a> {
	pub uniforms: FrameUniforms,
	pub view_proj: glam::Mat4,
	/// The SURFACE size. The world may render smaller (see `scale`), but
	/// the UI, name tags and every hit-test stay on this one.
	pub size: (u32, u32),
	/// World render scale + GI divisor for this frame.
	pub scale: RenderScale,
	/// Whole-UI scale for this frame (device scale factor × window size).
	pub ui_scale: f32,
	/// Remote-player instances `[pos.xyz, yaw, rgb, spare]`, uploaded
	/// every frame (tiny — no change detection).
	pub players: &'a [[f32; 8]],
	/// The item in each remote's hand: its `IconKind` (the geometry to
	/// draw, from `item_geom`) plus
	/// `[eye.xyz, yaw, emissive, height in twelfths, spare, spare]`.
	pub held: &'a [(crate::ui::hud::IconKind, [f32; 8])],
	/// Lights players are CARRYING this frame — yours and every remote's
	/// (`digblock_core::gi::carried_origin`). They join the ordinary GI
	/// light list, so a torch in a hand lights a room exactly as one on
	/// the floor does. Passed with the frame rather than set ahead of it,
	/// because `flush_lights` runs inside `GiDraw::encode` and an
	/// out-of-order setter would be a hazard for nothing.
	pub carried: &'a [crate::gi::Emitter],
	/// Live level-up confetti bursts `[origin.xyz, age][seed, …]`, aged
	/// by the engine — the flakes themselves are a pure function of these
	/// in the shader.
	pub confetti: &'a [crate::confetti::BurstInstance],
	/// The essence draw under the crosshair, if the siphon is working a
	/// vein. At most one — a player holds one button — and the engine
	/// owns its envelope, so this outlives the hold by the fade.
	pub siphon: Option<crate::siphon::DrawInstance>,
	pub overlay: Option<(&'a OverlayStats, &'a [f32])>,
	pub feed: &'a [(crate::voice::Msg, f32, u32)],
	/// Crosshair target for the highlight decal — cell, the shape's own
	/// box within it, and dig progress (`None` = idle outline).
	pub target: Option<crate::highlight::HighlightTarget>,
	/// The item in the player's own hand, or `None` (empty slot, a
	/// screen open, the menu up) to skip the viewmodel pass entirely.
	pub viewmodel: Option<crate::viewmodel::ViewmodelFrame>,
	/// This frame's UI draw commands (HUD, screens), engine-built.
	pub ui: &'a crate::ui::DrawList,
	/// Chat input line while typing (multiplayer only).
	pub chat: Option<crate::chat::ChatView<'a>>,
	/// Remote-player name tags, already projected to screen pixels.
	pub nametags: &'a [NameTag],
}

/// One name tag: screen-space anchor (pixels, tag centered on `x`),
/// overlay 0xAABBGGRR color, and the display name.
pub(crate) struct NameTag {
	pub x: f32,
	pub y: f32,
	pub color: u32,
	pub text: String,
}

/// Field order/layout must match the `Frame` struct in
/// `frame_common.wgsl` (the shared prelude every pass gets). The match
/// is CI-enforced: `tests::frame_layout_matches_wgsl` below runs naga's
/// Layouter over the compiled shader and compares struct sizes.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct FrameUniforms {
	pub view_proj: [[f32; 4]; 4],
	/// Inverse of view_proj — sky-pass view-ray reconstruction.
	pub inv_view_proj: [[f32; 4]; 4],
	/// xyz = active caster dir (sun or moon), toward the ground;
	/// w = caster strength 0..1 (twilight fade).
	pub light_dir: [f32; 4],
	/// rgb = direct light color (sun by day, moonlight by night);
	/// w = the animation clock in seconds (periodically wrapped, so it
	/// stays exact in f32 across a long session).
	pub light_color: [f32; 4],
	/// rgb = the scene-pass clear color (the sky horizon tint the sky
	/// pass then draws over); w reserved.
	pub clear_color: [f32; 4],
	/// xyz = camera position; w reserved.
	pub cam_pos: [f32; 4],
	/// xyz = direction toward the sun in the SKY (= −light dir);
	/// w = star intensity 0..1.
	pub sun_dir: [f32; 4],
	/// xyz = direction toward the moon; w = sunset glow strength 0..1.
	pub moon_dir: [f32; 4],
	/// rgb = sky zenith color.
	pub zenith: [f32; 4],
	/// rgb = sky horizon color.
	pub horizon: [f32; 4],
	/// x = water alpha, y = the streaming radius in blocks (the
	/// participating medium's rim distance — terrain is streamed in a
	/// vertical cylinder, so this is a HORIZONTAL bound); zw reserved
	/// (the remaining shadow-map lanes died with the cascades).
	pub misc: [f32; 4],
	/// x = camera underwater (0/1), y = SSR enabled (0/1), z = water-
	/// surface height above the camera (the shaders' underwater
	/// path-length clamp; meaningful only while x = 1), w reserved.
	pub water_misc: [f32; 4],
	/// RTGI fields, patched by `GiDraw::patch_frame` in `render` (the
	/// engine writes zeros) — mirror frame_common.wgsl's Frame struct.
	/// xyz = quantized dir toward the caster, w = above horizon (0/1).
	pub gi_sun: [f32; 4],
	/// C0 probe dims xyz, w = that cascade's probe spacing in blocks.
	pub gi0: [f32; 4],
	/// C1 probe dims xyz, w = that cascade's probe spacing in blocks.
	pub gi1: [f32; 4],
	/// x = voxel-window side in blocks. Appended last.
	pub gi_misc: [f32; 4],
}
