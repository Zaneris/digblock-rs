//! Device-lifetime wgpu state. `Renderer::new` is a pure constructor —
//! device-loss recovery is "build a new one and re-upload meshes".

use digblock_core::chunk::ChunkPos;
use digblock_core::mesher::{MeshData, quad_indices};
use digblock_core::metrics::RenderStats;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{
	GpuTimer, Q_DEPTH_BLIT, Q_GI_DENOISE, Q_GI_DENOISE2, Q_MAIN, Q_OVERLAY, Q_PREPASS, Q_UPSCALE,
	Q_VIEWMODEL, Q_WATER,
};
use crate::overlay::Overlay;
use crate::sky::SkyDraw;
use crate::texture::BlockTextures;
use crate::water::WaterDraw;
use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

/// The depth prepass's second output: one face-normal code per pixel
/// (chunk.wgsl's `code_of`), a sentinel code where no solid drew. Uint,
/// not unorm — this is an index into a table, and it has to survive the
/// round trip bit-exactly for the bilateral upsample's equality test to
/// mean anything.
const NORMAL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Uint;

/// Worst-case quads per chunk mesh: a solid chunk of the busiest shaped
/// variant (every template quad emits when neighbors don't cover them).
/// Dwarfs the old full-block checkerboard bound. The shared index buffer
/// this sizes is allocated once and stays small enough for mobile;
/// `upload_chunk` guards the cap instead of trusting it.
const MAX_QUADS: usize = 4096 * digblock_core::shapes::MAX_SHAPE_QUADS;

/// Chunk-origin stride: one `vec4f` per slot in the instance-step origin
/// stream. Was padded out to the uniform dynamic-offset alignment rather
/// than to the data — back when each draw rebound a slice of a UBO.
pub(crate) const ORIGIN_STRIDE: u64 = 16;

/// Chunk vertex-buffer size class. A remesh (an edit dirties every
/// neighbour whose shell it falls in; walking churns the rim continuously)
/// nearly always lands inside the class of the mesh it replaces, so it is
/// written over the live allocation; what does change class is recycled
/// through the free list below. Both matter most on Chrome, where every
/// buffer create and destroy is a GPU-process allocation plus a deferred
/// free that wgpu batches into periodic cleanup spikes.
const SIZE_CLASS: u64 = 16 * 1024;
/// Ceiling on retired bytes held for reuse. Rim churn at walking speed is
/// well under a MiB a frame, so this covers seconds of travel; past it
/// buffers are dropped for real, because a world swap or a draw-distance
/// cut has to give the memory back.
const POOL_BYTES_MAX: u64 = 16 * 1024 * 1024;

/// Coarse front-to-back ordering for the scene pass: one bucket per chunk
/// of column distance from the camera. Immediate-mode GPUs (what Chrome
/// usually runs on) get their early-Z rejection from draw order alone,
/// and the hash-map iteration this replaced was effectively random — pure
/// overdraw. Anything past the last bucket still draws, just unordered.
const DIST_BUCKETS: usize = 48;

/// `DeviceExt::create_buffer_init` replacement, mandatory on wasm and
/// used everywhere for uniformity. The util helper copies through a JS
/// `Uint8Array` view of wasm linear memory with wgpu-internal
/// allocations between creating the view and consuming it; if one of
/// them grows wasm memory, every live view detaches and the copy throws
/// "Cannot perform %TypedArray%.prototype.set on a detached
/// ArrayBuffer" (seen in the field from `upload_chunk` while streaming
/// grew the heap). `Queue::write_buffer` is immune: its FFI glue builds
/// the view and hands it to the synchronous GPU copy in one step, with
/// no wasm allocation in between.
fn buffer_init(gpu: &Gpu, label: &str, usage: wgpu::BufferUsages, contents: &[u8]) -> wgpu::Buffer {
	// write_buffer requires u32-aligned sizes; all callers upload
	// u32-packed data.
	debug_assert_eq!(contents.len() % 4, 0, "buffer_init needs u32-aligned data");
	let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
		label: Some(label),
		size: contents.len() as u64,
		usage: usage | wgpu::BufferUsages::COPY_DST,
		mapped_at_creation: false,
	});
	gpu.queue.write_buffer(&buf, 0, contents);
	buf
}

/// Allocation size for a mesh of `bytes`: the next whole size class.
fn size_class(bytes: u64) -> u64 {
	bytes.next_multiple_of(SIZE_CLASS).max(SIZE_CLASS)
}

/// Chunk-origin slot capacity for a draw distance. Sky chunks never
/// upload, but caves can make any buried chunk non-empty, so budget the
/// full stack of every in-range column — a slot is one `vec4f`, so even
/// the whole stack is a rounding error against the meshes. A starting
/// estimate, not a ceiling: `grow_chunk_buf` takes over before the buffer
/// fills, which is what a future runtime draw-distance change will lean
/// on.
fn chunk_slots(draw_dist: i32) -> u32 {
	let r = f64::from(draw_dist) + 1.0;
	let columns = (std::f64::consts::PI * r * r).ceil() as u32;
	(columns * digblock_core::chunk::WORLD_VCHUNK as u32)
		.next_multiple_of(256)
		.max(4096)
}

/// New capacity when `needed` exceeds the current slot count: round up
/// to the allocation granularity, but never less than double (amortized
/// growth — a handful of reallocations over a whole session).
fn grown_slots(current: u32, needed: u32) -> u32 {
	needed.next_multiple_of(256).max(current.saturating_mul(2))
}

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The packed two-word chunk vertex (see `mesher::pack`), shared by the
/// solid, water, and depth-prepass pipelines.
const GEOMETRY_ATTRS: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
	format: wgpu::VertexFormat::Uint32x2,
	offset: 0,
	shader_location: 0,
}];
/// The chunk-origin instance stream (see `origin_vertex_layout`).
const ORIGIN_ATTRS: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
	format: wgpu::VertexFormat::Float32x4,
	offset: 0,
	shader_location: 1,
}];

pub(crate) fn geometry_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
	wgpu::VertexBufferLayout {
		array_stride: 8,
		step_mode: wgpu::VertexStepMode::Vertex,
		attributes: &GEOMETRY_ATTRS,
	}
}

/// Slot-indexed chunk origins as an instance-step vertex stream, bound
/// once per pass. Every chunk draws one instance at `first_instance =
/// slot`, so the vertex stage picks its own origin out and the per-chunk
/// loop is `set_vertex_buffer` + `draw_indexed` — no bind group, no
/// dynamic offset. chunk.wgsl's `vs`/`vs_depth` and water.wgsl all read
/// it at `@location(1)` and must be changed together.
pub(crate) fn origin_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
	wgpu::VertexBufferLayout {
		array_stride: ORIGIN_STRIDE,
		step_mode: wgpu::VertexStepMode::Instance,
		attributes: &ORIGIN_ATTRS,
	}
}

/// Everything `Renderer::new` needs beyond the device and surface
/// format — derived from `EngineConfig` (see `render_config`), so the
/// renderer itself never infers platform tiers from other knobs.
#[derive(Clone)]
pub struct RenderConfig {
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
	/// `gi_cache_at` lookup, i.e. the pre-R8 cascade-blend answer.
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
pub struct FrameScene<'a> {
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
	pub feed: &'a [(String, f32, u32)],
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
pub struct NameTag {
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
pub struct FrameUniforms {
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

/// One uploaded mesh: the pooled buffer, the size class it was allocated
/// at, and the index count in use. `cap` is ≥ the live bytes — the tail
/// past them is stale and unreachable (`draw_indexed` is bounded by
/// `index_count`), which is exactly what lets a remesh write over the
/// existing allocation instead of trading it for a new one.
pub(crate) struct ChunkMesh {
	buf: wgpu::Buffer,
	cap: u64,
	index_count: u32,
}

/// A resident chunk's GPU state, owned by the renderer's flat `chunks`
/// Vec (`by_pos` maps ChunkPos back to its index). Flat because the cull
/// walks the whole set every frame: a hash map costs more in layout and
/// iteration order than the frustum test itself does.
pub struct ChunkGpu {
	pub(crate) pos: ChunkPos,
	pub(crate) solid: Option<ChunkMesh>,
	pub(crate) water: Option<ChunkMesh>,
	/// Origin-stream index; rides each draw as `first_instance`.
	pub(crate) slot: u32,
	/// Bounding-sphere center, precomputed — the cull is the hottest
	/// per-frame CPU loop in the renderer.
	pub(crate) center: glam::Vec3,
}

/// This frame's culled draw lists. The resident set is walked — and
/// frustum-tested — exactly once per frame, and the scene list carries
/// the only draw ordering the pass gets.
struct CullLists {
	/// Camera-visible solid chunks, coarsely front-to-back.
	scene: Vec<u32>,
	/// Camera-visible chunks carrying water geometry. Unordered: the
	/// water depth prepass is what decides which surface shades.
	water: Vec<u32>,
	/// Solid casters inside the near cascade's light frustum.
	near: Vec<u32>,
	/// Counting-sort input: (distance bucket, chunk index).
	keyed: Vec<(u8, u32)>,
	counts: [u32; DIST_BUCKETS],
}

impl Default for CullLists {
	// Hand-written: DIST_BUCKETS is past the array sizes Default covers.
	fn default() -> Self {
		Self {
			scene: Vec::new(),
			water: Vec::new(),
			near: Vec::new(),
			keyed: Vec::new(),
			counts: [0; DIST_BUCKETS],
		}
	}
}

/// Stable counting sort of `keyed` into `out` by bucket — O(n), no
/// comparisons, no allocation after the first frame. Cheap enough to run
/// every frame, which is the point: re-sorting the resident set only when
/// the camera chunk changes would trade a flat cost for a spike at every
/// chunk boundary, and spikes are what this whole pass exists to remove.
fn scatter_by_bucket(keyed: &[(u8, u32)], counts: &[u32; DIST_BUCKETS], out: &mut Vec<u32>) {
	let mut off = [0u32; DIST_BUCKETS];
	let mut acc = 0u32;
	for (o, n) in off.iter_mut().zip(counts.iter()) {
		*o = acc;
		acc += n;
	}
	out.clear();
	out.resize(acc as usize, 0);
	for &(b, i) in keyed {
		let cursor = &mut off[b as usize];
		out[*cursor as usize] = i;
		*cursor += 1;
	}
}

pub struct Renderer {
	solid_pipeline: wgpu::RenderPipeline,
	/// Depth-only prepass over the opaque solids: fills the scene depth
	/// the GI resolve reconstructs world positions from, before any
	/// shading runs.
	prepass_pipeline: wgpu::RenderPipeline,
	/// The water depth-prepass + color subpass pair.
	water: WaterDraw,
	/// Fullscreen procedural sky, drawn after solids, before water.
	sky: SkyDraw,
	frame_buf: wgpu::Buffer,
	frame_bg: wgpu::BindGroup,
	/// Water copy of the frame UBO, carrying its own alpha (Java u_Alpha
	/// for the transparent pass); bound through `FrameTargets::water_bg`.
	/// Group 1: block texture array + sampler — one static bind group.
	materials_bg: wgpu::BindGroup,
	/// Slot-indexed chunk origins, bound as an instance-step vertex
	/// stream once per pass.
	origin_buf: wgpu::Buffer,
	/// CPU mirror of `origin_buf`, indexed by slot. Keeps `grow_chunk_buf`
	/// to a single write of exactly the allocated slots (no multi-MB
	/// zero-filled staging block) and lets a remesh skip the origin write
	/// entirely — it keeps its slot, so the origin cannot have changed.
	origins: Vec<[f32; 4]>,
	index_buf: wgpu::Buffer,
	/// Resident chunks, flat (see `ChunkGpu`); `by_pos` is the index.
	chunks: Vec<ChunkGpu>,
	by_pos: FxHashMap<ChunkPos, u32>,
	/// Retired vertex buffers by size class (`cap / SIZE_CLASS - 1`), so
	/// an evict/stream cycle recycles instead of reallocating.
	pool: Vec<Vec<wgpu::Buffer>>,
	pool_bytes: u64,
	lists: CullLists,
	free_slots: Vec<u32>,
	next_slot: u32,
	/// Capacity of `origin_buf` in slots.
	chunk_slots: u32,
	/// Total vertex-buffer bytes ALLOCATED (live meshes plus everything
	/// parked in `pool`) — the honest GPU footprint, not the subset a
	/// mesh currently fills.
	pub gpu_buffer_bytes: u64,
	/// None when the device lacks TIMESTAMP_QUERY.
	gpu_timer: Option<GpuTimer>,
	/// RTGI: the voxel window, probe cascades, and the half-res resolve
	/// — the lighting system (GI-DESIGN.md).
	gi: crate::gi::GiDraw,
	/// Draw the voxel-window debug raymarch over the scene (DIG_GI).
	debug_gi: bool,
	/// Cost-stub mask (DIG_STUB) — only the pass-level bits are read here;
	/// the rest ride the frame uniform (water) or the resolve's own
	/// uniform (GI) into the shaders.
	stub: u32,
	/// Debug overlay; always constructed (cheap) so the device-loss
	/// rebuild path recreates it for free.
	overlay: Overlay,
	/// Size-dependent frame targets, lazily (re)built by `ensure_targets`
	/// each render — one path covers first frame, resize, resume, and
	/// device loss (`Renderer::new` starts empty).
	targets: Option<FrameTargets>,
	/// Stateless waterfall spray/mist billboards (instance list fed by
	/// MeshData.impacts through upload_chunk).
	spray: crate::spray::Spray,
	/// Level-up confetti bursts, fed per-frame by the engine (the bursts
	/// themselves are world state and live there).
	confetti: crate::confetti::Confetti,
	/// The Essence Siphon's draw — motes off a vein's face, streaming into
	/// the tool. Fed per-frame by the engine, which owns the envelope.
	siphon: crate::siphon::Siphon,
	/// Remote-player boxes (multiplayer), fed per-frame by the engine.
	players: crate::players::RemotePlayersDraw,
	/// Target-block outline + dig-crack decal (scene pass).
	highlight: crate::highlight::HighlightDraw,
	/// The item in your own hand, in its own pass over the finished
	/// frame.
	viewmodel: crate::viewmodel::ViewmodelDraw,
	/// Hotbar block icons (overlay pass, between the overlay's splits).
	ui_icons: crate::ui::icons::UiIcons,
	/// Layout of `FrameTargets::water_bg` (water frame UBO + depth copy
	/// + scene color + linear sampler): the water pipelines' group 0.
	water_frame_bgl: wgpu::BindGroupLayout,
	/// Layout of `FrameTargets::blit_bg` (scene color + linear sampler).
	blit_bgl: wgpu::BindGroupLayout,
	/// Layout of `FrameTargets::depth_blit_bg` (the live depth texture).
	depth_blit_bgl: wgpu::BindGroupLayout,
	linear_sampler: wgpu::Sampler,
	/// Composites the offscreen scene into the post target at the start of
	/// the water pass. Has a depth-stencil state, so it may ONLY run in a
	/// pass that has a depth attachment — see `upscale_pipeline`.
	blit_pipeline: wgpu::RenderPipeline,
	/// The same shader, depthless, for the standalone upscale pass.
	upscale_pipeline: wgpu::RenderPipeline,
	/// Resolves the live depth into the R32Float copy between the scene
	/// and water passes (WebGPU forbids copying into a depth texture).
	depth_blit_pipeline: wgpu::RenderPipeline,
	surface_format: wgpu::TextureFormat,
}

/// Renderer-owned per-frame render targets. The platform hands over only
/// the swapchain view; depth and the offscreen scene color live here so
/// the water pass can sample both.
/// The two resolutions the world renders at, as one value so they cannot
/// drift apart.
///
/// `render` is a fraction of the surface and `gi` is a whole-integer
/// fraction of THAT. Anchoring the GI to the surface instead would give a
/// fractional ratio the moment render scale left native, and `chunk.wgsl`'s
/// 4-tap bilateral gather walks a footprint that assumes an exact integer
/// ratio — the upsample would read texels that no longer line up with the
/// pixels it is weighting. `gi_ratio` is the number the shader is handed.
#[derive(Clone, Copy, PartialEq)]
pub struct RenderScale {
	pub render: f32,
	pub gi_div: u32,
}

impl RenderScale {
	/// World render size for a surface size. At least one pixel — a zero-sized
	/// attachment is a validation error, and a minimized window can ask.
	fn render_size(self, surface: (u32, u32)) -> (u32, u32) {
		let s = self.render.clamp(0.1, 1.0);
		(
			((surface.0 as f32 * s).round() as u32).max(1),
			((surface.1 as f32 * s).round() as u32).max(1),
		)
	}

	/// GI resolve size: the render size divided by `gi_div`, rounded UP so
	/// the GI buffer always covers every render pixel. Rounding down would
	/// leave the last row/column of a non-divisible render size reading
	/// past the edge of the GI texture.
	fn gi_size(self, surface: (u32, u32)) -> (u32, u32) {
		let r = self.render_size(surface);
		let d = self.gi_div.max(1);
		(r.0.div_ceil(d).max(1), r.1.div_ceil(d).max(1))
	}
}

struct FrameTargets {
	size: (u32, u32),
	/// The scale the targets were built at — part of the rebuild key, so
	/// moving either slider re-creates them.
	scale: RenderScale,
	/// Scene-pass depth attachment; resolved into `depth_copy_view` by
	/// the depth-blit pass before water. (Views keep textures alive.)
	depth_view: wgpu::TextureView,
	/// Pre-water solid depth as R32Float, sampleable (absorption / soft
	/// shores / SSR) — a color format because WebGPU forbids copying
	/// into a depth texture, so it's filled by a fullscreen draw.
	depth_copy_view: wgpu::TextureView,
	/// Exact face-normal codes, written by the depth prepass alongside
	/// depth and read by the GI resolve. Full res: the resolve is half
	/// res and samples this at the matching full-res texel, the same way
	/// it samples the depth copy.
	normal_view: wgpu::TextureView,
	/// Offscreen scene color view: solids + sky render here; water
	/// samples it (refraction/SSR) while blending into `post_view`.
	scene_view: wgpu::TextureView,
	/// Where the world is FINISHED, at render resolution: water, the
	/// viewmodel and the GI debug view draw here over a blit of
	/// `scene_view`, and the upscale pass takes this to the surface.
	/// Separate from `scene_view` because water samples what it blends
	/// over — see the comment at its creation.
	post_view: wgpu::TextureView,
	/// Water group 0: the water frame UBO + scene targets.
	water_bg: wgpu::BindGroup,
	/// Blit group 0: scene color + linear sampler — the water pass's
	/// opening composite of `scene_view` into `post_view`.
	blit_bg: wgpu::BindGroup,
	/// Upscale group 0: `post_view` + linear sampler.
	upscale_bg: wgpu::BindGroup,
	/// Depth-blit group 0: the live depth texture.
	depth_blit_bg: wgpu::BindGroup,
	/// GI lighting pairs, half res: indirect rgb + view distance, and sun
	/// visibility + normal code + water path + history length. Format is
	/// `gi::GI_ACC_FORMAT` because both hold a RUNNING AVERAGE — rgba8 and
	/// then rgba16f each failed the same way, their mantissa too short to
	/// resolve an update of one part in the longest history the accumulator
	/// reaches, which froze every pixel where it stood.
	///
	/// The roles are FIXED, not ping-ponged, because the a-trous filter
	/// gives the frame a natural cycle:
	///
	///   [0] <- resolve            [0] -> atrous(stride 1) -> [1]
	///   [1] -> atrous(stride 2) -> [0] -> the scene pass
	///
	/// so [1] holds the once-filtered image at the end of every frame, and
	/// that is exactly what the next frame wants as history (SVGF feeds the
	/// first iteration's output back, which converges faster than feeding
	/// back the raw accumulation). Two pairs cover it with no copy and no
	/// alternating index.
	gi_indirect_view: [wgpu::TextureView; 2],
	gi_sun_view: [wgpu::TextureView; 2],
	/// Scene group 2: pair [0], the one chunk.wgsl textureLoad's.
	gi_scene_bg: wgpu::BindGroup,
	/// Resolve group 0: uniform + depth/normal taps + pair [1] as history
	/// (the GI world data rides group 2 via `GiDraw::gather_bind`).
	gi_resolve_bg: wgpu::BindGroup,
	/// One per a-trous iteration, each bound to the pair it reads.
	gi_atrous_bg: [wgpu::BindGroup; 2],
}

impl Renderer {
	pub fn new(gpu: &Gpu, surface_format: wgpu::TextureFormat, config: &RenderConfig) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("chunk"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::CHUNK.into()),
		});

		let frame_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("frame"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
				ty: wgpu::BindingType::Buffer {
					ty: wgpu::BufferBindingType::Uniform,
					has_dynamic_offset: false,
					min_binding_size: None,
				},
				count: None,
			}],
		});

		// Group 1: block materials — texture array + sampler, one static
		// bind group shared by the solid and water pipelines.
		let materials_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("materials"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2Array,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count: None,
				},
			],
		});

		// Water group 0: the frame UBO (water copy) plus the scene
		// targets — the pre-water solid depth copy (textureLoad, no
		// sampler), the offscreen scene color, and a linear sampler
		// (refraction/SSR offsets). Folding the scene targets into
		// group 0 keeps the water pipelines within the default
		// bind-group limit while freeing group 2 for the GI world data.
		let water_frame_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("water frame + scene targets"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						// R32Float depth resolve — textureLoad only.
						sample_type: wgpu::TextureSampleType::Float { filterable: false },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 3,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count: None,
				},
			],
		});

		// Blit group 0: scene color + linear sampler (the shader fetches
		// with textureLoad; the sampler entry is there for a future
		// filtered composite).
		let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("blit"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count: None,
				},
			],
		});
		let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("scene linear"),
			address_mode_u: wgpu::AddressMode::ClampToEdge,
			address_mode_v: wgpu::AddressMode::ClampToEdge,
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			..Default::default()
		});

		// Textures + the GI system come before the pipelines: the solid
		// layout references the GI scene bind-group layout, and GiDraw's
		// material LUT needs the per-layer means the atlas upload computes.
		let textures = BlockTextures::new(gpu);
		let gi = crate::gi::GiDraw::new(
			gpu,
			config.draw_dist as u32,
			&textures.layer_means,
			surface_format,
			config.debug_gi_probes,
			config.stub,
		);

		// Shared group scheme: 0 = frame, 1 = materials, 2 = GI. For the
		// solids that's the two resolve outputs; water (and players)
		// carry the raw GI world data instead (`GiDraw::gather_bgl`) and
		// gather per fragment — their surfaces aren't in the depth
		// prepass the resolve maps. Chunk origins are NOT a bind group —
		// they arrive as the instance-step vertex stream at slot 1 (see
		// `origin_vertex_layout`). Water swaps group 0 for its
		// frame+scene combo layout.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("chunk"),
			bind_group_layouts: &[Some(&frame_bgl), Some(&materials_bgl), Some(gi.scene_bgl())],
			immediate_size: 0,
		});
		let water_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("water"),
			bind_group_layouts: &[
				Some(&water_frame_bgl),
				Some(&materials_bgl),
				Some(gi.gather_bgl()),
			],
			immediate_size: 0,
		});

		let chunk_vertex_layouts = [geometry_vertex_layout(), origin_vertex_layout()];

		let solid_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "solid",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &chunk_vertex_layouts,
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				// The depth prepass wrote these exact values (vs_depth is
				// the same transform on the same stream).
				depth_compare: wgpu::CompareFunction::LessEqual,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		// Depth prepass: same vertex stream, and since M3 a fragment
		// stage that rasterizes ONE face-normal code per pixel into an
		// R8Uint target. That costs the double-rate Z a position-only
		// pass would have had, and buys two things the GI could not get
		// any other way: an exact normal for the resolve (its
		// derivative-derived one was garbage on every silhouette) and a
		// code bit-identical to the one chunk.wgsl compares against, so
		// the bilateral upsample stops collapsing at edges.
		let prepass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("depth prepass"),
			bind_group_layouts: &[Some(&frame_bgl)],
			immediate_size: 0,
		});
		let prepass_pipeline = crate::pipeline_builder::build(
			device,
			Some(NORMAL_FORMAT),
			&crate::pipeline_builder::PipelineDesc {
				label: "depth prepass",
				layout: &prepass_layout,
				module: &shader,
				vs_entry: "vs_depth",
				fs_entry: Some("fs_depth"),
				vertex_buffers: &chunk_vertex_layouts,
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		let water = WaterDraw::new(gpu, surface_format, &water_layout);
		let sky = SkyDraw::new(gpu, surface_format, &frame_bgl);

		// Fullscreen composite of the offscreen scene into the swapchain:
		// first draw of the water pass. Depth Always/no-write — the pass
		// keeps the scene depth for water to test against.
		let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("blit"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::BLIT.into()),
		});
		let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("blit"),
			bind_group_layouts: &[Some(&blit_bgl)],
			immediate_size: 0,
		});
		let blit_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "blit",
				layout: &blit_layout,
				module: &blit_shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		// The upscale: the same shader and layout as `blit_pipeline`, but
		// DEPTHLESS. `build` gives a pipeline a depth-stencil state, and a
		// pipeline that has one may only run in a pass that has a depth
		// attachment — which is why `blit_pipeline` works inside the water
		// pass and silently invalidates the encoder in a standalone
		// fullscreen pass. The failure mode is worth remembering: an
		// invalid pass poisons the whole encoder, so `finish()` yields a
		// command buffer that drops EVERY pass, and the window goes black
		// with no HUD rather than showing a wrong-looking blit.
		let upscale_pipeline = crate::pipeline_builder::build_depthless(
			device,
			surface_format,
			&crate::pipeline_builder::PipelineDesc {
				label: "upscale",
				layout: &blit_layout,
				module: &blit_shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				// Ignored — the pass has no depth attachment.
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		// Depth resolve: one fullscreen draw filling the R32Float depth
		// copy from the live attachment (WebGPU forbids
		// copy_texture_to_texture into a depth32float destination).
		let depth_blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("depth blit"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::FRAGMENT,
				ty: wgpu::BindingType::Texture {
					sample_type: wgpu::TextureSampleType::Depth,
					view_dimension: wgpu::TextureViewDimension::D2,
					multisampled: false,
				},
				count: None,
			}],
		});
		let depth_blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("depth blit"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::DEPTH_BLIT.into()),
		});
		let depth_blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("depth blit"),
			bind_group_layouts: &[Some(&depth_blit_bgl)],
			immediate_size: 0,
		});
		let depth_blit_pipeline = crate::pipeline_builder::build_depthless(
			device,
			wgpu::TextureFormat::R32Float,
			&crate::pipeline_builder::PipelineDesc {
				label: "depth blit",
				layout: &depth_blit_layout,
				module: &depth_blit_shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				// Ignored — the pass has no depth attachment.
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		let frame_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("frame ubo"),
			size: std::mem::size_of::<FrameUniforms>() as u64,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let frame_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("frame"),
			layout: &frame_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: frame_buf.as_entire_binding(),
			}],
		});

		let chunk_slots = config
			.chunk_slots_override
			.map(|n| n.max(1))
			.unwrap_or_else(|| chunk_slots(config.draw_dist));
		let origin_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(chunk_slots) * ORIGIN_STRIDE,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});

		// Consumed here: the bind group holds the texture/sampler refs.
		let materials_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("materials"),
			layout: &materials_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&textures.array_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&textures.sampler),
				},
			],
		});

		let index_buf = buffer_init(
			gpu,
			"shared quad indices",
			wgpu::BufferUsages::INDEX,
			bytemuck::cast_slice(&quad_indices(MAX_QUADS)),
		);

		Self {
			solid_pipeline,
			prepass_pipeline,
			water,
			sky,
			frame_buf,
			frame_bg,
			materials_bg,
			origin_buf,
			origins: Vec::new(),
			index_buf,
			chunks: Vec::new(),
			by_pos: FxHashMap::default(),
			pool: Vec::new(),
			pool_bytes: 0,
			lists: CullLists::default(),
			free_slots: Vec::new(),
			next_slot: 0,
			chunk_slots,
			gpu_buffer_bytes: 0,
			gpu_timer: GpuTimer::new(gpu),
			debug_gi: config.debug_gi,
			stub: config.stub,
			overlay: Overlay::new(gpu, surface_format),
			spray: crate::spray::Spray::new(
				gpu,
				surface_format,
				&frame_bgl,
				gi.cam_light_buf(),
				config.spray_droplets,
			),
			confetti: crate::confetti::Confetti::new(
				gpu,
				surface_format,
				&frame_bgl,
				gi.cam_light_buf(),
			),
			siphon: crate::siphon::Siphon::new(gpu, surface_format, &frame_bgl),
			players: crate::players::RemotePlayersDraw::new(
				gpu,
				surface_format,
				&frame_bgl,
				&materials_bgl,
				gi.gather_bgl(),
			),
			highlight: crate::highlight::HighlightDraw::new(gpu, surface_format, &water_frame_bgl),
			viewmodel: crate::viewmodel::ViewmodelDraw::new(
				gpu,
				surface_format,
				&materials_bgl,
				gi.cam_light_buf(),
			),
			gi,
			ui_icons: crate::ui::icons::UiIcons::new(gpu, surface_format, &materials_bgl),
			targets: None,
			water_frame_bgl,
			blit_bgl,
			depth_blit_bgl,
			linear_sampler,
			blit_pipeline,
			upscale_pipeline,
			depth_blit_pipeline,
			surface_format,
		}
	}

	/// Latest GPU pass reading (a few frames latent); None when timestamps
	/// are unsupported or nothing has been read back yet.
	pub fn gpu_pass_ms(&self) -> Option<digblock_core::metrics::GpuPassMs> {
		self.gpu_timer.as_ref().and_then(|t| t.latest)
	}

	/// Replace `origin_buf` with a larger one and repopulate it from the
	/// CPU mirror in a single write of exactly the allocated slots — free
	/// slots keep whatever they held, since no draw ever names them. Every
	/// pass binds the origin stream fresh each frame.
	fn grow_chunk_buf(&mut self, gpu: &Gpu, needed_slots: u32) {
		let old = self.chunk_slots;
		let new_slots = grown_slots(old, needed_slots);
		let origin_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(new_slots) * ORIGIN_STRIDE,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		if !self.origins.is_empty() {
			gpu.queue
				.write_buffer(&origin_buf, 0, bytemuck::cast_slice(&self.origins));
		}
		self.origin_buf = origin_buf;
		self.chunk_slots = new_slots;
		log::info!(
			"chunk origin buffer grown: {old} -> {new_slots} slots ({} KiB), {} slots live",
			u64::from(new_slots) * ORIGIN_STRIDE / 1024,
			self.origins.len()
		);
	}

	/// Claim an origin slot for a newly resident chunk and publish its
	/// origin. Growth fires while there is still headroom rather than at
	/// overflow: the reallocate-and-repopulate lands in one frame, and the
	/// path that reaches it — a runtime draw-distance increase — is already
	/// the frame doing the most other work.
	fn alloc_slot(&mut self, gpu: &Gpu, pos: ChunkPos) -> u32 {
		let slot = match self.free_slots.pop() {
			Some(s) => s,
			None => {
				let s = self.next_slot;
				self.next_slot += 1;
				self.origins.push([0.0; 4]);
				if u64::from(self.next_slot) * 20 >= u64::from(self.chunk_slots) * 17 {
					self.grow_chunk_buf(gpu, self.next_slot + 1);
				}
				s
			}
		};
		debug_assert!(slot < self.chunk_slots, "origin slot out of range");
		let (ox, oy, oz) = pos.origin();
		let i = slot as usize;
		self.origins[i] = [ox, oy, oz, 0.0];
		gpu.queue.write_buffer(
			&self.origin_buf,
			u64::from(slot) * ORIGIN_STRIDE,
			bytemuck::cast_slice(&self.origins[i..=i]),
		);
		slot
	}

	/// Take a buffer of at least `bytes` from the free list, or make one.
	fn acquire(&mut self, gpu: &Gpu, bytes: u64) -> ChunkMesh {
		let cap = size_class(bytes);
		let class = (cap / SIZE_CLASS - 1) as usize;
		if let Some(buf) = self.pool.get_mut(class).and_then(Vec::pop) {
			self.pool_bytes -= cap;
			return ChunkMesh {
				buf,
				cap,
				index_count: 0,
			};
		}
		let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk verts"),
			size: cap,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		self.gpu_buffer_bytes += cap;
		ChunkMesh {
			buf,
			cap,
			index_count: 0,
		}
	}

	/// Park a retired buffer for reuse, or free it once the pool is at its
	/// ceiling. Safe to reuse immediately: the next write goes through the
	/// queue, which orders it behind any in-flight frame still reading it.
	fn recycle(&mut self, mesh: ChunkMesh) {
		if self.pool_bytes + mesh.cap > POOL_BYTES_MAX {
			self.gpu_buffer_bytes = self.gpu_buffer_bytes.saturating_sub(mesh.cap);
			return;
		}
		let class = (mesh.cap / SIZE_CLASS - 1) as usize;
		if self.pool.len() <= class {
			self.pool.resize_with(class + 1, Vec::new);
		}
		self.pool_bytes += mesh.cap;
		self.pool[class].push(mesh.buf);
	}

	/// Write `verts` over `prev`'s buffer when they fit its size class —
	/// the common case for a remesh of the same chunk, and the whole point
	/// of the classes. Anything else takes a pooled buffer of the right
	/// class and retires the old one. Empty verts release it outright.
	fn store_mesh(
		&mut self,
		gpu: &Gpu,
		prev: Option<ChunkMesh>,
		verts: &[u32],
	) -> Option<ChunkMesh> {
		// All vertices are two u32 words (see mesher::pack). The shared
		// index buffer covers MAX_QUADS; a mesh past it would make
		// draw_indexed over-read (a wgpu validation error). Truncate
		// loudly — visible dropped faces beat a device loss, and the cap
		// is sized to be unreachable.
		let mut verts = verts;
		let quads = (verts.len() / (4 * 2)).min(MAX_QUADS);
		if verts.len() / (4 * 2) > MAX_QUADS {
			log::error!(
				"chunk mesh exceeds MAX_QUADS ({} > {MAX_QUADS}); truncating",
				verts.len() / (4 * 2)
			);
			verts = &verts[..MAX_QUADS * 8];
		}
		if verts.is_empty() {
			if let Some(old) = prev {
				self.recycle(old);
			}
			return None;
		}
		let bytes = std::mem::size_of_val(verts) as u64;
		let mut mesh = match prev {
			Some(m) if m.cap >= bytes => m,
			other => {
				if let Some(old) = other {
					self.recycle(old);
				}
				self.acquire(gpu, bytes)
			}
		};
		gpu.queue
			.write_buffer(&mesh.buf, 0, bytemuck::cast_slice(verts));
		mesh.index_count = quads as u32 * 6;
		Some(mesh)
	}

	/// Upload (or replace) a chunk's mesh. Empty meshes remove the entry —
	/// unlike the Java, a re-mesh to zero faces cannot leave a stale VBO.
	pub fn upload_chunk(&mut self, gpu: &Gpu, pos: ChunkPos, mesh: &MeshData) {
		if mesh.is_empty() {
			self.remove_chunk(pos);
			return;
		}
		self.spray.set_chunk(pos, pos.origin(), &mesh.impacts);
		// The vein faces this mesh exposed, on the same rail and for the
		// same reason: the mesher is the one pass that knows which of them
		// this player can see (`mesher::lights`). Unconditionally, empty
		// list included — a completed extraction produces exactly that,
		// and "no faces here any more" has to reach the light list.
		self.gi.set_chunk_lights(pos, &mesh.lights);
		// A remesh keeps its entry, its slot (so the origin needs no
		// rewrite) and, whenever the new mesh fits, its buffers.
		let idx = match self.by_pos.get(&pos) {
			Some(&i) => i as usize,
			None => {
				let slot = self.alloc_slot(gpu, pos);
				let idx = self.chunks.len();
				self.chunks.push(ChunkGpu {
					pos,
					solid: None,
					water: None,
					slot,
					center: chunk_center(pos),
				});
				self.by_pos.insert(pos, idx as u32);
				idx
			}
		};
		let prev = self.chunks[idx].solid.take();
		self.chunks[idx].solid = self.store_mesh(gpu, prev, &mesh.solid);
		let prev = self.chunks[idx].water.take();
		self.chunks[idx].water = self.store_mesh(gpu, prev, &mesh.water);
	}

	/// RTGI: forward one classified chunk into the voxel window.
	pub fn gi_upload(&mut self, gpu: &Gpu, pos: ChunkPos, gi: &digblock_core::gi::GiChunk) {
		self.gi.upload_chunk(gpu, pos, gi);
	}

	/// RTGI: queue probe invalidation around an edited cell.
	pub fn gi_invalidate(&mut self, pos: [f32; 3]) {
		self.gi.invalidate(pos);
	}

	/// The same, for a light a player is CARRYING that has moved or gone
	/// out. Identical to the probes; the two are told apart only so a
	/// walking carrier's stream cannot evict a world edit from the
	/// resolve's four screen-cap slots (`GiDraw::resolve_inval`).
	pub fn gi_invalidate_moving(&mut self, pos: [f32; 3]) {
		self.gi.invalidate_moving(pos);
	}

	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		self.spray.remove_chunk(pos);
		// Before the early return: GI residency is its own set, and an
		// un-meshed chunk still owns an emitter entry.
		self.gi.remove_chunk(pos);
		let Some(idx) = self.by_pos.remove(&pos) else {
			return;
		};
		let old = self.chunks.swap_remove(idx as usize);
		// swap_remove filled the hole with the last entry (unless the hole
		// WAS the last) — retarget its index.
		if let Some(moved) = self.chunks.get(idx as usize) {
			self.by_pos.insert(moved.pos, idx);
		}
		self.free_slots.push(old.slot);
		for mesh in [old.solid, old.water].into_iter().flatten() {
			self.recycle(mesh);
		}
	}

	/// Drop every uploaded chunk mesh (world swap: the multiplayer
	/// handshake replaces the locally-seeded world with the server's).
	pub fn clear_chunks(&mut self) {
		for chunk in std::mem::take(&mut self.chunks) {
			self.spray.remove_chunk(chunk.pos);
			self.free_slots.push(chunk.slot);
			for mesh in [chunk.solid, chunk.water].into_iter().flatten() {
				self.recycle(mesh);
			}
		}
		self.by_pos.clear();
		// A world swap used to carry the OLD world's emitters and coarse
		// occupancy forward into the new one. Here — unlike a single
		// eviction — wiping the mirror is safe and correct, because every
		// slot is about to be rewritten by the incoming world anyway.
		self.gi.clear_derived_state();
	}

	/// One linear pass over the resident set producing every list this
	/// frame draws from. `near` is None when the cached near map stands,
	/// which also skips its plane test. Returns the count culled against
	/// the camera frustum.
	fn cull(
		chunks: &[ChunkGpu],
		lists: &mut CullLists,
		cam: &[glam::Vec4; 4],
		near: Option<&[glam::Vec4; 4]>,
		cam_chunk: (i32, i32),
	) -> u32 {
		lists.water.clear();
		lists.near.clear();
		lists.keyed.clear();
		lists.counts = [0; DIST_BUCKETS];
		let mut culled = 0;
		for (i, c) in chunks.iter().enumerate() {
			let i = i as u32;
			let solid = c.solid.is_some();
			if let Some(planes) = near
				&& solid && sphere_in_frustum(c.center, planes)
			{
				lists.near.push(i);
			}
			if !sphere_in_frustum(c.center, cam) {
				culled += 1;
				continue;
			}
			if solid {
				let dx = i64::from(c.pos.x - cam_chunk.0);
				let dz = i64::from(c.pos.z - cam_chunk.1);
				let b = ((dx * dx + dz * dz) as f64).sqrt() as usize;
				let b = b.min(DIST_BUCKETS - 1);
				lists.counts[b] += 1;
				lists.keyed.push((b as u8, i));
			}
			if c.water.is_some() {
				lists.water.push(i);
			}
		}
		let CullLists {
			scene,
			keyed,
			counts,
			..
		} = lists;
		scatter_by_bucket(keyed, counts, scene);
		culled
	}

	/// (Re)build the size-dependent frame targets when the size changes.
	/// Covers first frame, window resize, resume-with-new-size, and the
	/// device-loss rebuild (which resets `targets` to None).
	fn ensure_targets(&mut self, gpu: &Gpu, size: (u32, u32), scale: RenderScale) {
		if self.targets.as_ref().map(|t| (t.size, t.scale)) == Some((size, scale)) {
			return;
		}
		// `size` stays the SURFACE size — it is the rebuild key, and it is
		// what the overlay and every UI hit-test use. Only the world's
		// targets shrink.
		let render = scale.render_size(size);
		let extent = wgpu::Extent3d {
			width: render.0,
			height: render.1,
			depth_or_array_layers: 1,
		};
		let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("frame depth"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: DEPTH_FORMAT,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
		let depth_copy = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("solid depth copy (r32f)"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: wgpu::TextureFormat::R32Float,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let depth_copy_view = depth_copy.create_view(&wgpu::TextureViewDescriptor::default());
		let normal_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("solid face-normal codes (r8uint)"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: NORMAL_FORMAT,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let normal_view = normal_tex.create_view(&wgpu::TextureViewDescriptor::default());
		let scene = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("scene color"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: self.surface_format,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let scene_view = scene.create_view(&wgpu::TextureViewDescriptor::default());
		// The post target: water, the viewmodel and the GI debug view draw
		// HERE while sampling `scene` for refraction and SSR.
		//
		// Two textures rather than one, and this is not optional. Water
		// reads the scene colour it is blending over, so a pass that both
		// writes and samples one texture is a read-write hazard on the same
		// resource — and an invalid pass poisons the whole encoder, so
		// `finish()` yields a command buffer that drops EVERY pass in the
		// frame. The symptom is a completely black window with no HUD, not
		// a wrong-looking water surface.
		let post = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("post color"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: self.surface_format,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let post_view = post.create_view(&wgpu::TextureViewDescriptor::default());
		// Both group-0 combos that depend on the resize-recreated
		// textures are (re)built here: the water frame+scene group and
		// the blit group.
		let water_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("water frame + scene targets"),
			layout: &self.water_frame_bgl,
			entries: &[
				// The SAME frame UBO the solids read. Water used to get a
				// second copy of it for one field — a constant alpha — and that
				// field is gone (see water.wgsl's `alpha`), so the copy and
				// its per-frame upload went with it. The water group still
				// needs its own bind GROUP, because it also carries the
				// resize-recreated scene targets.
				wgpu::BindGroupEntry {
					binding: 0,
					resource: self.frame_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&depth_copy_view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(&scene_view),
				},
				wgpu::BindGroupEntry {
					binding: 3,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		let blit_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("blit"),
			layout: &self.blit_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&scene_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		// What the UPSCALE samples: the post target, i.e. the finished
		// world at render resolution.
		let upscale_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("upscale"),
			layout: &self.blit_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&post_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		let depth_blit_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("depth blit"),
			layout: &self.depth_blit_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: wgpu::BindingResource::TextureView(&depth_view),
			}],
		});
		// The GI resolve targets at a whole-integer fraction of the RENDER
		// size — the chunk shader's bilateral upsample bridges the gap,
		// and its 4-tap gather is why the ratio must divide exactly.
		let gi = scale.gi_size(size);
		let gi_tex = |format, label| {
			gpu.device.create_texture(&wgpu::TextureDescriptor {
				label: Some(label),
				size: wgpu::Extent3d {
					width: gi.0,
					height: gi.1,
					depth_or_array_layers: 1,
				},
				mip_level_count: 1,
				sample_count: 1,
				dimension: wgpu::TextureDimension::D2,
				format,
				usage: wgpu::TextureUsages::RENDER_ATTACHMENT
					| wgpu::TextureUsages::TEXTURE_BINDING,
				view_formats: &[],
			})
		};
		// One constant with the resolve pipeline's own targets — a skew
		// here is a validation error, and the reason it is f32 is in
		// gi::GI_ACC_FORMAT.
		let acc = crate::gi::GI_ACC_FORMAT;
		let gi_indirect_view = [
			gi_tex(acc, "gi indirect 0").create_view(&Default::default()),
			gi_tex(acc, "gi indirect 1").create_view(&Default::default()),
		];
		let gi_sun_view = [
			gi_tex(acc, "gi sun 0").create_view(&Default::default()),
			gi_tex(acc, "gi sun 1").create_view(&Default::default()),
		];
		let scene_bg = |i: usize| {
			gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("gi scene"),
				layout: self.gi.scene_bgl(),
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: wgpu::BindingResource::TextureView(&gi_indirect_view[i]),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&gi_sun_view[i]),
					},
				],
			})
		};
		// The scene reads [0]; the resolve reads [1], which is where last
		// frame's first a-trous iteration left the filtered image.
		let gi_scene_bg = scene_bg(0);
		let gi_resolve_bg = self.gi.make_resolve_bind(
			gpu,
			&depth_copy_view,
			&normal_view,
			&gi_indirect_view[1],
			&gi_sun_view[1],
		);
		let gi_atrous_bg = [
			self.gi
				.make_atrous_bind(gpu, 0, &gi_indirect_view[0], &gi_sun_view[0]),
			self.gi
				.make_atrous_bind(gpu, 1, &gi_indirect_view[1], &gi_sun_view[1]),
		];
		// Fresh textures hold nothing: last frame's screen history went
		// with them.
		self.gi.reset_history();
		self.targets = Some(FrameTargets {
			size,
			scale,
			depth_view,
			depth_copy_view,
			normal_view,
			scene_view,
			post_view,
			water_bg,
			blit_bg,
			upscale_bg,
			depth_blit_bg,
			gi_indirect_view,
			gi_sun_view,
			gi_scene_bg,
			gi_resolve_bg,
			gi_atrous_bg,
		});
	}

	/// The GI voxel window's radius in chunks, as this renderer built it.
	/// F7 drill: drop the GI's ratcheting derived state — see
	/// `GiDraw::clear_derived_state`.
	pub fn gi_clear_derived_state(&mut self) {
		self.gi.clear_derived_state();
	}

	/// `sky_y` and the emitter count, for the F7 drill's log line.
	pub fn gi_debug_state(&self) -> (f32, u32) {
		self.gi.debug_state()
	}

	pub fn gi_window_chunks(&self) -> i32 {
		self.gi.window_chunks()
	}

	/// Encode the frame in one command encoder, fixed order — probes →
	/// depth prepass → depth blit → GI resolve → scene → water →
	/// viewmodel → overlay (GI-DESIGN.md).
	pub fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		scene: &FrameScene<'_>,
	) -> RenderStats {
		let FrameScene {
			uniforms: ref frame,
			view_proj,
			size,
			scale,
			ui_scale,
			players,
			held,
			carried,
			overlay,
			confetti,
			siphon,
			feed,
			target: highlight_target,
			viewmodel,
			ui,
			chat,
			nametags,
		} = *scene;
		self.ensure_targets(gpu, size, scale);
		// Remote players re-upload every frame (a handful of instances —
		// cheaper than the change detection it replaces).
		self.players.set_instances(gpu, players);
		self.players.set_held(gpu, held);
		self.highlight.set_target(gpu, highlight_target);
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		self.viewmodel.prepare(gpu, aspect, viewmodel.as_ref());
		let mut stats = RenderStats {
			chunks_resident: self.chunks.len() as u32,
			..Default::default()
		};
		// GI parameters into the frame uniforms (quantized caster, cascade
		// dims, window side) — before the UBO uploads below.
		let mut frame = *frame;
		self.gi.patch_frame(&mut frame);
		let frame = &frame;
		if let Some(t) = self.gpu_timer.as_mut() {
			t.begin_frame();
		}
		self.overlay.prepare(
			gpu,
			size,
			ui_scale,
			ui,
			overlay,
			feed,
			chat.as_ref(),
			nametags,
		);
		self.ui_icons.prepare(gpu, size, ui);
		gpu.queue
			.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(frame));
		// One cull for the whole frame: scene and water lists out of a
		// single walk of the flat resident set.
		let cam_planes = side_planes(view_proj);
		let cam_chunk = (
			(frame.cam_pos[0] / 16.0).floor() as i32,
			(frame.cam_pos[2] / 16.0).floor() as i32,
		);
		stats.chunks_culled =
			Self::cull(&self.chunks, &mut self.lists, &cam_planes, None, cam_chunk);
		let spray_n = self.spray.prepare(
			gpu,
			glam::Vec3::new(frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]),
		);
		let confetti_n = self.confetti.prepare(gpu, confetti);
		let siphon_n = self.siphon.prepare(gpu, siphon.as_ref());

		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor {
				label: Some("frame"),
			});

		// RTGI probe update first: compute at the head of the encoder, so
		// every later pass that reads the atlas sees this frame's write.
		self.gi
			.encode(gpu, &mut encoder, self.gpu_timer.as_ref(), frame, carried);
		// AFTER the encode, not before it: `carried` is folded into the
		// light list inside `flush_lights`, so reading these earlier
		// reported the PREVIOUS frame's emitters — and zero on frame 0,
		// which is every `--frames 1` capture and exactly the run the
		// count exists to answer.
		(stats.sky_y, stats.emitters) = self.gi.debug_state();

		let draw = ChunkDraw {
			chunks: &self.chunks,
			origin_buf: &self.origin_buf,
			index_buf: &self.index_buf,
		};

		{
			let targets = self.targets.as_ref().expect("ensured above");
			// Depth prepass: opaque solids only, no shading — the depth
			// the GI resolve reconstructs world positions from.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("depth prepass"),
					// Cleared to a sentinel code no real face carries,
					// so a texel the prepass never wrote can never
					// match a surface in the upsample's equality test.
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.normal_view,
						resolve_target: None,
						ops: wgpu::Operations {
							load: wgpu::LoadOp::Clear(wgpu::Color {
								r: 255.0,
								g: 0.0,
								b: 0.0,
								a: 0.0,
							}),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Clear(1.0),
							store: wgpu::StoreOp::Store,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_PREPASS)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				pass.set_pipeline(&self.prepass_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				draw.bind(&mut pass);
				// NORASTER empties the depth buffer as well as the pass, so
				// the resolve downstream takes the sky early-out everywhere
				// — an upper bound on geometry, not a prepass measurement.
				if self.stub & stub::NORASTER == 0 {
					for &i in &self.lists.scene {
						let chunk = &draw.chunks[i as usize];
						let Some(mesh) = chunk.solid.as_ref() else {
							continue;
						};
						stats.triangles += draw_chunk(&mut pass, mesh, chunk.slot) / 3;
					}
				}
			}

			// Solid depth becomes sampleable R32Float — moved BEFORE the
			// scene pass, because the GI resolve consumes it; water and
			// the highlight decal read the same copy later. A fullscreen
			// draw, not copy_texture_to_texture — WebGPU forbids
			// depth32float as a copy destination.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("depth blit"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.depth_copy_view,
						resolve_target: None,
						ops: wgpu::Operations {
							// The blit overwrites every pixel.
							load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: None,
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_DEPTH_BLIT)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				pass.set_pipeline(&self.depth_blit_pipeline);
				pass.set_bind_group(0, &targets.depth_blit_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;
			}

			// GI resolve: probes + the per-pixel sun ray → the two
			// half-res lighting inputs the scene pass upsamples.
			self.gi.encode_resolve(
				gpu,
				&mut encoder,
				self.gpu_timer.as_ref(),
				frame,
				crate::gi::ResolveTargets {
					bind: &targets.gi_resolve_bg,
					indirect_view: &targets.gi_indirect_view[0],
					sun_view: &targets.gi_sun_view[0],
				},
			);
			// The spatial half. Two edge-stopped a-trous iterations,
			// [0] -> [1] -> [0], which leaves the scene reading [0] and the
			// once-filtered image parked in [1] as next frame's history.
			// Without this the resolve's only estimator is time, which forces
			// a history so deep it smears under motion — see gi_atrous.wgsl.
			// DIG_STUB=giatrous drops both iterations, which leaves the
			// scene reading the RAW resolve out of [0] and next frame's
			// history stale. That is the honest price of measuring the
			// filter: the number is the filter's cost, not a picture.
			for (i, bind) in targets.gi_atrous_bg.iter().enumerate() {
				if self.stub & stub::GIATROUS != 0 {
					break;
				}
				let out = 1 - i;
				self.gi.encode_atrous(
					&mut encoder,
					self.gpu_timer.as_ref(),
					// One query pair per iteration — see encode_atrous.
					if i == 0 { Q_GI_DENOISE } else { Q_GI_DENOISE2 },
					bind,
					&targets.gi_indirect_view[out],
					&targets.gi_sun_view[out],
				);
				stats.draw_calls += 1;
			}

			// Scene pass: solids + sky into the offscreen color target;
			// depth arrives from the prepass (Load, LessEqual).
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("scene"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.scene_view,
						resolve_target: None,
						ops: wgpu::Operations {
							// Fog color, matching the sky horizon — belt and
							// braces for far-plane precision edges (the sky
							// pass covers every non-terrain pixel anyway).
							load: wgpu::LoadOp::Clear(wgpu::Color {
								r: f64::from(frame.clear_color[0]),
								g: f64::from(frame.clear_color[1]),
								b: f64::from(frame.clear_color[2]),
								a: 1.0,
							}),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Load,
							store: wgpu::StoreOp::Store,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_MAIN)),
					occlusion_query_set: None,
					multiview_mask: None,
				});

				pass.set_pipeline(&self.solid_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, &targets.gi_scene_bg, &[]);
				// Origins for every chunk, bound once — each draw selects
				// its own with first_instance.
				draw.bind(&mut pass);
				// The clean raster stub: the prepass above still ran, so
				// depth and face codes are intact and the GI is untouched.
				if self.stub & (stub::NOSCENE | stub::NORASTER) == 0 {
					for &i in &self.lists.scene {
						let chunk = &draw.chunks[i as usize];
						let Some(mesh) = chunk.solid.as_ref() else {
							continue;
						};
						stats.triangles += draw_chunk(&mut pass, mesh, chunk.slot) / 3;
						stats.chunks_drawn += 1;
					}
				}

				// Remote players: opaque, depth-writing, before the sky so
				// they occlude and fog like terrain (frame_bg is bound at
				// 0). They gather the probe field per fragment, so their
				// group 2 is the GI world data, not the resolve outputs.
				pass.set_bind_group(2, self.gi.gather_bind(), &[]);
				let player_tris = self.players.draw(&mut pass, &self.index_buf);
				if player_tris > 0 {
					stats.draw_calls += self.players.draw_calls();
					stats.triangles += player_tris;
				}

				// Sky after solids (depth test discards occluded fragments —
				// near-free on TBDR); frame_bg is still bound at 0.
				self.sky.draw(&mut pass);
				stats.draw_calls += 1;
			}

			// Water pass, in place on the scene target: transparent water in
			// two subpasses — a depth-only prepass records the nearest water
			// surface, then the color pass (no depth write, double-sided)
			// shades only fragments at that depth, one blended layer per
			// pixel. Depth is discarded at the end: nothing after this pass
			// reads it.
			//
			// This used to target the SWAPCHAIN. It now targets `post_view`
			// at RENDER resolution and one upscale pass at the end takes
			// that to the surface — so the opening blit did not disappear,
			// it just composites scene → post at render res instead of at
			// native. It cannot be dropped: water SAMPLES `scene_view`, so
			// the pass needs a destination that is not its source.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("water"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.post_view,
						resolve_target: None,
						ops: wgpu::Operations {
							// The blit overwrites every pixel; Clear beats
							// Load on TBDR.
							load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Load,
							store: wgpu::StoreOp::Discard,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_WATER)),
					occlusion_query_set: None,
					multiview_mask: None,
				});

				pass.set_pipeline(&self.blit_pipeline);
				pass.set_bind_group(0, &targets.blit_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;

				pass.set_bind_group(0, &targets.water_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, self.gi.gather_bind(), &[]);
				draw.bind(&mut pass);
				// DIG_STUB=nowater keeps the blit — that is what composites
				// the scene into the post target — and drops only the water
				// geometry, which bounds everything the water pass costs.
				if self.stub & stub::NOWATER == 0 {
					let (water_draws, water_tris) =
						self.water.draw(&mut pass, draw.chunks, &self.lists.water);
					stats.water_draws += water_draws;
					stats.triangles += water_tris;
				}

				// Target highlight/crack decal, over the finished water so
				// a box on a source (or a submerged block) survives the
				// water's depth tint; it occludes itself against the solid
				// depth copy in-shader (water_bg is still bound at 0).
				let highlight_tris = self.highlight.draw(&mut pass);
				if highlight_tris > 0 {
					stats.draw_calls += 1;
					stats.triangles += highlight_tris;
				}

				// Spray billboards over the finished water (blends over
				// sheet and pool alike). Water left its frame+scene group
				// bound at 0; spray wants the plain frame group.
				pass.set_bind_group(0, &self.frame_bg, &[]);
				let live = u32::from(self.stub & stub::SPRAY == 0) * spray_n;
				let spray_tris = self.spray.draw(&mut pass, live);
				stats.draw_calls += u32::from(spray_tris > 0);
				stats.triangles += spray_tris;

				// Level-up confetti, over the same finished world as the
				// spray it shares a pass (and a bind group) with.
				let confetti_tris = self.confetti.draw(&mut pass, confetti_n);
				stats.draw_calls += u32::from(confetti_tris > 0);
				stats.triangles += confetti_tris;

				// The essence draw last of the three, because it is the
				// only ADDITIVE one: it has to land on top of whatever the
				// alpha-blended billboards left, not underneath it.
				let siphon_tris = self.siphon.draw(&mut pass, siphon_n);
				stats.draw_calls += u32::from(siphon_tris > 0);
				stats.triangles += siphon_tris;
			}

			// The item in your own hand, over the finished world. Its own
			// pass for one reason: it clears the depth buffer first. The
			// item is a metre from the eye and would otherwise be sliced
			// The voxel-window debug raymarch paints over the finished
			// scene (overlay still lands on top) — the P1 gate capture.
			if self.debug_gi {
				let cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
				self.gi.encode_debug(
					gpu,
					&mut encoder,
					&targets.post_view,
					frame.inv_view_proj,
					cam,
				);
			}
			// open by the wall you walk into — but it still needs depth
			// among its OWN faces, because a staircase in hand is not
			// convex and back-face culling alone can't sort it. The water
			// pass discarded this attachment, so reusing it costs nothing.
			if !self.viewmodel.is_empty() {
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("viewmodel"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.post_view,
						resolve_target: None,
						ops: wgpu::Operations {
							load: wgpu::LoadOp::Load,
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Clear(1.0),
							store: wgpu::StoreOp::Discard,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_VIEWMODEL)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				let tris = self
					.viewmodel
					.draw(&mut pass, &self.index_buf, &self.materials_bg);
				stats.draw_calls += 1;
				stats.triangles += tris;
			}

			// THE UPSCALE. Everything above rendered at RENDER resolution
			// into `scene_view`; this is the one place it becomes the
			// surface. At native render scale it is a straight 1:1 blit —
			// the same fullscreen draw that used to open the water pass —
			// so the default frame gains a pass boundary and nothing else.
			//
			// The overlay deliberately comes AFTER: UI, text and name tags
			// are drawn at native resolution straight onto the swapchain,
			// so nothing the player reads is ever resampled.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("upscale"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: target,
						resolve_target: None,
						ops: wgpu::Operations {
							// Covers every pixel; Clear beats Load on TBDR.
							load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: None,
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_UPSCALE)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				pass.set_pipeline(&self.upscale_pipeline);
				pass.set_bind_group(0, &targets.upscale_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;
			}
		}

		// HUD + debug overlay: own pass over the finished frame —
		// LoadOp::Load, no depth. Always encoded now that the crosshair is
		// part of it; with the debug panel hidden it's a handful of quads.
		{
			let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label: Some("overlay"),
				color_attachments: &[Some(wgpu::RenderPassColorAttachment {
					view: target,
					resolve_target: None,
					ops: wgpu::Operations {
						load: wgpu::LoadOp::Load,
						store: wgpu::StoreOp::Store,
					},
					depth_slice: None,
				})],
				depth_stencil_attachment: None,
				timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_OVERLAY)),
				occlusion_query_set: None,
				multiview_mask: None,
			});
			// Sandwich: backdrops → main icons → content (counts, panel,
			// feed, menu) → ghost icon → topmost ghost quads/text. One
			// pass, one timer bracket; empty stages cost nothing.
			self.overlay.draw_below(&mut pass);
			self.ui_icons.draw_main(&mut pass, &self.materials_bg);
			self.overlay.draw_mid(&mut pass);
			self.ui_icons.draw_ghost(&mut pass, &self.materials_bg);
			self.overlay.draw_above(&mut pass);
		}

		if let Some(t) = self.gpu_timer.as_mut() {
			t.resolve(&mut encoder);
		}
		gpu.queue.submit([encoder.finish()]);
		if let Some(t) = self.gpu_timer.as_mut() {
			t.after_submit(gpu);
		}
		stats.draw_calls += stats.chunks_drawn + stats.water_draws;
		stats
	}
}

/// Everything a pass needs to draw chunks: the resident set plus the two
/// buffers bound once per pass. One borrow instead of three in the pass
/// signatures, which otherwise grow an argument per feature.
pub(crate) struct ChunkDraw<'a> {
	pub chunks: &'a [ChunkGpu],
	pub origin_buf: &'a wgpu::Buffer,
	pub index_buf: &'a wgpu::Buffer,
}

impl ChunkDraw<'_> {
	/// Bind what every chunk draw in a pass shares: the quad index buffer
	/// and the slot-indexed origin stream.
	pub(crate) fn bind(&self, pass: &mut wgpu::RenderPass<'_>) {
		pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
		pass.set_vertex_buffer(1, self.origin_buf.slice(..));
	}
}

/// One chunk draw: bind its vertex stream, then one instance at
/// `first_instance = slot` — which is how the vertex stage picks its
/// origin out of the instance-step origin buffer bound once per pass.
/// Two encoder calls where the dynamic-offset bind group cost three; on
/// wasm each is a JS hop with validation, and the depth prepass, the
/// scene, and both water subpasses each walk thousands of chunks.
/// Returns the index count drawn.
pub(crate) fn draw_chunk(pass: &mut wgpu::RenderPass<'_>, mesh: &ChunkMesh, slot: u32) -> u32 {
	pass.set_vertex_buffer(0, mesh.buf.slice(..));
	pass.draw_indexed(0..mesh.index_count, 0, slot..slot + 1);
	mesh.index_count
}

/// Chunk center in world units — the bounding sphere's origin, cached on
/// `ChunkGpu` because the cull recomputes nothing per frame.
pub(crate) fn chunk_center(pos: ChunkPos) -> glam::Vec3 {
	glam::Vec3::new(
		(pos.x * 16 + 8) as f32,
		(pos.y * 16 + 8) as f32,
		(pos.z * 16 + 8) as f32,
	)
}

/// Chunk bounding-sphere test against extracted side planes. RADIUS is the
/// chunk's half-diagonal (Java sphereInFrustumWithoutNearFar — near/far
/// deliberately ignored). One definition, so the magic constant can't
/// drift between the callers that share it.
pub(crate) fn sphere_in_frustum(center: glam::Vec3, planes: &[glam::Vec4; 4]) -> bool {
	const RADIUS: f32 = 13.86;
	planes
		.iter()
		.all(|p| p.truncate().dot(center) + p.w > -RADIUS)
}

/// Gribb–Hartmann frustum plane extraction: left/right/bottom/top only.
pub(crate) fn side_planes(m: glam::Mat4) -> [glam::Vec4; 4] {
	let r0 = m.row(0);
	let r1 = m.row(1);
	let r3 = m.row(3);
	[r3 + r0, r3 - r0, r3 + r1, r3 - r1].map(|p| {
		let len = p.truncate().length();
		if len > 0.0 { p / len } else { p }
	})
}

#[cfg(test)]
mod tests {
	/// The WGSL `Frame` struct (frame_common.wgsl) and the Rust-side
	/// `FrameUniforms` are hand-mirrored — field order and packing are
	/// load-bearing. This turns that comment into CI enforcement: naga's
	/// Layouter computes the shader-side uniform layout, and a size
	/// mismatch (reordered/added/removed field, packing drift) fails the
	/// test without needing a GPU.
	#[test]
	fn frame_layout_matches_wgsl() {
		let module = naga::front::wgsl::parse_str(digblock_assets::shader::CHUNK)
			.expect("chunk shader parses");
		let mut layouter = naga::proc::Layouter::default();
		layouter.update(module.to_ctx()).expect("layout");
		let frame = module
			.global_variables
			.iter()
			.find(|(_, v)| v.name.as_deref() == Some("frame"))
			.expect("global `frame` exists")
			.1;
		let wgsl_size = layouter[frame.ty].size as usize;
		assert_eq!(
			wgsl_size,
			std::mem::size_of::<super::FrameUniforms>(),
			"frame_common.wgsl Frame layout diverged from renderer.rs FrameUniforms"
		);
	}

	/// Every pipeline this renderer builds, validated with no GPU present.
	///
	/// The shader test in `digblock-assets` parses each WGSL file ALONE, so
	/// it can only see that a shader is well-formed. It cannot see the seam
	/// that actually breaks: a shader's bindings against the
	/// `BindGroupLayout` the Rust side hand-wrote for it, a vertex buffer's
	/// formats against the attributes the vertex stage declares, a
	/// fragment's outputs against the target formats. All three are checked
	/// at `create_render_pipeline` and nowhere earlier, which used to mean
	/// "when somebody runs the game".
	///
	/// wgpu's noop backend stubs the HAL — nothing is compiled and nothing
	/// is allocated — but `wgpu-core` runs its full validation first,
	/// backend-independent. So this constructs the real `Renderer` on a
	/// device that does not exist and gets the real errors, in the time a
	/// unit test takes.
	///
	/// What it does NOT cover: naga is the permissive front end, and the
	/// browser's is Tint. A shader that passes here can still invalidate a
	/// pipeline in Chrome (see the `highlight.wgsl` precedence bug), so
	/// this is not a substitute for loading the page.
	#[cfg(not(target_arch = "wasm32"))]
	#[test]
	fn every_pipeline_builds_without_a_gpu() {
		let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
			backends: wgpu::Backends::NOOP,
			backend_options: wgpu::BackendOptions {
				noop: wgpu::NoopBackendOptions::enabled(),
				..Default::default()
			},
			..wgpu::InstanceDescriptor::new_without_display_handle()
		});
		let adapter =
			pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
				.expect("the noop backend always yields an adapter");
		let (device, queue) =
			pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
				.expect("the noop backend always yields a device");

		// Validation failures are reported out-of-band — `create_*` hands
		// back an error resource rather than panicking — so the scope is
		// what turns them into a failing test instead of a silent pass.
		let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
		let gpu = digblock_platform::Gpu {
			instance,
			adapter,
			device,
			queue,
		};
		let _renderer = super::Renderer::new(
			&gpu,
			// The surface format the desktop swapchain actually picks;
			// fragment outputs are validated against it.
			wgpu::TextureFormat::Bgra8UnormSrgb,
			&super::RenderConfig {
				draw_dist: 8,
				spray_droplets: 4,
				chunk_slots_override: None,
				debug_gi: false,
				debug_gi_probes: false,
				stub: 0,
			},
		);
		if let Some(err) = pollster::block_on(scope.pop()) {
			panic!("a pipeline failed validation:\n{err}");
		}
	}

	#[test]
	fn grown_slots_sizing() {
		// The doubling floor dominates just past the boundary.
		assert_eq!(super::grown_slots(4096, 4097), 8192);
		// The granularity floor dominates for tiny forced capacities.
		assert_eq!(super::grown_slots(8, 9), 256);
		// A large jump lands on the next granularity step past `needed`.
		assert_eq!(super::grown_slots(8192, 20000), 20224);
		// An exact multiple stays put when it already covers the doubling.
		assert_eq!(super::grown_slots(100, 512), 512);
	}

	#[test]
	fn size_classes_round_up_and_never_shrink() {
		let c = super::SIZE_CLASS;
		// Anything non-empty gets at least one class...
		assert_eq!(super::size_class(1), c);
		assert_eq!(super::size_class(c), c);
		// ...and a byte past a class boundary takes the next whole one, so
		// a remesh within the class writes in place.
		assert_eq!(super::size_class(c + 1), 2 * c);
		assert_eq!(super::size_class(3 * c), 3 * c);
		// The class index the pool is keyed by stays in range.
		assert_eq!(super::size_class(0), c);
	}

	#[test]
	fn scatter_orders_by_bucket_and_is_stable() {
		let keyed = [(2u8, 10u32), (0, 20), (2, 30), (1, 40), (0, 50)];
		let mut counts = [0u32; super::DIST_BUCKETS];
		for &(b, _) in &keyed {
			counts[b as usize] += 1;
		}
		let mut out = vec![9999u32; 3]; // reused scratch, stale contents
		super::scatter_by_bucket(&keyed, &counts, &mut out);
		// Near buckets first; ties keep their input order (the resident
		// set's own order), so the result is deterministic frame to frame.
		assert_eq!(out, vec![20, 50, 40, 10, 30]);
	}
}
