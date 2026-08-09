//! Target-block highlight: the idle outline + hold-to-dig crack decal,
//! one instanced box (players.rs shape) drawn in the WATER pass after
//! the water surfaces — a box on a targeted source or a submerged block
//! must blend over the water, not vanish under its depth tint. The live
//! depth buffer holds water-surface depth there, so the hardware test is
//! Always and the shader occludes by hand against the pre-water solid
//! depth copy (bound through the water frame group). Never depth-writes.
//!
//! The box is the target's own bounds, not the unit cell — see
//! [`HighlightTarget`].

use digblock_platform::Gpu;

/// One instance: cell + progress, box lo + seed, box hi + spare.
const INSTANCE_BYTES: wgpu::BufferAddress = 48;

/// The crosshair target the decal wraps: the cell, the block-local box
/// within it, and the dig progress (`None` = idle outline, no crack web).
///
/// The box is what makes a shaped block outline at its own size. It comes
/// from `shapes::draw_bounds`, i.e. from the geometry the mesher actually
/// drew — a full cube's is the whole cell, which is why every unshaped
/// block looks exactly as it always did.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct HighlightTarget {
	pub cell: (i32, i32, i32),
	/// Block-local AABB, 0..1 per axis.
	pub lo: [f32; 3],
	pub hi: [f32; 3],
	pub progress: Option<f32>,
}

impl HighlightTarget {
	/// Box a raw cell value's drawn geometry.
	pub fn new(cell: (i32, i32, i32), value: u16, progress: Option<f32>) -> Self {
		let s = 1.0 / f32::from(digblock_core::shapes::GRID);
		let (lo, hi) = match digblock_core::shapes::draw_bounds(value) {
			Some(b) => (
				b.min.map(|v| f32::from(v) * s),
				b.max.map(|v| f32::from(v) * s),
			),
			None => ([0.0; 3], [1.0; 3]),
		};
		Self {
			cell,
			lo,
			hi,
			progress,
		}
	}
}

pub struct HighlightDraw {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
	active: bool,
}

impl HighlightDraw {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		water_frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("highlight"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::HIGHLIGHT.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("highlight"),
			bind_group_layouts: &[water_frame_bgl],
			push_constant_ranges: &[],
		});
		// Translucent decal: depth compare Always (occlusion is manual,
		// against the solid depth copy — see the module doc), never write.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "highlight",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: INSTANCE_BYTES,
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
				cull: Some(wgpu::Face::Back),
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState::ALPHA_BLENDING),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("highlight instance"),
			size: INSTANCE_BYTES,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
			active: false,
		}
	}

	/// Upload this frame's target, or clear it.
	pub fn set_target(&mut self, gpu: &Gpu, target: Option<HighlightTarget>) {
		self.active = target.is_some();
		let Some(t) = target else {
			return;
		};
		let cell = t.cell;
		// Seed from the cell so each block cracks with its own pattern.
		let seed = (cell.0 as u32)
			.wrapping_mul(73_856_093)
			.wrapping_add((cell.1 as u32).wrapping_mul(19_349_663))
			.wrapping_add((cell.2 as u32).wrapping_mul(83_492_791))
			& 0xFFFF;
		let data: [f32; 12] = [
			cell.0 as f32,
			cell.1 as f32,
			cell.2 as f32,
			t.progress.unwrap_or(0.0),
			t.lo[0],
			t.lo[1],
			t.lo[2],
			seed as f32,
			t.hi[0],
			t.hi[1],
			t.hi[2],
			0.0,
		];
		gpu.queue
			.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&data));
	}

	/// Draw in the water pass, after the water surfaces (the water frame
	/// bind group at 0 — it carries the solid depth copy the shader
	/// occludes against). Returns triangles drawn.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) -> u32 {
		if !self.active {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..36, 0..1);
		12
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{door_state, slab_state, stair_state, Facing};

	/// The instance box, GPU-free. A full cube and a stair both fill the
	/// cell; a slab and a door shrink to the volume they actually occupy.
	#[test]
	fn the_box_is_the_shape_not_the_cell() {
		let cell = (3, 4, 5);
		let of = |v: u16| {
			let t = HighlightTarget::new(cell, v, None);
			(t.lo, t.hi)
		};
		let unit = ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
		assert_eq!(of(B::Stone as u16), unit);
		assert_eq!(
			of(make_cell(B::WoodStairs, stair_state(Facing::PosX))),
			unit,
			"a stair's best fit already is the cell"
		);
		assert_eq!(
			of(make_cell(B::WoodSlab, slab_state(false, false))),
			([0.0, 0.0, 0.0], [1.0, 1.0 / 3.0, 1.0])
		);
		assert_eq!(
			of(make_cell(B::StoneSlab, slab_state(true, false))),
			([0.0, 0.0, 0.0], [1.0, 2.0 / 3.0, 1.0])
		);
		// A ceiling slab is boxed against the top of its cell.
		assert_eq!(
			of(make_cell(B::WoodSlab, slab_state(false, true))),
			([0.0, 2.0 / 3.0, 0.0], [1.0, 1.0, 1.0])
		);
		assert_eq!(
			of(make_cell(B::StoneSlab, slab_state(true, true))),
			([0.0, 1.0 / 3.0, 0.0], [1.0, 1.0, 1.0])
		);
		// A closed +x door is a plate on x, full on y and z.
		let (lo, hi) = of(make_cell(
			B::WoodDoor,
			door_state(Facing::PosX, false, false),
		));
		assert!(hi[0] - lo[0] <= 1.0 / 3.0, "leaf is thin: {lo:?}..{hi:?}");
		assert_eq!(([lo[1], lo[2]], [hi[1], hi[2]]), ([0.0, 0.0], [1.0, 1.0]));
	}
}
