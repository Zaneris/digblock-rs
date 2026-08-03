//! Hotbar HUD: a pure view + pixel layout (headless-testable, the
//! menu.rs pattern) and the tiny icon pipeline that samples the block
//! texture array inside the overlay pass. Slot backgrounds, selection
//! border, and count text ride the existing overlay quad buffer (below /
//! above the icons via its draw split); only the RGBA icons need this
//! second pipeline — the overlay's own texture is the R8 font atlas.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{Hotbar, HotbarItem, HOTBAR_SLOTS};
use digblock_core::inventory::Inventory;
use digblock_platform::Gpu;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SlotView {
	Empty,
	Tool,
	Block { block: BlockType, count: u32 },
}

pub struct HotbarView {
	pub slots: [SlotView; HOTBAR_SLOTS],
	pub selected: usize,
}

impl HotbarView {
	pub fn build(hotbar: &Hotbar, inventory: &Inventory) -> Self {
		let mut slots = [SlotView::Empty; HOTBAR_SLOTS];
		for (i, s) in hotbar.slots().iter().enumerate() {
			slots[i] = match s {
				None => SlotView::Empty,
				Some(HotbarItem::WaterTool) => SlotView::Tool,
				Some(HotbarItem::Block(b)) => SlotView::Block {
					block: *b,
					count: inventory.count(*b),
				},
			};
		}
		Self {
			slots,
			selected: hotbar.selected_index(),
		}
	}
}

pub const SLOT_PX: f32 = 48.0;
pub const SLOT_GAP: f32 = 4.0;
const ICON_PX: f32 = 40.0;
const BOTTOM_MARGIN: f32 = 12.0;
/// Zero-count slots stay visible but greyed (sticky slots never shift).
const GREY_TINT: u32 = 0xFF60_6060;
const FULL_TINT: u32 = 0xFFFF_FFFF;
/// The water tool: the water texture pushed hard toward blue.
const TOOL_TINT: u32 = 0xFFFF_9060;

/// One laid-out slot, consumed by both the overlay (rect, selection,
/// count) and the icon pipeline (icon layer + tint).
pub struct PlacedSlot {
	/// Background rect in pixels: x, y, w, h.
	pub rect: [f32; 4],
	/// (texture-array layer, 0xAABBGGRR tint); None for empty slots.
	pub icon: Option<(u16, u32)>,
	/// Block count to print (greyed at zero); None for tool/empty.
	pub count: Option<u32>,
	pub selected: bool,
}

/// Pure pixel layout: bottom-center row (the feed sits bottom-LEFT — no
/// collision at any sane width).
pub fn layout(size: (u32, u32), view: &HotbarView) -> [PlacedSlot; HOTBAR_SLOTS] {
	let total_w = HOTBAR_SLOTS as f32 * SLOT_PX + (HOTBAR_SLOTS as f32 - 1.0) * SLOT_GAP;
	let x0 = (size.0 as f32 - total_w) / 2.0;
	let y = size.1 as f32 - BOTTOM_MARGIN - SLOT_PX;
	std::array::from_fn(|i| {
		let x = x0 + i as f32 * (SLOT_PX + SLOT_GAP);
		let (icon, count) = match view.slots[i] {
			SlotView::Empty => (None, None),
			SlotView::Tool => (Some((BlockType::Water.props().tex_side, TOOL_TINT)), None),
			SlotView::Block { block, count } => (
				Some((
					block.props().tex_side,
					if count == 0 { GREY_TINT } else { FULL_TINT },
				)),
				Some(count),
			),
		};
		PlacedSlot {
			rect: [x, y, SLOT_PX, SLOT_PX],
			icon,
			count,
			selected: i == view.selected,
		}
	})
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IconVertex {
	pos: [f32; 2],
	uv: [f32; 2],
	layer: u32,
	tint: u32,
}

const MAX_ICON_VERTS: usize = HOTBAR_SLOTS * 6;

pub struct HotbarIcons {
	pipeline: wgpu::RenderPipeline,
	bind_group: wgpu::BindGroup,
	uniform_buf: wgpu::Buffer,
	vertex_buf: wgpu::Buffer,
	verts: Vec<IconVertex>,
	count: u32,
}

impl HotbarIcons {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		materials_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("hotbar icons uniform"),
			size: 16,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("hotbar icons"),
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
			label: Some("hotbar icons"),
			layout: &bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: uniform_buf.as_entire_binding(),
			}],
		});
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("hotbar icons"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::HOTBAR_ICONS.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("hotbar icons"),
			bind_group_layouts: &[&bgl, materials_bgl],
			push_constant_ranges: &[],
		});
		let pipeline = crate::pipeline_builder::build_depthless(
			device,
			surface_format,
			&crate::pipeline_builder::PipelineDesc {
				label: "hotbar icons",
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
			label: Some("hotbar icon verts"),
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
			count: 0,
		}
	}

	/// Rebuild this frame's icon quads; `None` slots draw nothing.
	pub fn prepare(&mut self, gpu: &Gpu, size: (u32, u32), slots: Option<&[PlacedSlot]>) {
		self.verts.clear();
		for s in slots.unwrap_or(&[]) {
			let Some((layer, tint)) = s.icon else {
				continue;
			};
			let [sx, sy, sw, sh] = s.rect;
			let (x, y) = (sx + (sw - ICON_PX) / 2.0, sy + (sh - ICON_PX) / 2.0);
			let v = |px, py, u, w| IconVertex {
				pos: [px, py],
				uv: [u, w],
				layer: u32::from(layer),
				tint,
			};
			let (x1, y1) = (x + ICON_PX, y + ICON_PX);
			self.verts.extend_from_slice(&[
				v(x, y, 0.0, 0.0),
				v(x1, y, 1.0, 0.0),
				v(x, y1, 0.0, 1.0),
				v(x1, y, 1.0, 0.0),
				v(x1, y1, 1.0, 1.0),
				v(x, y1, 0.0, 1.0),
			]);
		}
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

	/// Draw in the overlay pass, between the overlay's below/above splits.
	pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, materials_bg: &wgpu::BindGroup) {
		if self.count == 0 {
			return;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.bind_group, &[]);
		pass.set_bind_group(1, materials_bg, &[]);
		pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
		pass.draw(0..self.count, 0..1);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample_view() -> HotbarView {
		let mut hotbar = Hotbar::new();
		hotbar.auto_assign(BlockType::Dirt);
		hotbar.auto_assign(BlockType::Stone);
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 3);
		HotbarView::build(&hotbar, &inv)
	}

	#[test]
	fn view_mirrors_hotbar_and_counts() {
		let v = sample_view();
		assert_eq!(
			v.slots[0],
			SlotView::Block {
				block: BlockType::Dirt,
				count: 3
			}
		);
		assert_eq!(
			v.slots[1],
			SlotView::Block {
				block: BlockType::Stone,
				count: 0
			}
		);
		assert_eq!(v.slots[2], SlotView::Empty);
		assert_eq!(v.slots[HOTBAR_SLOTS - 1], SlotView::Tool);
		assert_eq!(v.selected, 0);
	}

	#[test]
	fn layout_is_bottom_centered_and_marks_state() {
		let size = (1280u32, 720u32);
		let placed = layout(size, &sample_view());
		let first = placed[0].rect;
		let last = placed[HOTBAR_SLOTS - 1].rect;
		let left = first[0];
		let right = last[0] + last[2];
		assert!(
			(left - (size.0 as f32 - right)).abs() < 0.5,
			"centered: {left} vs {}",
			size.0 as f32 - right
		);
		assert_eq!(first[1] + first[3], size.1 as f32 - BOTTOM_MARGIN);
		assert!(placed[0].selected && !placed[1].selected);
		// Zero-count stone: greyed icon, count 0 printed.
		assert_eq!(placed[1].icon.unwrap().1, GREY_TINT);
		assert_eq!(placed[1].count, Some(0));
		// The tool (last slot) has an icon but no count; empties neither.
		let tool = &placed[HOTBAR_SLOTS - 1];
		assert!(tool.icon.is_some() && tool.count.is_none());
		assert!(placed[2].icon.is_none() && placed[2].count.is_none());
	}
}
