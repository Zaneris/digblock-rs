//! Every GI pipeline, texture and bind group, built once. Pure
//! construction, so device-loss recovery is a fresh `GiDraw`.

use super::*;

/// The `RenderConfig` slice GiDraw consumes at construction — one bundle
/// so a new knob is a field here, not another positional argument.
pub(crate) struct GiConfig {
	pub debug_probes: bool,
	pub stub: u32,
	pub gather_iters: u32,
	pub sun_iters: u32,
}

impl GiDraw {
	pub(crate) fn new(
		gpu: &Gpu,
		window_chunks: u32,
		layer_means: &[[u8; 4]],
		surface_format: wgpu::TextureFormat,
		cfg: GiConfig,
	) -> Self {
		let GiConfig {
			debug_probes,
			stub,
			gather_iters,
			sun_iters,
		} = cfg;
		let device = &gpu.device;
		let queue = &gpu.queue;
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
			size: (256 * 16) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue
			.write_buffer(&lut_buf, 0, bytemuck::cast_slice(&lut));
		let lights_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi light list"),
			// A header vec4, then TWO vec4s an entry: the position and
			// class word, then the rectangle a surface emitter radiates
			// from (zero for a point source).
			size: (16 + MAX_LIGHTS * 32) as u64,
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
		// the render reads — one buffer, both usages).
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
			make_cascade(C0_DIMS, C0_SPACING, "c0"),
			make_cascade(C1_DIMS, 8, "c1"),
		];

		let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi probe"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_PROBE.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi probe layout"),
			bind_group_layouts: &[Some(&bgl)],
			immediate_size: 0,
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
			bind_group_layouts: &[Some(&debug_bgl)],
			immediate_size: 0,
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

		// The half-res resolve (gi_resolve.wgsl): its own uniform + a depth tap at group 0, the
		// shared gather group at 2 (group 1 deliberately empty — gi_common.wgsl pins the world
		// data to group(2) because water/players have materials at 1); two MRT outputs, no depth
		// attachment, so it's built directly rather than through the single-target funnel.
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
				// The emitter list moved into the shared gather group (R33)
				// so players can run the same NEE the resolve does.
			],
		});
		// gi_common.wgsl's group(2): the GI world data plus the emitter list, everything a direct
		// gatherer binds (the resolve here, water, players and the confetti). One per atlas ping.
		// VERTEX visibility serves water's per-vertex gather (R17) and the confetti's per-flake
		// direct term (R34), and costs the fragment-only users nothing. The lights buffer lives
		// HERE rather than in the resolve's own group, so every consumer reads one declaration.
		let g_entry = |binding, ty| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
			ty,
			count: None,
		};
		let gtex = |binding, sample_type| {
			g_entry(
				binding,
				wgpu::BindingType::Texture {
					sample_type,
					view_dimension: wgpu::TextureViewDimension::D3,
					multisampled: false,
				},
			)
		};
		let gather_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi gather bgl"),
			entries: &[
				gtex(0, wgpu::TextureSampleType::Uint),
				gtex(1, wgpu::TextureSampleType::Uint),
				g_entry(
					2,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
				gtex(3, wgpu::TextureSampleType::Float { filterable: false }),
				gtex(4, wgpu::TextureSampleType::Float { filterable: false }),
				g_entry(
					5,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
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
					wgpu::BindGroupEntry {
						binding: 5,
						resource: lights_buf.as_entire_binding(),
					},
				],
			})
		});
		let resolve_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi resolve uniforms"),
			// The camera matrices, the resolve's vec4 block, the
			// invalidation spheres, then the ray-budget lane. Grew at M4 by
			// prev_view_proj, the previous camera + history-valid flag, the
			// exact caster, and the sample-sequence lane; the spheres came
			// with the deterministic sun (screen history follows a lighting
			// edit at the short rate).
			size: 368,
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
			bind_group_layouts: &[Some(&resolve_bgl), Some(&empty_bgl), Some(&gather_bgl)],
			immediate_size: 0,
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
				targets: &[Some(GI_ACC_FORMAT.into()), Some(GI_ACC_FORMAT.into())],
			}),
			multiview_mask: None,
			cache: None,
		});
		// The resolve's spatial half. It reads only what the two lighting
		// targets already carry, so its layout is a uniform and that pair —
		// no world bindings, no moments buffer (gi_atrous.wgsl derives both
		// sigmas analytically from the stored mean and sample count).
		let atrous_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi atrous bgl"),
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
				ftex(
					2,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
			],
		});
		// One buffer per iteration rather than one rewritten between them:
		// both passes are encoded into the same submission, so a single
		// buffer would hand BOTH of them whichever stride was written last.
		// The strides are compile-time constants anyway.
		let atrous_buf: [wgpu::Buffer; ATROUS_STEPS.len()] = std::array::from_fn(|i| {
			let b = device.create_buffer(&wgpu::BufferDescriptor {
				label: Some("gi atrous uniform"),
				size: 16,
				usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
				mapped_at_creation: false,
			});
			queue.write_buffer(
				&b,
				0,
				bytemuck::cast_slice(&[ATROUS_STEPS[i], 0.0, 0.0, 0.0]),
			);
			b
		});
		let atrous_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi atrous"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_ATROUS.into()),
		});
		let atrous_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi atrous layout"),
			bind_group_layouts: &[Some(&atrous_bgl)],
			immediate_size: 0,
		});
		let atrous_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("gi atrous"),
			layout: Some(&atrous_layout),
			vertex: wgpu::VertexState {
				module: &atrous_module,
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
				module: &atrous_module,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(GI_ACC_FORMAT.into()), Some(GI_ACC_FORMAT.into())],
			}),
			multiview_mask: None,
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
			coarse_mirror: vec![0u8; (side / 8 * (WORLD_H / 8) * (side / 8)) as usize]
				.into_boxed_slice(),
			coarse_mips_dirty: false,
			sky_y: 255.0,
			window_chunks,
			stub,
			gather_iters,
			sun_iters,
			emitters: FxHashMap::default(),
			surfaces: FxHashMap::default(),
			carried: Vec::new(),
			prev_vp: [[0.0; 4]; 4],
			prev_cam: [0.0; 3],
			prev_sun: [0.0; 3],
			history_valid: false,
			frame_i: 0,
			lights_dirty: false,
			lights_buf,
			lights_anchor: [f32::INFINITY; 3],
			lights_len: 0,
			light_cap: MAX_LIGHTS,
			inval_queue: Vec::new(),
			resolve_inval: Vec::new(),
			resolve_pipeline,
			resolve_bgl,
			resolve_buf,
			atrous_pipeline,
			atrous_bgl,
			atrous_buf,
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
}
