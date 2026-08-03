//! Device-lifetime wgpu state. `Renderer::new` is a pure constructor —
//! device-loss recovery is "build a new one and re-upload meshes".

use digblock_core::chunk::ChunkPos;
use digblock_core::mesher::{quad_indices, MeshData};
use digblock_core::metrics::RenderStats;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_MAIN, Q_OVERLAY, Q_WATER};
use crate::overlay::Overlay;
use crate::shadow::ShadowCascades;
use crate::sky::SkyDraw;
use crate::texture::BlockTextures;
use crate::water::WaterDraw;
use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

/// Worst-case faces per 16³ chunk mesh (checkerboard): 2048 blocks × 6.
const MAX_QUADS: usize = 12288;
pub(crate) const CHUNK_STRIDE: u64 = 256;

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

/// Chunk-origin UBO slot capacity for a draw distance. Sky chunks never
/// upload, but caves can make any buried chunk non-empty, so budget the
/// full stack of every in-range column (256B a slot — a few MB total).
/// A starting estimate, not a ceiling: overflow grows the buffer
/// (`grow_chunk_buf`), which is what a future runtime draw-distance
/// change will lean on.
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

/// Everything `Renderer::new` needs beyond the device and surface
/// format — derived from `EngineConfig` (see `render_config`), so the
/// renderer itself never infers platform tiers from other knobs.
#[derive(Clone)]
pub struct RenderConfig {
	/// Shadow map resolution (both cascades).
	pub shadow_size: u32,
	/// Draw distance in chunks (sizes the chunk-origin UBO).
	pub draw_dist: i32,
	/// Spray droplets per waterfall-impact instance (on top of the mist
	/// slabs): 64 desktop, 24 mobile.
	pub spray_droplets: u32,
	/// Per-frame wide-shadow-refresh encode cap (see WIDE_SLICE_DRAWS).
	pub wide_slice_draws: u32,
	/// Force the initial chunk-origin UBO capacity in slots
	/// (`DIG_CHUNK_SLOTS` via the launcher). Tiny values exercise
	/// `grow_chunk_buf` in a normal run. None = sized from `draw_dist`.
	pub chunk_slots_override: Option<u32>,
}

/// Everything `Renderer::render` consumes for one frame, built by the
/// engine — one argument instead of a parameter list that grew with
/// every feature.
pub struct FrameScene<'a> {
	pub uniforms: FrameUniforms,
	pub view_proj: glam::Mat4,
	pub near_vp: glam::Mat4,
	pub wide_vp: glam::Mat4,
	pub wide_due: bool,
	pub size: (u32, u32),
	/// Remote-player instances `[pos.xyz, yaw, rgb, spare]`, uploaded
	/// every frame (tiny — no change detection).
	pub players: &'a [[f32; 8]],
	pub overlay: Option<(&'a OverlayStats, &'a [f32])>,
	pub feed: &'a [(String, f32, u32)],
	pub menu: Option<&'a crate::menu::MenuView>,
	/// Crosshair target for the highlight decal: (cell, dig progress);
	/// `None` progress = idle outline.
	pub target: Option<((i32, i32, i32), Option<f32>)>,
	/// Hotbar HUD view; None hides the bar (menu open, bench).
	pub hotbar: Option<&'a crate::hotbar::HotbarView>,
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
	pub light_near: [[f32; 4]; 4],
	pub light_wide: [[f32; 4]; 4],
	/// xyz = active caster dir (sun or moon), toward the ground;
	/// w = shadow strength 0..1 (twilight fade).
	pub light_dir: [f32; 4],
	/// rgb = direct light color (sun by day, moonlight by night);
	/// w = the animation clock in seconds (wraps at 600).
	pub light_color: [f32; 4],
	/// rgb = sky-tinted ambient floor.
	pub ambient: [f32; 4],
	/// rgb = fog color (sky horizon); w = fog start distance (units).
	pub fog_color: [f32; 4],
	/// xyz = camera position; w = 1 / (fog_end − fog_start).
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
	/// x = alpha, y = shadows enabled, z = shadow map texel size (UV),
	/// w = wide-cascade texel size in world units (normal-offset bias).
	pub misc: [f32; 4],
	/// x = camera underwater (0/1), y = SSR enabled (0/1), z = water-
	/// surface height above the camera (the shaders' underwater
	/// path-length clamp; meaningful only while x = 1), w reserved.
	/// Appended last — mirror in frame_common.wgsl's Frame struct.
	pub water_misc: [f32; 4],
}

pub struct ChunkGpu {
	pub solid: Option<(wgpu::Buffer, u32)>,
	pub water: Option<(wgpu::Buffer, u32)>,
	pub(crate) slot: u32,
}

/// Default per-frame wide-refresh encode cap (RenderConfig can
/// override). A full-set encode in one frame is a guaranteed missed
/// vsync; sliced it stays ~1-2ms/frame. Sized so a 256-block-tall
/// world's drawn set (surface shell, not the full resident count) still
/// refreshes within a handful of frames — the sun moves imperceptibly
/// in that window.
pub(crate) const WIDE_SLICE_DRAWS: u32 = 1536;

pub struct Renderer {
	solid_pipeline: wgpu::RenderPipeline,
	/// The two-cascade shadow system (pipeline, maps, ping-pong,
	/// sampling bind groups).
	shadow: ShadowCascades,
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
	chunk_buf: wgpu::Buffer,
	chunk_bg: wgpu::BindGroup,
	/// Retained so `grow_chunk_buf` can rebuild `chunk_bg` when
	/// `chunk_buf` is replaced.
	chunk_bgl: wgpu::BindGroupLayout,
	index_buf: wgpu::Buffer,
	chunks: FxHashMap<ChunkPos, ChunkGpu>,
	free_slots: Vec<u32>,
	next_slot: u32,
	/// Capacity of `chunk_buf` in slots, derived from draw distance.
	chunk_slots: u32,
	pub gpu_buffer_bytes: u64,
	/// None when the device lacks TIMESTAMP_QUERY.
	gpu_timer: Option<GpuTimer>,
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
	/// Hotbar block icons (overlay pass, between the overlay's splits).
	hotbar_icons: crate::hotbar::HotbarIcons,
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

		let chunk_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("chunk"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::VERTEX,
				ty: wgpu::BindingType::Buffer {
					ty: wgpu::BufferBindingType::Uniform,
					has_dynamic_offset: true,
					min_binding_size: wgpu::BufferSize::new(16),
				},
				count: None,
			}],
		});

		// Water group 0: the frame UBO (water copy) plus the scene
		// targets — the pre-water solid depth copy (textureLoad, no
		// sampler), the offscreen scene color, and a linear sampler
		// (refraction/SSR offsets). Folding the scene targets into
		// group 0 keeps the water pipelines within the 4-bind-group
		// default limit while freeing group 2 for the shadow split.
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

		// The shadow system owns its pipeline, maps, and the sampling
		// layout the main pipelines reference as group 2.
		let shadow =
			ShadowCascades::new(gpu, config.shadow_size, config.wide_slice_draws, &chunk_bgl);

		// Shared group scheme: 0 = frame, 1 = materials, 2 = shadows,
		// 3 = chunk origins (dynamic offset). Water swaps group 0 for
		// its frame+scene combo layout.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("chunk"),
			bind_group_layouts: &[
				&frame_bgl,
				&materials_bgl,
				shadow.bind_group_layout(),
				&chunk_bgl,
			],
			push_constant_ranges: &[],
		});
		let water_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("water"),
			bind_group_layouts: &[
				&water_frame_bgl,
				&materials_bgl,
				shadow.bind_group_layout(),
				&chunk_bgl,
			],
			push_constant_ranges: &[],
		});

		let vertex_layout = wgpu::VertexBufferLayout {
			array_stride: 8,
			step_mode: wgpu::VertexStepMode::Vertex,
			attributes: &[wgpu::VertexAttribute {
				format: wgpu::VertexFormat::Uint32x2,
				offset: 0,
				shader_location: 0,
			}],
		};

		let solid_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "solid",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: std::slice::from_ref(&vertex_layout),
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
		let chunk_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(chunk_slots) * CHUNK_STRIDE,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let chunk_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("chunk"),
			layout: &chunk_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
					buffer: &chunk_buf,
					offset: 0,
					size: wgpu::BufferSize::new(16),
				}),
			}],
		});

		// Consumed here: the bind group holds the texture/sampler refs.
		let textures = BlockTextures::new(gpu);
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
			shadow,
			water,
			sky,
			frame_buf,
			frame_bg,
			frame_water_buf,
			materials_bg,
			chunk_buf,
			chunk_bg,
			chunk_bgl,
			index_buf,
			chunks: FxHashMap::default(),
			free_slots: Vec::new(),
			next_slot: 0,
			chunk_slots,
			gpu_buffer_bytes: 0,
			gpu_timer: GpuTimer::new(gpu),
			overlay: Overlay::new(gpu, surface_format),
			spray: crate::spray::Spray::new(gpu, surface_format, &frame_bgl, config.spray_droplets),
			players: crate::players::RemotePlayersDraw::new(gpu, surface_format, &frame_bgl),
			highlight: crate::highlight::HighlightDraw::new(gpu, surface_format, &water_frame_bgl),
			hotbar_icons: crate::hotbar::HotbarIcons::new(gpu, surface_format, &materials_bgl),
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

	/// Replace `chunk_buf` with a larger one, repopulate every live
	/// slot's origin in one batched write (origins are pure functions of
	/// ChunkPos; `self.chunks` is the authoritative slot map), and
	/// rebuild `chunk_bg` from the retained layout. Slot numbers are
	/// untouched, so the shadow wide-refresh job (which snapshots
	/// ChunkPos, not slots) and the water list are unaffected; every
	/// pass takes `chunk_bg` fresh each frame.
	fn grow_chunk_buf(&mut self, gpu: &Gpu, needed_slots: u32) {
		let old = self.chunk_slots;
		let new_slots = grown_slots(old, needed_slots);
		let chunk_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(new_slots) * CHUNK_STRIDE,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		if self.next_slot > 0 {
			// One batched repopulate covering every allocated slot; free
			// slots stay zeroed — no draw ever binds them.
			let mut staging = vec![0u8; self.next_slot as usize * CHUNK_STRIDE as usize];
			for (pos, chunk) in &self.chunks {
				let (ox, oy, oz) = pos.origin();
				let off = chunk.slot as usize * CHUNK_STRIDE as usize;
				staging[off..off + 16].copy_from_slice(bytemuck::cast_slice(&[ox, oy, oz, 0.0f32]));
			}
			gpu.queue.write_buffer(&chunk_buf, 0, &staging);
		}
		self.chunk_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("chunk"),
			layout: &self.chunk_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
					buffer: &chunk_buf,
					offset: 0,
					size: wgpu::BufferSize::new(16),
				}),
			}],
		});
		self.chunk_buf = chunk_buf;
		self.chunk_slots = new_slots;
		log::info!(
			"chunk UBO grown: {old} -> {new_slots} slots ({} KiB), {} chunks repopulated",
			u64::from(new_slots) * CHUNK_STRIDE / 1024,
			self.chunks.len()
		);
	}

	/// Upload (or replace) a chunk's mesh. Empty meshes remove the entry —
	/// unlike the Java, a re-mesh to zero faces cannot leave a stale VBO.
	pub fn upload_chunk(&mut self, gpu: &Gpu, pos: ChunkPos, mesh: &MeshData) {
		self.remove_chunk(pos);
		if mesh.is_empty() {
			return;
		}
		let slot = self.free_slots.pop().unwrap_or_else(|| {
			let s = self.next_slot;
			self.next_slot += 1;
			s
		});
		if slot >= self.chunk_slots {
			self.grow_chunk_buf(gpu, slot + 1);
		}
		debug_assert!(
			slot < self.chunk_slots,
			"grow_chunk_buf left slot out of range"
		);
		let (ox, oy, oz) = pos.origin();
		gpu.queue.write_buffer(
			&self.chunk_buf,
			u64::from(slot) * CHUNK_STRIDE,
			bytemuck::cast_slice(&[ox, oy, oz, 0.0]),
		);
		// All vertices are two u32 words (see mesher::pack).
		let mut make = |verts: &Vec<u32>| -> Option<(wgpu::Buffer, u32)> {
			if verts.is_empty() {
				return None;
			}
			let buf = buffer_init(
				gpu,
				"chunk verts",
				wgpu::BufferUsages::VERTEX,
				bytemuck::cast_slice(verts),
			);
			self.gpu_buffer_bytes += buf.size();
			let quads = (verts.len() / (4 * 2)) as u32;
			Some((buf, quads * 6))
		};
		let solid = make(&mesh.solid);
		let water = make(&mesh.water);
		self.chunks.insert(pos, ChunkGpu { solid, water, slot });
		self.spray.set_chunk(pos, pos.origin(), &mesh.impacts);
	}

	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		self.spray.remove_chunk(pos);
		if let Some(old) = self.chunks.remove(&pos) {
			for (buf, _) in old.solid.iter().chain(old.water.iter()) {
				self.gpu_buffer_bytes = self.gpu_buffer_bytes.saturating_sub(buf.size());
			}
			self.free_slots.push(old.slot);
		}
	}

	/// Drop every uploaded chunk mesh (world swap: the multiplayer
	/// handshake replaces the locally-seeded world with the server's).
	pub fn clear_chunks(&mut self) {
		let all: Vec<ChunkPos> = self.chunks.keys().copied().collect();
		for pos in all {
			self.remove_chunk(pos);
		}
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
		self.targets = Some(FrameTargets {
			size,
			depth_view,
			depth_copy_view,
			scene_view,
			water_bg,
			blit_bg,
			depth_blit_bg,
		});
	}

	/// Encode the frame in one command encoder, fixed order — wide shadow
	/// (only when due) → near shadow → main pass. Shadow maps are written
	/// and consumed in the same frame (fixes the Java one-frame lag).
	pub fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		scene: &FrameScene<'_>,
	) -> RenderStats {
		let FrameScene {
			uniforms: ref frame,
			view_proj,
			near_vp,
			wide_vp,
			wide_due,
			size,
			players,
			overlay,
			feed,
			menu,
			target: highlight_target,
			hotbar,
			chat,
			nametags,
		} = *scene;
		self.ensure_targets(gpu, size);
		// Remote players re-upload every frame (a handful of instances —
		// cheaper than the change detection it replaces).
		self.players.set_instances(gpu, players);
		self.highlight.set_target(gpu, highlight_target);
		let placed_slots = hotbar.map(|view| crate::hotbar::layout(size, view));
		let mut stats = RenderStats {
			chunks_resident: self.chunks.len() as u32,
			wide_refreshed: self.shadow.refresh_pending(wide_due),
			..Default::default()
		};
		// Light-VP uploads + wide-refresh kickoff; the returned sample is
		// captured before the encode below can flip the ping-pong.
		let wide_sample =
			self.shadow
				.begin_frame(&gpu.queue, near_vp, wide_vp, wide_due, &self.chunks);
		// The front wide map lags the engine's wide_vp by up to one job
		// (a few frames); the main pass must project with the VP that map
		// was rendered with, not this frame's.
		let mut frame = *frame;
		frame.light_wide = wide_sample.active_vp.to_cols_array_2d();
		let frame = &frame;
		if let Some(t) = self.gpu_timer.as_mut() {
			t.begin_frame();
		}
		self.overlay.prepare(
			gpu,
			size,
			overlay,
			feed,
			menu,
			placed_slots.as_ref().map(|s| s.as_slice()),
			chat.as_ref(),
			nametags,
		);
		self.hotbar_icons
			.prepare(gpu, size, placed_slots.as_ref().map(|s| s.as_slice()));
		// Chunk bounding-sphere frustum cull against the four side planes.
		let planes = side_planes(view_proj);
		let in_frustum = |pos: &ChunkPos| -> bool { chunk_in_frustum(pos, &planes) };
		gpu.queue
			.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(frame));
		let mut water_frame = *frame;
		water_frame.misc[0] = 0.7; // Java transparent-pass u_Alpha
		gpu.queue
			.write_buffer(&self.frame_water_buf, 0, bytemuck::bytes_of(&water_frame));
		let spray_n = self.spray.prepare(
			gpu,
			glam::Vec3::new(frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]),
		);

		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor {
				label: Some("frame"),
			});

		// All shadow work (bootstrap clear → wide slice → near pass) in
		// fixed order; a completed wide slice flips the ping-pong AFTER
		// this frame's sample was captured above.
		let shadow_stats = self.shadow.encode(
			&mut encoder,
			&self.chunks,
			&self.chunk_bg,
			&self.index_buf,
			self.gpu_timer.as_ref(),
		);
		stats.shadow_casters = shadow_stats.casters;
		stats.draw_calls += shadow_stats.draw_calls;
		stats.triangles += shadow_stats.triangles;

		// Water chunk list, shared by both water subpasses below.
		self.water.build_list(&self.chunks, in_frustum);
		{
			let targets = self.targets.as_ref().expect("ensured above");
			// Scene pass: solids + sky into the offscreen color target;
			// depth is stored for the copy below.
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
								r: f64::from(frame.fog_color[0]),
								g: f64::from(frame.fog_color[1]),
								b: f64::from(frame.fog_color[2]),
								a: 1.0,
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
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_MAIN)),
					occlusion_query_set: None,
				});

				pass.set_pipeline(&self.solid_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, self.shadow.sample_bind_group(&wide_sample), &[]);
				pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
				for (pos, chunk) in &self.chunks {
					if !in_frustum(pos) {
						stats.chunks_culled += 1;
						continue;
					}
					if let Some((buf, index_count)) = &chunk.solid {
						pass.set_bind_group(
							3,
							&self.chunk_bg,
							&[(u64::from(chunk.slot) * CHUNK_STRIDE) as u32],
						);
						pass.set_vertex_buffer(0, buf.slice(..));
						pass.draw_indexed(0..*index_count, 0, 0..1);
						stats.chunks_drawn += 1;
						stats.triangles += *index_count / 3;
					}
				}

				// Remote players: opaque, depth-writing, before the sky so
				// they occlude and fog like terrain (frame_bg is bound at 0).
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

			// Pre-water solid depth becomes sampleable: water reads it for
			// column thickness (absorption, soft shores) while the live
			// attachment goes on to take the water prepass's writes. A
			// fullscreen draw, not copy_texture_to_texture — WebGPU forbids
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
				pass.set_bind_group(2, self.shadow.sample_bind_group(&wide_sample), &[]);
				pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
				let (water_draws, water_tris) =
					self.water.draw(&mut pass, &self.chunks, &self.chunk_bg);
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
			// Sandwich: overlay-below (slot backdrops) → block icons →
			// overlay-above (counts, panel, feed, menu). One pass, one
			// timer bracket.
			self.overlay.draw_below(&mut pass);
			self.hotbar_icons.draw(&mut pass, &self.materials_bg);
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

/// Chunk bounding-sphere test against extracted side planes. RADIUS is
/// 8·√3, the chunk half-diagonal (Java sphereInFrustumWithoutNearFar —
/// near/far deliberately ignored). Shared by the main pass and the
/// shadow passes so the magic constant lives once.
pub(crate) fn chunk_in_frustum(pos: &ChunkPos, planes: &[glam::Vec4; 4]) -> bool {
	let c = glam::Vec3::new(
		(pos.x * 16 + 8) as f32,
		(pos.y * 16 + 8) as f32,
		(pos.z * 16 + 8) as f32,
	);
	const RADIUS: f32 = 13.86;
	planes.iter().all(|p| p.truncate().dot(c) + p.w > -RADIUS)
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
}
