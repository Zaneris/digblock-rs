//! Target-block highlight: the idle outline + hold-to-dig crack decal,
//! one instanced box (players.rs shape) drawn in the WATER pass after
//! the water surfaces — a box on a targeted source or a submerged block
//! must blend over the water, not vanish under its depth tint. The live
//! depth buffer holds water-surface depth there, so the hardware test is
//! Always and the shader occludes by hand against the pre-water solid
//! depth copy (bound through the water frame group). Never depth-writes.

use digblock_platform::Gpu;

pub struct HighlightDraw {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
	active: bool,
}

impl HighlightDraw {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		water_frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("highlight"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::HIGHLIGHT.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("highlight"),
			bind_group_layouts: &[water_frame_bgl],
			push_constant_ranges: &[],
		});
		// Translucent decal: depth compare Always (occlusion is manual,
		// against the solid depth copy — see the module doc), never write.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "highlight",
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
				cull: Some(wgpu::Face::Back),
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState::ALPHA_BLENDING),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("highlight instance"),
			size: 32,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
			active: false,
		}
	}

	/// `target` = (cell, dig progress); `None` progress = idle outline.
	pub fn set_target(&mut self, gpu: &Gpu, target: Option<((i32, i32, i32), Option<f32>)>) {
		self.active = target.is_some();
		let Some((cell, progress)) = target else {
			return;
		};
		// Seed from the cell so each block cracks with its own pattern.
		let seed = (cell.0 as u32)
			.wrapping_mul(73_856_093)
			.wrapping_add((cell.1 as u32).wrapping_mul(19_349_663))
			.wrapping_add((cell.2 as u32).wrapping_mul(83_492_791))
			& 0xFFFF;
		let data: [f32; 8] = [
			cell.0 as f32,
			cell.1 as f32,
			cell.2 as f32,
			progress.unwrap_or(0.0),
			seed as f32,
			0.0,
			0.0,
			0.0,
		];
		gpu.queue
			.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&data));
	}

	/// Draw in the water pass, after the water surfaces (the water frame
	/// bind group at 0 — it carries the solid depth copy the shader
	/// occludes against). Returns triangles drawn.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) -> u32 {
		if !self.active {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..36, 0..1);
		12
	}
}
