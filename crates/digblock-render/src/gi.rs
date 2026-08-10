//! RTGI probe volume (GI-DESIGN.md). P1 built the voxel window the
//! rays march; P2 makes the probes real: two toroidal 6-axis
//! ambient-cube cascades (C0 2-block spacing near, C1 8-block far),
//! ping-ponged rgba16float atlases updated by an amortized compute
//! trace (spherical-Fibonacci rays, coarse-skip DDA, sun + sky +
//! emissive radiance, infinite bounce by sampling last frame's atlas
//! at hits), an r32uint flags texture carrying per-probe age and the
//! toroidal self-heal checksum, and an edit-invalidation dispatch.
//! P3 added the half-res resolve pass (gi_resolve.wgsl) — RTGI is the
//! only lighting path; there is no flag.

use digblock_core::block::BlockType;
use digblock_core::chunk::ChunkPos;
use digblock_core::gi::{opacity, GiChunk, GiOpacity};
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_GI, Q_GI_TRACE};
use crate::renderer::FrameUniforms;

/// C0 probe grid (2-block spacing near cascade; see GI-DESIGN.md).
pub const C0_DIMS: (u32, u32, u32) = (32, 24, 32);
/// C1 far cascade (8-block spacing).
pub const C1_DIMS: (u32, u32, u32) = (32, 20, 32);
/// Probes traced per cascade per frame (the amortization grain).
const PROBES_PER_DISPATCH: u32 = 2048;
/// World height in blocks — the window's fixed Y extent (an up-ray
/// always reaches sky; the window never scrolls vertically).
const WORLD_H: u32 = 256;
/// Light-list capacity (GI-DESIGN.md): nearest emitters win.
const MAX_LIGHTS: usize = 256;

/// Cosine of the sun cone's half-angle for the per-pixel shadow ray.
/// The real sun is about 0.27 degrees of angular radius; this is 0.5,
/// roughly twice that, which buys a penumbra that is sharp at contact
/// and softens with distance to the occluder — the thing a shadow map
/// never had — at no extra ray. Widen it and shadows get mushy AND
/// noisier, since one sample has to cover more solid angle.
const SUN_CONE_COS: f32 = 0.999_961_9; // cos(0.5 degrees)

/// Below this cos between consecutive caster directions, the sun did not
/// drift — it was TELEPORTED (`/time`). ~5.7 degrees is far more than the
/// world clock moves in a frame and far less than any admin jump.
const SUN_JUMP_COS: f32 = 0.995;
/// Sun-direction quantization for GI rays: ~1.5° so shadow work stays
/// still between steps and hysteresis glides across them.
const SUN_QUANT: f32 = 38.0;

struct Cascade {
	dims: (u32, u32, u32),
	spacing: u32,
	cursor: u32,
	/// Permutation stride for the sweep (see `schedule_stride`).
	stride: u32,
	uniform_buf: wgpu::Buffer,
	atlas: [wgpu::Texture; 2],
	/// bind[i]: atlas[i] is prev (sampled), atlas[1-i] is out (written).
	bind: [wgpu::BindGroup; 2],
}

impl Cascade {
	fn probes(&self) -> u32 {
		self.dims.0 * self.dims.1 * self.dims.2
	}
}

/// Stride for the permuted probe sweep: about the golden fraction of the
/// probe count, nudged until it is coprime with it, so `k -> k*stride mod
/// count` is a bijection and consecutive sweep steps land far apart.
/// Clamped so `wg.x * stride` cannot overflow u32 in the shader.
fn schedule_stride(count: u32) -> u32 {
	fn gcd(a: u32, b: u32) -> u32 {
		if b == 0 {
			a
		} else {
			gcd(b, a % b)
		}
	}
	let cap = u32::MAX / PROBES_PER_DISPATCH.max(1);
	let mut s = ((f64::from(count) * 0.618_033_988_749_895) as u32).clamp(1, cap.min(count.max(1)));
	// `s` and `count` share a factor at most `count` times; in practice
	// the first or second candidate lands.
	let mut tries = 0;
	while (gcd(s, count.max(1)) != 1 || s == 0) && tries < 64 {
		s = if s + 1 > cap { 1 } else { s + 1 };
		tries += 1;
	}
	if gcd(s, count.max(1)) != 1 {
		1
	} else {
		s
	}
}

pub struct GiDraw {
	probe_pipeline: wgpu::ComputePipeline,
	inval_pipeline: wgpu::ComputePipeline,
	/// One invocation sampling probes + sun visibility at the camera
	/// into `cam_buf` — the light rig for spray and the viewmodel.
	cam_pipeline: wgpu::ComputePipeline,
	cam_buf: wgpu::Buffer,
	cascades: [Cascade; 2],
	/// Which atlas is the front (last written) — flips per frame.
	ping: usize,
	frame: u32,
	window: wgpu::Texture,
	coarse: wgpu::Texture,
	/// XZ extent in chunks (= draw distance): the toroidal modulus.
	window_chunks: u32,
	emitters: FxHashMap<ChunkPos, Vec<(f32, f32, f32, u8)>>,
	/// Temporal state for the resolve's reprojection. `history_valid` is
	/// cleared whenever last frame's buffers cannot be trusted: the first
	/// frame, a renderer rebuild after device loss, or a sun jump (`/time`
	/// can rotate the caster 180 degrees between two frames, and without
	/// this the screen ghosts for seconds).
	prev_vp: [[f32; 4]; 4],
	prev_cam: [f32; 3],
	prev_sun: [f32; 3],
	history_valid: bool,
	frame_i: u32,
	lights_dirty: bool,
	lights_buf: wgpu::Buffer,
	/// Edit positions awaiting probe invalidation (≤4 consumed/frame).
	inval_queue: Vec<[f32; 3]>,
	/// The half-res resolve pass (gi_resolve.wgsl): probes + per-pixel
	/// sun DDA → the scene's two lighting inputs.
	resolve_pipeline: wgpu::RenderPipeline,
	resolve_bgl: wgpu::BindGroupLayout,
	resolve_buf: wgpu::Buffer,
	/// Bound at the resolve's empty group 1 (every slot in a pipeline
	/// layout must hold a bind group, empty or not).
	empty_bg: wgpu::BindGroup,
	/// gi_common.wgsl's group(2) — the GI world data every direct
	/// gatherer binds: the resolve, water, and players.
	gather_bgl: wgpu::BindGroupLayout,
	/// One per atlas ping; `gather_bind()` picks the front.
	gather_bind: [wgpu::BindGroup; 2],
	/// The scene pass's group 2: the two resolve outputs, textureLoad'd.
	scene_bgl: wgpu::BindGroupLayout,
	/// Last quantized sun (xyz dir + w above-horizon), set by `encode`
	/// and consumed by the resolve uniform in the same frame.
	last_sun: [f32; 4],
	/// Debug raymarch (`DIG_GI=window|probes`).
	debug_pipeline: wgpu::RenderPipeline,
	/// One per front atlas (the debug view samples C0's newest).
	debug_bind: [wgpu::BindGroup; 2],
	debug_buf: wgpu::Buffer,
	debug_probes: bool,
}

/// The 256-entry material LUT (GI-DESIGN.md): everything a ray needs
/// per class byte. Two words per entry: albedo rgb + opacity class in
/// the first, the cast-light level in the second (emission color is
/// the albedo — a torch glows its own warm tone).
fn material_lut(layer_means: &[[u8; 3]]) -> [[u32; 2]; 256] {
	let mut lut = [[0u32; 2]; 256];
	for b in BlockType::ALL {
		let id = b as usize;
		if id >= 256 || b == BlockType::Air {
			continue;
		}
		let p = b.props();
		let m = layer_means
			.get(p.tex_top as usize)
			.copied()
			.unwrap_or([255, 255, 255]);
		let op = match opacity(b) {
			GiOpacity::Air => 0u32,
			GiOpacity::Translucent => 1,
			GiOpacity::Opaque => 2,
		};
		lut[id] = [
			u32::from(m[0]) | (u32::from(m[1]) << 8) | (u32::from(m[2]) << 16) | (op << 24),
			u32::from(p.light),
		];
	}
	lut
}

fn atlas_tex(device: &wgpu::Device, dims: (u32, u32, u32), label: &str) -> wgpu::Texture {
	device.create_texture(&wgpu::TextureDescriptor {
		label: Some(label),
		size: wgpu::Extent3d {
			width: dims.0,
			height: dims.1,
			depth_or_array_layers: dims.2 * 6,
		},
		mip_level_count: 1,
		sample_count: 1,
		dimension: wgpu::TextureDimension::D3,
		format: wgpu::TextureFormat::Rgba16Float,
		usage: wgpu::TextureUsages::STORAGE_BINDING
			| wgpu::TextureUsages::TEXTURE_BINDING
			| wgpu::TextureUsages::COPY_SRC
			| wgpu::TextureUsages::COPY_DST,
		view_formats: &[],
	})
}

/// GiUniforms layout (must match gi_probe.wgsl): dims, cam, sun,
/// sun_color, zenith, horizon, sched, 4 inval spheres = 176 bytes.
const GI_UNIFORM_SIZE: u64 = 176;

/// The GI resolve's group 0 bind plus its two MRT attachments — one
/// argument instead of three, which is what keeps the encode signature
/// under clippy's limit as the pass grows.
pub struct ResolveTargets<'a> {
	pub bind: &'a wgpu::BindGroup,
	pub indirect_view: &'a wgpu::TextureView,
	pub sun_view: &'a wgpu::TextureView,
}

impl GiDraw {
	pub fn new(
		gpu: &Gpu,
		window_chunks: u32,
		layer_means: &[[u8; 3]],
		surface_format: wgpu::TextureFormat,
		debug_probes: bool,
	) -> Self {
		let device = &gpu.device;
		let side = window_chunks * 16;
		let window = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("gi voxel window"),
			size: wgpu::Extent3d {
				width: side,
				height: WORLD_H,
				depth_or_array_layers: side,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::R8Uint,
			usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let coarse = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("gi coarse occupancy"),
			size: wgpu::Extent3d {
				width: side / 8,
				height: WORLD_H / 8,
				depth_or_array_layers: side / 8,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::R8Uint,
			usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let lut = material_lut(layer_means);
		let lut_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi material lut"),
			size: (256 * 8) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue
			.write_buffer(&lut_buf, 0, bytemuck::cast_slice(&lut));
		let lights_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi light list"),
			size: (16 + MAX_LIGHTS * 16) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue.write_buffer(&lights_buf, 0, &[0u8; 16]);

		// One bind-group layout serves both compute entry points.
		let tex3 = |binding, sample_type| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::COMPUTE,
			ty: wgpu::BindingType::Texture {
				sample_type,
				view_dimension: wgpu::TextureViewDimension::D3,
				multisampled: false,
			},
			count: None,
		};
		let storage_buf = |binding| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::COMPUTE,
			ty: wgpu::BindingType::Buffer {
				ty: wgpu::BufferBindingType::Storage { read_only: true },
				has_dynamic_offset: false,
				min_binding_size: None,
			},
			count: None,
		};
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi probe bgl"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				tex3(1, wgpu::TextureSampleType::Uint),
				tex3(2, wgpu::TextureSampleType::Uint),
				storage_buf(3),
				storage_buf(4),
				tex3(5, wgpu::TextureSampleType::Float { filterable: false }),
				wgpu::BindGroupLayoutEntry {
					binding: 6,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::StorageTexture {
						access: wgpu::StorageTextureAccess::WriteOnly,
						format: wgpu::TextureFormat::Rgba16Float,
						view_dimension: wgpu::TextureViewDimension::D3,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 7,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::StorageTexture {
						access: wgpu::StorageTextureAccess::ReadWrite,
						format: wgpu::TextureFormat::R32Uint,
						view_dimension: wgpu::TextureViewDimension::D3,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 8,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: false },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
			],
		});
		// The camera-light sample cam_light writes and spray/viewmodel
		// bind as a uniform (STORAGE for the compute write, UNIFORM for
		// the render reads — same 32 bytes).
		let cam_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi camera light"),
			size: 32,
			usage: wgpu::BufferUsages::STORAGE
				| wgpu::BufferUsages::UNIFORM
				| wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue.write_buffer(&cam_buf, 0, &[0u8; 32]);
		let window_view = window.create_view(&Default::default());
		let coarse_view = coarse.create_view(&Default::default());

		let make_cascade = |dims: (u32, u32, u32), spacing: u32, name: &str| -> Cascade {
			let atlas = [
				atlas_tex(device, dims, &format!("gi atlas {name} a")),
				atlas_tex(device, dims, &format!("gi atlas {name} b")),
			];
			let flags = device.create_texture(&wgpu::TextureDescriptor {
				label: Some(&format!("gi flags {name}")),
				size: wgpu::Extent3d {
					width: dims.0,
					height: dims.1,
					depth_or_array_layers: dims.2,
				},
				mip_level_count: 1,
				sample_count: 1,
				dimension: wgpu::TextureDimension::D3,
				format: wgpu::TextureFormat::R32Uint,
				usage: wgpu::TextureUsages::STORAGE_BINDING,
				view_formats: &[],
			});
			let flags_view = flags.create_view(&Default::default());
			let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
				label: Some(&format!("gi uniforms {name}")),
				size: GI_UNIFORM_SIZE,
				usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
				mapped_at_creation: false,
			});
			let views = [
				atlas[0].create_view(&Default::default()),
				atlas[1].create_view(&Default::default()),
			];
			let bind = std::array::from_fn(|prev| {
				device.create_bind_group(&wgpu::BindGroupDescriptor {
					label: Some(&format!("gi probe bg {name} {prev}")),
					layout: &bgl,
					entries: &[
						wgpu::BindGroupEntry {
							binding: 0,
							resource: uniform_buf.as_entire_binding(),
						},
						wgpu::BindGroupEntry {
							binding: 1,
							resource: wgpu::BindingResource::TextureView(&window_view),
						},
						wgpu::BindGroupEntry {
							binding: 2,
							resource: wgpu::BindingResource::TextureView(&coarse_view),
						},
						wgpu::BindGroupEntry {
							binding: 3,
							resource: lut_buf.as_entire_binding(),
						},
						wgpu::BindGroupEntry {
							binding: 4,
							resource: lights_buf.as_entire_binding(),
						},
						wgpu::BindGroupEntry {
							binding: 5,
							resource: wgpu::BindingResource::TextureView(&views[prev]),
						},
						wgpu::BindGroupEntry {
							binding: 6,
							resource: wgpu::BindingResource::TextureView(&views[1 - prev]),
						},
						wgpu::BindGroupEntry {
							binding: 7,
							resource: wgpu::BindingResource::TextureView(&flags_view),
						},
						wgpu::BindGroupEntry {
							binding: 8,
							resource: cam_buf.as_entire_binding(),
						},
					],
				})
			});
			Cascade {
				dims,
				spacing,
				cursor: 0,
				stride: schedule_stride(dims.0 * dims.1 * dims.2),
				uniform_buf,
				atlas,
				bind,
			}
		};
		let cascades = [
			make_cascade(C0_DIMS, 2, "c0"),
			make_cascade(C1_DIMS, 8, "c1"),
		];

		let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi probe"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_PROBE.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi probe layout"),
			bind_group_layouts: &[&bgl],
			push_constant_ranges: &[],
		});
		let compute = |entry: &str, label: &str| {
			device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
				label: Some(label),
				layout: Some(&layout),
				module: &module,
				entry_point: Some(entry),
				compilation_options: Default::default(),
				cache: None,
			})
		};
		let probe_pipeline = compute("update_probes", "gi probe update");
		let inval_pipeline = compute("inval", "gi invalidate");
		let cam_pipeline = compute("cam_light", "gi camera light");

		// Debug raymarch pipeline (fullscreen, no depth, swapchain target).
		let debug_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi debug uniforms"),
			size: 96,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let ftex3 = |binding, sample_type, vis| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: vis,
			ty: wgpu::BindingType::Texture {
				sample_type,
				view_dimension: wgpu::TextureViewDimension::D3,
				multisampled: false,
			},
			count: None,
		};
		let debug_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi debug bgl"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				ftex3(
					1,
					wgpu::TextureSampleType::Uint,
					wgpu::ShaderStages::FRAGMENT,
				),
				ftex3(
					2,
					wgpu::TextureSampleType::Uint,
					wgpu::ShaderStages::FRAGMENT,
				),
				wgpu::BindGroupLayoutEntry {
					binding: 3,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				ftex3(
					4,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::ShaderStages::FRAGMENT,
				),
			],
		});
		let debug_bind = std::array::from_fn(|i| {
			let av = cascades[0].atlas[i].create_view(&Default::default());
			device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("gi debug bg"),
				layout: &debug_bgl,
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: debug_buf.as_entire_binding(),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&window_view),
					},
					wgpu::BindGroupEntry {
						binding: 2,
						resource: wgpu::BindingResource::TextureView(&coarse_view),
					},
					wgpu::BindGroupEntry {
						binding: 3,
						resource: lut_buf.as_entire_binding(),
					},
					wgpu::BindGroupEntry {
						binding: 4,
						resource: wgpu::BindingResource::TextureView(&av),
					},
				],
			})
		});
		let debug_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi debug"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_DEBUG.into()),
		});
		let debug_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi debug layout"),
			bind_group_layouts: &[&debug_bgl],
			push_constant_ranges: &[],
		});
		let debug_pipeline = crate::pipeline_builder::build_depthless(
			device,
			surface_format,
			&crate::pipeline_builder::PipelineDesc {
				label: "gi debug",
				layout: &debug_layout,
				module: &debug_module,
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

		// The half-res resolve (gi_resolve.wgsl): its own uniform + a
		// depth tap at group 0, the shared gather group at 2 (group 1
		// deliberately empty — gi_common.wgsl pins the world data to
		// group(2) because water/players have materials at 1); two MRT
		// outputs, no depth attachment, so it's built directly rather
		// than through the single-target pipeline funnel.
		let f_entry = |binding, ty| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::FRAGMENT,
			ty,
			count: None,
		};
		let ftex = |binding, sample_type, dim| {
			f_entry(
				binding,
				wgpu::BindingType::Texture {
					sample_type,
					view_dimension: dim,
					multisampled: false,
				},
			)
		};
		let resolve_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi resolve bgl"),
			entries: &[
				f_entry(
					0,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
				ftex(
					1,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				// The prepass's exact face-normal codes. Uint sample type
				// — the value is a table index, not a colour.
				ftex(
					2,
					wgpu::TextureSampleType::Uint,
					wgpu::TextureViewDimension::D2,
				),
				// Last frame's two outputs, ping-ponged in.
				ftex(
					3,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				ftex(
					4,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
			],
		});
		// gi_common.wgsl's group(2): the GI world data every direct
		// gatherer binds (the resolve here, water and players through
		// their pipeline layouts). One per atlas ping.
		let gather_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi gather bgl"),
			entries: &[
				ftex(
					0,
					wgpu::TextureSampleType::Uint,
					wgpu::TextureViewDimension::D3,
				),
				ftex(
					1,
					wgpu::TextureSampleType::Uint,
					wgpu::TextureViewDimension::D3,
				),
				f_entry(
					2,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
				ftex(
					3,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D3,
				),
				ftex(
					4,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D3,
				),
			],
		});
		let gather_bind = std::array::from_fn(|i| {
			let a0 = cascades[0].atlas[i].create_view(&Default::default());
			let a1 = cascades[1].atlas[i].create_view(&Default::default());
			device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("gi gather bg"),
				layout: &gather_bgl,
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: wgpu::BindingResource::TextureView(&window_view),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&coarse_view),
					},
					wgpu::BindGroupEntry {
						binding: 2,
						resource: lut_buf.as_entire_binding(),
					},
					wgpu::BindGroupEntry {
						binding: 3,
						resource: wgpu::BindingResource::TextureView(&a0),
					},
					wgpu::BindGroupEntry {
						binding: 4,
						resource: wgpu::BindingResource::TextureView(&a1),
					},
				],
			})
		});
		let resolve_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi resolve uniforms"),
			// 2 mat4 + 10 vec4. Grew at M4 by prev_view_proj, the previous
			// camera + history-valid flag, the exact caster, and the
			// sample-sequence lane.
			size: 288,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let resolve_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi resolve"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_RESOLVE.into()),
		});
		let empty_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi resolve empty"),
			entries: &[],
		});
		let empty_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi resolve empty"),
			layout: &empty_bgl,
			entries: &[],
		});
		let resolve_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi resolve layout"),
			bind_group_layouts: &[&resolve_bgl, &empty_bgl, &gather_bgl],
			push_constant_ranges: &[],
		});
		let resolve_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("gi resolve"),
			layout: Some(&resolve_layout),
			vertex: wgpu::VertexState {
				module: &resolve_module,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				..Default::default()
			},
			depth_stencil: None,
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &resolve_module,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				// Both f16: the sun target holds a running average now, and
				// rgba8 cannot (1/512 of a difference rounds to zero).
				targets: &[
					Some(wgpu::TextureFormat::Rgba16Float.into()),
					Some(wgpu::TextureFormat::Rgba16Float.into()),
				],
			}),
			multiview: None,
			cache: None,
		});
		// The scene pass's group 2: both resolve outputs, textureLoad'd
		// by chunk.wgsl's bilateral upsample.
		let scene_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi scene bgl"),
			entries: &[
				ftex(
					0,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				ftex(
					1,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
			],
		});

		Self {
			probe_pipeline,
			inval_pipeline,
			cam_pipeline,
			cam_buf,
			cascades,
			ping: 0,
			frame: 0,
			window,
			coarse,
			window_chunks,
			emitters: FxHashMap::default(),
			prev_vp: [[0.0; 4]; 4],
			prev_cam: [0.0; 3],
			prev_sun: [0.0; 3],
			history_valid: false,
			frame_i: 0,
			lights_dirty: false,
			lights_buf,
			inval_queue: Vec::new(),
			resolve_pipeline,
			resolve_bgl,
			resolve_buf,
			empty_bg,
			gather_bgl,
			gather_bind,
			scene_bgl,
			last_sun: [0.0, 1.0, 0.0, 1.0],
			debug_pipeline,
			debug_bind,
			debug_buf,
			debug_probes,
		}
	}

	pub fn scene_bgl(&self) -> &wgpu::BindGroupLayout {
		&self.scene_bgl
	}

	/// gi_common.wgsl's group(2) layout — water and players reference it
	/// in their pipeline layouts.
	pub fn gather_bgl(&self) -> &wgpu::BindGroupLayout {
		&self.gather_bgl
	}

	/// The GI world bind group reading the FRONT (just written) atlases
	/// — valid after this frame's `encode`.
	pub fn gather_bind(&self) -> &wgpu::BindGroup {
		&self.gather_bind[self.ping]
	}

	/// The 32-byte camera-light buffer `cam_light` writes — spray and
	/// the viewmodel bind it as a uniform.
	pub fn cam_light_buf(&self) -> &wgpu::Buffer {
		&self.cam_buf
	}

	/// Build the resolve's group-0 bind (uniform + the depth and
	/// face-normal taps) against the current targets — called whenever
	/// the frame targets rebuild.
	pub fn make_resolve_bind(
		&self,
		gpu: &Gpu,
		depth_copy_view: &wgpu::TextureView,
		normal_view: &wgpu::TextureView,
		hist_indirect: &wgpu::TextureView,
		hist_sun: &wgpu::TextureView,
	) -> wgpu::BindGroup {
		gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi resolve bg"),
			layout: &self.resolve_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: self.resolve_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(depth_copy_view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(normal_view),
				},
				wgpu::BindGroupEntry {
					binding: 3,
					resource: wgpu::BindingResource::TextureView(hist_indirect),
				},
				wgpu::BindGroupEntry {
					binding: 4,
					resource: wgpu::BindingResource::TextureView(hist_sun),
				},
			],
		})
	}

	/// Encode the half-res GI resolve into the two lighting targets.
	pub fn encode_resolve(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		frame: &FrameUniforms,
		out: ResolveTargets<'_>,
	) {
		let ResolveTargets {
			bind,
			indirect_view,
			sun_view,
		} = out;
		let side = (self.window_chunks * 16) as f32;
		let mut data = Vec::with_capacity(288);
		for col in frame.inv_view_proj {
			data.extend_from_slice(bytemuck::cast_slice(&col));
		}
		data.extend_from_slice(bytemuck::cast_slice(&[
			frame.cam_pos[0],
			frame.cam_pos[1],
			frame.cam_pos[2],
			side,
		]));
		let d0 = self.cascades[0].dims;
		let d1 = self.cascades[1].dims;
		data.extend_from_slice(bytemuck::cast_slice(&[
			d0.0 as f32,
			d0.1 as f32,
			d0.2 as f32,
			self.cascades[0].spacing as f32,
		]));
		data.extend_from_slice(bytemuck::cast_slice(&[
			d1.0 as f32,
			d1.1 as f32,
			d1.2 as f32,
			self.cascades[1].spacing as f32,
		]));
		data.extend_from_slice(bytemuck::cast_slice(&self.last_sun));
		data.extend_from_slice(bytemuck::cast_slice(&frame.light_color));
		data.extend_from_slice(bytemuck::cast_slice(&frame.zenith));
		data.extend_from_slice(bytemuck::cast_slice(&frame.horizon));
		for col in self.prev_vp {
			data.extend_from_slice(bytemuck::cast_slice(&col));
		}
		data.extend_from_slice(bytemuck::cast_slice(&[
			self.prev_cam[0],
			self.prev_cam[1],
			self.prev_cam[2],
			f32::from(self.history_valid),
		]));
		// The EXACT caster, unquantized. Quantization exists so probes
		// hold still between updates; on a per-pixel ray it is the
		// lattice resonance, so the resolve takes the real direction and
		// jitters it instead.
		let e = [
			-frame.light_dir[0],
			-frame.light_dir[1],
			-frame.light_dir[2],
		];
		let el = (e[0] * e[0] + e[1] * e[1] + e[2] * e[2]).sqrt().max(1e-6);
		data.extend_from_slice(bytemuck::cast_slice(&[
			e[0] / el,
			e[1] / el,
			e[2] / el,
			self.last_sun[3],
		]));
		data.extend_from_slice(bytemuck::cast_slice(&[
			self.frame_i as f32,
			SUN_CONE_COS,
			0.0f32,
			0.0f32,
		]));
		gpu.queue.write_buffer(&self.resolve_buf, 0, &data);
		let attach = |view| {
			Some(wgpu::RenderPassColorAttachment {
				view,
				resolve_target: None,
				ops: wgpu::Operations {
					load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
					store: wgpu::StoreOp::Store,
				},
				depth_slice: None,
			})
		};
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("gi resolve"),
			color_attachments: &[attach(indirect_view), attach(sun_view)],
			depth_stencil_attachment: None,
			timestamp_writes: timer.and_then(|t| t.writes(Q_GI_TRACE)),
			occlusion_query_set: None,
		});
		pass.set_pipeline(&self.resolve_pipeline);
		pass.set_bind_group(0, bind, &[]);
		pass.set_bind_group(1, &self.empty_bg, &[]);
		pass.set_bind_group(2, self.gather_bind(), &[]);
		pass.draw(0..3, 0..1);
		drop(pass);
		// Latch this frame as next frame's history. A large caster swing
		// between two frames is `/time`, not the sky turning, and screen
		// history that survives it ghosts for seconds.
		let jumped = e[0] * self.prev_sun[0] + e[1] * self.prev_sun[1] + e[2] * self.prev_sun[2]
			< SUN_JUMP_COS;
		self.history_valid = !jumped;
		self.prev_sun = [e[0] / el, e[1] / el, e[2] / el];
		self.prev_vp = frame.view_proj;
		self.prev_cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
		self.frame_i = self.frame_i.wrapping_add(1);
	}

	/// Drop the screen-space history: device loss, a resize, or anything
	/// else that leaves last frame's buffers describing another world.
	pub fn reset_history(&mut self) {
		self.history_valid = false;
	}

	/// Patch this frame's uniforms with the GI parameters the direct
	/// gatherers (water, players) read: quantized caster, cascade dims,
	/// window side. Also latches `last_sun` for this frame's `encode` /
	/// `encode_resolve`. Call BEFORE the frame UBO uploads.
	pub fn patch_frame(&mut self, frame: &mut FrameUniforms) {
		// Quantized caster: the ACTIVE light (sun by day, moon by night —
		// light_dir points toward the ground and light_color already
		// matches it), snapped so probe results hold still between steps
		// (~1.5°).
		let s = [
			-frame.light_dir[0],
			-frame.light_dir[1],
			-frame.light_dir[2],
		];
		let len = (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt().max(1e-6);
		let q = [
			(s[0] / len * SUN_QUANT).round() / SUN_QUANT,
			(s[1] / len * SUN_QUANT).round() / SUN_QUANT,
			(s[2] / len * SUN_QUANT).round() / SUN_QUANT,
		];
		let ql = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt().max(1e-6);
		let sun_up = if q[1] > 0.02 { 1.0f32 } else { 0.0 };
		self.last_sun = [q[0] / ql, q[1] / ql, q[2] / ql, sun_up];
		frame.gi_sun = self.last_sun;
		let d0 = self.cascades[0].dims;
		let d1 = self.cascades[1].dims;
		frame.gi0 = [
			d0.0 as f32,
			d0.1 as f32,
			d0.2 as f32,
			self.cascades[0].spacing as f32,
		];
		frame.gi1 = [
			d1.0 as f32,
			d1.1 as f32,
			d1.2 as f32,
			self.cascades[1].spacing as f32,
		];
		frame.gi_misc = [(self.window_chunks * 16) as f32, 0.0, 0.0, 0.0];
	}

	/// Upload one classified chunk into its toroidal window region (and
	/// its coarse bytes, and its emitter-list slice).
	pub fn upload_chunk(&mut self, gpu: &Gpu, pos: ChunkPos, gi: &GiChunk) {
		let wc = self.window_chunks as i32;
		let (tx, tz) = (pos.x.rem_euclid(wc) as u32, pos.z.rem_euclid(wc) as u32);
		let ty = pos.y as u32;
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture: &self.window,
				mip_level: 0,
				origin: wgpu::Origin3d {
					x: tx * 16,
					y: ty * 16,
					z: tz * 16,
				},
				aspect: wgpu::TextureAspect::All,
			},
			&gi.bytes[..],
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(16),
				rows_per_image: Some(16),
			},
			wgpu::Extent3d {
				width: 16,
				height: 16,
				depth_or_array_layers: 16,
			},
		);
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture: &self.coarse,
				mip_level: 0,
				origin: wgpu::Origin3d {
					x: tx * 2,
					y: ty * 2,
					z: tz * 2,
				},
				aspect: wgpu::TextureAspect::All,
			},
			&gi.coarse,
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(2),
				rows_per_image: Some(2),
			},
			wgpu::Extent3d {
				width: 2,
				height: 2,
				depth_or_array_layers: 2,
			},
		);
		let world = (
			pos.x as f32 * 16.0,
			pos.y as f32 * 16.0,
			pos.z as f32 * 16.0,
		);
		let list: Vec<_> = gi
			.emitters
			.iter()
			.map(|&(x, y, z, class)| {
				(
					world.0 + f32::from(x) + 0.5,
					world.1 + f32::from(y) + 0.5,
					world.2 + f32::from(z) + 0.5,
					class,
				)
			})
			.collect();
		let changed = if list.is_empty() {
			self.emitters.remove(&pos).is_some()
		} else {
			self.emitters.insert(pos, list);
			true
		};
		self.lights_dirty |= changed;
	}

	/// The voxel window's radius in chunks, AS BUILT.
	///
	/// The streaming side must key off this and not off the live
	/// `config.draw_dist`, which the settings screen mutates at runtime
	/// while the window texture keeps the size it was created with.
	/// Approving a chunk outside the built window hands `upload_chunk` a
	/// position whose `rem_euclid` wraps onto a DIFFERENT cell than the
	/// gather's does — silently writing one part of the world into
	/// another's slot, which is exactly the aliasing `gi_uploadable`
	/// exists to prevent.
	pub fn window_chunks(&self) -> i32 {
		self.window_chunks as i32
	}

	/// Queue an edit position for probe invalidation (age reset around
	/// it, ≤4 spheres applied per frame).
	pub fn invalidate(&mut self, pos: [f32; 3]) {
		// Bounded: only 4 spheres are drained per frame, and an unbounded
		// queue grows without limit under any burst of edits (a fluid
		// cascade, a bulk placement) and then spends minutes replaying
		// invalidations for cells that have since changed again. Dropping
		// the OLDEST is right — the newest edits are the ones whose light
		// is most wrong.
		const INVAL_MAX: usize = 256;
		if self.inval_queue.len() >= INVAL_MAX {
			self.inval_queue.remove(0);
		}
		self.inval_queue.push(pos);
	}

	/// Rebuild the flattened light list when it changed: the nearest
	/// MAX_LIGHTS emitters to the camera, count in the header.
	fn flush_lights(&mut self, gpu: &Gpu, cam: [f32; 3]) {
		if !self.lights_dirty {
			return;
		}
		self.lights_dirty = false;
		let mut all: Vec<(f32, [f32; 4])> = self
			.emitters
			.values()
			.flatten()
			.map(|&(x, y, z, class)| {
				let d2 = (x - cam[0]).powi(2) + (y - cam[1]).powi(2) + (z - cam[2]).powi(2);
				(d2, [x, y, z, f32::from_bits(u32::from(class))])
			})
			.collect();
		all.sort_by(|a, b| a.0.total_cmp(&b.0));
		all.truncate(MAX_LIGHTS);
		let count = all.len() as u32;
		let mut bytes = Vec::with_capacity(16 + all.len() * 16);
		bytes.extend_from_slice(bytemuck::cast_slice(&[count, 0, 0, 0]));
		for (_, e) in &all {
			bytes.extend_from_slice(bytemuck::cast_slice(e));
		}
		gpu.queue.write_buffer(&self.lights_buf, 0, &bytes);
	}

	/// Encode this frame's probe updates (both cascades) at the head of
	/// the frame's encoder: full prev→out copy, then the amortized
	/// trace over PROBES_PER_DISPATCH probes, then edit invalidation.
	pub fn encode(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		frame: &FrameUniforms,
	) {
		let cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
		self.flush_lights(gpu, cam);
		// The quantized caster was latched by `patch_frame` (which the
		// renderer calls before the frame uploads, every frame).
		let spheres: Vec<[f32; 4]> = {
			let take = self.inval_queue.len().min(4);
			let drained: Vec<[f32; 3]> = self.inval_queue.drain(..take).collect();
			(0..4)
				.map(|i| {
					drained
						.get(i)
						.map(|p| [p[0], p[1], p[2], 0.0]) // radius per cascade below
						.unwrap_or([0.0; 4])
				})
				.collect()
		};
		let side = (self.window_chunks * 16) as f32;
		let has_inval = spheres.iter().any(|s| s != &[0.0; 4]);
		for ci in 0..2 {
			let c = &self.cascades[ci];
			let mut data = Vec::with_capacity(GI_UNIFORM_SIZE as usize);
			let dims = [c.dims.0, c.dims.1, c.dims.2, c.spacing];
			data.extend_from_slice(bytemuck::cast_slice(&dims));
			data.extend_from_slice(bytemuck::cast_slice(&[cam[0], cam[1], cam[2], side]));
			data.extend_from_slice(bytemuck::cast_slice(&self.last_sun));
			data.extend_from_slice(bytemuck::cast_slice(&frame.light_color));
			data.extend_from_slice(bytemuck::cast_slice(&frame.zenith));
			data.extend_from_slice(bytemuck::cast_slice(&frame.horizon));
			// The sweep is PERMUTED, not contiguous. `cursor` walks the
			// sweep index; the probe it selects is `(k * stride) % count`
			// with stride coprime to count, so a full cycle still touches
			// every probe exactly once while consecutive dispatches land
			// far apart. A contiguous cursor made convergence sweep the
			// volume as a visible band — dark regions appearing and
			// clearing at random-looking moments while flying.
			//
			// The base is reduced here because `cursor * stride` would
			// overflow u32 in the shader; `wg.x * stride` cannot, since
			// the stride is clamped against PROBES_PER_DISPATCH.
			let base = ((u64::from(c.cursor) * u64::from(c.stride)) % u64::from(c.probes())) as u32;
			data.extend_from_slice(bytemuck::cast_slice(&[
				base,
				self.frame,
				MAX_LIGHTS as u32,
				c.stride,
			]));
			let radius = if ci == 0 { 8.0f32 } else { 16.0 };
			for s in &spheres {
				let r = if s == &[0.0; 4] { 0.0 } else { radius };
				data.extend_from_slice(bytemuck::cast_slice(&[s[0], s[1], s[2], r]));
			}
			gpu.queue.write_buffer(&c.uniform_buf, 0, &data);
			// Full prev→out copy so untouched probes carry forward.
			encoder.copy_texture_to_texture(
				self.cascades[ci].atlas[self.ping].as_image_copy(),
				self.cascades[ci].atlas[1 - self.ping].as_image_copy(),
				wgpu::Extent3d {
					width: c.dims.0,
					height: c.dims.1,
					depth_or_array_layers: c.dims.2 * 6,
				},
			);
		}
		{
			let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
				label: Some("gi probes"),
				timestamp_writes: timer.and_then(|t| t.compute_writes(Q_GI)),
			});
			pass.set_pipeline(&self.probe_pipeline);
			for c in &self.cascades {
				pass.set_bind_group(0, &c.bind[self.ping], &[]);
				pass.dispatch_workgroups(PROBES_PER_DISPATCH.min(c.probes()), 1, 1);
			}
			// Camera light for spray/viewmodel: one invocation against
			// the C0 cascade (prev_atlas = last frame's front, gi.dims
			// = C0 — the camera always sits inside C0's coverage).
			pass.set_pipeline(&self.cam_pipeline);
			pass.set_bind_group(0, &self.cascades[0].bind[self.ping], &[]);
			pass.dispatch_workgroups(1, 1, 1);
			if has_inval {
				pass.set_pipeline(&self.inval_pipeline);
				for c in &self.cascades {
					pass.set_bind_group(0, &c.bind[self.ping], &[]);
					pass.dispatch_workgroups(c.probes().div_ceil(64), 1, 1);
				}
			}
		}
		for c in &mut self.cascades {
			let n = c.probes();
			c.cursor = (c.cursor + PROBES_PER_DISPATCH.min(n)) % n;
		}
		self.ping = 1 - self.ping;
		self.frame = self.frame.wrapping_add(1);
	}

	/// Draw the debug raymarch over the whole target (`DIG_GI=`).
	pub fn encode_debug(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		target: &wgpu::TextureView,
		inv_view_proj: [[f32; 4]; 4],
		cam_pos: [f32; 3],
	) {
		let side = (self.window_chunks * 16) as f32;
		let mut data = Vec::with_capacity(96);
		for col in inv_view_proj {
			data.extend_from_slice(bytemuck::cast_slice(&col));
		}
		data.extend_from_slice(bytemuck::cast_slice(&[
			cam_pos[0], cam_pos[1], cam_pos[2], side,
		]));
		// misc.x: 1 = shade hits with the C0 probe field (DIG_GI=probes).
		let mode = if self.debug_probes { 1.0f32 } else { 0.0 };
		data.extend_from_slice(bytemuck::cast_slice(&[
			mode,
			C0_DIMS.0 as f32,
			C0_DIMS.1 as f32,
			C0_DIMS.2 as f32,
		]));
		gpu.queue.write_buffer(&self.debug_buf, 0, &data);
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("gi debug"),
			color_attachments: &[Some(wgpu::RenderPassColorAttachment {
				view: target,
				depth_slice: None,
				resolve_target: None,
				ops: wgpu::Operations {
					load: wgpu::LoadOp::Load,
					store: wgpu::StoreOp::Store,
				},
			})],
			depth_stencil_attachment: None,
			timestamp_writes: None,
			occlusion_query_set: None,
		});
		pass.set_pipeline(&self.debug_pipeline);
		// The front atlas is the one the flip just published.
		pass.set_bind_group(0, &self.debug_bind[self.ping], &[]);
		pass.draw(0..3, 0..1);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The sweep must be a PERMUTATION, not merely a shuffle: over one
	/// full cycle every probe has to be traced exactly once, or some
	/// probes never converge and others burn budget twice. That is the
	/// whole reason the stride has to be coprime with the count, and it
	/// is the kind of property that fails silently — the picture just
	/// keeps a stale patch somewhere.
	#[test]
	fn the_permuted_sweep_visits_every_probe_exactly_once() {
		for dims in [C0_DIMS, C1_DIMS, (1, 1, 1), (7, 5, 3), (16, 16, 16)] {
			let count = dims.0 * dims.1 * dims.2;
			let stride = schedule_stride(count);
			let mut seen = vec![false; count as usize];
			for k in 0..count {
				let pi = ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
				assert!(!seen[pi as usize], "{dims:?}: probe {pi} traced twice");
				seen[pi as usize] = true;
			}
			assert!(
				seen.iter().all(|&s| s),
				"{dims:?}: a probe was never traced"
			);
		}
	}

	/// `wg.x * stride` is computed in u32 IN THE SHADER, where an
	/// overflow silently wraps and the permutation stops being one.
	#[test]
	fn the_stride_cannot_overflow_the_shaders_multiply() {
		for dims in [C0_DIMS, C1_DIMS, (64, 64, 64), (1, 1, 1)] {
			let count = dims.0 * dims.1 * dims.2;
			let stride = schedule_stride(count);
			assert!(
				u64::from(stride) * u64::from(PROBES_PER_DISPATCH) <= u64::from(u32::MAX),
				"{dims:?}: stride {stride} overflows"
			);
		}
	}

	/// A contiguous cursor is what made convergence sweep as a visible
	/// band. Consecutive sweep steps must land far apart in the probe
	/// grid — this is the property, not an implementation detail.
	#[test]
	fn consecutive_sweep_steps_are_not_neighbours() {
		let count = C0_DIMS.0 * C0_DIMS.1 * C0_DIMS.2;
		let stride = schedule_stride(count);
		let probe = |k: u32| ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
		let mut worst = u32::MAX;
		for k in 0..count.min(4096) {
			let d = probe(k).abs_diff(probe(k + 1));
			worst = worst.min(d.min(count - d));
		}
		assert!(
			worst > count / 8,
			"consecutive steps land {worst} apart out of {count} — that is a band"
		);
	}
}
