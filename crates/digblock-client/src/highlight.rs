//! Target-block highlight: the idle outline + hold-to-dig crack decal,
//! one instanced box (players.rs shape) drawn in the WATER pass after
//! the water surfaces — a box on a targeted source or a submerged block
//! must blend over the water, not vanish under its depth tint. The live
//! depth buffer holds water-surface depth there, so the hardware test is
//! Always and the shader occludes by hand against the pre-water solid
//! depth copy (bound through the water frame group). Never depth-writes.
//!
//! The box is the target's own bounds, not the unit cell, and for a door
//! it is the whole two-cell leaf — see [`HighlightTarget`].

use digblock_platform::Gpu;

/// One instance: cell + progress, box lo + seed, box hi + spare.
const INSTANCE_BYTES: wgpu::BufferAddress = 48;

/// The crosshair target the decal wraps: the anchor cell, the box around the
/// whole object relative to it, and the dig progress (`None` = idle outline, no
/// crack).
///
/// The box is what makes a shaped block outline at its own size. It comes from
/// `shapes::object_bounds`, i.e. from the geometry the mesher actually drew — a
/// full cube's is the whole cell.
///
/// A door is the one thing here bigger than its cell: `cell` is its LOWER half
/// whichever half the ray hit, and the box runs 0..2 in y. Both halves therefore
/// produce an identical target — same outline, same fracture — which is what
/// makes it read as one object, and matches the click.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct HighlightTarget {
	/// The cell the box is anchored to — the targeted cell for everything
	/// except a door, where it is the pair's lower half.
	pub cell: (i32, i32, i32),
	/// Anchor-local AABB, 0..1 per axis for a single-cell object and up to
	/// 0..2 in y for a door.
	pub lo: [f32; 3],
	pub hi: [f32; 3],
	pub progress: Option<f32>,
}

impl HighlightTarget {
	/// Box the whole object a raw cell value belongs to.
	pub(crate) fn new(cell: (i32, i32, i32), value: u16, progress: Option<f32>) -> Self {
		let s = 1.0 / f32::from(digblock_core::shapes::GRID);
		let (cell, bounds) = digblock_core::shapes::object_bounds(cell, value);
		let (lo, hi) = match bounds {
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

/// Which crack a cell wears. Rides the instance in the `w` of the vec4
/// that carries the box's low corner — the shader's box is `i1.xyz` and
/// the seed is `i1.w`, and the two are one attribute because the instance
/// is already a whole number of vec4s and adding another for a lone scalar
/// is wasteful.
///
/// That packing is the whole reason for [`the_seed_is_the_cell_not_the_box`]:
/// the shader read `i1.x` — the box's own minimum x, which is 0.0 on
/// every full cube — from the day it was written until R28, so the
/// per-cell pattern this function computes was thrown away and every
/// block in the world cracked identically. Nothing crashed and nothing
/// looked broken; it just silently wasn't varying.
fn cell_seed(cell: (i32, i32, i32)) -> u32 {
	(cell.0 as u32)
		.wrapping_mul(73_856_093)
		.wrapping_add((cell.1 as u32).wrapping_mul(19_349_663))
		.wrapping_add((cell.2 as u32).wrapping_mul(83_492_791))
		& 0xFFFF
}

pub(crate) struct HighlightDraw {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
	active: bool,
}

impl HighlightDraw {
	pub(crate) fn new(
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
			bind_group_layouts: &[Some(water_frame_bgl)],
			immediate_size: 0,
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
	pub(crate) fn set_target(&mut self, gpu: &Gpu, target: Option<HighlightTarget>) {
		self.active = target.is_some();
		let Some(t) = target else {
			return;
		};
		let cell = t.cell;
		let seed = cell_seed(cell);
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
	pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) -> u32 {
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
	use digblock_core::shapes::{Facing, door_state, slab_state, stair_state};

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
		// A closed +x door is a plate on x, full on z — and two cells tall,
		// which `a_door_boxes_as_one_object` covers.
		let (lo, hi) = of(make_cell(
			B::LumberDoor,
			door_state(Facing::PosX, false, false),
		));
		assert!(hi[0] - lo[0] <= 1.0 / 3.0, "leaf is thin: {lo:?}..{hi:?}");
		assert_eq!(([lo[1], lo[2]], [hi[2]]), ([0.0, 0.0], [1.0]));
	}

	/// A door is one object across two cells, so aiming at either half
	/// must produce the SAME target: the same anchor (hence the same crack
	/// seed, since the seed is the cell), and one box around both leaves.
	/// Boxing the half under the crosshair framed whichever panel you
	/// happened to aim at, which is not what the click acts on — digging
	/// either half takes the pair.
	#[test]
	fn a_door_boxes_as_one_object() {
		let lower = (3, 40, 7);
		let upper = (lower.0, lower.1 + 1, lower.2);
		for f in Facing::ALL {
			for open in [false, true] {
				let a = HighlightTarget::new(
					lower,
					make_cell(B::LumberDoor, door_state(f, open, false)),
					None,
				);
				let b = HighlightTarget::new(
					upper,
					make_cell(B::LumberDoor, door_state(f, open, true)),
					None,
				);
				assert_eq!(a, b, "{f:?} open={open}: both halves, one target");
				assert_eq!(a.cell, lower, "anchored on the lower half");
				assert_eq!(
					(a.lo[1], a.hi[1]),
					(0.0, 2.0),
					"{f:?} open={open}: two cells tall"
				);
				// The union is a real union: the handle protrudes past the
				// leaf on the lower half only, so the box's thin axis is
				// wider there than on the upper panel alone.
				let thin = (a.hi[0] - a.lo[0]).min(a.hi[2] - a.lo[2]);
				assert!(
					(thin - 4.0 / 12.0).abs() < 1e-6,
					"{f:?} open={open}: leaf + handle = 4/12, got {thin}"
				);
			}
		}
		// The seed is a function of the anchor, so equal targets crack
		// alike — which is only interesting because the two cells do not.
		assert_ne!(cell_seed(lower), cell_seed(upper));
	}

	/// Neighbouring cells must not crack alike, and the seed must not be
	/// derivable from the box — the bug this pins had the shader reading
	/// the box's min x as the seed, which is 0.0 on every full cube, so
	/// the whole world shared one pattern.
	#[test]
	fn the_seed_is_the_cell_not_the_box() {
		let base = (12, 70, -5);
		let seen: std::collections::HashSet<u32> = (-2..=2)
			.flat_map(|dx| (-2..=2).flat_map(move |dy| (-2..=2).map(move |dz| (dx, dy, dz))))
			.map(|(dx, dy, dz)| cell_seed((base.0 + dx, base.1 + dy, base.2 + dz)))
			.collect();
		assert_eq!(seen.len(), 125, "125 neighbouring cells, 125 patterns");
		// One step on any axis changes it — an adjacent pair is the case
		// a player actually sees, quarrying along a wall.
		for step in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
			let next = (base.0 + step.0, base.1 + step.1, base.2 + step.2);
			assert_ne!(cell_seed(base), cell_seed(next), "neighbour on {step:?}");
		}
	}
}
