//! 2D overlay: the always-on HUD (crosshair) plus the toggleable debug
//! panel (metrics text + frame-time graph), drawn as one vertex buffer of
//! pixel-space quads through a single alpha-blended pipeline. With the
//! panel hidden the cost is a handful of quads and a tiny write_buffer.

use std::fmt::Write as _;

use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

use crate::font;

/// Frames shown in the frame-time graph.
pub const GRAPH_FRAMES: usize = 120;

use crate::ui::scale::{glyph_px, FIXED_GLYPH_PX};

// Logical bases, multiplied by the frame's ui_scale where a surface
// scales. The debug panel deliberately uses them raw (a diagnostic, not
// UI — small and dense is a feature).
const LINE_PX: f32 = 20.0;
const MARGIN: f32 = 8.0;
const PAD: f32 = 6.0;

const BAR_W: f32 = 2.0;
const GRAPH_H: f32 = 66.0;
/// Vertical scale: 3 px per ms (16.7ms line lands at 50px).
const MS_TO_PX: f32 = 3.0;

/// Scale a color's alpha byte (0xAABBGGRR layout) by `a` in 0..=1.
fn fade(color: u32, a: f32) -> u32 {
	let alpha = ((color >> 24) as f32 * a) as u32;
	(alpha << 24) | (color & 0x00FF_FFFF)
}

const WHITE: u32 = 0xFFFFFFFF;
const DIM: u32 = 0xFFB0B0B0;
const GREEN: u32 = 0xFF40E070;
const YELLOW: u32 = 0xFF30D0F0;
const RED: u32 = 0xFF4040F0;
const BG: u32 = 0x90000000;
const REF_LINE: u32 = 0x80FFFFFF;

/// Debug panel worst case ≈ 500 quads; a long menu server list adds ~60
/// chars × ~20 rows ≈ 1300 more. 8192 leaves lots of headroom.
const MAX_VERTS: usize = 6 * 8192;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OverlayVertex {
	pos: [f32; 2],
	uv: [f32; 2],
	color: u32,
}

pub struct Overlay {
	pipeline: wgpu::RenderPipeline,
	bind_group: wgpu::BindGroup,
	uniform_buf: wgpu::Buffer,
	vertex_buf: wgpu::Buffer,
	verts: Vec<OverlayVertex>,
	vert_count: u32,
	/// Vertex indices splitting the buffer into three ranges: below the
	/// main icons (crosshair + draw-list Backdrop), the content band
	/// (draw-list Content + legacy text surfaces), and above the ghost
	/// icon (draw-list Ghost). The icon pipeline's main and ghost
	/// stages draw between them. Empty ranges cost nothing.
	splits: [u32; 2],
	line_scratch: String,
	/// This frame's UI scale and derived glyph size, set by `prepare`.
	scale: f32,
	glyph: f32,
}

impl Overlay {
	pub fn new(gpu: &Gpu, surface_format: wgpu::TextureFormat) -> Self {
		let device = &gpu.device;
		let atlas = font::build_atlas();
		let tex = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("overlay font"),
			size: wgpu::Extent3d {
				width: font::ATLAS_W,
				height: font::ATLAS_H,
				depth_or_array_layers: 1,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: wgpu::TextureFormat::R8Unorm,
			usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
			view_formats: &[],
		});
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture: &tex,
				mip_level: 0,
				origin: wgpu::Origin3d::ZERO,
				aspect: wgpu::TextureAspect::All,
			},
			&atlas,
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(font::ATLAS_W),
				rows_per_image: None,
			},
			wgpu::Extent3d {
				width: font::ATLAS_W,
				height: font::ATLAS_H,
				depth_or_array_layers: 1,
			},
		);
		let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
		// Nearest everywhere: crisp pixel font at integer scales.
		let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("overlay font"),
			mag_filter: wgpu::FilterMode::Nearest,
			min_filter: wgpu::FilterMode::Nearest,
			mipmap_filter: wgpu::FilterMode::Nearest,
			..Default::default()
		});
		let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("overlay uniform"),
			size: 16,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("overlay"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::VERTEX,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: wgpu::BufferSize::new(16),
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count: None,
				},
			],
		});
		let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("overlay"),
			layout: &bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: uniform_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::Sampler(&sampler),
				},
			],
		});
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("overlay"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::OVERLAY.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("overlay"),
			bind_group_layouts: &[&bgl],
			push_constant_ranges: &[],
		});
		let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("overlay"),
			layout: Some(&layout),
			vertex: wgpu::VertexState {
				module: &shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[wgpu::VertexBufferLayout {
					array_stride: std::mem::size_of::<OverlayVertex>() as u64,
					step_mode: wgpu::VertexStepMode::Vertex,
					attributes: &wgpu::vertex_attr_array![
						0 => Float32x2, 1 => Float32x2, 2 => Uint32
					],
				}],
			},
			fragment: Some(wgpu::FragmentState {
				module: &shader,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_format,
					blend: Some(wgpu::BlendState::ALPHA_BLENDING),
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				cull_mode: None,
				..Default::default()
			},
			depth_stencil: None,
			multisample: wgpu::MultisampleState::default(),
			multiview: None,
			cache: None,
		});
		let vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("overlay verts"),
			size: (MAX_VERTS * std::mem::size_of::<OverlayVertex>()) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			bind_group,
			uniform_buf,
			vertex_buf,
			verts: Vec::with_capacity(MAX_VERTS),
			vert_count: 0,
			splits: [0; 2],
			line_scratch: String::with_capacity(96),
			scale: 1.0,
			glyph: FIXED_GLYPH_PX,
		}
	}

	fn push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
		if self.verts.len() + 6 > MAX_VERTS {
			return;
		}
		let uv = font::solid_uv();
		let v = |px, py| OverlayVertex {
			pos: [px, py],
			uv,
			color,
		};
		let (x1, y1) = (x + w, y + h);
		self.verts
			.extend_from_slice(&[v(x, y), v(x1, y), v(x, y1), v(x1, y), v(x1, y1), v(x, y1)]);
	}

	/// Draw `text` with `px`-sized square glyphs (`self.glyph` for scaled
	/// surfaces, [`FIXED_GLYPH_PX`] for pinned ones).
	fn push_text(&mut self, x: f32, y: f32, px: f32, color: u32, text: &str) {
		let mut cx = x;
		for c in text.bytes() {
			if c != b' ' && self.verts.len() + 6 <= MAX_VERTS {
				let (uv0, uv1) = font::glyph_uv(c);
				let v = |px, py, u, w| OverlayVertex {
					pos: [px, py],
					uv: [u, w],
					color,
				};
				let (x1, y1) = (cx + px, y + px);
				self.verts.extend_from_slice(&[
					v(cx, y, uv0[0], uv0[1]),
					v(x1, y, uv1[0], uv0[1]),
					v(cx, y1, uv0[0], uv1[1]),
					v(x1, y, uv1[0], uv0[1]),
					v(x1, y1, uv1[0], uv1[1]),
					v(cx, y1, uv0[0], uv1[1]),
				]);
			}
			cx += px;
		}
	}

	/// Execute one draw-list layer's quad/text commands into the vertex
	/// buffer (icon commands belong to `ui::icons::UiIcons`).
	fn run_cmds(&mut self, dl: &crate::ui::DrawList, layer: crate::ui::Layer) {
		let (cmds, arena) = dl.cmds(layer);
		for cmd in cmds {
			match *cmd {
				crate::ui::draw::Cmd::Quad { rect, color } => {
					self.push_quad(rect.x, rect.y, rect.w, rect.h, color);
				}
				crate::ui::draw::Cmd::Text {
					x,
					y,
					px,
					color,
					range,
				} => {
					let text = &arena[range.0 as usize..range.1 as usize];
					self.push_text(x, y, px, color, text);
				}
				crate::ui::draw::Cmd::Icon { .. } => {}
			}
		}
	}

	/// Rebuild the vertex buffer for this frame: the draw list's
	/// Backdrop/Content/Ghost layers (crosshair, HUD, screens, menu —
	/// all engine-emitted), plus the legacy direct-push surfaces (debug
	/// panel, feed, chat, nametags), which draw UNDER the Content layer
	/// so screens can dim them. The two derived splits bracket the icon
	/// pipeline's main and ghost stages.
	#[allow(clippy::too_many_arguments)]
	pub fn prepare(
		&mut self,
		gpu: &Gpu,
		size: (u32, u32),
		ui_scale: f32,
		ui: &crate::ui::DrawList,
		stats: Option<(&OverlayStats, &[f32])>,
		feed: &[(String, f32, u32)],
		chat: Option<&crate::chat::ChatView<'_>>,
		nametags: &[crate::renderer::NameTag],
	) {
		self.scale = ui_scale;
		self.glyph = glyph_px(ui_scale);
		self.verts.clear();
		self.run_cmds(ui, crate::ui::Layer::Backdrop);
		self.splits[0] = self.verts.len() as u32;
		self.push_nametags(size, nametags);
		if let Some((stats, history)) = stats {
			self.push_debug_panel(size, stats, history);
		}
		// Chat column: the input line is pinned to the top-left corner and
		// the feed stacks directly beneath it (newest at the top, so the
		// latest line always sits next to where the player types).
		let mut feed_top = MARGIN * ui_scale;
		if let Some(view) = chat {
			feed_top = self.push_chat_input(size, view);
		}
		if !feed.is_empty() {
			self.push_feed(size, feed, feed_top);
		}
		self.run_cmds(ui, crate::ui::Layer::Content);
		self.splits[1] = self.verts.len() as u32;
		self.run_cmds(ui, crate::ui::Layer::Ghost);
		self.vert_count = self.verts.len() as u32;
		gpu.queue.write_buffer(
			&self.uniform_buf,
			0,
			bytemuck::cast_slice(&[size.0 as f32, size.1 as f32, 0.0, 0.0]),
		);
		if !self.verts.is_empty() {
			gpu.queue
				.write_buffer(&self.vertex_buf, 0, bytemuck::cast_slice(&self.verts));
		}
	}

	fn push_debug_panel(&mut self, size: (u32, u32), stats: &OverlayStats, history: &[f32]) {
		// Compose the text block first so the backdrop can be sized to it.
		let mut lines: [(u32, String); 6] = Default::default();
		let s = stats;
		let mut line = |i: usize, color: u32, args: std::fmt::Arguments<'_>| {
			self.line_scratch.clear();
			let _ = self.line_scratch.write_fmt(args);
			lines[i] = (color, self.line_scratch.clone());
		};
		line(
			0,
			WHITE,
			format_args!(
				"fps {:5.1} | avg {:5.2} p95 {:5.2} p99 {:5.2} worst {:6.2} | hitch {} ({})",
				s.fps, s.avg_ms, s.p95_ms, s.p99_ms, s.worst_ms, s.hitches_window, s.hitches_total
			),
		);
		let c = &s.smooth;
		line(
			1,
			DIM,
			format_args!(
				"cpu  acq {:5.2} upd {:5.2} mnt {:5.2} upl {:5.2} enc {:5.2} ms",
				c.acquire_ms, c.update_ms, c.maintain_ms, c.upload_ms, c.encode_ms
			),
		);
		match s.gpu {
			Some(g) => line(
				2,
				DIM,
				format_args!(
					"gpu  shadow {:5.2} main {:5.2} water {:5.2} overlay {:5.2} ms",
					g.shadow_ms, g.main_ms, g.water_ms, g.overlay_ms
				),
			),
			None => line(2, DIM, format_args!("gpu  n/a")),
		}
		line(
			3,
			DIM,
			format_args!(
				"jobs gen q {} n {} avg {:4.2} max {:4.2} | mesh q {} n {} avg {:4.2} max {:4.2} | fly {}",
				s.gauges.gen_queue,
				s.gen_jobs.count,
				s.gen_jobs.avg_ms(),
				s.gen_jobs.max_ms,
				s.gauges.mesh_queue,
				s.mesh_jobs.count,
				s.mesh_jobs.avg_ms(),
				s.mesh_jobs.max_ms,
				s.gauges.in_flight
			),
		);
		line(
			4,
			DIM,
			format_args!(
				"work up {} stale {} req {} evict {} sat {} | res {} ({} gpu) | wrk {}",
				s.uploaded,
				s.stale_dropped,
				s.requeued,
				s.evicted,
				s.upload_saturated,
				s.gauges.resident,
				s.render.chunks_resident,
				s.workers
			),
		);
		let r = &s.render;
		line(
			5,
			DIM,
			format_args!(
				"draw {} drawn {} culled | water {} cast {} calls {} tri {:.2}M vram {:.1}M{}",
				r.chunks_drawn,
				r.chunks_culled,
				r.water_draws,
				r.shadow_casters,
				r.draw_calls,
				f64::from(r.triangles) / 1.0e6,
				s.gpu_buffer_bytes as f64 / (1024.0 * 1024.0),
				if r.wide_refreshed { " | wide" } else { "" }
			),
		);

		let max_chars = lines.iter().map(|(_, l)| l.len()).max().unwrap_or(0);
		let text_w = max_chars as f32 * FIXED_GLYPH_PX;
		let graph_w = GRAPH_FRAMES as f32 * BAR_W;
		let panel_w = text_w.max(graph_w) + PAD * 2.0;
		let panel_h = lines.len() as f32 * LINE_PX + GRAPH_H + PAD * 3.0;
		// Right-anchored: the top-left corner belongs to the chat column.
		let panel_x = (size.0 as f32 - MARGIN - panel_w).max(MARGIN);
		self.push_quad(panel_x, MARGIN, panel_w, panel_h, BG);

		let (tx, mut ty) = (panel_x + PAD, MARGIN + PAD);
		for entry in &mut lines {
			let (color, text) = std::mem::take(entry);
			self.push_text(tx, ty, FIXED_GLYPH_PX, color, &text);
			ty += LINE_PX;
		}

		// Frame-time graph: newest bar on the right, green under 16.7ms,
		// yellow under 33.4, red above; faint line at the 60Hz budget.
		let gy = ty + PAD + GRAPH_H; // baseline
		let start = GRAPH_FRAMES.saturating_sub(history.len());
		for (i, &ms) in history.iter().enumerate() {
			let h = (ms * MS_TO_PX).clamp(1.0, GRAPH_H);
			let color = if ms <= 16.7 {
				GREEN
			} else if ms <= 33.4 {
				YELLOW
			} else {
				RED
			};
			let x = tx + (start + i) as f32 * BAR_W;
			self.push_quad(x, gy - h, BAR_W - 0.5, h, color);
		}
		self.push_quad(tx, gy - 16.7 * MS_TO_PX, graph_w, 1.0, REF_LINE);
	}

	/// Characters per wrapped chat-column line: the column starts at the
	/// left margin and wraps when it reaches the midpoint of the screen.
	fn wrap_chars(&self, size: (u32, u32)) -> usize {
		let usable = size.0 as f32 / 2.0 - (MARGIN + PAD * 2.0) * self.scale;
		((usable / self.glyph) as usize).max(8)
	}

	/// Server-message feed (system notifications + chat): top-left under
	/// the input line, NEWEST at the top so the latest message sits next
	/// to where the player types; long lines wrap at the screen midpoint;
	/// each entry fades out over its final second (`ttl` clamps to alpha)
	/// in its own color (chat lines are tinted per sender). `top` is
	/// where the block starts (below the open input line).
	fn push_feed(&mut self, size: (u32, u32), feed: &[(String, f32, u32)], top: f32) {
		let s = self.scale;
		let (line, margin, pad, glyph) = (LINE_PX * s, MARGIN * s, PAD * s, self.glyph);
		let wrap = self.wrap_chars(size);
		let mut y = top;
		for (text, ttl, color) in feed.iter().rev() {
			let a = ttl.clamp(0.0, 1.0);
			// Feed text is printable ASCII end-to-end (the server
			// sanitizes chat; system lines are ours), so byte chunks
			// are char chunks.
			for chunk in text.as_bytes().chunks(wrap) {
				let chunk = std::str::from_utf8(chunk).unwrap_or_default();
				let w = chunk.len() as f32 * glyph + pad * 2.0;
				self.push_quad(margin, y, w, line, fade(BG, a));
				self.push_text(
					margin + pad,
					y + (line - glyph) / 2.0,
					glyph,
					fade(*color, a),
					chunk,
				);
				y += line;
			}
		}
	}

	/// Chat input: pinned to the top-left corner, wrapping at the screen
	/// midpoint, with a blinking block caret riding the end of the text
	/// (same blink clock as the menu's name field). Returns the y where
	/// the feed continues below.
	fn push_chat_input(&mut self, size: (u32, u32), view: &crate::chat::ChatView<'_>) -> f32 {
		let s = self.scale;
		let (line, margin, pad, glyph) = (LINE_PX * s, MARGIN * s, PAD * s, self.glyph);
		let wrap = self.wrap_chars(size);
		// "> " prefix + text + one caret cell, wrapped as a single run
		// (chat input is ASCII-only, byte chunks are char chunks). The
		// +1 caret cell can spill onto a row of its own when the text
		// exactly fills the last row.
		let content = format!("> {}", view.line);
		let rows = (content.len() + 1).div_ceil(wrap);
		let mut y = margin;
		for r in 0..rows {
			let chunk = &content.as_bytes()
				[(r * wrap).min(content.len())..((r + 1) * wrap).min(content.len())];
			let chunk = std::str::from_utf8(chunk).unwrap_or_default();
			let last = r == rows - 1;
			let cells = chunk.len() + usize::from(last);
			let min_w = if r == 0 { 240.0 * s } else { 0.0 };
			let w = (cells as f32 * glyph + pad * 2.0).max(min_w);
			self.push_quad(margin, y, w, line, 0xC0000000);
			let ty = y + (line - glyph) / 2.0;
			self.push_text(margin + pad, ty, glyph, WHITE, chunk);
			if last && view.caret {
				let cx = margin + pad + glyph * chunk.len() as f32;
				self.push_quad(cx, ty, glyph * 0.6, glyph, WHITE);
			}
			y += line;
		}
		y
	}

	/// Name tags: centered text with a dim backdrop at pre-projected
	/// screen positions. No depth — tags read through walls, which keeps
	/// them useful as a "someone is over there" cue.
	fn push_nametags(&mut self, size: (u32, u32), tags: &[crate::renderer::NameTag]) {
		let (pad, glyph) = (PAD * self.scale, self.glyph);
		for tag in tags {
			let w = tag.text.len() as f32 * glyph;
			let x = (tag.x - w / 2.0).clamp(0.0, (size.0 as f32 - w).max(0.0));
			let y = tag.y.clamp(0.0, size.1 as f32 - glyph);
			self.push_quad(x - pad / 2.0, y - 2.0, w + pad, glyph + 4.0, 0x60000000);
			self.push_text(x, y, glyph, tag.color, &tag.text);
		}
	}

	/// The range under the main icons (crosshair, slot/panel backdrops).
	pub fn draw_below(&self, pass: &mut wgpu::RenderPass<'_>) {
		self.draw_range(pass, 0, self.splits[0]);
	}

	/// The content band over the main icons (counts, labels, panel,
	/// feed, menu) but under the ghost icon.
	pub fn draw_mid(&self, pass: &mut wgpu::RenderPass<'_>) {
		self.draw_range(pass, self.splits[0], self.splits[1]);
	}

	/// The topmost range (ghost-layer quads/text, over the ghost icon).
	pub fn draw_above(&self, pass: &mut wgpu::RenderPass<'_>) {
		self.draw_range(pass, self.splits[1], self.vert_count);
	}

	fn draw_range(&self, pass: &mut wgpu::RenderPass<'_>, from: u32, to: u32) {
		if from >= to {
			return;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.bind_group, &[]);
		pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
		pass.draw(from..to, 0..1);
	}
}
