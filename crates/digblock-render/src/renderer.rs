//! Device-lifetime wgpu state. `Renderer::new` is a pure constructor —
//! device-loss recovery is "build a new one and re-upload meshes".

use digblock_core::chunk::ChunkPos;
use digblock_core::mesher::{quad_indices, MeshData};
use rustc_hash::FxHashMap;
use wgpu::util::DeviceExt;

use crate::texture::BlockTextures;
use digblock_platform::Gpu;

/// Worst-case faces per 16³ chunk mesh (checkerboard): 2048 blocks × 6.
const MAX_QUADS: usize = 12288;
/// Chunk-origin UBO slots (dynamic offsets, 256-byte stride).
const CHUNK_SLOTS: u32 = 4096;
const CHUNK_STRIDE: u64 = 256;

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FrameUniforms {
	pub view_proj: [[f32; 4]; 4],
	pub light_near: [[f32; 4]; 4],
	pub light_wide: [[f32; 4]; 4],
	pub light_dir: [f32; 4],
	/// x = alpha, y = shadows enabled, z = near shadow texel size, w unused.
	pub misc: [f32; 4],
}

pub struct ChunkGpu {
	pub solid: Option<(wgpu::Buffer, u32)>,
	pub water: Option<(wgpu::Buffer, u32)>,
	slot: u32,
}

pub struct Renderer {
	solid_pipeline: wgpu::RenderPipeline,
	frame_buf: wgpu::Buffer,
	frame_bg: wgpu::BindGroup,
	tex_bg: wgpu::BindGroup,
	chunk_buf: wgpu::Buffer,
	chunk_bg: wgpu::BindGroup,
	index_buf: wgpu::Buffer,
	// Read from M6 on (shadow passes render into the shadow views).
	#[allow(dead_code)]
	pub textures: BlockTextures,
	chunks: FxHashMap<ChunkPos, ChunkGpu>,
	free_slots: Vec<u32>,
	next_slot: u32,
	pub gpu_buffer_bytes: u64,
}

impl Renderer {
	pub fn new(gpu: &Gpu, surface_format: wgpu::TextureFormat, shadow_size: u32) -> Self {
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

		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("chunk"),
			bind_group_layouts: &[&frame_bgl, &tex_bgl, &chunk_bgl],
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
				buffers: &[vertex_layout],
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

		let chunk_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(CHUNK_SLOTS) * CHUNK_STRIDE,
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
		let tex_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
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
					resource: wgpu::BindingResource::TextureView(&textures.shadow_wide_view),
				},
				wgpu::BindGroupEntry {
					binding: 4,
					resource: wgpu::BindingResource::Sampler(&textures.shadow_sampler),
				},
			],
		});

		let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
			label: Some("shared quad indices"),
			contents: bytemuck::cast_slice(&quad_indices(MAX_QUADS)),
			usage: wgpu::BufferUsages::INDEX,
		});

		Self {
			solid_pipeline,
			frame_buf,
			frame_bg,
			tex_bg,
			chunk_buf,
			chunk_bg,
			index_buf,
			textures,
			chunks: FxHashMap::default(),
			free_slots: Vec::new(),
			next_slot: 0,
			gpu_buffer_bytes: 0,
		}
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
		assert!(slot < CHUNK_SLOTS, "chunk UBO slots exhausted");
		let (ox, oy, oz) = pos.origin();
		gpu.queue.write_buffer(
			&self.chunk_buf,
			u64::from(slot) * CHUNK_STRIDE,
			bytemuck::cast_slice(&[ox, oy, oz, 0.0]),
		);
		let mut make = |verts: &Vec<u32>| -> Option<(wgpu::Buffer, u32)> {
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
			let quads = (verts.len() / 4) as u32;
			Some((buf, quads * 6))
		};
		let solid = make(&mesh.solid);
		let water = make(&mesh.water);
		self.chunks.insert(pos, ChunkGpu { solid, water, slot });
	}

	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		if let Some(old) = self.chunks.remove(&pos) {
			for (buf, _) in old.solid.iter().chain(old.water.iter()) {
				self.gpu_buffer_bytes = self.gpu_buffer_bytes.saturating_sub(buf.size());
			}
			self.free_slots.push(old.slot);
		}
	}

	pub fn resident_chunks(&self) -> usize {
		self.chunks.len()
	}

	/// Encode the frame: (shadow passes arrive in M6) → main pass with
	/// solid geometry. Water pass lands in M7.
	pub fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		depth: &wgpu::TextureView,
		frame: &FrameUniforms,
	) {
		gpu.queue
			.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(frame));

		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor {
				label: Some("frame"),
			});
		{
			let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label: Some("main"),
				color_attachments: &[Some(wgpu::RenderPassColorAttachment {
					view: target,
					resolve_target: None,
					ops: wgpu::Operations {
						load: wgpu::LoadOp::Clear(crate::SKY),
						store: wgpu::StoreOp::Store,
					},
					depth_slice: None,
				})],
				depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
					view: depth,
					depth_ops: Some(wgpu::Operations {
						load: wgpu::LoadOp::Clear(1.0),
						store: wgpu::StoreOp::Store,
					}),
					stencil_ops: None,
				}),
				timestamp_writes: None,
				occlusion_query_set: None,
			});

			pass.set_pipeline(&self.solid_pipeline);
			pass.set_bind_group(0, &self.frame_bg, &[]);
			pass.set_bind_group(1, &self.tex_bg, &[]);
			pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
			for chunk in self.chunks.values() {
				if let Some((buf, index_count)) = &chunk.solid {
					pass.set_bind_group(
						2,
						&self.chunk_bg,
						&[(u64::from(chunk.slot) * CHUNK_STRIDE) as u32],
					);
					pass.set_vertex_buffer(0, buf.slice(..));
					pass.draw_indexed(0..*index_count, 0, 0..1);
				}
			}
		}
		gpu.queue.submit([encoder.finish()]);
	}
}
