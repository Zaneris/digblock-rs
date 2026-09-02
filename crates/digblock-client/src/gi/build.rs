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
	pub rate_blocks: [f32; 2],
}

impl GiDraw {
	pub(crate) fn new(
		gpu: &Gpu,
		window_chunks: u32,
		layer_means: &[[u8; 4]],
		surface_format: wgpu::TextureFormat,
		cfg: GiConfig,
		ledger: &mut GpuLedger,
	) -> Self {
		let GiConfig {
			debug_probes,
			stub,
			gather_iters,
			sun_iters,
			rate_blocks,
		} = cfg;
		let device = &gpu.device;
		let queue = &gpu.queue;
		let side = window_chunks * 16;
		let (window, coarse) = window_textures(device, side, ledger);
		let lut = material_lut(layer_means);
		let lut_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi material lut"),
			size: (256 * 16) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue
			.write_buffer(&lut_buf, 0, bytemuck::cast_slice(&lut));
		let covers = cover_table();
		let covers_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi cover table"),
			size: (256 * 8) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue
			.write_buffer(&covers_buf, 0, bytemuck::cast_slice(&covers));
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
				storage_buf(9),
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
		let world = WorldBinds {
			window: &window_view,
			coarse: &coarse_view,
			lut: &lut_buf,
			covers: &covers_buf,
			lights: &lights_buf,
			cam: &cam_buf,
		};

		let make_cascade =
			|dims: (u32, u32, u32), spacing: u32, coarse_from: u32, name: &str| -> Cascade {
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
				let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
					label: Some(&format!("gi uniforms {name}")),
					size: GI_UNIFORM_SIZE,
					usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
					mapped_at_creation: false,
				});
				let bind = probe_binds(device, &bgl, name, &uniform_buf, &atlas, &flags, &world);
				Cascade {
					dims,
					spacing,
					coarse_from,
					cursor: 0,
					stride: schedule_stride(dims.0 * dims.1 * dims.2),
					uniform_buf,
					atlas,
					flags,
					bind,
				}
			};
		let cascades = [
			make_cascade(C0_DIMS, C0_SPACING, 16, "c0"),
			make_cascade(C1_DIMS, 8, 8, "c1"),
		];
		for c in &cascades {
			for tex in c.atlas.iter().chain([&c.flags]) {
				ledger.charge_texture(GpuKind::GiAtlas, tex);
			}
		}

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
		let debug_bind = debug_binds(device, &debug_bgl, &debug_buf, &cascades[0].atlas, &world);
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
				g_entry(
					6,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
			],
		});
		let gather_bind = gather_binds(device, &gather_bgl, &cascades, &world);
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
			coarse_mirror: CoarseMirror::new(window_chunks),
			coarse_mips_dirty: false,
			sky_y: 255.0,
			window_chunks,
			probe_bgl: bgl,
			debug_bgl,
			lut_buf,
			covers_buf,
			stub,
			gather_iters,
			sun_iters,
			rate_blocks,
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
			lights_reachable: 0,
			lights_floor: None,
			light_cap: MAX_LIGHTS,
			inval_queue: std::collections::VecDeque::new(),
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

	/// Rebuild the voxel window at a new side (`window_chunks_for`, the draw-distance slider).
	/// Only what holds the window's texels moves: the two textures, the CPU mirror and the bind
	/// groups that name them; the cascades keep their history and the light maps their entries.
	/// The fresh window is EMPTY, so the caller re-dirties every resident chunk
	/// (`Pipeline::gi_redirty_all`) — exactly the device-loss rebuild's second half.
	pub(crate) fn resize_window(&mut self, gpu: &Gpu, window_chunks: u32, ledger: &mut GpuLedger) {
		if window_chunks == self.window_chunks {
			return;
		}
		let device = &gpu.device;
		let side = window_chunks * 16;
		let (window, coarse) = window_textures(device, side, ledger);
		// The old pair is DESTROYED, not dropped — 200 MiB at draw 20 that a drop would strand
		// on the web (`gpu_ledger`). Safe here: the frame still reading it is already submitted,
		// and every bind group naming it is rebuilt below before the next encode.
		let old_window = std::mem::replace(&mut self.window, window);
		let old_coarse = std::mem::replace(&mut self.coarse, coarse);
		ledger.retire_texture(GpuKind::GiWindow, old_window);
		ledger.retire_texture(GpuKind::GiWindow, old_coarse);
		self.coarse_mirror = CoarseMirror::new(window_chunks);
		// As at construction: the ceiling sits at the world top until the
		// first upload lands, never at the empty mirror's answer.
		self.coarse_mips_dirty = false;
		self.sky_y = 255.0;
		self.window_chunks = window_chunks;
		let window_view = self.window.create_view(&Default::default());
		let coarse_view = self.coarse.create_view(&Default::default());
		let world = WorldBinds {
			window: &window_view,
			coarse: &coarse_view,
			lut: &self.lut_buf,
			covers: &self.covers_buf,
			lights: &self.lights_buf,
			cam: &self.cam_buf,
		};
		for (c, name) in self.cascades.iter_mut().zip(["c0", "c1"]) {
			c.bind = probe_binds(
				device,
				&self.probe_bgl,
				name,
				&c.uniform_buf,
				&c.atlas,
				&c.flags,
				&world,
			);
		}
		self.debug_bind = debug_binds(
			device,
			&self.debug_bgl,
			&self.debug_buf,
			&self.cascades[0].atlas,
			&world,
		);
		self.gather_bind = gather_binds(device, &self.gather_bgl, &self.cascades, &world);
		// The light reach is the window's half-extent (`lateral_max`), so
		// the list's membership moved with it.
		self.lights_dirty = true;
	}
}

/// Everything a window-dependent bind group names besides the cascade it serves — shared by
/// construction and `resize_window`, so the two cannot bind the world differently.
struct WorldBinds<'a> {
	window: &'a wgpu::TextureView,
	coarse: &'a wgpu::TextureView,
	lut: &'a wgpu::Buffer,
	covers: &'a wgpu::Buffer,
	lights: &'a wgpu::Buffer,
	cam: &'a wgpu::Buffer,
}

/// The per-block look and cover bytes (`core::gi::BYTES_PER_CELL`) and the
/// coarse occupancy over them, for a side in blocks, charged to the ledger.
fn window_textures(
	device: &wgpu::Device,
	side: u32,
	ledger: &mut GpuLedger,
) -> (wgpu::Texture, wgpu::Texture) {
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
		format: wgpu::TextureFormat::Rg8Uint,
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
		format: wgpu::TextureFormat::R32Uint,
		usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
		view_formats: &[],
	});
	ledger.charge_texture(GpuKind::GiWindow, &window);
	ledger.charge_texture(GpuKind::GiWindow, &coarse);
	(window, coarse)
}

/// One cascade's probe-update bind groups, per atlas ping.
fn probe_binds(
	device: &wgpu::Device,
	bgl: &wgpu::BindGroupLayout,
	name: &str,
	uniform_buf: &wgpu::Buffer,
	atlas: &[wgpu::Texture; 2],
	flags: &wgpu::Texture,
	world: &WorldBinds,
) -> [wgpu::BindGroup; 2] {
	let flags_view = flags.create_view(&Default::default());
	let views = [
		atlas[0].create_view(&Default::default()),
		atlas[1].create_view(&Default::default()),
	];
	std::array::from_fn(|prev| {
		device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some(&format!("gi probe bg {name} {prev}")),
			layout: bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: uniform_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(world.window),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(world.coarse),
				},
				wgpu::BindGroupEntry {
					binding: 3,
					resource: world.lut.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 4,
					resource: world.lights.as_entire_binding(),
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
					resource: world.cam.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 9,
					resource: world.covers.as_entire_binding(),
				},
			],
		})
	})
}

/// The debug raymarch's bind groups, one per front atlas of C0.
fn debug_binds(
	device: &wgpu::Device,
	bgl: &wgpu::BindGroupLayout,
	debug_buf: &wgpu::Buffer,
	atlas0: &[wgpu::Texture; 2],
	world: &WorldBinds,
) -> [wgpu::BindGroup; 2] {
	std::array::from_fn(|i| {
		let av = atlas0[i].create_view(&Default::default());
		device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi debug bg"),
			layout: bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: debug_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(world.window),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(world.coarse),
				},
				wgpu::BindGroupEntry {
					binding: 3,
					resource: world.lut.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 4,
					resource: wgpu::BindingResource::TextureView(&av),
				},
			],
		})
	})
}

/// gi_common.wgsl's group(2), one per atlas ping.
fn gather_binds(
	device: &wgpu::Device,
	bgl: &wgpu::BindGroupLayout,
	cascades: &[Cascade; 2],
	world: &WorldBinds,
) -> [wgpu::BindGroup; 2] {
	std::array::from_fn(|i| {
		let a0 = cascades[0].atlas[i].create_view(&Default::default());
		let a1 = cascades[1].atlas[i].create_view(&Default::default());
		device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi gather bg"),
			layout: bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(world.window),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(world.coarse),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: world.lut.as_entire_binding(),
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
					resource: world.lights.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 6,
					resource: world.covers.as_entire_binding(),
				},
			],
		})
	})
}
