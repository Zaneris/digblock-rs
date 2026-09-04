//! Fullscreen procedural sky pass: one triangle at the NDC far plane,
//! depth-tested against the solids (LessEqual, no write) so occluded
//! fragments are free; drawn after solids, before water so water
//! blends over it. Owns the cloud shape volume (group 1) its march
//! samples; see cloud_noise.rs for the bake.

use digblock_platform::Gpu;

pub(crate) struct SkyDraw {
	pipeline: wgpu::RenderPipeline,
	cloud_bg: wgpu::BindGroup,
}

impl SkyDraw {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
		ledger: &mut crate::gpu_ledger::GpuLedger,
	) -> Self {
		let device = &gpu.device;
		let n = crate::cloud_noise::SIZE as u32;
		let extent = wgpu::Extent3d {
			width: n,
			height: n,
			depth_or_array_layers: n,
		};
		let cloud_tex = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("cloud shape"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::R8Unorm,
			usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
			view_formats: &[],
		});
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture: &cloud_tex,
				mip_level: 0,
				origin: wgpu::Origin3d::ZERO,
				aspect: wgpu::TextureAspect::All,
			},
			&crate::cloud_noise::volume(),
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(n),
				rows_per_image: Some(n),
			},
			extent,
		);
		ledger.charge_texture(crate::gpu_ledger::GpuKind::Textures, &cloud_tex);
		let cloud_view = cloud_tex.create_view(&Default::default());
		let cloud_smp = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("cloud sampler"),
			address_mode_u: wgpu::AddressMode::Repeat,
			address_mode_v: wgpu::AddressMode::Repeat,
			address_mode_w: wgpu::AddressMode::Repeat,
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			..Default::default()
		});
		let cloud_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("cloud shape"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D3,
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
		let cloud_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("cloud shape"),
			layout: &cloud_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&cloud_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&cloud_smp),
				},
			],
		});

		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("sky"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SKY.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("sky"),
			bind_group_layouts: &[Some(frame_bgl), Some(&cloud_bgl)],
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
		Self { pipeline, cloud_bg }
	}

	/// Draw into the scene pass; the frame bind group must be bound at 0.
	pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(1, &self.cloud_bg, &[]);
		pass.draw(0..3, 0..1);
	}
}
