//! The generic burst rack: block-break debris, splash droplets, sparkle
//! and poof through ONE pipeline — confetti's stateless arrangement (the
//! CPU owns a burst's origin, age, seed; every particle is a pure
//! function of those in bursts.wgsl) extended with a kind lane and an
//! instance color, so a new polish effect is a `BurstKind` row rather
//! than another module. Bursts are engine state exactly like confetti:
//! the engine spawns them through `Engine::spawn_burst`, ages lane 3,
//! drops each at its kind's life, and a world swap forgets them.

use digblock_platform::Gpu;

/// Simultaneous bursts. Digging fast spawns one per broken block, so the
/// cap is well past a burst's life worth of frantic digging; over it the
/// engine drops the oldest.
pub(crate) const MAX_BURSTS: usize = 64;
/// Particle budget per burst — keep in sync with bursts.wgsl PARTICLES.
/// Kinds that want fewer draw degenerate quads for the surplus.
const PARTICLES: u32 = 64;
/// Bytes per instance: three vec4 lanes.
const INSTANCE_SIZE: usize = 48;

/// What a burst IS — picks particle count, motion, footprint and fade in
/// the shader. Discriminants ride the instance's kind lane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BurstKind {
	/// Block-break chips, tinted by the block's mean albedo.
	Debris,
	/// Water droplets thrown up and out.
	Splash,
	/// Twinkling motes, no gravity.
	Sparkle,
	/// A soft billowing puff.
	Poof,
}

impl BurstKind {
	/// Seconds a burst of this kind lives; the shader's fade reaches zero
	/// exactly here, so dropping at this age can never cut a particle.
	pub(crate) fn life(self) -> f32 {
		match self {
			BurstKind::Debris => 0.9,
			BurstKind::Splash => 0.7,
			BurstKind::Sparkle => 1.2,
			BurstKind::Poof => 0.9,
		}
	}

	fn lane(self) -> f32 {
		match self {
			BurstKind::Debris => 0.0,
			BurstKind::Splash => 1.0,
			BurstKind::Sparkle => 2.0,
			BurstKind::Poof => 3.0,
		}
	}

	/// Every kind, for the debug rack and tests.
	pub(crate) const ALL: [BurstKind; 4] = [
		BurstKind::Debris,
		BurstKind::Splash,
		BurstKind::Sparkle,
		BurstKind::Poof,
	];
}

/// One live burst in the flat layout the vertex shader reads:
/// `[pos.xyz, age][seed, kind, life, size][color.rgb, surface_y + 1 (zero when absent)]`.
pub(crate) type BurstInstance = [f32; 12];

/// Pack a burst; the layout lives here, with the shader that decodes it.
pub(crate) fn instance(
	pos: [f32; 3],
	age: f32,
	seed: f32,
	kind: BurstKind,
	size: f32,
	color: [f32; 3],
) -> BurstInstance {
	[
		pos[0],
		pos[1],
		pos[2],
		age,
		seed,
		kind.lane(),
		kind.life(),
		size,
		color[0],
		color[1],
		color[2],
		0.0,
	]
}

pub(crate) struct Bursts {
	pipeline: wgpu::RenderPipeline,
	/// Group 1: the RTGI camera-light buffer, as confetti uses.
	cam_bg: wgpu::BindGroup,
	instance_buf: wgpu::Buffer,
}

impl Bursts {
	/// Same groups as confetti (`gi_gather_bgl` is the GI world data the
	/// vertex NEE runs over): debris flies wherever somebody is digging,
	/// beside a torch as often as under the sky.
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
		cam_light_buf: &wgpu::Buffer,
		gi_gather_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("bursts"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::BURSTS.into()),
		});
		let cam_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("bursts cam light"),
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
			label: Some("bursts cam light"),
			layout: &cam_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: cam_light_buf.as_entire_binding(),
			}],
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("bursts"),
			bind_group_layouts: &[Some(frame_bgl), Some(&cam_bgl), Some(gi_gather_bgl)],
			immediate_size: 0,
		});
		// Depth: occluded by terrain, never occludes anything itself.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "bursts",
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
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x4,
							offset: 32,
							shader_location: 2,
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
			label: Some("bursts instances"),
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

	/// Upload this frame's live bursts; returns the instance count.
	pub(crate) fn prepare(&self, gpu: &Gpu, bursts: &[BurstInstance]) -> u32 {
		let n = bursts.len().min(MAX_BURSTS);
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&bursts[..n]));
		}
		n as u32
	}

	/// Draw beside confetti (same pass; frame group already bound at 0).
	/// Returns triangles.
	pub(crate) fn draw(
		&self,
		pass: &mut wgpu::RenderPass<'_>,
		instances: u32,
		gi_gather_bg: &wgpu::BindGroup,
	) -> u32 {
		if instances == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(1, &self.cam_bg, &[]);
		pass.set_bind_group(2, gi_gather_bg, &[]);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * PARTICLES, 0..instances);
		instances * PARTICLES * 2
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The instance layout is the shader's contract: 12 lanes, the kind
	/// and life where bursts.wgsl reads them, and every kind's life
	/// positive so the age/retain sweep terminates.
	#[test]
	fn the_instance_layout_matches_the_shader() {
		let i = instance(
			[1.0, 2.0, 3.0],
			0.25,
			7.0,
			BurstKind::Splash,
			1.5,
			[0.2, 0.4, 0.6],
		);
		assert_eq!(&i[0..4], &[1.0, 2.0, 3.0, 0.25]);
		assert_eq!(i[4], 7.0);
		assert_eq!(i[5], 1.0, "splash kind lane");
		assert_eq!(i[6], BurstKind::Splash.life());
		assert_eq!(i[7], 1.5);
		assert_eq!(&i[8..11], &[0.2, 0.4, 0.6]);
		assert_eq!(std::mem::size_of::<BurstInstance>(), INSTANCE_SIZE);
		for k in BurstKind::ALL {
			assert!(k.life() > 0.0, "{k:?}");
		}
		// Kind lanes are the wgsl KIND_* discriminants, in order.
		let lanes: Vec<f32> = BurstKind::ALL.iter().map(|k| k.lane()).collect();
		assert_eq!(lanes, [0.0, 1.0, 2.0, 3.0]);
	}
}
