//! Teleport light: the cast channel's winding aura, the departure
//! collapse, and the arrival bloom — deliberately NOT confetti. Nothing
//! here falls or flutters; a jump is made of light, and it moves on a
//! helix (teleport.wgsl).
//!
//! The CPU owns each effect's origin, mode, age (or progress) and an
//! envelope; every mote's flight is a pure function of those and the
//! shared animation clock — [`crate::confetti`]'s ownership (effects are
//! ENGINE state a world swap forgets), [`crate::siphon`]'s additive light
//! and envelope. Nothing to save on suspend, nothing to rebuild on device
//! loss but the pipeline.

use digblock_platform::Gpu;

/// Simultaneous effects: the caster's own channel, a handful of bursts
/// (a departure and an arrival per jump, short-lived — [`LIFE`]), and
/// the kindled orbs within [`KINDLED_RADIUS`].
pub(crate) const MAX_FX: usize = 24;
/// Beyond this many blocks a kindled orb is not uploaded at all — the
/// marker's rule; its LIGHT still reaches the GI list at any range.
pub(crate) const KINDLED_RADIUS: f32 = 64.0;
/// Motes per effect — keep in sync with teleport.wgsl `MOTES`, held by
/// [`the_teleport_mote_count_matches_the_shader`]. Particle 0 is the
/// column of light, so the draw call is one quad longer.
const MOTES: u32 = 48;
/// Seconds a burst lives — the shader's weight curves reach zero exactly
/// here, so dropping an effect at this age can never cut a visible mote.
pub(crate) const LIFE: f32 = 1.15;
/// Bytes per instance: origin + mode, t + envelope + seed + spare, tint.
const INSTANCE_BYTES: usize = 48;

/// The channel aura's fade — siphon's envelope shape: quick to open so a
/// cast reads at once, slow enough shutting that an interrupt dissolves
/// rather than pops.
pub(crate) const FADE_IN_SECS: f32 = 0.15;
pub(crate) const FADE_OUT_SECS: f32 = 0.35;

/// The effect's kind, packed into the instance's mode lane.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
	Departure,
	Arrival,
	Channel,
	/// A standing kindled orb; `t` is its age, the envelope its last
	/// seconds.
	Kindled,
}

/// One effect, in the flat `[origin.xyz, mode][t, envelope, seed, 0]
/// [tint.rgb, 0]` layout the vertex shader reads.
pub(crate) type FxInstance = [f32; 12];

/// The one colour Arcana casts in — the marker spiral's violet, warmed a
/// step toward white so a jump reads hotter than a standing site.
pub(crate) const TINT: [f32; 3] = [0.62, 0.55, 1.0];

/// Pack one effect. `t` is a burst's age in seconds or the channel's
/// progress (0..=1); `envelope` scales the whole effect (bursts pass 1).
pub(crate) fn instance(
	origin: [f32; 3],
	mode: Mode,
	t: f32,
	envelope: f32,
	seed: f32,
) -> FxInstance {
	let m = match mode {
		Mode::Departure => 0.0,
		Mode::Arrival => 1.0,
		Mode::Channel => 2.0,
		Mode::Kindled => 3.0,
	};
	[
		origin[0], origin[1], origin[2], m, t, envelope, seed, 0.0, TINT[0], TINT[1], TINT[2], 0.0,
	]
}

pub(crate) struct Teleport {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
}

impl Teleport {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("teleport"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::TELEPORT.into()),
		});
		// No camera-light group, siphon's reason: arcane light is its own
		// light, and a mote lit by the room would go out in a night jump.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("teleport"),
			bind_group_layouts: &[Some(frame_bgl)],
			immediate_size: 0,
		});
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "teleport",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: INSTANCE_BYTES as u64,
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
				blend: Some(wgpu::BlendState {
					color: wgpu::BlendComponent {
						src_factor: wgpu::BlendFactor::SrcAlpha,
						dst_factor: wgpu::BlendFactor::One,
						operation: wgpu::BlendOperation::Add,
					},
					alpha: wgpu::BlendComponent {
						src_factor: wgpu::BlendFactor::Zero,
						dst_factor: wgpu::BlendFactor::One,
						operation: wgpu::BlendOperation::Add,
					},
				}),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("teleport instances"),
			size: (MAX_FX * INSTANCE_BYTES) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
		}
	}

	/// Upload this frame's live effects (engine-owned; over the cap the
	/// oldest simply don't draw). Returns the instance count.
	pub(crate) fn prepare(&self, gpu: &Gpu, fx: &[FxInstance]) -> u32 {
		let n = fx.len().min(MAX_FX);
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&fx[..n]));
		}
		n as u32
	}

	/// Draw in the water pass beside siphon (frame bind group already
	/// bound at 0). Returns triangles.
	pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instances: u32) -> u32 {
		if instances == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * (MOTES + 1), 0..instances);
		instances * (MOTES + 1) * 2
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The count that sizes the draw call lives on both sides of the
	/// CPU/GPU boundary — siphon's pin, same reason.
	#[test]
	fn the_teleport_mote_count_matches_the_shader() {
		let decl = format!("const MOTES: u32 = {MOTES}u;");
		assert!(
			digblock_assets::shader::TELEPORT.contains(&decl),
			"teleport.wgsl must declare `{decl}`"
		);
	}

	/// A burst outlives nothing: the shader's weight curves are authored
	/// to reach zero AT `LIFE`, and the wgsl must agree on the number.
	#[test]
	fn the_burst_life_matches_the_shader() {
		let decl = format!("const LIFE: f32 = {LIFE};");
		assert!(
			digblock_assets::shader::TELEPORT.contains(&decl),
			"teleport.wgsl must declare `{decl}`"
		);
	}

	/// The lanes the vertex shader reads by name.
	#[test]
	fn an_effect_packs_where_the_shader_looks() {
		let i = instance([1.0, 2.0, 3.0], Mode::Channel, 0.7, 0.9, 5.0);
		assert_eq!(&i[0..3], &[1.0, 2.0, 3.0], "origin");
		assert_eq!(i[3], 2.0, "mode rides the origin's spare lane");
		assert_eq!(i[4], 0.7, "t");
		assert_eq!(i[5], 0.9, "envelope");
		assert_eq!(i[6], 5.0, "seed");
		assert_eq!(&i[8..11], &TINT, "tint");
	}
}
