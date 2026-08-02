//! Remote-player rendering: instanced eye-anchored boxes, drawn in the
//! scene pass (depth-writing, opaque) so terrain occlusion, sky, water,
//! and fog all interact correctly for free. Follows the spray.rs shape:
//! no geometry buffer, one instance-stepped attribute pair.

use digblock_platform::Gpu;

/// Matches the u8 player-id space comfortably; 32 B per instance.
const MAX_INSTANCES: usize = 64;

pub struct RemotePlayersDraw {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
	count: u32,
}

impl RemotePlayersDraw {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("players"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::PLAYERS.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("players"),
			bind_group_layouts: &[frame_bgl],
			push_constant_ranges: &[],
		});
		// Opaque scene geometry: writes depth, occluded by terrain.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "players",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: 32,
					step_mode: wgpu::VertexStepMode::Instance,
					attributes: &[
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x4,
							offset: 0,
							shader_location: 0,
						},
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x4,
							offset: 16,
							shader_location: 1,
						},
					],
				}],
				cull: None,
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("player instances"),
			size: (MAX_INSTANCES * 32) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
			count: 0,
		}
	}

	/// Upload this frame's instances: `[pos.xyz, yaw, rgb, spare]` each.
	pub fn set_instances(&mut self, gpu: &Gpu, instances: &[[f32; 8]]) {
		let n = instances.len().min(MAX_INSTANCES);
		self.count = n as u32;
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&instances[..n]));
		}
	}

	/// Draw in the scene pass, after solids, before the sky (frame bind
	/// group must be bound at 0). Returns triangles drawn.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) -> u32 {
		if self.count == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..36, 0..self.count);
		self.count * 12
	}
}
