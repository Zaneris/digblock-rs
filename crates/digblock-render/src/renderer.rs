//! Device-lifetime wgpu state. `Renderer::new` is a pure constructor —
//! device-loss recovery is "build a new one and re-upload meshes".

use digblock_core::chunk::ChunkPos;
use digblock_core::mesher::{quad_indices, MeshData};
use digblock_core::metrics::RenderStats;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_MAIN, Q_OVERLAY, Q_PREPASS, Q_WATER};
use crate::overlay::Overlay;
use crate::sky::SkyDraw;
use crate::texture::BlockTextures;
use crate::water::WaterDraw;
use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

/// Worst-case quads per 16³ chunk mesh: a solid chunk of the busiest
/// shaped variant (every template quad emits when neighbors don't cover
/// them). Dwarfs the old full-block checkerboard bound (2048 × 6). The
/// shared index buffer this sizes is ~3.75 MiB — one-time, fine on
/// mobile; `upload_chunk` guards the cap instead of trusting it.
const MAX_QUADS: usize = 4096 * digblock_core::shapes::MAX_SHAPE_QUADS;

/// Chunk-origin stride: one `vec4f` per slot in the instance-step origin
/// stream. Was 256 bytes — the uniform dynamic-offset alignment, not the
/// data — back when each draw rebound a slice of a UBO.
pub(crate) const ORIGIN_STRIDE: u64 = 16;

/// Chunk vertex-buffer size class. A remesh (an edit dirties up to 27
/// neighbours; walking churns the rim continuously) nearly always lands
/// inside the class of the mesh it replaces, so it is written over the
/// live allocation; what does change class is recycled through the free
/// list below. Both matter most on Chrome, where every buffer create and
/// destroy is a GPU-process allocation plus a deferred free that wgpu
/// batches into periodic cleanup spikes.
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
	// write_buffer requires 4-byte-aligned sizes; all callers upload
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
/// full stack of every in-range column (16B a slot — a few hundred KB).
/// A starting estimate, not a ceiling: `grow_chunk_buf` takes over at 85%
/// occupancy, which is what a future runtime draw-distance change will
/// lean on.
fn chunk_slots(draw_dist: i32) -> u32 {
	let r = f64::from(draw_dist) + 1.0;
	let columns = (std::f64::consts::PI * r * r).ceil() as u32;
	(columns * digblock_core::chunk::WORLD_VCHUNK as u32)
		.next_multiple_of(256)
		.max(4096)
}

/// New capacity when `needed` exceeds the current slot count: round up
/// to the 256-slot granularity, but never less than double (amortized
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
	/// slabs): 64 desktop, 24 mobile.
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
}

/// Everything `Renderer::render` consumes for one frame, built by the
/// engine — one argument instead of a parameter list that grew with
/// every feature.
pub struct FrameScene<'a> {
	pub uniforms: FrameUniforms,
	pub view_proj: glam::Mat4,
	pub size: (u32, u32),
	/// Whole-UI scale for this frame (device scale factor × window size).
	pub ui_scale: f32,
	/// Remote-player instances `[pos.xyz, yaw, rgb, spare]`, uploaded
	/// every frame (tiny — no change detection).
	pub players: &'a [[f32; 8]],
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
	/// w = the animation clock in seconds (wraps at 600).
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
	/// x = water alpha; yzw reserved (the shadow-map lanes died with the
	/// cascades — nothing has read them since).
	pub misc: [f32; 4],
	/// x = camera underwater (0/1), y = SSR enabled (0/1), z = water-
	/// surface height above the camera (the shaders' underwater
	/// path-length clamp; meaningful only while x = 1), w reserved.
	pub water_misc: [f32; 4],
	/// RTGI fields, patched by `GiDraw::patch_frame` in `render` (the
	/// engine writes zeros) — mirror frame_common.wgsl's Frame struct.
	/// xyz = quantized dir toward the caster, w = above horizon (0/1).
	pub gi_sun: [f32; 4],
	/// C0 probe dims xyz, w = spacing (2).
	pub gi0: [f32; 4],
	/// C1 probe dims xyz, w = spacing (8).
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
	/// Water copy of the frame UBO, alpha 0.7 (Java u_Alpha for the
	/// transparent pass); bound through `FrameTargets::water_bg`.
	frame_water_buf: wgpu::Buffer,
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
	/// Composites the offscreen scene into the swapchain at the start of
	/// the water pass.
	blit_pipeline: wgpu::RenderPipeline,
	/// Resolves the live depth into the R32Float copy between the scene
	/// and water passes (WebGPU forbids copying into a depth texture).
	depth_blit_pipeline: wgpu::RenderPipeline,
	surface_format: wgpu::TextureFormat,
}

/// Renderer-owned per-frame render targets. The platform hands over only
/// the swapchain view; depth and the offscreen scene color live here so
/// the water pass can sample both.
struct FrameTargets {
	size: (u32, u32),
	/// Scene-pass depth attachment; resolved into `depth_copy_view` by
	/// the depth-blit pass before water. (Views keep textures alive.)
	depth_view: wgpu::TextureView,
	/// Pre-water solid depth as R32Float, sampleable (absorption / soft
	/// shores / SSR) — a color format because WebGPU forbids copying
	/// into a depth texture, so it's filled by a fullscreen draw.
	depth_copy_view: wgpu::TextureView,
	/// Offscreen scene color view: solids + sky render here; water
	/// samples it (refraction/SSR) while blending into the swapchain.
	scene_view: wgpu::TextureView,
	/// Water group 0: the water frame UBO + scene targets.
	water_bg: wgpu::BindGroup,
	/// Blit group 0: scene color + linear sampler.
	blit_bg: wgpu::BindGroup,
	/// Depth-blit group 0: the live depth texture.
	depth_blit_bg: wgpu::BindGroup,
	/// GI resolve outputs, half res: indirect rgb + view dist (rgba16f)
	/// and sun visibility + normal code (rgba8).
	gi_indirect_view: wgpu::TextureView,
	gi_sun_view: wgpu::TextureView,
	/// Scene group 2: the two views above, textureLoad'd by chunk.wgsl.
	gi_scene_bg: wgpu::BindGroup,
	/// Resolve group 0: its uniform + the depth-copy tap (the GI world
	/// data rides group 2 via `GiDraw::gather_bind`).
	gi_resolve_bg: wgpu::BindGroup,
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
		// group 0 keeps the water pipelines within the 4-bind-group
		// default limit while freeing group 2 for the GI world data.
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
			bind_group_layouts: &[&frame_bgl, &materials_bgl, gi.scene_bgl()],
			push_constant_ranges: &[],
		});
		let water_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("water"),
			bind_group_layouts: &[&water_frame_bgl, &materials_bgl, gi.gather_bgl()],
			push_constant_ranges: &[],
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

		// Depth-only prepass: same vertex stream, position-only entry,
		// no fragment stage — fills the depth the GI resolve consumes.
		let prepass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("depth prepass"),
			bind_group_layouts: &[&frame_bgl],
			push_constant_ranges: &[],
		});
		let prepass_pipeline = crate::pipeline_builder::build(
			device,
			None,
			&crate::pipeline_builder::PipelineDesc {
				label: "depth prepass",
				layout: &prepass_layout,
				module: &shader,
				vs_entry: "vs_depth",
				fs_entry: None,
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
			bind_group_layouts: &[&blit_bgl],
			push_constant_ranges: &[],
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
			bind_group_layouts: &[&depth_blit_bgl],
			push_constant_ranges: &[],
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
		// Bound through FrameTargets::water_bg (the water group 0 also
		// holds the resize-recreated scene targets).
		let frame_water_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("frame ubo (water)"),
			size: std::mem::size_of::<FrameUniforms>() as u64,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
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
			frame_water_buf,
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
			overlay: Overlay::new(gpu, surface_format),
			spray: crate::spray::Spray::new(
				gpu,
				surface_format,
				&frame_bgl,
				gi.cam_light_buf(),
				config.spray_droplets,
			),
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
			depth_blit_pipeline,
			surface_format,
		}
	}

	/// Latest GPU pass reading (2-3 frames latent); None when timestamps
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
	/// origin. Growth fires at 85% occupancy rather than at overflow: the
	/// reallocate-and-repopulate lands in one frame, and the path that
	/// reaches it — a runtime draw-distance increase — is already the
	/// frame doing the most other work.
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

	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		self.spray.remove_chunk(pos);
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
			if let Some(planes) = near {
				if solid && sphere_in_frustum(c.center, planes) {
					lists.near.push(i);
				}
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
	fn ensure_targets(&mut self, gpu: &Gpu, size: (u32, u32)) {
		if self.targets.as_ref().map(|t| t.size) == Some(size) {
			return;
		}
		let extent = wgpu::Extent3d {
			width: size.0.max(1),
			height: size.1.max(1),
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
		// Both group-0 combos that depend on the resize-recreated
		// textures are (re)built here: the water frame+scene group and
		// the blit group.
		let water_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("water frame + scene targets"),
			layout: &self.water_frame_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: self.frame_water_buf.as_entire_binding(),
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
		let depth_blit_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("depth blit"),
			layout: &self.depth_blit_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: wgpu::BindingResource::TextureView(&depth_view),
			}],
		});
		// The GI resolve targets at half resolution — the chunk shader's
		// bilateral upsample bridges the gap.
		let gi_tex = |format, label| {
			gpu.device.create_texture(&wgpu::TextureDescriptor {
				label: Some(label),
				size: wgpu::Extent3d {
					width: (size.0 / 2).max(1),
					height: (size.1 / 2).max(1),
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
		let gi_indirect_view = gi_tex(wgpu::TextureFormat::Rgba16Float, "gi indirect")
			.create_view(&Default::default());
		let gi_sun_view =
			gi_tex(wgpu::TextureFormat::Rgba8Unorm, "gi sun").create_view(&Default::default());
		let gi_scene_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi scene"),
			layout: self.gi.scene_bgl(),
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&gi_indirect_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&gi_sun_view),
				},
			],
		});
		let gi_resolve_bg = self.gi.make_resolve_bind(gpu, &depth_copy_view);
		self.targets = Some(FrameTargets {
			size,
			depth_view,
			depth_copy_view,
			scene_view,
			water_bg,
			blit_bg,
			depth_blit_bg,
			gi_indirect_view,
			gi_sun_view,
			gi_scene_bg,
			gi_resolve_bg,
		});
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
			ui_scale,
			players,
			overlay,
			feed,
			target: highlight_target,
			viewmodel,
			ui,
			chat,
			nametags,
		} = *scene;
		self.ensure_targets(gpu, size);
		// Remote players re-upload every frame (a handful of instances —
		// cheaper than the change detection it replaces).
		self.players.set_instances(gpu, players);
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
		let mut water_frame = *frame;
		water_frame.misc[0] = 0.7; // Java transparent-pass u_Alpha
		gpu.queue
			.write_buffer(&self.frame_water_buf, 0, bytemuck::bytes_of(&water_frame));
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

		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor {
				label: Some("frame"),
			});

		// RTGI probe update first: compute at the head of the encoder, so
		// every later pass that reads the atlas sees this frame's write.
		self.gi
			.encode(gpu, &mut encoder, self.gpu_timer.as_ref(), frame);

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
					color_attachments: &[],
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
				});
				pass.set_pipeline(&self.prepass_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				draw.bind(&mut pass);
				for &i in &self.lists.scene {
					let chunk = &draw.chunks[i as usize];
					let Some(mesh) = chunk.solid.as_ref() else {
						continue;
					};
					stats.triangles += draw_chunk(&mut pass, mesh, chunk.slot) / 3;
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
					timestamp_writes: None,
					occlusion_query_set: None,
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
					indirect_view: &targets.gi_indirect_view,
					sun_view: &targets.gi_sun_view,
				},
			);

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
				});

				pass.set_pipeline(&self.solid_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, &targets.gi_scene_bg, &[]);
				// Origins for every chunk, bound once — each draw selects
				// its own with first_instance.
				draw.bind(&mut pass);
				for &i in &self.lists.scene {
					let chunk = &draw.chunks[i as usize];
					let Some(mesh) = chunk.solid.as_ref() else {
						continue;
					};
					stats.triangles += draw_chunk(&mut pass, mesh, chunk.slot) / 3;
					stats.chunks_drawn += 1;
				}

				// Remote players: opaque, depth-writing, before the sky so
				// they occlude and fog like terrain (frame_bg is bound at
				// 0). They gather the probe field per fragment, so their
				// group 2 is the GI world data, not the resolve outputs.
				pass.set_bind_group(2, self.gi.gather_bind(), &[]);
				let player_tris = self.players.draw(&mut pass);
				if player_tris > 0 {
					stats.draw_calls += 1;
					stats.triangles += player_tris;
				}

				// Sky after solids (depth test discards occluded fragments —
				// near-free on TBDR); frame_bg is still bound at 0.
				self.sky.draw(&mut pass);
				stats.draw_calls += 1;
			}

			// Water pass into the swapchain: composite the scene (blit),
			// then transparent water in two subpasses — a depth-only
			// prepass records the nearest water surface, then the color
			// pass (no depth write, double-sided) shades only fragments at
			// that depth, one blended layer per pixel. Depth is discarded
			// at the end: nothing after this pass reads it.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("water"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: target,
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
				});

				pass.set_pipeline(&self.blit_pipeline);
				pass.set_bind_group(0, &targets.blit_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;

				pass.set_bind_group(0, &targets.water_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, self.gi.gather_bind(), &[]);
				draw.bind(&mut pass);
				let (water_draws, water_tris) =
					self.water.draw(&mut pass, draw.chunks, &self.lists.water);
				stats.water_draws += water_draws;
				stats.triangles += water_tris;

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
				let spray_tris = self.spray.draw(&mut pass, spray_n);
				stats.draw_calls += u32::from(spray_tris > 0);
				stats.triangles += spray_tris;
			}

			// The item in your own hand, over the finished world. Its own
			// pass for one reason: it clears the depth buffer first. The
			// item is a metre from the eye and would otherwise be sliced
			// The voxel-window debug raymarch paints over the finished
			// scene (overlay still lands on top) — the P1 gate capture.
			if self.debug_gi {
				let cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
				self.gi
					.encode_debug(gpu, &mut encoder, target, frame.inv_view_proj, cam);
			}
			// open by the wall you walk into — but it still needs depth
			// among its OWN faces, because a staircase in hand is not
			// convex and back-face culling alone can't sort it. The water
			// pass discarded this attachment, so reusing it costs nothing.
			if !self.viewmodel.is_empty() {
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("viewmodel"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: target,
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
					timestamp_writes: None,
					occlusion_query_set: None,
				});
				let tris = self
					.viewmodel
					.draw(&mut pass, &self.index_buf, &self.materials_bg);
				stats.draw_calls += 1;
				stats.triangles += tris;
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

/// Chunk bounding-sphere test against extracted side planes. RADIUS is
/// 8·√3, the chunk half-diagonal (Java sphereInFrustumWithoutNearFar —
/// near/far deliberately ignored). One definition, so the magic constant
/// can't drift between the callers that share it.
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
		if len > 0.0 {
			p / len
		} else {
			p
		}
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

	#[test]
	fn grown_slots_sizing() {
		// Min ×2 dominates just past the boundary.
		assert_eq!(super::grown_slots(4096, 4097), 8192);
		// 256-granularity dominates for tiny forced capacities.
		assert_eq!(super::grown_slots(8, 9), 256);
		// A large jump lands on the next multiple of 256 past `needed`.
		assert_eq!(super::grown_slots(8192, 20000), 20224);
		// An exact multiple stays put when it already covers ×2.
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
