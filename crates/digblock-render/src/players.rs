//! Remote-player rendering: instanced eye-anchored boxes, drawn in the scene
//! pass (depth-writing, opaque) so terrain occlusion, sky, water and fog all
//! interact correctly for free. The body follows the spray.rs shape: no geometry
//! buffer, one instance-stepped attribute pair.
//!
//! The item in their HAND is a second draw over the same pipeline layout,
//! because it needs the two things the body's generator cannot give it: a vertex
//! buffer (item geometry is not a box) and the opposite `cull`. Its geometry
//! comes from `item_geom` by way of `viewmodel::build` — the one authority your
//! own hand and the inventory icons already share — so a remote's bucket shows
//! its charge and a stair is a stair.

use digblock_platform::Gpu;

use crate::ui::hud::IconKind;
use crate::viewmodel::Vert;

/// Comfortably past `MAX_SNAPSHOT_PLAYERS` with room for the debug
/// carrier; the per-instance record is small, so the buffer is too.
///
/// It used to be smaller AND shared between bodies and held items, which
/// truncated silently once a busy server filled half of it: each player
/// pushed two instances into one stream. Splitting the streams is what
/// fixes it, and the headroom is what keeps it fixed.
const MAX_INSTANCES: usize = 128;

/// Quad budget for the cached item meshes.
///
/// Every item a hand can hold, together: the slottable catalog (a cube's
/// faces, a door up to two shape variants), both bucket charges, and the
/// authored tools, which are the expensive ones. The whole catalog fits
/// with room to spare, so the cache never needs eviction. Overflow logs
/// and SKIPS the kind rather than truncating a mesh, which would silently
/// drop faces.
///
/// Sized against `every_held_item_fits_the_quad_budget`, which counts
/// exactly what the cache can be asked for — so the catalog growing past
/// it fails a test rather than dropping an item from somebody's hand.
const ITEM_QUAD_BUDGET: usize = 2048;

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
		// The authored tools carry no payload, so each is exactly one
		// kind. Their own band rather than borrowing a block id: this
		// only has to be a total order, and a collision would merge two
		// different items into one instanced draw.
		IconKind::Siphon => 3 << 16,
		IconKind::Pick => (3 << 16) | 1,
		IconKind::Sigil => (3 << 16) | 2,
	}
}

/// Sort a frame's held items into one contiguous run per distinct item,
/// and answer where each run sits.
///
/// Its own function so the pin can call THIS rather than a copy of it —
/// a test that restates the sort-and-scan in its own body proves the
/// restatement works and nothing about the draw path. The set is one
/// entry per person on screen, so this is a sort over a couple of dozen
/// entries at most.
fn group_by_kind(items: &mut [(IconKind, [f32; 8])]) -> Vec<(IconKind, std::ops::Range<usize>)> {
	items.sort_unstable_by_key(|(k, _)| kind_key(*k));
	let mut runs = Vec::new();
	let mut i = 0;
	while i < items.len() {
		let kind = items[i].0;
		let mut j = i;
		while j < items.len() && items[j].0 == kind {
			j += 1;
		}
		runs.push((kind, i..j));
		i = j;
	}
	runs
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
			bind_group_layouts: &[Some(frame_bgl), Some(materials_bgl), Some(gi_gather_bgl)],
			immediate_size: 0,
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
	///
	/// A kind that CANNOT be cached is remembered too, as a zero-quad
	/// entry. Without that the lookup below misses it every frame, so
	/// `viewmodel::build` re-tessellates and `log::warn!` fires once a
	/// frame for as long as anybody holds the thing.
	fn ensure_mesh(&mut self, gpu: &Gpu, kind: IconKind) -> Option<usize> {
		if let Some(i) = self.items.iter().position(|m| m.kind == kind) {
			return (self.items[i].quads > 0).then_some(i);
		}
		crate::viewmodel::build(kind, &mut self.scratch);
		let quads = self.scratch.len() / 4;
		// Skip, never truncate — half an item is worse than none, and a
		// truncated mesh would be CACHED, so it would render with faces
		// missing for the rest of the process rather than once. Both
		// bounds are real: MAX_QUADS is what one item's vertex range can
		// address, ITEM_QUAD_BUDGET what the whole catalog shares.
		if quads == 0 || quads > crate::viewmodel::MAX_QUADS {
			if quads > 0 {
				log::warn!("held item {kind:?} is {quads} quads, over MAX_QUADS; skipping");
			}
			self.items.push(ItemMesh {
				kind,
				base_vertex: 0,
				quads: 0,
			});
			return None;
		}
		if self.item_quads + quads > ITEM_QUAD_BUDGET {
			// The budget holds the whole catalog, so this can only fire if
			// the catalog grew past it.
			log::warn!("held-item cache full, skipping {kind:?}");
			self.items.push(ItemMesh {
				kind,
				base_vertex: 0,
				quads: 0,
			});
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
	/// `held` is `(kind, [eye.xyz, yaw, emissive, height in twelfths, then
	/// padding])`. Grouped by kind so each distinct item is ONE instanced
	/// draw; the set is one entry per person on screen, so the sort is
	/// over a handful of entries.
	pub fn set_held(&mut self, gpu: &Gpu, held: &[(IconKind, [f32; 8])]) {
		self.batches.clear();
		self.instances.clear();
		self.instances.extend_from_slice(held);
		self.instances.truncate(MAX_INSTANCES);
		let runs = group_by_kind(&mut self.instances);
		let mut flat: Vec<[f32; 8]> = Vec::with_capacity(self.instances.len());
		for (kind, run) in runs {
			if let Some(mi) = self.ensure_mesh(gpu, kind) {
				let first = flat.len() as u32;
				flat.extend(self.instances[run].iter().map(|(_, inst)| *inst));
				self.batches.push((mi, first..flat.len() as u32));
			}
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

	/// `vs_held`'s vertex layout spells its attribute offsets out as
	/// literals, so the struct they describe has to stay the shape they
	/// assume — a field added to `Vert` would otherwise reinterpret every
	/// held item's uv and texture layer as garbage with nothing failing.
	#[test]
	fn the_held_vertex_layout_matches_the_struct() {
		assert_eq!(std::mem::size_of::<Vert>(), 28);
		assert_eq!(std::mem::align_of::<Vert>(), 4);
	}

	/// players.wgsl's DIR_NORMALS is indexed by `Dir`'s discriminants, and
	/// nothing but this pin holds it to them: get one wrong and a held
	/// item's faces light from the wrong side, which reads as a shading
	/// bug rather than a table bug.
	#[test]
	fn the_shader_normal_table_matches_dir() {
		use digblock_core::dir::DIRS;
		let table: Vec<&str> = digblock_assets::shader::PLAYERS
			.split("const DIR_NORMALS = array<vec3f, 6>(")
			.nth(1)
			.expect("DIR_NORMALS declared")
			.split(");")
			.next()
			.expect("terminated")
			.lines()
			.filter_map(|l| l.trim().strip_prefix("vec3f("))
			.filter_map(|l| l.split(')').next())
			.collect();
		assert_eq!(table.len(), DIRS.len());
		for (d, row) in DIRS.into_iter().zip(table) {
			let (x, y, z) = d.offset();
			let want = format!("{:.1}, {:.1}, {:.1}", x as f32, y as f32, z as f32);
			assert_eq!(row, want, "{d:?} is index {}", d as usize);
		}
	}

	/// Every icon a hand can hold, enumerated the way `held_icon` derives
	/// them — through `hud::item_icon`, not by re-reading `props().shape`.
	/// A test that rebuilds the mapping cannot notice the mapping change.
	fn every_kind() -> Vec<IconKind> {
		use digblock_core::hotbar::HotbarItem;
		let mut v: Vec<IconKind> = BlockType::ALL
			.into_iter()
			.filter(|b| b.is_slottable())
			.map(|b| crate::ui::hud::item_icon(HotbarItem::Block(b), false).0)
			.collect();
		for full in [false, true] {
			v.push(crate::ui::hud::item_icon(HotbarItem::WaterTool, full).0);
		}
		// The TOOLS too. A tool is not slottable, so the filter above
		// cannot reach one — but `held_icon` admits a tool id off the
		// wire (it is how `Hotbar::to_wire` sends a non-bucket active
		// tool), so a remote can be holding one and the cache has to
		// have room for its authored geometry. The budget is what makes
		// eviction unnecessary; a kind it never counted breaks that.
		for b in BlockType::ALL.into_iter().filter(|b| b.is_tool()) {
			v.push(crate::ui::hud::tool_icon(b, false));
		}
		v
	}

	/// The cache holds the whole catalog at once, which is what makes
	/// eviction genuinely unnecessary rather than merely unlikely — and
	/// no single item may exceed what one vertex range can address, since
	/// `ensure_mesh` now SKIPS such an item rather than truncating it.
	#[test]
	fn every_held_item_fits_the_quad_budget() {
		let mut total = 0usize;
		let mut scratch = Vec::new();
		for kind in every_kind() {
			crate::viewmodel::build(kind, &mut scratch);
			let quads = scratch.len() / 4;
			assert!(
				quads <= crate::viewmodel::MAX_QUADS,
				"{kind:?} is {quads} quads and would not be drawn at all"
			);
			total += quads;
		}
		assert!(
			total <= ITEM_QUAD_BUDGET,
			"the whole catalog is {total} quads against a {ITEM_QUAD_BUDGET} budget"
		);
	}

	/// Grouping is what keeps a crowd to one draw per distinct item
	/// rather than one per player, and every instance must land in
	/// exactly one contiguous run.
	///
	/// Calls `group_by_kind` — the function `set_held` calls — so that
	/// changing the grouping breaks this rather than leaving it green
	/// against a copy of the old logic.
	#[test]
	fn held_items_group_one_run_per_kind() {
		let inst = |n: f32| [n, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		let mut v = [
			(IconKind::Shaped(BlockType::Torch), inst(0.0)),
			(IconKind::Cube(BlockType::Dirt), inst(1.0)),
			(IconKind::Shaped(BlockType::Torch), inst(2.0)),
			(IconKind::WaterTool { full: true }, inst(3.0)),
			(IconKind::Cube(BlockType::Dirt), inst(4.0)),
			(IconKind::Shaped(BlockType::Torch), inst(5.0)),
		];
		let runs = group_by_kind(&mut v);
		assert_eq!(runs.len(), 3, "three distinct items, three draws");
		// Contiguous, in order, and covering everything: that triple is
		// what lets a run be handed to draw_indexed as one instance range.
		assert_eq!(runs.first().map(|(_, r)| r.start), Some(0));
		assert_eq!(runs.last().map(|(_, r)| r.end), Some(v.len()));
		assert!(runs.windows(2).all(|w| w[0].1.end == w[1].1.start));
		assert!(
			runs.iter()
				.any(|(k, r)| *k == IconKind::Shaped(BlockType::Torch) && r.len() == 3)
		);
		// Every instance still present exactly once, just reordered — a
		// sort that dropped or duplicated one would still group cleanly.
		let mut seen: Vec<u32> = v.iter().map(|(_, i)| i[0] as u32).collect();
		seen.sort_unstable();
		assert_eq!(seen, [0, 1, 2, 3, 4, 5]);
	}

	/// Every kind a hand can hold must survive the cache, which is the
	/// property `set_held` needs and `every_held_item_fits_the_quad_budget`
	/// only half implies: an item can be inside both budgets and still
	/// build zero quads.
	#[test]
	fn every_held_item_builds_geometry() {
		let mut scratch = Vec::new();
		for kind in every_kind() {
			crate::viewmodel::build(kind, &mut scratch);
			assert!(!scratch.is_empty(), "{kind:?} builds nothing to draw");
		}
	}
}
