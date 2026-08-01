//! Stateless waterfall spray/mist billboards. The CPU side only tracks
//! impact cells per chunk (fed by `MeshData::impacts` through the
//! normal upload path) and writes a culled instance list each frame;
//! all motion lives in spray.wgsl as a pure function of the animation
//! clock — nothing to save on suspend, nothing to rebuild on device
//! loss but the pipeline.

use digblock_core::chunk::ChunkPos;
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

/// Instance cap (nearest win); 16 B each — a fixed 4 KB buffer.
const MAX_INSTANCES: usize = 256;
/// CPU cull radius; matches the shader's 56..64 fade band so emitters
/// never pop.
const SPRAY_RADIUS: f32 = 64.0;

pub struct Spray {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
	/// World-space impact centers, only for chunks that have any.
	impacts: FxHashMap<ChunkPos, Vec<[f32; 3]>>,
	scratch: Vec<[f32; 4]>,
	/// Droplets per instance (particle 0 is the mist slab): 20 desktop,
	/// 8 on the mobile config.
	droplets: u32,
}

impl Spray {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		depth_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
		droplets: u32,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("spray"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SPRAY.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("spray"),
			bind_group_layouts: &[frame_bgl],
			push_constant_ranges: &[],
		});
		let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("spray"),
			layout: Some(&layout),
			vertex: wgpu::VertexState {
				module: &shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				// No geometry buffer: one instance-stepped attribute
				// (impact center + spare w); vertex_index does the rest.
				buffers: &[wgpu::VertexBufferLayout {
					array_stride: 16,
					step_mode: wgpu::VertexStepMode::Instance,
					attributes: &[wgpu::VertexAttribute {
						format: wgpu::VertexFormat::Float32x4,
						offset: 0,
						shader_location: 0,
					}],
				}],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: None,
				..Default::default()
			},
			// Occluded by terrain, never occludes anything itself.
			depth_stencil: Some(wgpu::DepthStencilState {
				format: depth_format,
				depth_write_enabled: false,
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
					blend: Some(wgpu::BlendState::ALPHA_BLENDING),
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			multiview: None,
			cache: None,
		});
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("spray instances"),
			size: (MAX_INSTANCES * 16) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
			impacts: FxHashMap::default(),
			scratch: Vec::new(),
			droplets,
		}
	}

	/// Record a chunk's impact cells (empty clears the entry). Fed from
	/// `Renderer::upload_chunk`, so emitters follow remeshes for free.
	pub fn set_chunk(&mut self, pos: ChunkPos, origin: (f32, f32, f32), impacts: &[u16]) {
		if impacts.is_empty() {
			self.impacts.remove(&pos);
			return;
		}
		let pts = impacts
			.iter()
			.map(|&p| {
				[
					origin.0 + (p & 0xf) as f32 + 0.5,
					origin.1 + ((p >> 4) & 0xf) as f32 + 0.1,
					origin.2 + ((p >> 8) & 0xf) as f32 + 0.5,
				]
			})
			.collect();
		self.impacts.insert(pos, pts);
	}

	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		self.impacts.remove(&pos);
	}

	/// Cull to the radius (nearest-first when over the cap) and upload
	/// the instance list. The map holds only splash-bearing chunks, so
	/// rebuilding every frame is microseconds and needs no dirty flags.
	pub fn prepare(&mut self, gpu: &Gpu, cam: glam::Vec3) -> u32 {
		self.scratch.clear();
		for pts in self.impacts.values() {
			for p in pts {
				let d2 = cam.distance_squared(glam::Vec3::from_array(*p));
				if d2 <= SPRAY_RADIUS * SPRAY_RADIUS {
					// Stash d² in the spare lane for the cap sort.
					self.scratch.push([p[0], p[1], p[2], d2]);
				}
			}
		}
		if self.scratch.len() > MAX_INSTANCES {
			self.scratch.sort_by(|a, b| a[3].total_cmp(&b[3]));
			self.scratch.truncate(MAX_INSTANCES);
		}
		for inst in &mut self.scratch {
			inst[3] = 1.0; // spare lane (future: fall-height strength)
		}
		if !self.scratch.is_empty() {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&self.scratch));
		}
		self.scratch.len() as u32
	}

	/// Draw after the water color subpass (same pass; the caller must
	/// have re-bound the frame bind group at 0). Returns triangles.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instances: u32) -> u32 {
		if instances == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * (1 + self.droplets), 0..instances);
		instances * (1 + self.droplets) * 2
	}
}
