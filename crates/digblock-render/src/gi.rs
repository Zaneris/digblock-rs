//! RTGI probe volume (GI-DESIGN.md). P0: the crate's first compute
//! pipeline — the real C0 probe atlas, one timed dispatch per frame,
//! a placeholder kernel. P2 grows the trace (voxel window reads, ray
//! budget amortization, ping-pong); what P0 pins is the shape: the
//! atlas as six axis-major Z slabs, a small GI uniform block of its
//! own (never the pinned Frame layout), and a dispatch that lives in
//! the frame's single encoder ahead of every render pass.
//!
//! Constructed only when `EngineConfig::rtgi` is on — a flag-off build
//! allocates nothing and encodes nothing, which is what makes the P0
//! bench-parity gate trivial to trust.

use digblock_core::block::BlockType;
use digblock_core::chunk::ChunkPos;
use digblock_core::gi::{opacity, GiChunk, GiOpacity};
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_GI};

/// C0 probe grid (2-block spacing near cascade; see GI-DESIGN.md).
pub const C0_DIMS: (u32, u32, u32) = (32, 24, 32);
/// One probe = 64 threads today (P2 splits: threads = rays).
const WORKGROUP: u32 = 64;
/// World height in blocks — the window's fixed Y extent (an up-ray
/// always reaches sky; the window never scrolls vertically).
const WORLD_H: u32 = 256;
/// Light-list capacity (GI-DESIGN.md): nearest emitters win.
const MAX_LIGHTS: usize = 256;

pub struct GiDraw {
	pipeline: wgpu::ComputePipeline,
	bind_group: wgpu::BindGroup,
	uniform_buf: wgpu::Buffer,
	/// Frame counter riding the uniform: the amortization cursor and
	/// ray-rotation seed once P2 traces.
	frame: u32,
	/// Kept alive for the debug view and the P3 resolve pass; the
	/// compute writes it through `bind_group`.
	#[allow(dead_code)]
	atlas_view: wgpu::TextureView,
	/// The voxel window: R8Uint, one texel per block, XZ-toroidal at
	/// chunk granularity, Y = the whole world column.
	window: wgpu::Texture,
	/// The /8 coarse occupancy (bit 0 any blocker, bit 1 any emitter).
	coarse: wgpu::Texture,
	/// XZ extent in chunks (= draw distance): the toroidal modulus.
	window_chunks: u32,
	/// Per-chunk emitter lists feeding the flattened light list.
	emitters: FxHashMap<ChunkPos, Vec<(f32, f32, f32, u8)>>,
	lights_dirty: bool,
	lights_buf: wgpu::Buffer,
	/// Debug raymarch (`DIG_GI=window`): fullscreen DDA over the window
	/// — the P1 gate capture, and the first consumer of the exact
	/// march the P2 probe trace reuses.
	debug_pipeline: wgpu::RenderPipeline,
	debug_bind_group: wgpu::BindGroup,
	debug_buf: wgpu::Buffer,
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

impl GiDraw {
	pub fn new(
		gpu: &Gpu,
		window_chunks: u32,
		layer_means: &[[u8; 3]],
		surface_format: wgpu::TextureFormat,
	) -> Self {
		let device = &gpu.device;
		let (nx, ny, nz) = C0_DIMS;
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
		// Header (count + pad) then MAX_LIGHTS × (vec3f pos, u32 class).
		let lights_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi light list"),
			size: (16 + MAX_LIGHTS * 16) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue.write_buffer(&lights_buf, 0, &[0u8; 16]);
		let atlas = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("gi probe atlas c0"),
			size: wgpu::Extent3d {
				width: nx,
				height: ny,
				// Six axis slabs, stacked in Z.
				depth_or_array_layers: nz * 6,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::Rgba16Float,
			usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let atlas_view = atlas.create_view(&Default::default());
		let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi uniforms"),
			size: 16,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
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
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::StorageTexture {
						access: wgpu::StorageTextureAccess::WriteOnly,
						format: wgpu::TextureFormat::Rgba16Float,
						view_dimension: wgpu::TextureViewDimension::D3,
					},
					count: None,
				},
			],
		});
		let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi probe bg"),
			layout: &bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: uniform_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&atlas_view),
				},
			],
		});
		let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi probe"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_PROBE.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi probe layout"),
			bind_group_layouts: &[&bgl],
			push_constant_ranges: &[],
		});
		let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
			label: Some("gi probe update"),
			layout: Some(&layout),
			module: &module,
			entry_point: Some("update_probes"),
			compilation_options: Default::default(),
			cache: None,
		});

		// Debug raymarch pipeline (fullscreen, no depth, swapchain target).
		let debug_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi debug uniforms"),
			size: 96,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let tex3u = |binding| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::FRAGMENT,
			ty: wgpu::BindingType::Texture {
				sample_type: wgpu::TextureSampleType::Uint,
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
				tex3u(1),
				tex3u(2),
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
			],
		});
		let window_view = window.create_view(&Default::default());
		let coarse_view = coarse.create_view(&Default::default());
		let debug_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
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
			],
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

		Self {
			pipeline,
			bind_group,
			uniform_buf,
			frame: 0,
			atlas_view,
			window,
			coarse,
			window_chunks,
			emitters: FxHashMap::default(),
			lights_dirty: false,
			lights_buf,
			debug_pipeline,
			debug_bind_group,
			debug_buf,
		}
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

	/// Draw the debug raymarch over the whole target (`DIG_GI=window`).
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
		// The window's valid world range is the draw disc around the
		// camera; the shader clamps its march to it.
		data.extend_from_slice(bytemuck::cast_slice(&[0.0f32, 0.0, 0.0, 0.0]));
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
		pass.set_bind_group(0, &self.debug_bind_group, &[]);
		pass.draw(0..3, 0..1);
	}

	/// Encode this frame's probe update. Belongs at the head of the
	/// frame's encoder, before any render pass reads the atlas.
	pub fn encode(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		cam_pos: [f32; 3],
	) {
		self.flush_lights(gpu, cam_pos);
		let (nx, ny, nz) = C0_DIMS;
		gpu.queue.write_buffer(
			&self.uniform_buf,
			0,
			bytemuck::cast_slice(&[nx, ny, nz, self.frame]),
		);
		self.frame = self.frame.wrapping_add(1);
		let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
			label: Some("gi probes"),
			timestamp_writes: timer.and_then(|t| t.compute_writes(Q_GI)),
		});
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.bind_group, &[]);
		pass.dispatch_workgroups((nx * ny * nz).div_ceil(WORKGROUP), 1, 1);
	}
}
