//! Fullscreen procedural sky pass: one triangle at the NDC far plane,
//! depth-tested against the solids (LessEqual, no write) so occluded
//! fragments are free; drawn after solids, before water so water
//! blends over it.

use digblock_platform::Gpu;

pub(crate) struct SkyDraw {
	pipeline: wgpu::RenderPipeline,
}

impl SkyDraw {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("sky"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SKY.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("sky"),
			bind_group_layouts: &[Some(frame_bgl)],
			immediate_size: 0,
		});
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "sky",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				depth_write: false,
				depth_compare: wgpu::CompareFunction::LessEqual,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		Self { pipeline }
	}

	/// Draw into the scene pass; the frame bind group must be bound at 0.
	pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
		pass.set_pipeline(&self.pipeline);
		pass.draw(0..3, 0..1);
	}
}
