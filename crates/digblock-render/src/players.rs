//! Remote-player rendering: instanced eye-anchored boxes, drawn in the
//! scene pass (depth-writing, opaque) so terrain occlusion, sky, water,
//! and fog all interact correctly for free. The body follows the
//! spray.rs shape: no geometry buffer, one instance-stepped attribute
//! pair.
//!
//! The item in their HAND is a second draw over the same pipeline
//! layout, because it needs the two things the body's generator cannot
//! give it: a vertex buffer (item geometry is not a box) and the
//! opposite `cull`. Its geometry comes from `item_geom` by way of
//! `viewmodel::build` — the one authority your own hand and the
//! inventory icons already share — so a remote's torch is a torch, a
//! bucket shows its charge, and a stair is a stair. It used to be one
//! 0.35-block cube textured with whatever `tex_side` the item happened
//! to have.

use digblock_platform::Gpu;

use crate::ui::hud::IconKind;
use crate::viewmodel::Vert;

/// Comfortably past `MAX_SNAPSHOT_PLAYERS` (40) with room for the
/// debug carrier; 32 B per instance, so 4 KiB a buffer.
///
/// It was 64 and shared between bodies and held items, which truncated
/// silently from 33 players up: each player pushed two instances into
/// one stream. Splitting the streams is what fixes it, and the headroom
/// is what keeps it fixed.
const MAX_INSTANCES: usize = 128;

/// Quad budget for the cached item meshes.
///
/// Every slottable item in the game together is a few hundred quads (a
/// cube is 6, the bucket 30, a door up to two shape variants), so this
/// holds the whole catalog with room to spare and the cache never needs
/// eviction. Overflow logs and SKIPS the kind rather than truncating a
/// mesh, which would silently drop faces.
const ITEM_QUAD_BUDGET: usize = 1024;

/// One item's geometry inside the shared vertex buffer.
struct ItemMesh {
	kind: IconKind,
	/// First vertex, fed to `draw_indexed`'s `base_vertex` so every kind
	/// shares the one quad index buffer.
	base_vertex: u32,
	quads: u32,
}

pub struct RemotePlayersDraw {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
	count: u32,
	held_pipeline: wgpu::RenderPipeline,
	held_vertex_buf: wgpu::Buffer,
	held_instance_buf: wgpu::Buffer,
	/// Lazily built, one entry per item kind ever seen. Empty after a
	/// device loss and refilled on the next frame, which is exactly why
	/// lazy beats prewarming: `Renderer::new` is a pure constructor and
	/// needs no new rebuild hook.
	items: Vec<ItemMesh>,
	item_quads: usize,
	/// `(index into `items`, instance range)`, one per distinct kind.
	batches: Vec<(usize, std::ops::Range<u32>)>,
	scratch: Vec<Vert>,
	instances: Vec<(IconKind, [f32; 8])>,
}

/// A total order over `IconKind`, so a frame's held items can be grouped
/// into one instanced draw per distinct item.
fn kind_key(k: IconKind) -> u32 {
	match k {
		IconKind::Cube(b) => b as u32,
		IconKind::Shaped(b) => (1 << 16) | b as u32,
		IconKind::WaterTool { full } => (2 << 16) | u32::from(full),
	}
}

impl RemotePlayersDraw {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
		materials_bgl: &wgpu::BindGroupLayout,
		gi_gather_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("players"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::PLAYERS.into()),
		});
		// Group 1 (block materials) textures the held items; the scene
		// pass has it bound already when we draw. Group 2 is gi_common's
		// GI world data — the renderer binds the front gather group
		// before our draw (the resolve outputs the solids hold at 2 don't
		// know player surfaces). ONE layout for both pipelines, so
		// switching between them leaves every bound group valid.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("players"),
			bind_group_layouts: &[frame_bgl, materials_bgl, gi_gather_bgl],
			push_constant_ranges: &[],
		});
		// Opaque scene geometry: writes depth, occluded by terrain.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "players",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: 32,
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
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		// The held item: closed geometry wound outward by
		// `item_geom`/`mesher::quad_indices`, so back faces cull — which
		// is what lets a non-convex item (a stair, a door) self-occlude.
		let held_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "players held",
				layout: &layout,
				module: &shader,
				vs_entry: "vs_held",
				fs_entry: Some("fs"),
				vertex_buffers: &[
					wgpu::VertexBufferLayout {
						array_stride: std::mem::size_of::<Vert>() as u64,
						step_mode: wgpu::VertexStepMode::Vertex,
						attributes: &[
							wgpu::VertexAttribute {
								format: wgpu::VertexFormat::Float32x3,
								offset: 0,
								shader_location: 0,
							},
							wgpu::VertexAttribute {
								format: wgpu::VertexFormat::Float32x2,
								offset: 12,
								shader_location: 1,
							},
							wgpu::VertexAttribute {
								format: wgpu::VertexFormat::Uint32,
								offset: 20,
								shader_location: 2,
							},
							wgpu::VertexAttribute {
								format: wgpu::VertexFormat::Uint32,
								offset: 24,
								shader_location: 3,
							},
						],
					},
					wgpu::VertexBufferLayout {
						array_stride: 32,
						step_mode: wgpu::VertexStepMode::Instance,
						attributes: &[
							wgpu::VertexAttribute {
								format: wgpu::VertexFormat::Float32x4,
								offset: 0,
								shader_location: 4,
							},
							wgpu::VertexAttribute {
								format: wgpu::VertexFormat::Float32x4,
								offset: 16,
								shader_location: 5,
							},
						],
					},
				],
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("player instances"),
			size: (MAX_INSTANCES * 32) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let held_instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("player held instances"),
			size: (MAX_INSTANCES * 32) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let held_vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("player held items"),
			size: (ITEM_QUAD_BUDGET * 4 * std::mem::size_of::<Vert>()) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
			count: 0,
			held_pipeline,
			held_vertex_buf,
			held_instance_buf,
			items: Vec::new(),
			item_quads: 0,
			batches: Vec::new(),
			scratch: Vec::new(),
			instances: Vec::new(),
		}
	}

	/// Upload this frame's body instances: `[pos.xyz, yaw, rgb, spare]`.
	pub fn set_instances(&mut self, gpu: &Gpu, instances: &[[f32; 8]]) {
		let n = instances.len().min(MAX_INSTANCES);
		self.count = n as u32;
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&instances[..n]));
		}
	}

	/// Cache `kind`'s geometry if this is the first time it has been in
	/// anybody's hand, and answer where it landed.
	fn ensure_mesh(&mut self, gpu: &Gpu, kind: IconKind) -> Option<usize> {
		if let Some(i) = self.items.iter().position(|m| m.kind == kind) {
			return Some(i);
		}
		crate::viewmodel::build(kind, &mut self.scratch);
		let quads = (self.scratch.len() / 4).min(crate::viewmodel::MAX_QUADS);
		if quads == 0 {
			return None;
		}
		if self.item_quads + quads > ITEM_QUAD_BUDGET {
			// Skip, never truncate: half an item is worse than none, and
			// the budget holds the whole catalog, so this can only fire if
			// the catalog grew past it.
			log::warn!("held-item cache full, skipping {kind:?}");
			return None;
		}
		let base_vertex = (self.item_quads * 4) as u32;
		gpu.queue.write_buffer(
			&self.held_vertex_buf,
			u64::from(base_vertex) * std::mem::size_of::<Vert>() as u64,
			bytemuck::cast_slice(&self.scratch[..quads * 4]),
		);
		self.item_quads += quads;
		self.items.push(ItemMesh {
			kind,
			base_vertex,
			quads: quads as u32,
		});
		Some(self.items.len() - 1)
	}

	/// Upload this frame's held items and plan their draws.
	///
	/// `held` is `(kind, [eye.xyz, yaw, emissive, height in twelfths, 0,
	/// 0])`. Grouped by kind so each distinct item is ONE instanced draw;
	/// the set is one entry per person on screen, so the sort is over a
	/// couple of dozen entries at most.
	pub fn set_held(&mut self, gpu: &Gpu, held: &[(IconKind, [f32; 8])]) {
		self.batches.clear();
		self.instances.clear();
		self.instances.extend_from_slice(held);
		self.instances.truncate(MAX_INSTANCES);
		self.instances.sort_unstable_by_key(|(k, _)| kind_key(*k));
		let mut flat: Vec<[f32; 8]> = Vec::with_capacity(self.instances.len());
		let mut i = 0;
		while i < self.instances.len() {
			let kind = self.instances[i].0;
			let mut j = i;
			while j < self.instances.len() && self.instances[j].0 == kind {
				j += 1;
			}
			if let Some(mi) = self.ensure_mesh(gpu, kind) {
				let first = flat.len() as u32;
				flat.extend(self.instances[i..j].iter().map(|(_, inst)| *inst));
				self.batches.push((mi, first..flat.len() as u32));
			}
			i = j;
		}
		if !flat.is_empty() {
			gpu.queue
				.write_buffer(&self.held_instance_buf, 0, bytemuck::cast_slice(&flat));
		}
	}

	/// Draw in the scene pass, after solids, before the sky (frame bind
	/// group must be bound at 0). Returns triangles drawn.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, index_buf: &wgpu::Buffer) -> u32 {
		let mut tris = 0;
		if self.count > 0 {
			pass.set_pipeline(&self.pipeline);
			pass.set_vertex_buffer(0, self.instance_buf.slice(..));
			pass.draw(0..36, 0..self.count);
			tris += self.count * 12;
		}
		if !self.batches.is_empty() {
			pass.set_pipeline(&self.held_pipeline);
			pass.set_vertex_buffer(0, self.held_vertex_buf.slice(..));
			pass.set_vertex_buffer(1, self.held_instance_buf.slice(..));
			pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
			for (mi, range) in &self.batches {
				let m = &self.items[*mi];
				// `base_vertex` plus the shared quad index buffer is what
				// lets one vertex buffer serve every kind: the indices are
				// relative to quad 0 and the draw adds the base.
				pass.draw_indexed(0..m.quads * 6, m.base_vertex as i32, range.clone());
				tris += m.quads * 2 * range.len() as u32;
			}
		}
		tris
	}

	/// Draw calls this frame — the body's one, plus one per distinct
	/// item anybody is holding.
	pub fn draw_calls(&self) -> u32 {
		u32::from(self.count > 0) + self.batches.len() as u32
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::block::BlockType;

	/// The cache holds the whole catalog at once, which is what makes
	/// eviction genuinely unnecessary rather than merely unlikely.
	#[test]
	fn every_held_item_fits_the_quad_budget() {
		let mut total = 0usize;
		let mut scratch = Vec::new();
		for b in BlockType::ALL {
			if !b.is_slottable() {
				continue;
			}
			let kind = if b.props().shape.is_some() {
				IconKind::Shaped(b)
			} else {
				IconKind::Cube(b)
			};
			crate::viewmodel::build(kind, &mut scratch);
			let quads = scratch.len() / 4;
			assert!(quads <= crate::viewmodel::MAX_QUADS, "{b:?} {quads} quads");
			total += quads;
		}
		for full in [false, true] {
			crate::viewmodel::build(IconKind::WaterTool { full }, &mut scratch);
			total += scratch.len() / 4;
		}
		assert!(
			total <= ITEM_QUAD_BUDGET,
			"the whole catalog is {total} quads against a {ITEM_QUAD_BUDGET} budget"
		);
	}

	/// Grouping is what keeps a crowd to one draw per distinct item
	/// rather than one per player, and every instance must land in
	/// exactly one contiguous run.
	#[test]
	fn held_items_group_one_run_per_kind() {
		let mut v = [
			(IconKind::Shaped(BlockType::Torch), 0u32),
			(IconKind::Cube(BlockType::Dirt), 1),
			(IconKind::Shaped(BlockType::Torch), 2),
			(IconKind::WaterTool { full: true }, 3),
			(IconKind::Cube(BlockType::Dirt), 4),
			(IconKind::Shaped(BlockType::Torch), 5),
		];
		v.sort_by_key(|(k, _)| kind_key(*k));
		let mut runs = Vec::new();
		let mut i = 0;
		while i < v.len() {
			let kind = v[i].0;
			let mut j = i;
			while j < v.len() && v[j].0 == kind {
				j += 1;
			}
			runs.push((kind, j - i));
			i = j;
		}
		assert_eq!(runs.len(), 3, "three distinct items, three draws");
		let total: usize = runs.iter().map(|(_, n)| n).sum();
		assert_eq!(total, 6, "every instance drawn exactly once");
		assert!(runs
			.iter()
			.any(|(k, n)| *k == IconKind::Shaped(BlockType::Torch) && *n == 3));
	}
}
