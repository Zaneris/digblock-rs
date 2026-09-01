//! GPU consumer for [`Layer::Icons`]/[`Layer::Ghost`] icon commands: a
//! depthless alpha-blended pipeline sampling the block materials
//! texture array (bind group 1 = the renderer's materials bind group).
//! Draws in the overlay pass, sandwiched by the overlay's own splits.

use digblock_core::block::{BlockType, SHAPED_COUNT};
use digblock_core::hotbar::HOTBAR_SLOTS;
use digblock_core::inv_grid::GRID_SLOTS;
use digblock_platform::Gpu;

use super::draw::{Cmd, DrawList, Layer};

/// Worst case: every visible slot (grid + bar + ghost) carries a 3D icon. Plain cubes and the
/// bucket project a handful of quads; shaped items project up to two variants' templates (a
/// door's two halves), and a shaped type can sit in the grid AND on the bar only once each, since
/// assignments are unique per type. Derived from the core counts so a new `blocks!` row can never
/// overflow it.
const BUCKET_QUADS: usize = 7;
const SHAPED_SITES: usize = 2 * SHAPED_COUNT + 1;
/// Chips: the essence readout, one price's lines, every spell fare and
/// every live toast, each at worst a cube (a feedstock chip) — plus the
/// spell bar's flat sprites and their drag ghost.
const CHIP_SITES: usize = 2 * digblock_core::block::Essence::COUNT
	+ digblock_core::alchemy::MAX_COST_LINES
	+ digblock_core::worldgen::SurfaceKind::ALL.len()
	+ digblock_core::skills::Skill::COUNT
	+ 4 + crate::spellbar::SPELL_SLOTS
	+ 1;
const MAX_ICON_QUADS: usize = (GRID_SLOTS + HOTBAR_SLOTS + 1 + CHIP_SITES) * BUCKET_QUADS
	+ SHAPED_SITES * 2 * digblock_core::shapes::MAX_SHAPE_QUADS;
const MAX_ICON_VERTS: usize = MAX_ICON_QUADS * 6;
// The budget must dominate the naive one-flat-quad-per-site bound too
// (alchemy rows reuse the same sites).
const _: () = assert!(MAX_ICON_QUADS > BlockType::COUNT + HOTBAR_SLOTS);

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IconVertex {
	pos: [f32; 2],
	uv: [f32; 2],
	layer: u32,
	tint: u32,
}

pub struct UiIcons {
	pipeline: wgpu::RenderPipeline,
	bind_group: wgpu::BindGroup,
	uniform_buf: wgpu::Buffer,
	vertex_buf: wgpu::Buffer,
	verts: Vec<IconVertex>,
	/// Vertex index where the Ghost-layer icons start.
	ghost_split: u32,
	count: u32,
}

impl UiIcons {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		materials_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("ui icons uniform"),
			size: 16,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("ui icons"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::VERTEX,
				ty: wgpu::BindingType::Buffer {
					ty: wgpu::BufferBindingType::Uniform,
					has_dynamic_offset: false,
					min_binding_size: wgpu::BufferSize::new(16),
				},
				count: None,
			}],
		});
		let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("ui icons"),
			layout: &bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: uniform_buf.as_entire_binding(),
			}],
		});
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("ui icons"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::HOTBAR_ICONS.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("ui icons"),
			bind_group_layouts: &[Some(&bgl), Some(materials_bgl)],
			immediate_size: 0,
		});
		let pipeline = crate::pipeline_builder::build_depthless(
			device,
			surface_format,
			&crate::pipeline_builder::PipelineDesc {
				label: "ui icons",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: std::mem::size_of::<IconVertex>() as u64,
					step_mode: wgpu::VertexStepMode::Vertex,
					attributes: &wgpu::vertex_attr_array![
						0 => Float32x2, 1 => Float32x2, 2 => Uint32, 3 => Uint32
					],
				}],
				cull: None,
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState::ALPHA_BLENDING),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("ui icon verts"),
			size: (MAX_ICON_VERTS * std::mem::size_of::<IconVertex>()) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			bind_group,
			uniform_buf,
			vertex_buf,
			verts: Vec::with_capacity(MAX_ICON_VERTS),
			ghost_split: 0,
			count: 0,
		}
	}

	fn push_layer(&mut self, dl: &DrawList, layer: Layer) {
		for cmd in dl.cmds(layer).0 {
			if self.verts.len() + 6 > MAX_ICON_VERTS {
				debug_assert!(false, "icon vertex budget overflow");
				return;
			}
			match *cmd {
				Cmd::Icon {
					rect,
					tex_layer,
					tint,
				} => {
					let v = |px, py, u, w| IconVertex {
						pos: [px, py],
						uv: [u, w],
						layer: u32::from(tex_layer),
						tint,
					};
					let (x, y, x1, y1) = (rect.x, rect.y, rect.x + rect.w, rect.y + rect.h);
					self.verts.extend_from_slice(&[
						v(x, y, 0.0, 0.0),
						v(x1, y, 1.0, 0.0),
						v(x, y1, 0.0, 1.0),
						v(x1, y, 1.0, 0.0),
						v(x1, y1, 1.0, 1.0),
						v(x, y1, 0.0, 1.0),
					]);
				}
				// Projected 3D-icon face: [TL, TR, BL, BR] corners with
				// per-corner UVs — same vertex format, two triangles.
				Cmd::IconQuad {
					corners,
					uvs,
					tex_layer,
					tint,
				} => {
					let v = |i: usize| IconVertex {
						pos: corners[i],
						uv: uvs[i],
						layer: u32::from(tex_layer),
						tint,
					};
					self.verts
						.extend_from_slice(&[v(0), v(1), v(2), v(1), v(3), v(2)]);
				}
				_ => {}
			}
		}
	}

	/// Rebuild this frame's icon quads from the draw list's Icons layer,
	/// then the Ghost layer (drawn separately, above all content).
	pub fn prepare(&mut self, gpu: &Gpu, size: (u32, u32), dl: &DrawList) {
		self.verts.clear();
		self.push_layer(dl, Layer::Icons);
		self.ghost_split = self.verts.len() as u32;
		self.push_layer(dl, Layer::Ghost);
		self.count = self.verts.len() as u32;
		if self.count > 0 {
			gpu.queue.write_buffer(
				&self.uniform_buf,
				0,
				bytemuck::cast_slice(&[size.0 as f32, size.1 as f32, 0.0, 0.0]),
			);
			gpu.queue
				.write_buffer(&self.vertex_buf, 0, bytemuck::cast_slice(&self.verts));
		}
	}

	/// Main icons: between the overlay's Backdrop and Content stages.
	pub fn draw_main(&self, pass: &mut wgpu::RenderPass<'_>, materials_bg: &wgpu::BindGroup) {
		self.draw_range(pass, materials_bg, 0, self.ghost_split);
	}

	/// The held/ghost icon: above everything, including Content text.
	pub fn draw_ghost(&self, pass: &mut wgpu::RenderPass<'_>, materials_bg: &wgpu::BindGroup) {
		self.draw_range(pass, materials_bg, self.ghost_split, self.count);
	}

	fn draw_range(
		&self,
		pass: &mut wgpu::RenderPass<'_>,
		materials_bg: &wgpu::BindGroup,
		from: u32,
		to: u32,
	) {
		if from >= to {
			return;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.bind_group, &[]);
		pass.set_bind_group(1, materials_bg, &[]);
		pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
		pass.draw(from..to, 0..1);
	}
}
