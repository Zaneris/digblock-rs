//! Level-up confetti: a burst of paper flakes thrown out of one world
//! point. The CPU owns nothing but a burst's origin, its age and a seed;
//! every flake's arc is a pure function of those in confetti.wgsl —
//! spray.rs's arrangement, minus the looping (a burst begins, runs once,
//! and ends).
//!
//! Bursts are engine state, not renderer state: they belong to the world
//! the way remote players do, and a world swap must forget them. The
//! engine ages them and hands this module a flat instance list, exactly
//! as it does for `FrameScene::players`.

use digblock_platform::Gpu;

/// Simultaneous bursts. Sixteen is far past what a session ever shows at
/// once (a level-up is rare and a burst is under three seconds), and
/// costs a 512-byte buffer.
pub const MAX_BURSTS: usize = 16;
/// Flakes per burst — keep in sync with confetti.wgsl PARTICLES.
const PARTICLES: u32 = 96;
/// Seconds a burst lives. The shader fades out over its last stretch and
/// reaches zero exactly here, so dropping the burst at this age can never
/// cut a visible flake.
pub const LIFE: f32 = 2.6;
/// Bytes per instance: origin + age, then the seed lane.
const INSTANCE_SIZE: usize = 32;

/// One live burst, in the flat `[origin.xyz, age][seed, 0, 0, 0]` layout
/// the vertex shader reads.
pub type BurstInstance = [f32; 8];

/// Build the instance for a burst at `pos`, `age` seconds old, with
/// per-burst `seed`. Shared by the engine so the packing lives with the
/// shader that decodes it.
pub fn instance(pos: [f32; 3], age: f32, seed: f32) -> BurstInstance {
	[pos[0], pos[1], pos[2], age, seed, 0.0, 0.0, 0.0]
}

pub struct Confetti {
	pipeline: wgpu::RenderPipeline,
	/// Group 1: the RTGI camera-light buffer, as spray uses.
	cam_bg: wgpu::BindGroup,
	instance_buf: wgpu::Buffer,
}

impl Confetti {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
		cam_light_buf: &wgpu::Buffer,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("confetti"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::CONFETTI.into()),
		});
		let cam_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("confetti cam light"),
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
			label: Some("confetti cam light"),
			layout: &cam_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: cam_light_buf.as_entire_binding(),
			}],
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("confetti"),
			bind_group_layouts: &[frame_bgl, &cam_bgl],
			push_constant_ranges: &[],
		});
		// Depth: occluded by terrain, never occludes anything itself —
		// flakes are alpha-blended and drawn after the opaque world.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "confetti",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: INSTANCE_SIZE as u64,
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
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState::ALPHA_BLENDING),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("confetti instances"),
			size: (MAX_BURSTS * INSTANCE_SIZE) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			cam_bg,
			instance_buf,
		}
	}

	/// Upload this frame's live bursts (engine-owned; over the cap the
	/// oldest simply don't draw). Returns the instance count to draw.
	pub fn prepare(&self, gpu: &Gpu, bursts: &[BurstInstance]) -> u32 {
		let n = bursts.len().min(MAX_BURSTS);
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&bursts[..n]));
		}
		n as u32
	}

	/// Draw alongside the spray billboards (same pass, frame bind group
	/// already bound at 0). Returns triangles.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instances: u32) -> u32 {
		if instances == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(1, &self.cam_bg, &[]);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * PARTICLES, 0..instances);
		instances * PARTICLES * 2
	}
}
