//! Device-lifetime wgpu state. `Renderer::new` is a pure constructor —
//! device-loss recovery is "build a new one and re-upload meshes".

use digblock_core::chunk::ChunkPos;
use digblock_core::mesher::{quad_indices, MeshData};
use digblock_core::metrics::RenderStats;
use rustc_hash::FxHashMap;
use wgpu::util::DeviceExt;

use crate::gpu_timer::{GpuTimer, Q_MAIN, Q_OVERLAY, Q_SHADOW_NEAR, Q_SHADOW_WIDE, Q_WATER};
use crate::overlay::Overlay;
use crate::texture::BlockTextures;
use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

/// Worst-case faces per 16³ chunk mesh (checkerboard): 2048 blocks × 6.
const MAX_QUADS: usize = 12288;
const CHUNK_STRIDE: u64 = 256;

/// Chunk-origin UBO slot capacity for a draw distance. Sky chunks never
/// upload, but caves can make any buried chunk non-empty, so budget the
/// full stack of every in-range column (256B a slot — a few MB total).
/// The upload-time assert stays as the tripwire.
fn chunk_slots(draw_dist: i32) -> u32 {
	let r = f64::from(draw_dist) + 1.0;
	let columns = (std::f64::consts::PI * r * r).ceil() as u32;
	(columns * digblock_core::chunk::WORLD_VCHUNK as u32)
		.next_multiple_of(256)
		.max(4096)
}

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Field order/layout must match the `Frame` struct in BOTH
/// `chunk.wgsl` and `sky.wgsl` (naga can't check cross-file layout).
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
	/// x = camera underwater (0/1), y = SSR enabled (0/1), z/w reserved.
	/// Appended last — mirror in the Frame structs of water.wgsl,
	/// chunk.wgsl, and sky.wgsl (hand-synced, order load-bearing).
	pub water_misc: [f32; 4],
}

pub struct ChunkGpu {
	pub solid: Option<(wgpu::Buffer, u32)>,
	pub water: Option<(wgpu::Buffer, u32)>,
	slot: u32,
}

/// Wide-cascade refresh in progress: the resident set snapshotted at start,
/// re-rendered into the back map `WIDE_SLICE_DRAWS` chunks per frame.
/// Chunks uploaded mid-job appear at the next refresh (same as before the
/// split); evicted ones are skipped at encode time.
struct WideJob {
	vp: glam::Mat4,
	list: Vec<ChunkPos>,
	next: usize,
}

/// Per-frame wide-refresh encode cap. A full-set encode in one frame is a
/// guaranteed missed vsync; sliced it stays ~1-2ms/frame. Sized so a
/// 256-block-tall world's drawn set (surface shell, not the full resident
/// count) still refreshes within a handful of frames — the sun moves
/// imperceptibly in that window.
const WIDE_SLICE_DRAWS: u32 = 1536;

pub struct Renderer {
	solid_pipeline: wgpu::RenderPipeline,
	water_pipeline: wgpu::RenderPipeline,
	/// Depth-only water prepass: records the nearest water surface so
	/// `water_pipeline` (depth Equal) blends exactly one layer — interior
	/// faces of cascades never reach the blender.
	water_depth_pipeline: wgpu::RenderPipeline,
	shadow_pipeline: wgpu::RenderPipeline,
	/// Fullscreen procedural sky, drawn after solids, before water.
	sky_pipeline: wgpu::RenderPipeline,
	frame_buf: wgpu::Buffer,
	frame_bg: wgpu::BindGroup,
	/// Same layout, alpha 0.7 (Java u_Alpha for the transparent pass).
	frame_water_buf: wgpu::Buffer,
	frame_water_bg: wgpu::BindGroup,
	/// One per wide map (binding 3 differs); `wide_front` picks the one
	/// whose wide map is complete and safe to sample.
	tex_bgs: [wgpu::BindGroup; 2],
	chunk_buf: wgpu::Buffer,
	chunk_bg: wgpu::BindGroup,
	light_near_buf: wgpu::Buffer,
	light_near_bg: wgpu::BindGroup,
	light_wide_buf: wgpu::Buffer,
	light_wide_bg: wgpu::BindGroup,
	/// Set at construction: the persistent wide cascade has no contents
	/// yet (also the device-loss recovery path).
	pub wide_refresh_needed: bool,
	/// Index of the wide map currently sampled; 1-front is the render
	/// target of the in-progress refresh.
	wide_front: usize,
	/// The view-proj the front wide map was rendered with — what the main
	/// pass must project with until the next flip, regardless of how far
	/// the sun has moved since.
	wide_active_vp: glam::Mat4,
	/// In-progress wide refresh, encoded a slice per frame.
	wide_job: Option<WideJob>,
	/// False until the first render() bootstraps a clear of the front map
	/// (it is uninitialized at construction / device rebuild).
	wide_front_valid: bool,
	index_buf: wgpu::Buffer,
	// Read from M6 on (shadow passes render into the shadow views).
	#[allow(dead_code)]
	pub textures: BlockTextures,
	chunks: FxHashMap<ChunkPos, ChunkGpu>,
	free_slots: Vec<u32>,
	next_slot: u32,
	/// Capacity of `chunk_buf` in slots, derived from draw distance.
	chunk_slots: u32,
	pub gpu_buffer_bytes: u64,
	/// Reused water chunk list (was a fresh alloc every frame). The
	/// depth prepass makes draw order irrelevant, so no sort key.
	water_scratch: Vec<ChunkPos>,
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
	/// Layout of `FrameTargets::scene_bg` (depth copy + scene color +
	/// linear sampler): blit group 0, water group 3.
	scene_bgl: wgpu::BindGroupLayout,
	linear_sampler: wgpu::Sampler,
	/// Composites the offscreen scene into the swapchain at the start of
	/// the water pass.
	blit_pipeline: wgpu::RenderPipeline,
	surface_format: wgpu::TextureFormat,
}

/// Renderer-owned per-frame render targets. The platform hands over only
/// the swapchain view; depth and the offscreen scene color live here so
/// the water pass can sample both.
struct FrameTargets {
	size: (u32, u32),
	/// Scene-pass depth attachment; copied to `depth_copy` before water.
	depth: wgpu::Texture,
	depth_view: wgpu::TextureView,
	/// Pre-water solid depth, sampleable (absorption / soft shores / SSR).
	depth_copy: wgpu::Texture,
	/// Offscreen scene color view: solids + sky render here; water
	/// samples it (refraction/SSR) while blending into the swapchain.
	/// (The view keeps the texture alive.)
	scene_view: wgpu::TextureView,
	scene_bg: wgpu::BindGroup,
}

impl Renderer {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		shadow_size: u32,
		draw_dist: i32,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("chunk"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::CHUNK.into()),
		});
		let water_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("water"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::WATER.into()),
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

		let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("textures"),
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
				wgpu::BindGroupLayoutEntry {
					binding: 2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Depth,
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 3,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Depth,
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 4,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
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

		// Scene-target bindings for the water pass: the pre-water solid
		// depth copy (textureLoad, no sampler), the offscreen scene color,
		// and a linear sampler (refraction/SSR offsets). The blit uses
		// only binding 1; water declares this as group 3.
		let scene_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("scene targets"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Depth,
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 2,
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

		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("chunk"),
			bind_group_layouts: &[&frame_bgl, &tex_bgl, &chunk_bgl],
			push_constant_ranges: &[],
		});
		// Water pipelines carry a 4th group (the scene targets) so
		// chunk.wgsl/sky.wgsl never have to declare bindings they don't use.
		let water_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("water"),
			bind_group_layouts: &[&frame_bgl, &tex_bgl, &chunk_bgl, &scene_bgl],
			push_constant_ranges: &[],
		});

		// Depth-only shadow pass: group0 = light VP, group1 = chunk origins.
		let light_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("light"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::VERTEX,
				ty: wgpu::BindingType::Buffer {
					ty: wgpu::BufferBindingType::Uniform,
					has_dynamic_offset: false,
					min_binding_size: None,
				},
				count: None,
			}],
		});
		let shadow_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("shadow"),
			bind_group_layouts: &[&light_bgl, &chunk_bgl],
			push_constant_ranges: &[],
		});

		let vertex_layout = wgpu::VertexBufferLayout {
			array_stride: 4,
			step_mode: wgpu::VertexStepMode::Vertex,
			attributes: &[wgpu::VertexAttribute {
				format: wgpu::VertexFormat::Uint32,
				offset: 0,
				shader_location: 0,
			}],
		};

		let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("solid"),
			layout: Some(&layout),
			vertex: wgpu::VertexState {
				module: &shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: std::slice::from_ref(&vertex_layout),
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: Some(wgpu::Face::Back),
				..Default::default()
			},
			depth_stencil: Some(wgpu::DepthStencilState {
				format: DEPTH_FORMAT,
				depth_write_enabled: true,
				depth_compare: wgpu::CompareFunction::Less,
				stencil: Default::default(),
				bias: Default::default(),
			}),
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &shader,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_format,
					blend: None,
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			multiview: None,
			cache: None,
		});

		// Water: blended, double-sided (the dropped surface is visible
		// from below), depth test on but depth WRITE off — fixes the Java
		// artifact of water writing depth in the transparent pass. Its
		// vertices are two u32 words (see mesher MeshData).
		let water_vertex_layout = wgpu::VertexBufferLayout {
			array_stride: 8,
			step_mode: wgpu::VertexStepMode::Vertex,
			attributes: &[wgpu::VertexAttribute {
				format: wgpu::VertexFormat::Uint32x2,
				offset: 0,
				shader_location: 0,
			}],
		};
		let water_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("water"),
			layout: Some(&water_layout),
			vertex: wgpu::VertexState {
				module: &water_shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: std::slice::from_ref(&water_vertex_layout),
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: None,
				..Default::default()
			},
			depth_stencil: Some(wgpu::DepthStencilState {
				format: DEPTH_FORMAT,
				depth_write_enabled: false,
				// LessEqual against the prepass depth: only the nearest
				// water surface shades (Equal in effect, tolerant of
				// cross-pipeline depth invariance).
				depth_compare: wgpu::CompareFunction::LessEqual,
				stencil: Default::default(),
				bias: Default::default(),
			}),
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &water_shader,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_format,
					blend: Some(wgpu::BlendState::ALPHA_BLENDING),
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			multiview: None,
			cache: None,
		});

		// Depth-only prepass over the same water geometry: writes the
		// nearest water depth, color masked off. Stacked water/air
		// interfaces (fall-sheet backsides, pool surfaces behind them)
		// then fail the color pass's depth test, so a connected body of
		// water reads as one surface instead of accumulated alpha layers.
		let water_depth_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("water depth"),
			layout: Some(&water_layout),
			vertex: wgpu::VertexState {
				module: &water_shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: std::slice::from_ref(&water_vertex_layout),
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: None,
				..Default::default()
			},
			depth_stencil: Some(wgpu::DepthStencilState {
				format: DEPTH_FORMAT,
				depth_write_enabled: true,
				depth_compare: wgpu::CompareFunction::Less,
				stencil: Default::default(),
				bias: Default::default(),
			}),
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &water_shader,
				entry_point: Some("fs_depth"),
				compilation_options: Default::default(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_format,
					blend: None,
					write_mask: wgpu::ColorWrites::empty(),
				})],
			}),
			multiview: None,
			cache: None,
		});

		// Sky: fullscreen triangle at NDC depth 1.0, depth-tested against
		// the solids (LessEqual, no write) so occluded fragments are free;
		// drawn before water so water blends over it.
		let sky_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("sky"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SKY.into()),
		});
		let sky_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("sky"),
			bind_group_layouts: &[&frame_bgl],
			push_constant_ranges: &[],
		});
		let sky_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("sky"),
			layout: Some(&sky_layout),
			vertex: wgpu::VertexState {
				module: &sky_shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: None,
				..Default::default()
			},
			depth_stencil: Some(wgpu::DepthStencilState {
				format: DEPTH_FORMAT,
				depth_write_enabled: false,
				depth_compare: wgpu::CompareFunction::LessEqual,
				stencil: Default::default(),
				bias: Default::default(),
			}),
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &sky_shader,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_format,
					blend: None,
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			multiview: None,
			cache: None,
		});

		// Fullscreen composite of the offscreen scene into the swapchain:
		// first draw of the water pass. Depth Always/no-write — the pass
		// keeps the scene depth for water to test against.
		let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("blit"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::BLIT.into()),
		});
		let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("blit"),
			bind_group_layouts: &[&scene_bgl],
			push_constant_ranges: &[],
		});
		let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("blit"),
			layout: Some(&blit_layout),
			vertex: wgpu::VertexState {
				module: &blit_shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: None,
				..Default::default()
			},
			depth_stencil: Some(wgpu::DepthStencilState {
				format: DEPTH_FORMAT,
				depth_write_enabled: false,
				depth_compare: wgpu::CompareFunction::Always,
				stencil: Default::default(),
				bias: Default::default(),
			}),
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &blit_shader,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_format,
					blend: None,
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			multiview: None,
			cache: None,
		});

		let shadow_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("shadow"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SHADOW.into()),
		});
		let shadow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("shadow"),
			layout: Some(&shadow_layout),
			vertex: wgpu::VertexState {
				module: &shadow_shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[wgpu::VertexBufferLayout {
					array_stride: 4,
					step_mode: wgpu::VertexStepMode::Vertex,
					attributes: &[wgpu::VertexAttribute {
						format: wgpu::VertexFormat::Uint32,
						offset: 0,
						shader_location: 0,
					}],
				}],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: Some(wgpu::Face::Back),
				..Default::default()
			},
			depth_stencil: Some(wgpu::DepthStencilState {
				format: DEPTH_FORMAT,
				depth_write_enabled: true,
				depth_compare: wgpu::CompareFunction::Less,
				stencil: Default::default(),
				// Replaces the Java heightmap biases (0.0005/0.0008) and
				// the 3° tilt hack. Tunable during M6 A/B.
				bias: wgpu::DepthBiasState {
					constant: 2,
					slope_scale: 2.0,
					clamp: 0.0,
				},
			}),
			multisample: Default::default(),
			fragment: None,
			multiview: None,
			cache: None,
		});

		let make_light = |label: &str| {
			let buf = device.create_buffer(&wgpu::BufferDescriptor {
				label: Some(label),
				size: 64,
				usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
				mapped_at_creation: false,
			});
			let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some(label),
				layout: &light_bgl,
				entries: &[wgpu::BindGroupEntry {
					binding: 0,
					resource: buf.as_entire_binding(),
				}],
			});
			(buf, bg)
		};
		let (light_near_buf, light_near_bg) = make_light("light near");
		let (light_wide_buf, light_wide_bg) = make_light("light wide");

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
		let frame_water_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("frame ubo (water)"),
			size: std::mem::size_of::<FrameUniforms>() as u64,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let frame_water_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("frame (water)"),
			layout: &frame_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: frame_water_buf.as_entire_binding(),
			}],
		});

		let chunk_slots = chunk_slots(draw_dist);
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

		let textures = BlockTextures::new(gpu, shadow_size);
		let tex_bgs = [0, 1].map(|i| {
			device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("textures"),
				layout: &tex_bgl,
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: wgpu::BindingResource::TextureView(&textures.array_view),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::Sampler(&textures.sampler),
					},
					wgpu::BindGroupEntry {
						binding: 2,
						resource: wgpu::BindingResource::TextureView(&textures.shadow_near_view),
					},
					wgpu::BindGroupEntry {
						binding: 3,
						resource: wgpu::BindingResource::TextureView(
							&textures.shadow_wide_views[i],
						),
					},
					wgpu::BindGroupEntry {
						binding: 4,
						resource: wgpu::BindingResource::Sampler(&textures.shadow_sampler),
					},
				],
			})
		});

		let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
			label: Some("shared quad indices"),
			contents: bytemuck::cast_slice(&quad_indices(MAX_QUADS)),
			usage: wgpu::BufferUsages::INDEX,
		});

		Self {
			solid_pipeline,
			water_pipeline,
			water_depth_pipeline,
			shadow_pipeline,
			sky_pipeline,
			frame_buf,
			frame_bg,
			frame_water_buf,
			frame_water_bg,
			tex_bgs,
			chunk_buf,
			chunk_bg,
			light_near_buf,
			light_near_bg,
			light_wide_buf,
			light_wide_bg,
			wide_refresh_needed: true,
			wide_front: 0,
			wide_active_vp: glam::Mat4::IDENTITY,
			wide_job: None,
			wide_front_valid: false,
			index_buf,
			textures,
			chunks: FxHashMap::default(),
			free_slots: Vec::new(),
			next_slot: 0,
			chunk_slots,
			gpu_buffer_bytes: 0,
			water_scratch: Vec::new(),
			gpu_timer: GpuTimer::new(gpu),
			overlay: Overlay::new(gpu, surface_format),
			// Draw dist 10 is the mobile config: fewer droplets there.
			spray: crate::spray::Spray::new(
				gpu,
				surface_format,
				DEPTH_FORMAT,
				&frame_bgl,
				if draw_dist <= 10 { 24 } else { 64 },
			),
			players: crate::players::RemotePlayersDraw::new(
				gpu,
				surface_format,
				DEPTH_FORMAT,
				&frame_bgl,
			),
			targets: None,
			scene_bgl,
			linear_sampler,
			blit_pipeline,
			surface_format,
		}
	}

	/// Latest GPU pass reading (2-3 frames latent); None when timestamps
	/// are unsupported or nothing has been read back yet.
	pub fn gpu_pass_ms(&self) -> Option<digblock_core::metrics::GpuPassMs> {
		self.gpu_timer.as_ref().and_then(|t| t.latest)
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
		assert!(slot < self.chunk_slots, "chunk UBO slots exhausted");
		let (ox, oy, oz) = pos.origin();
		gpu.queue.write_buffer(
			&self.chunk_buf,
			u64::from(slot) * CHUNK_STRIDE,
			bytemuck::cast_slice(&[ox, oy, oz, 0.0]),
		);
		// Solid vertices are one u32 word; water vertices are two.
		let mut make = |verts: &Vec<u32>, words_per_vert: usize| -> Option<(wgpu::Buffer, u32)> {
			if verts.is_empty() {
				return None;
			}
			let buf = gpu
				.device
				.create_buffer_init(&wgpu::util::BufferInitDescriptor {
					label: Some("chunk verts"),
					contents: bytemuck::cast_slice(verts),
					usage: wgpu::BufferUsages::VERTEX,
				});
			self.gpu_buffer_bytes += buf.size();
			let quads = (verts.len() / (4 * words_per_vert)) as u32;
			Some((buf, quads * 6))
		};
		let solid = make(&mesh.solid, 1);
		let water = make(&mesh.water, 2);
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

	/// This frame's remote-player instances: `[pos.xyz, yaw, rgb, spare]`.
	pub fn set_remote_players(&mut self, gpu: &Gpu, instances: &[[f32; 8]]) {
		self.players.set_instances(gpu, instances);
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
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
			view_formats: &[],
		});
		let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
		let depth_copy = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("solid depth copy"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: DEPTH_FORMAT,
			usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
			view_formats: &[],
		});
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
		let scene_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("scene targets"),
			layout: &self.scene_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(
						&depth_copy.create_view(&wgpu::TextureViewDescriptor::default()),
					),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&scene_view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		self.targets = Some(FrameTargets {
			size,
			depth,
			depth_view,
			depth_copy,
			scene_view,
			scene_bg,
		});
	}

	/// Encode the frame in one command encoder, fixed order — wide shadow
	/// (only when due) → near shadow → main pass. Shadow maps are written
	/// and consumed in the same frame (fixes the Java one-frame lag).
	#[allow(clippy::too_many_arguments)]
	pub fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		frame: &FrameUniforms,
		view_proj: glam::Mat4,
		near_vp: glam::Mat4,
		wide_vp: glam::Mat4,
		wide_due: bool,
		size: (u32, u32),
		overlay: Option<(&OverlayStats, &[f32])>,
	) -> RenderStats {
		self.ensure_targets(gpu, size);
		let mut stats = RenderStats {
			chunks_resident: self.chunks.len() as u32,
			wide_refreshed: wide_due || self.wide_refresh_needed,
			..Default::default()
		};
		let wide_due = wide_due || self.wide_refresh_needed;
		self.wide_refresh_needed = false;
		if wide_due {
			// Kick off (or restart) the sliced re-render of the back map.
			// light_wide_buf feeds only the shadow-pass encode, so it can
			// take the new VP now; the main pass keeps projecting with
			// wide_active_vp until the flip.
			self.wide_job = Some(WideJob {
				vp: wide_vp,
				list: self.chunks.keys().copied().collect(),
				next: 0,
			});
			gpu.queue.write_buffer(
				&self.light_wide_buf,
				0,
				bytemuck::bytes_of(&wide_vp.to_cols_array_2d()),
			);
		}
		// The front wide map lags the engine's wide_vp by up to one job
		// (a few frames); sample it with the VP it was rendered with. The
		// sampled index is captured HERE, before the slice encode below can
		// flip `wide_front` — map and VP must change together on the same
		// frame or far shadows mis-project for one frame (a visible dark
		// flash at every refresh). A just-completed map is therefore first
		// sampled on the next frame, paired with its VP.
		let wide_sample = self.wide_front;
		let mut frame = *frame;
		frame.light_wide = self.wide_active_vp.to_cols_array_2d();
		let frame = &frame;
		if let Some(t) = self.gpu_timer.as_mut() {
			t.begin_frame();
		}
		self.overlay.prepare(gpu, size, overlay);
		gpu.queue.write_buffer(
			&self.light_near_buf,
			0,
			bytemuck::bytes_of(&near_vp.to_cols_array_2d()),
		);
		// Chunk bounding-sphere frustum cull against the four side planes
		// (near/far deliberately ignored, like the Java
		// sphereInFrustumWithoutNearFar).
		let planes = side_planes(view_proj);
		let in_frustum = |pos: &ChunkPos| -> bool {
			let c = glam::Vec3::new(
				(pos.x * 16 + 8) as f32,
				(pos.y * 16 + 8) as f32,
				(pos.z * 16 + 8) as f32,
			);
			const RADIUS: f32 = 13.86; // 8·√3, chunk half-diagonal (Java)
			planes.iter().all(|p| p.truncate().dot(c) + p.w > -RADIUS)
		};
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

		if !self.wide_front_valid {
			// Startup / device rebuild: the front map is uninitialized;
			// clear it to depth 1.0 (fully lit) so sampling is sane until
			// the first refresh completes and flips.
			encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label: Some("shadow wide bootstrap clear"),
				color_attachments: &[],
				depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
					view: &self.textures.shadow_wide_views[self.wide_front],
					depth_ops: Some(wgpu::Operations {
						load: wgpu::LoadOp::Clear(1.0),
						store: wgpu::StoreOp::Store,
					}),
					stencil_ops: None,
				}),
				timestamp_writes: None,
				occlusion_query_set: None,
			});
			self.wide_front_valid = true;
		}
		if let Some(mut job) = self.wide_job.take() {
			let (draws, tris, done) = self.encode_wide_slice(&mut encoder, &mut job);
			stats.draw_calls += draws;
			stats.triangles += tris;
			if done {
				self.wide_front = 1 - self.wide_front;
				self.wide_active_vp = job.vp;
			} else {
				self.wide_job = Some(job);
			}
		}
		let (casters, tris) = self.encode_shadow_pass(&mut encoder, near_vp, Q_SHADOW_NEAR);
		stats.shadow_casters = casters;
		stats.draw_calls += casters;
		stats.triangles += tris;

		// Water chunk list, shared by both water subpasses below. The
		// depth prepass makes draw order irrelevant: no sorting.
		self.water_scratch.clear();
		self.water_scratch.extend(
			self.chunks
				.iter()
				.filter(|(pos, c)| c.water.is_some() && in_frustum(pos))
				.map(|(pos, _)| *pos),
		);
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
				pass.set_bind_group(1, &self.tex_bgs[wide_sample], &[]);
				pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
				for (pos, chunk) in &self.chunks {
					if !in_frustum(pos) {
						stats.chunks_culled += 1;
						continue;
					}
					if let Some((buf, index_count)) = &chunk.solid {
						pass.set_bind_group(
							2,
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
				// near-free on TBDR).
				pass.set_pipeline(&self.sky_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;
			}

			// Pre-water solid depth becomes sampleable: water reads it for
			// column thickness (absorption, soft shores) while the live
			// attachment goes on to take the water prepass's writes.
			encoder.copy_texture_to_texture(
				targets.depth.as_image_copy(),
				targets.depth_copy.as_image_copy(),
				wgpu::Extent3d {
					width: targets.size.0.max(1),
					height: targets.size.1.max(1),
					depth_or_array_layers: 1,
				},
			);

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
				pass.set_bind_group(0, &targets.scene_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;

				pass.set_bind_group(0, &self.frame_water_bg, &[]);
				pass.set_bind_group(1, &self.tex_bgs[wide_sample], &[]);
				pass.set_bind_group(3, &targets.scene_bg, &[]);
				pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
				for pipeline in [&self.water_depth_pipeline, &self.water_pipeline] {
					pass.set_pipeline(pipeline);
					for pos in &self.water_scratch {
						let Some(chunk) = self.chunks.get(pos) else {
							continue;
						};
						if let Some((buf, index_count)) = &chunk.water {
							pass.set_bind_group(
								2,
								&self.chunk_bg,
								&[(u64::from(chunk.slot) * CHUNK_STRIDE) as u32],
							);
							pass.set_vertex_buffer(0, buf.slice(..));
							pass.draw_indexed(0..*index_count, 0, 0..1);
							stats.water_draws += 1;
							stats.triangles += *index_count / 3;
						}
					}
				}

				// Spray billboards over the finished water (blends over
				// sheet and pool alike). Water left frame_water_bg bound.
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
			self.overlay.draw(&mut pass);
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

	/// Depth-only cascade render. Solid meshes only — water never casts
	/// shadows (Java depth passes drew solidMesh exclusively). Casters are
	/// culled against the light's own frustum.
	/// Near-cascade depth render (whole caster set, every frame).
	/// Returns (draw calls, triangles) encoded.
	fn encode_shadow_pass(
		&self,
		encoder: &mut wgpu::CommandEncoder,
		light_vp: glam::Mat4,
		ts_first: u32,
	) -> (u32, u32) {
		let planes = side_planes(light_vp);
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("shadow near"),
			color_attachments: &[],
			depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
				view: &self.textures.shadow_near_view,
				depth_ops: Some(wgpu::Operations {
					load: wgpu::LoadOp::Clear(1.0),
					store: wgpu::StoreOp::Store,
				}),
				stencil_ops: None,
			}),
			timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(ts_first)),
			occlusion_query_set: None,
		});
		pass.set_pipeline(&self.shadow_pipeline);
		pass.set_bind_group(0, &self.light_near_bg, &[]);
		pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
		let (mut draws, mut tris) = (0u32, 0u32);
		for (pos, chunk) in &self.chunks {
			if let Some(d) = Self::shadow_draw(&mut pass, &self.chunk_bg, planes, pos, chunk) {
				draws += 1;
				tris += d / 3;
			}
		}
		(draws, tris)
	}

	/// Encode the next slice of an in-progress wide refresh into the back
	/// map (cleared on the first slice, appended thereafter). Returns
	/// (draws, tris, done).
	fn encode_wide_slice(
		&self,
		encoder: &mut wgpu::CommandEncoder,
		job: &mut WideJob,
	) -> (u32, u32, bool) {
		let planes = side_planes(job.vp);
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("shadow wide slice"),
			color_attachments: &[],
			depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
				view: &self.textures.shadow_wide_views[1 - self.wide_front],
				depth_ops: Some(wgpu::Operations {
					load: if job.next == 0 {
						wgpu::LoadOp::Clear(1.0)
					} else {
						wgpu::LoadOp::Load
					},
					store: wgpu::StoreOp::Store,
				}),
				stencil_ops: None,
			}),
			timestamp_writes: self
				.gpu_timer
				.as_ref()
				.and_then(|t| t.writes(Q_SHADOW_WIDE)),
			occlusion_query_set: None,
		});
		pass.set_pipeline(&self.shadow_pipeline);
		pass.set_bind_group(0, &self.light_wide_bg, &[]);
		pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
		let (mut draws, mut tris) = (0u32, 0u32);
		while job.next < job.list.len() && draws < WIDE_SLICE_DRAWS {
			let pos = job.list[job.next];
			job.next += 1;
			// Evicted mid-job: skip.
			let Some(chunk) = self.chunks.get(&pos) else {
				continue;
			};
			if let Some(d) = Self::shadow_draw(&mut pass, &self.chunk_bg, planes, &pos, chunk) {
				draws += 1;
				tris += d / 3;
			}
		}
		(draws, tris, job.next >= job.list.len())
	}

	/// Cull-and-draw one chunk into a depth pass; returns the index count
	/// if drawn. Solid meshes only — water never casts shadows.
	fn shadow_draw(
		pass: &mut wgpu::RenderPass<'_>,
		chunk_bg: &wgpu::BindGroup,
		planes: [glam::Vec4; 4],
		pos: &ChunkPos,
		chunk: &ChunkGpu,
	) -> Option<u32> {
		let c = glam::Vec3::new(
			(pos.x * 16 + 8) as f32,
			(pos.y * 16 + 8) as f32,
			(pos.z * 16 + 8) as f32,
		);
		const RADIUS: f32 = 13.86;
		if !planes.iter().all(|p| p.truncate().dot(c) + p.w > -RADIUS) {
			return None;
		}
		let (buf, index_count) = chunk.solid.as_ref()?;
		pass.set_bind_group(
			1,
			chunk_bg,
			&[(u64::from(chunk.slot) * CHUNK_STRIDE) as u32],
		);
		pass.set_vertex_buffer(0, buf.slice(..));
		pass.draw_indexed(0..*index_count, 0, 0..1);
		Some(*index_count)
	}
}

/// Gribb–Hartmann frustum plane extraction: left/right/bottom/top only.
fn side_planes(m: glam::Mat4) -> [glam::Vec4; 4] {
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
