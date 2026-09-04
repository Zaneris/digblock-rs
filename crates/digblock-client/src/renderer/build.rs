//! Pipeline and bind-group construction. A pure constructor by
//! design: device-loss recovery is "build a new Renderer and re-upload
//! the meshes", which only works while nothing here reads live state.

use super::*;

impl Renderer {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		config: &RenderConfig,
	) -> Self {
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

		// Group 1: block materials — texture array + sampler, one static
		// bind group shared by the solid and water pipelines.
		let materials_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("materials"),
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
			],
		});

		// Water group 0: the frame UBO (water copy) plus the scene targets — the pre-water solid
		// depth copy (textureLoad, no sampler), the offscreen scene color, and a linear sampler for
		// refraction/SSR offsets. Folding the scene targets into group 0 keeps the water pipelines
		// within the default bind-group limit while freeing group 2 for the GI world data.
		let water_frame_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("water frame + scene targets"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						// R32Float depth resolve — textureLoad only.
						sample_type: wgpu::TextureSampleType::Float { filterable: false },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 3,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count: None,
				},
			],
		});

		// Blit group 0: scene color + linear sampler (the shader fetches
		// with textureLoad; the sampler entry is there for a future
		// filtered composite).
		let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("blit"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
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
		let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("scene linear"),
			address_mode_u: wgpu::AddressMode::ClampToEdge,
			address_mode_v: wgpu::AddressMode::ClampToEdge,
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			..Default::default()
		});

		// Textures + the GI system come before the pipelines: the solid
		// layout references the GI scene bind-group layout, and GiDraw's
		// material LUT needs the per-layer means the atlas upload computes.
		let mut ledger = GpuLedger::default();
		let textures = BlockTextures::new(gpu, &mut ledger);
		let gi = crate::gi::GiDraw::new(
			gpu,
			crate::gi::window_chunks_for(config.draw_dist),
			&textures.layer_means,
			surface_format,
			crate::gi::GiConfig {
				debug_probes: config.debug_gi_probes,
				stub: config.stub,
				gather_iters: config.gi_gather_iters,
				sun_iters: config.gi_sun_iters,
				rate_blocks: config.gi_rate_blocks,
			},
			&mut ledger,
		);

		// Shared group scheme: 0 = frame, 1 = materials, 2 = GI. For the solids that's the two
		// resolve outputs; water and players carry the raw GI world data instead
		// (`GiDraw::gather_bgl`) and gather per fragment, since their surfaces aren't in the depth
		// prepass the resolve maps. Chunk origins are NOT a bind group — they arrive as the
		// instance-step vertex stream at slot 1. Water swaps group 0 for its frame+scene layout.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("chunk"),
			bind_group_layouts: &[Some(&frame_bgl), Some(&materials_bgl), Some(gi.scene_bgl())],
			immediate_size: 0,
		});
		let water_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("water"),
			bind_group_layouts: &[
				Some(&water_frame_bgl),
				Some(&materials_bgl),
				Some(gi.gather_bgl()),
			],
			immediate_size: 0,
		});

		let chunk_vertex_layouts = [geometry_vertex_layout(), origin_vertex_layout()];

		let solid_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "solid",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &chunk_vertex_layouts,
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				// The depth prepass wrote these exact values (vs_depth is
				// the same transform on the same stream).
				depth_compare: wgpu::CompareFunction::LessEqual,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		// Depth prepass: same vertex stream, plus a fragment stage rasterizing ONE face-normal code
		// per pixel into an R8Uint target. That costs the double-rate Z a position-only pass would
		// have had, and buys two things the GI could not get otherwise: an exact normal for the
		// resolve (its derivative-derived one was garbage on every silhouette) and a code
		// bit-identical to chunk.wgsl's, so the bilateral upsample stops collapsing at edges.
		let prepass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("depth prepass"),
			bind_group_layouts: &[Some(&frame_bgl)],
			immediate_size: 0,
		});
		let prepass_pipeline = crate::pipeline_builder::build(
			device,
			Some(NORMAL_FORMAT),
			&crate::pipeline_builder::PipelineDesc {
				label: "depth prepass",
				layout: &prepass_layout,
				module: &shader,
				vs_entry: "vs_depth",
				fs_entry: Some("fs_depth"),
				vertex_buffers: &chunk_vertex_layouts,
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		let water = WaterDraw::new(gpu, surface_format, &water_layout);
		// Glass shares water's pipeline layout: its shader declares only
		// group 0's frame UBO out of the water frame group, which is a
		// legal subset — and sharing means the pass binds nothing new
		// between the water draw and this one.
		let glass = crate::glass::GlassDraw::new(gpu, surface_format, &water_layout);
		let sky = SkyDraw::new(gpu, surface_format, &frame_bgl, &mut ledger);

		// Fullscreen composite of the offscreen scene into the swapchain:
		// first draw of the water pass. Depth Always/no-write — the pass
		// keeps the scene depth for water to test against.
		let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("blit"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::BLIT.into()),
		});
		let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("blit"),
			bind_group_layouts: &[Some(&blit_bgl)],
			immediate_size: 0,
		});
		let blit_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "blit",
				layout: &blit_layout,
				module: &blit_shader,
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

		// The upscale: the same shader and layout as `blit_pipeline`, but DEPTHLESS. A pipeline
		// `build` gave a depth-stencil state may only run in a pass that has a depth attachment,
		// which is why `blit_pipeline` works inside the water pass and silently invalidates the
		// encoder in a standalone fullscreen pass. An invalid pass poisons the whole encoder, so
		// `finish()` drops EVERY pass and the window goes black rather than showing a wrong blit.
		let upscale_pipeline = crate::pipeline_builder::build_depthless(
			device,
			surface_format,
			&crate::pipeline_builder::PipelineDesc {
				label: "upscale",
				layout: &blit_layout,
				module: &blit_shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				// Ignored — the pass has no depth attachment.
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		// Depth resolve: one fullscreen draw filling the R32Float depth
		// copy from the live attachment (WebGPU forbids
		// copy_texture_to_texture into a depth32float destination).
		let depth_blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("depth blit"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::FRAGMENT,
				ty: wgpu::BindingType::Texture {
					sample_type: wgpu::TextureSampleType::Depth,
					view_dimension: wgpu::TextureViewDimension::D2,
					multisampled: false,
				},
				count: None,
			}],
		});
		let depth_blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("depth blit"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::DEPTH_BLIT.into()),
		});
		let depth_blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("depth blit"),
			bind_group_layouts: &[Some(&depth_blit_bgl)],
			immediate_size: 0,
		});
		let depth_blit_pipeline = crate::pipeline_builder::build_depthless(
			device,
			wgpu::TextureFormat::R32Float,
			&crate::pipeline_builder::PipelineDesc {
				label: "depth blit",
				layout: &depth_blit_layout,
				module: &depth_blit_shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				// Ignored — the pass has no depth attachment.
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

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

		let chunk_slots = config
			.chunk_slots_override
			.map(|n| n.max(1))
			.unwrap_or_else(|| chunk_slots(config.draw_dist));
		let origin_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(chunk_slots) * ORIGIN_STRIDE,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});

		// Consumed here: the bind group holds the texture/sampler refs.
		let materials_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("materials"),
			layout: &materials_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&textures.array_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&textures.sampler),
				},
			],
		});

		let index_buf = buffer_init(
			gpu,
			"shared quad indices",
			wgpu::BufferUsages::INDEX,
			bytemuck::cast_slice(&quad_indices(MAX_QUADS)),
		);

		Self {
			solid_pipeline,
			prepass_pipeline,
			water,
			glass,
			sky,
			frame_buf,
			frame_bg,
			materials_bg,
			origin_buf,
			origins: Vec::new(),
			index_buf,
			chunks: Vec::new(),
			by_pos: FxHashMap::default(),
			pool: Vec::new(),
			pool_bytes: 0,
			lists: CullLists::default(),
			free_slots: Vec::new(),
			next_slot: 0,
			chunk_slots,
			ledger,
			block_texture: textures.texture,
			gpu_timer: GpuTimer::new(gpu),
			debug_gi: config.debug_gi,
			stub: config.stub,
			overlay: Overlay::new(gpu, surface_format),
			spray: crate::spray::Spray::new(
				gpu,
				surface_format,
				&frame_bgl,
				gi.cam_light_buf(),
				config.spray_droplets,
			),
			confetti: crate::confetti::Confetti::new(
				gpu,
				surface_format,
				&frame_bgl,
				gi.cam_light_buf(),
				gi.gather_bgl(),
			),
			bursts: crate::bursts::Bursts::new(
				gpu,
				surface_format,
				&frame_bgl,
				gi.cam_light_buf(),
				gi.gather_bgl(),
			),
			siphon: crate::siphon::Siphon::new(gpu, surface_format, &frame_bgl),
			teleport: crate::teleport::Teleport::new(gpu, surface_format, &frame_bgl),
			waystone_fx: crate::waystone_fx::WaystoneFx::new(gpu, surface_format, &frame_bgl),
			players: crate::players::RemotePlayersDraw::new(
				gpu,
				surface_format,
				&frame_bgl,
				&materials_bgl,
				gi.gather_bgl(),
			),
			highlight: crate::highlight::HighlightDraw::new(gpu, surface_format, &water_frame_bgl),
			viewmodel: crate::viewmodel::ViewmodelDraw::new(
				gpu,
				surface_format,
				&materials_bgl,
				gi.cam_light_buf(),
			),
			gi,
			ui_icons: crate::ui::icons::UiIcons::new(gpu, surface_format, &materials_bgl),
			targets: None,
			water_frame_bgl,
			blit_bgl,
			depth_blit_bgl,
			linear_sampler,
			blit_pipeline,
			upscale_pipeline,
			depth_blit_pipeline,
			surface_format,
		}
	}
}
