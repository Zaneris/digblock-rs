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
/// Mist slabs per instance — keep in sync with spray.wgsl MIST_N.
const MIST_N: u32 = 3;
/// CPU cull radius; matches the shader's 56..64 fade band so emitters
/// never pop.
const SPRAY_RADIUS: f32 = 64.0;

pub struct Spray {
	pipeline: wgpu::RenderPipeline,
	/// Group 1: the RTGI camera-light buffer (gi_probe's cam_light) —
	/// billboards have no surface to gather at, so the camera's probe
	/// ambient + sun visibility light them.
	cam_bg: wgpu::BindGroup,
	instance_buf: wgpu::Buffer,
	/// World-space impact centers, only for chunks that have any.
	impacts: FxHashMap<ChunkPos, Vec<[f32; 3]>>,
	scratch: Vec<[f32; 4]>,
	/// Droplets per instance (on top of the MIST_N mist slabs):
	/// 128 desktop, 48 on the mobile config.
	droplets: u32,
}

impl Spray {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
		cam_light_buf: &wgpu::Buffer,
		droplets: u32,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("spray"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SPRAY.into()),
		});
		let cam_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("spray cam light"),
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
		let cam_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("spray cam light"),
			layout: &cam_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: cam_light_buf.as_entire_binding(),
			}],
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("spray"),
			bind_group_layouts: &[frame_bgl, &cam_bgl],
			push_constant_ranges: &[],
		});
		// No geometry buffer: one instance-stepped attribute (impact
		// center + spare w); vertex_index does the rest. Depth: occluded
		// by terrain, never occludes anything itself.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "spray",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: 16,
					step_mode: wgpu::VertexStepMode::Instance,
					attributes: &[wgpu::VertexAttribute {
						format: wgpu::VertexFormat::Float32x4,
						offset: 0,
						shader_location: 0,
					}],
				}],
				cull: None,
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState::ALPHA_BLENDING),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("spray instances"),
			size: (MAX_INSTANCES * 16) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			cam_bg,
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
		pass.set_bind_group(1, &self.cam_bg, &[]);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * (MIST_N + self.droplets), 0..instances);
		instances * (MIST_N + self.droplets) * 2
	}
}
