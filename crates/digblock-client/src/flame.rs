//! Stateless fire shared by placed, remote-held and first-person torches.
//! One transform carries each flame from torch-local blocks into clip space.

use digblock_platform::Gpu;
use glam::{Mat4, Vec3};

#[cfg(test)]
mod tests;

pub(crate) const CAPACITY: usize = 257;
const QUADS: u32 = 8;
const INSTANCE_BYTES: usize = 80;
pub(crate) type FlameInstance = [f32; 20];

/// The light origin follows the torch shape's emissive band.
pub(crate) fn tip() -> Vec3 {
	let (_, cover) = digblock_core::gi::gi_classes(digblock_core::block::BlockType::Torch as u16);
	Vec3::from_array(digblock_core::gi::light_origin(cover))
}

pub(crate) fn instance(transform: Mat4, origin: Vec3, seed: f32) -> FlameInstance {
	let mut result = [0.0; 20];
	result[..16].copy_from_slice(&transform.to_cols_array());
	result[16..19].copy_from_slice(&origin.to_array());
	result[19] = seed;
	result
}

/// Remote-held geometry uses the same centering, hand offset and rotations as vs_held.
pub(crate) fn remote_transform(i: &[f32; 8]) -> Mat4 {
	Mat4::from_translation(Vec3::new(i[0], i[1], i[2]))
		* Mat4::from_rotation_y(i[3])
		* Mat4::from_translation(Vec3::new(-0.44, -0.5, 0.34))
		* Mat4::from_rotation_y(-std::f32::consts::FRAC_PI_4)
		* Mat4::from_scale(Vec3::splat(0.35))
		* Mat4::from_translation(tip() - Vec3::new(0.5, i[5] / 24.0, 0.5))
}

pub(crate) struct Flames {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
}

impl Flames {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("flame"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::FLAME.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("flame"),
			bind_group_layouts: &[Some(frame_bgl)],
			immediate_size: 0,
		});
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "flame",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: INSTANCE_BYTES as u64,
					step_mode: wgpu::VertexStepMode::Instance,
					attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4],
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
			label: Some("flame instances"),
			size: (CAPACITY * INSTANCE_BYTES) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
		}
	}

	/// Upload world flames and the separately drawn first-person flame into distinct ranges.
	pub(crate) fn prepare(
		&self,
		gpu: &Gpu,
		world: &[FlameInstance],
		hand: Option<FlameInstance>,
	) -> (u32, u32) {
		let n = world.len().min(CAPACITY - 1);
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&world[..n]));
		}
		if let Some(hand) = hand {
			gpu.queue.write_buffer(
				&self.instance_buf,
				(n * INSTANCE_BYTES) as u64,
				bytemuck::bytes_of(&hand),
			);
		}
		(n as u32, u32::from(hand.is_some()))
	}

	/// The hand range is drawn after the viewmodel clears scene depth.
	pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>, range: std::ops::Range<u32>) -> u32 {
		let n = range.end - range.start;
		if n == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * QUADS, range);
		n * QUADS * 2
	}
}
