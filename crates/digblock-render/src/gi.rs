//! RTGI probe volume (GI-DESIGN.md). P0: the crate's first compute
//! pipeline — the real C0 probe atlas, one timed dispatch per frame,
//! a placeholder kernel. P2 grows the trace (voxel window reads, ray
//! budget amortization, ping-pong); what P0 pins is the shape: the
//! atlas as six axis-major Z slabs, a small GI uniform block of its
//! own (never the pinned Frame layout), and a dispatch that lives in
//! the frame's single encoder ahead of every render pass.
//!
//! Constructed only when `EngineConfig::rtgi` is on — a flag-off build
//! allocates nothing and encodes nothing, which is what makes the P0
//! bench-parity gate trivial to trust.

use digblock_platform::Gpu;

use crate::gpu_timer::{GpuTimer, Q_GI};

/// C0 probe grid (2-block spacing near cascade; see GI-DESIGN.md).
pub const C0_DIMS: (u32, u32, u32) = (32, 24, 32);
/// One probe = 64 threads today (P2 splits: threads = rays).
const WORKGROUP: u32 = 64;

pub struct GiDraw {
	pipeline: wgpu::ComputePipeline,
	bind_group: wgpu::BindGroup,
	uniform_buf: wgpu::Buffer,
	/// Frame counter riding the uniform: the amortization cursor and
	/// ray-rotation seed once P2 traces.
	frame: u32,
	/// Kept alive for the debug view and the P3 resolve pass; the
	/// compute writes it through `bind_group`.
	#[allow(dead_code)]
	atlas_view: wgpu::TextureView,
}

impl GiDraw {
	pub fn new(gpu: &Gpu) -> Self {
		let device = &gpu.device;
		let (nx, ny, nz) = C0_DIMS;
		let atlas = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("gi probe atlas c0"),
			size: wgpu::Extent3d {
				width: nx,
				height: ny,
				// Six axis slabs, stacked in Z.
				depth_or_array_layers: nz * 6,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::Rgba16Float,
			usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let atlas_view = atlas.create_view(&Default::default());
		let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi uniforms"),
			size: 16,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
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
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::StorageTexture {
						access: wgpu::StorageTextureAccess::WriteOnly,
						format: wgpu::TextureFormat::Rgba16Float,
						view_dimension: wgpu::TextureViewDimension::D3,
					},
					count: None,
				},
			],
		});
		let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi probe bg"),
			layout: &bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: uniform_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&atlas_view),
				},
			],
		});
		let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi probe"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_PROBE.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi probe layout"),
			bind_group_layouts: &[&bgl],
			push_constant_ranges: &[],
		});
		let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
			label: Some("gi probe update"),
			layout: Some(&layout),
			module: &module,
			entry_point: Some("update_probes"),
			compilation_options: Default::default(),
			cache: None,
		});
		Self {
			pipeline,
			bind_group,
			uniform_buf,
			frame: 0,
			atlas_view,
		}
	}

	/// Encode this frame's probe update. Belongs at the head of the
	/// frame's encoder, before any render pass reads the atlas.
	pub fn encode(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
	) {
		let (nx, ny, nz) = C0_DIMS;
		gpu.queue.write_buffer(
			&self.uniform_buf,
			0,
			bytemuck::cast_slice(&[nx, ny, nz, self.frame]),
		);
		self.frame = self.frame.wrapping_add(1);
		let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
			label: Some("gi probes"),
			timestamp_writes: timer.and_then(|t| t.compute_writes(Q_GI)),
		});
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.bind_group, &[]);
		pass.dispatch_workgroups((nx * ny * nz).div_ceil(WORKGROUP), 1, 1);
	}
}
