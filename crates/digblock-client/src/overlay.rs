//! 2D overlay: the always-on HUD (crosshair) plus the toggleable debug
//! panel (metrics text + frame-time graph), drawn as one vertex buffer of
//! pixel-space quads through a single alpha-blended pipeline. With the
//! panel hidden the cost is a handful of quads and a tiny write_buffer.

use std::fmt::Write as _;

use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

use crate::font;

/// Frames shown in the frame-time graph.
pub(crate) const GRAPH_FRAMES: usize = 120;

use crate::ui::scale::{FIXED_GLYPH_PX, glyph_px};

// Logical bases, multiplied by the frame's ui_scale where a surface
// scales. The debug panel deliberately uses them raw (a diagnostic, not
// UI — small and dense is a feature).
const LINE_PX: f32 = 20.0;
const MARGIN: f32 = 8.0;
const PAD: f32 = 6.0;

/// A millisecond figure that may be unknown, in a FIXED column width either way — the debug
/// panel's width is a budget, not a measurement, so a field that changed width would slide every
/// column after it. Unknown is dashes, never 0.0: a transport with no estimate (loopback, the
/// benchmark wires) would otherwise read as a perfect zero-latency connection, which is the one
/// wrong answer worse than no answer.
struct MsOrDash(f32);

impl std::fmt::Display for MsOrDash {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.0 > 0.0 {
			write!(f, "{:5.1}", self.0)
		} else {
			f.write_str("   --")
		}
	}
}

/// Column budget for the debug panel. The panel's width is derived from this rather than from the
/// current frame's longest line, so it never resizes or slides while numbers stream through it.
/// Every field in the panel's lines is printf-padded to a fixed width, so this only has to cover
/// the widest. `debug_lines_fit_the_column_budget` pins it from both sides: every line must fit,
/// and the budget must not sit far past the widest, so growing it to paper over a line is out.
const DEBUG_COLS: usize = 116;
/// Rows on the debug panel — `debug_lines` fills exactly this many.
const DEBUG_LINES: usize = 9;

/// Baseline-to-baseline spacing in the debug panel. Never tighter than
/// the mono face's own line box, so swapping in a taller face can't make
/// rows overlap.
fn debug_line_px() -> f32 {
	LINE_PX.max(font::mono().line_h(FIXED_GLYPH_PX).ceil())
}

/// First `cols` characters — the debug panel's backdrop is a fixed width,
/// so an unexpectedly long line is clipped rather than allowed to spill
/// out of it.
fn truncate_cols(s: &str, cols: usize) -> &str {
	match s.char_indices().nth(cols) {
		Some((i, _)) => &s[..i],
		None => s,
	}
}

/// Caret width as a fraction of the text box height. A block caret, as
/// before — but now it can't be one glyph cell wide, because glyph cells
/// aren't a thing any more.
const CARET_W: f32 = 0.5;

/// Already-faded messages the chat column brings back while the input
/// box is open, so you can see what you are replying to.
const CHAT_HISTORY_LINES: usize = 6;
/// How many matching commands the box offers at once. A bare `/` matches
/// every one of them, and the whole catalog under the input line is the
/// wall this list exists to avoid.
const CHAT_SUGGESTIONS: usize = 6;
/// Alpha for those recalled lines: legible, but plainly in the past.
const HISTORY_ALPHA: f32 = 0.45;

/// The feed entries to draw, newest first, each with the alpha to draw it at. Live messages fade
/// over their final second; once the ttl goes non-positive an entry is normally invisible, but
/// with the chat box open the most recent [`CHAT_HISTORY_LINES`] come back dim. The engine pushes
/// oldest-first and never reorders, so walking in reverse visits strictly decreasing ttls and
/// `map_while` can stop at the first entry that isn't being shown.
fn visible_feed<T>(
	feed: &[(T, f32, u32)],
	chat_open: bool,
) -> impl Iterator<Item = (&T, f32, u32)> {
	let mut history = 0usize;
	feed.iter().rev().map_while(move |(line, ttl, color)| {
		if *ttl > 0.0 {
			Some((line, ttl.clamp(0.0, 1.0), *color))
		} else if chat_open && history < CHAT_HISTORY_LINES {
			history += 1;
			Some((line, HISTORY_ALPHA, *color))
		} else {
			None
		}
	})
}

const BAR_W: f32 = 2.0;
const GRAPH_H: f32 = 66.0;
/// Vertical scale of the frame-time graph, in px per ms.
const MS_TO_PX: f32 = 3.0;

/// A run's colour: `Plain` keeps the entry's own, so chat stays tinted by
/// its sender and only a line the locale divided spends the inks.
fn ink_color(ink: crate::voice::Ink, own: u32) -> u32 {
	match ink {
		crate::voice::Ink::Plain => own,
		crate::voice::Ink::Command => INK_COMMAND,
		crate::voice::Ink::Arg => INK_ARG,
		crate::voice::Ink::Note => INK_NOTE,
	}
}

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
/// Backdrop behind loose HUD text (chat lines, the debug panel) — the
/// same fill the UI framework puts behind slots and buttons.
/// The feed's three structural inks, for a line the locale divided: the
/// command, the arguments it takes, and the note explaining it. Only a
/// catalog row divides today — everything else draws in its own colour.
const INK_COMMAND: u32 = 0xFF5AC8F0;
const INK_ARG: u32 = 0xFFE6C878;
const INK_NOTE: u32 = 0xFFAAAAAA;

const BG: u32 = crate::ui::theme::FILL_BG;
const REF_LINE: u32 = 0x80FFFFFF;

/// Sized for the debug panel's worst case plus a long menu server list,
/// with plenty of headroom over both.
const MAX_VERTS: usize = 6 * 8192;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OverlayVertex {
	pos: [f32; 2],
	uv: [f32; 2],
	color: u32,
}

pub(crate) struct Overlay {
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
	/// Reused across frames by the wrapped text surfaces (feed, chat).
	wrap_scratch: Vec<std::ops::Range<usize>>,
	atlas: font::GlyphAtlas,
	atlas_tex: wgpu::Texture,
	/// Cached from the atlas — every untextured quad samples it.
	solid_uv: [f32; 2],
	/// This frame's UI scale and derived glyph size, set by `prepare`.
	scale: f32,
	glyph: f32,
}

/// Compose the debug panel's lines. Split out from the drawing so the
/// column budget can be pinned headless — see
/// `debug_lines_fit_the_column_budget`.
fn debug_lines(
	stats: &OverlayStats,
	scratch: &mut String,
	lines: &mut [(u32, String); DEBUG_LINES],
) {
	let s = stats;
	let mut line = |i: usize, color: u32, args: std::fmt::Arguments<'_>| {
		scratch.clear();
		let _ = scratch.write_fmt(args);
		lines[i] = (color, scratch.clone());
	};
	// `drop` answers "did we miss a vsync slot": frames enough past the MEASURED display period
	// to have missed one, so it works at any refresh rate, where the fixed-threshold `hitch`
	// cannot see a single dropped frame on a fast panel. `ref` is the estimated period behind
	// drop. Without these a p95 a little over the display period is unfalsifiable by eye: vsync
	// presents only whole multiples of that period, so the tail is sampling noise.
	let refresh = match s.refresh_ms {
		r if r > 0.0 => format!("{r:5.2}"),
		_ => "  n/a".to_owned(),
	};
	line(
		0,
		WHITE,
		format_args!(
			"fps {:5.1} | avg {:5.2} p95 {:5.2} p99 {:5.2} worst {:6.2} | hitch {:>3} ({:>5}) drop {:>3} ({:>5}) ref {}",
			s.fps,
			s.avg_ms,
			s.p95_ms,
			s.p99_ms,
			s.worst_ms,
			s.hitches_window,
			s.hitches_total,
			s.dropped_window,
			s.dropped_total,
			refresh
		),
	);
	let c = &s.smooth;
	// `busy`/`out` are the frame-level partition the phases to their
	// left sum into; `out` is everything with the main thread stopped
	// (present, compositor, and on the web the GPU-process staging
	// copy). Read them together: `out` climbing while `busy` holds is
	// the signature of a cost no phase on this line can charge.
	line(
		1,
		DIM,
		format_args!(
			"cpu  acq {:5.2} upd {:5.2} mnt {:5.2} gi {:5.2} upl {:5.2} enc {:5.2} | busy {:5.2} out {:5.2} ms",
			c.acquire_ms,
			c.update_ms,
			c.maintain_ms,
			c.gi_ms,
			c.upload_ms,
			c.encode_ms,
			c.busy_ms,
			c.outside_ms
		),
	);
	match s.gpu {
		Some(g) => line(
			2,
			DIM,
			format_args!(
				// `ovl` is not here: it measures as nothing in every capture
				// this engine has taken, and the line is at its column budget.
				// It is still a CSV column.
				"gpu  pre {:5.2} main {:5.2} water {:5.2} up {:5.2} | gi {:5.2} trace {:5.2} den {:5.2} blit {:5.2} vm {:5.2}",
				g.prepass_ms,
				g.main_ms,
				g.water_ms,
				g.upscale_ms,
				g.gi_ms,
				g.gi_trace_ms,
				g.gi_denoise_ms,
				g.depth_blit_ms,
				g.viewmodel_ms
			),
		),
		None => line(2, DIM, format_args!("gpu  n/a")),
	}
	line(
		3,
		DIM,
		format_args!(
			"jobs gen q {:>4} n {:>4} avg {:4.2} max {:4.2} | mesh q {:>4} n {:>4} avg {:4.2} max {:4.2} | fly {:>3} | bud {:4.1}",
			s.gauges.gen_queue,
			s.gen_jobs.count,
			s.gen_jobs.avg_ms(),
			s.gen_jobs.max_ms,
			s.gauges.mesh_queue,
			s.mesh_jobs.count,
			s.mesh_jobs.avg_ms(),
			s.mesh_jobs.max_ms,
			s.gauges.in_flight,
			s.stream_budget_ms
		),
	);
	line(
		4,
		DIM,
		format_args!(
			"work up {:>4} ({:>6}K) now {:>5}K stale {:>4} req {:>4} evict {:>4} sat {:>4} | res {:>5} ({:>5} gpu) | wrk {:>2}",
			s.uploaded,
			s.upload_kb,
			// THIS frame's bytes, not the window's — the quantity the
			// web upload cap governs and the one that lines up with a
			// single spike in `out`.
			s.gauges.upload_bytes / 1024,
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
			"draw {:>5} drawn {:>5} culled | water {:>4} calls {:>5} tri {:6.2}M | sky {:>4.0} emit {:>4}",
			r.chunks_drawn,
			r.chunks_culled,
			r.water_draws,
			r.draw_calls,
			f64::from(r.triangles) / 1.0e6,
			// The GI window's sky ceiling and emitter count. Both track
			// RESIDENCY (eviction prunes them — `GiDraw::debug_state`), so
			// a climb is terrain in the window, not a session ratchet.
			r.sky_y,
			r.emitters
		),
	);
	// Memory. `vram` is the renderer's own ledger of what it holds on the GPU — every big
	// texture and buffer, not just the meshes — and `retired` everything it has ever destroyed.
	// At rest both must hold still: before retirement DESTROYED (`gpu_ledger`), `retired` is
	// what a session stranded in the browser's GPU process. `wasm`/`js` are the host's numbers
	// on the web (a wasm high-water mark and Chrome's JS heap; n/a natively), and `gi q` the
	// voxel-window backlog — a window still filling traces cheaply, so a fast frame with it
	// above zero is not yet the steady state.
	let m = &s.gpu_mem;
	let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
	let host = |b: u64| {
		if b == 0 {
			"n/a".to_owned()
		} else {
			format!("{:.1}M", mib(b))
		}
	};
	line(
		6,
		DIM,
		format_args!(
			"mem  vram {:6.1}M (mesh {:6.1} gi {:6.1} tgt {:6.1}) retired {:6.1}M | wasm {:>7} js {:>7} | gi q {:>5}",
			mib(m.live),
			mib(m.mesh),
			mib(m.gi),
			mib(m.targets),
			mib(m.retired),
			host(s.host_mem.wasm_bytes),
			host(s.host_mem.js_heap_bytes),
			s.gauges.gi_backlog.min(99999)
		),
	);
	// Connection health. TWO round trips, deliberately, because they answer
	// different questions and neither substitutes for the other.
	//
	// `ping` is the transport's own estimate — QUIC's, the same quantity the
	// server browser shows. `rtt` is the INPUT→ACK loop: the wire PLUS the
	// server's input buffer, the sim's tick boundary and the snapshot cadence,
	// which is what the player feels when they turn or dig. Their gap is the
	// server-side pipeline, the part that moves when SNAPSHOT_DIVISOR or the
	// send stride change. Subtracting a constant from `rtt` was measured wrong:
	// on a LAN every sample is one tick or two against a wire far shorter than a
	// tick.
	//
	// `+-` is the mean deviation of `rtt` — an rtt without it is not a number
	// you can act on. `rtt_last_ms` is deliberately NOT here; the raw value is
	// still in `NetStats` for the CSV. `lead` is how far ahead of the last acked
	// tick prediction is running, and `rep` is replays per second, which should
	// sit at 0.0 unless the server is rejecting claims.
	match &s.net {
		Some(n) => line(
			7,
			DIM,
			format_args!(
				"net  ping {} rtt {:5.1} +-{:4.1} ms | dn {:6.1} up {:6.1} kB/s | snap {:4.1}/s rep {:4.1}/s | lead {:>2} pend {:>3} rem {:>2}",
				MsOrDash(n.wire_rtt_ms),
				n.rtt_ms,
				n.jitter_ms,
				n.down_kbps,
				n.up_kbps,
				n.snapshots_per_sec,
				n.replays_per_sec,
				n.tick_lead.min(99),
				n.pending_edits.min(999),
				n.remotes.min(99),
			),
		),
		None => line(7, DIM, format_args!("net  offline")),
	}
	// Where this frame was taken. Not a measurement — it is on a performance panel because a
	// screenshot of a rendering bug is useless without the world it happened in, and every field
	// here is an argument the capture flags take (`--seed`, `--pos`, `--yaw`, `--pitch`, `--tod`),
	// so photographing the panel is enough to reproduce the frame. The facing is the same
	// eight-point compass the admin commands answer with — a heading is what a player reports.
	let w = &s.world;
	let facing = digblock_core::compass::compass_abbrev(look_dir(w.yaw_deg, w.pitch_deg));
	line(
		8,
		DIM,
		format_args!(
			"pos {:>9.1} {:>7.1} {:>9.1} facing {:>2} yaw {:>6.1} pitch {:>5.1} | tod {:>5.1} | seed {}",
			w.pos[0],
			w.pos[1],
			w.pos[2],
			facing,
			w.yaw_deg,
			w.pitch_deg,
			w.tod_deg,
			seed_text(w.seed)
		),
	);
}

/// The seed at full round-trip precision, in the shorter of the two forms Rust prints an f64 in.
/// It has to round-trip exactly — worldgen is a pure function of the seed, so a rounded seed names
/// a DIFFERENT world — and it has to be bounded, because the panel clips at [`DEBUG_COLS`] and
/// clipping is how a wrong seed would arrive looking right. `{}` never uses an exponent, so a
/// denormal seed runs to hundreds of characters; `{:e}` reads badly for the ordinary case.
fn seed_text(seed: f64) -> String {
	let plain = format!("{seed}");
	let exp = format!("{seed:e}");
	if plain.len() <= exp.len() { plain } else { exp }
}

/// The look vector for a yaw/pitch pair — `FlyCamera::dir`'s formula, kept
/// here rather than borrowed so this module (and its headless tests) stay
/// free of the camera.
fn look_dir(yaw_deg: f32, pitch_deg: f32) -> glam::Vec3 {
	let (ys, yc) = yaw_deg.to_radians().sin_cos();
	let (ps, pc) = pitch_deg.to_radians().sin_cos();
	glam::Vec3::new(ys * pc, ps, yc * pc)
}

impl Overlay {
	pub(crate) fn new(gpu: &Gpu, surface_format: wgpu::TextureFormat) -> Self {
		let device = &gpu.device;
		// Starts empty; glyphs are rasterized and uploaded as they are
		// first drawn (see `prepare`).
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
		let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
		// Linear, not Nearest. Glyphs are rasterized at the exact pixel size they're drawn
		// at and land on whole-pixel origins, so the common case is a 1:1 blit where the
		// two filters are identical. The case that isn't 1:1 is name tags, at arbitrary
		// subpixel positions: Nearest would drop and duplicate whole texel columns there,
		// popping stems between one and two pixels wide as the player walks. Linear blurs.
		let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("overlay font"),
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			mipmap_filter: wgpu::MipmapFilterMode::Nearest,
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
			bind_group_layouts: &[Some(&bgl)],
			immediate_size: 0,
		});
		let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("overlay"),
			layout: Some(&layout),
			vertex: wgpu::VertexState {
				module: &shader,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[Some(wgpu::VertexBufferLayout {
					array_stride: std::mem::size_of::<OverlayVertex>() as u64,
					step_mode: wgpu::VertexStepMode::Vertex,
					attributes: &wgpu::vertex_attr_array![
						0 => Float32x2, 1 => Float32x2, 2 => Uint32
					],
				})],
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
			multiview_mask: None,
			cache: None,
		});
		let vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("overlay verts"),
			size: (MAX_VERTS * std::mem::size_of::<OverlayVertex>()) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let atlas = font::GlyphAtlas::new();
		Self {
			pipeline,
			bind_group,
			uniform_buf,
			vertex_buf,
			verts: Vec::with_capacity(MAX_VERTS),
			vert_count: 0,
			splits: [0; 2],
			line_scratch: String::with_capacity(96),
			wrap_scratch: Vec::new(),
			solid_uv: atlas.solid_uv(),
			atlas,
			atlas_tex: tex,
			scale: 1.0,
			glyph: FIXED_GLYPH_PX,
		}
	}

	fn push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
		if self.verts.len() + 6 > MAX_VERTS {
			return;
		}
		let uv = self.solid_uv;
		let v = |px, py| OverlayVertex {
			pos: [px, py],
			uv,
			color,
		};
		let (x1, y1) = (x + w, y + h);
		self.verts
			.extend_from_slice(&[v(x, y), v(x1, y), v(x, y1), v(x1, y), v(x1, y1), v(x, y1)]);
	}

	/// Draw `text` in a `px`-tall layout box whose TOP edge is at `y` (`self.glyph` for scaled
	/// surfaces, [`FIXED_GLYPH_PX`] for pinned ones). Returns the advance, so callers that need
	/// the width don't measure the same string twice. The origin and every glyph's pen are
	/// rounded to whole pixels, since the glyph was rasterized at exactly `px`; each pen rounds
	/// independently rather than accumulating rounded advances, so a long line cannot drift.
	fn push_text(
		&mut self,
		id: font::FaceId,
		x: f32,
		y: f32,
		px: f32,
		color: u32,
		text: &str,
	) -> f32 {
		let face = font::face(id);
		let ox = x.round();
		let oy = (y + face.baseline(px)).round();
		let mut run = face.run(text, px);
		for (c, pen) in run.by_ref() {
			let Some(g) = self.atlas.glyph(id, face, c, px) else {
				continue; // blank (space), or the page is full
			};
			if self.verts.len() + 6 > MAX_VERTS {
				break;
			}
			let v = |px, py, u, w| OverlayVertex {
				pos: [px, py],
				uv: [u, w],
				color,
			};
			let (x0, y0) = (ox + pen.round() + g.left, oy + g.top);
			let (x1, y1) = (x0 + g.w, y0 + g.h);
			self.verts.extend_from_slice(&[
				v(x0, y0, g.uv0[0], g.uv0[1]),
				v(x1, y0, g.uv1[0], g.uv0[1]),
				v(x0, y1, g.uv0[0], g.uv1[1]),
				v(x1, y0, g.uv1[0], g.uv0[1]),
				v(x1, y1, g.uv1[0], g.uv1[1]),
				v(x0, y1, g.uv0[0], g.uv1[1]),
			]);
		}
		run.pen()
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
					face,
					x,
					y,
					px,
					color,
					range,
				} => {
					let text = &arena[range.0 as usize..range.1 as usize];
					self.push_text(face, x, y, px, color, text);
				}
				crate::ui::draw::Cmd::Icon { .. } | crate::ui::draw::Cmd::IconQuad { .. } => {}
			}
		}
	}

	/// Rebuild the vertex buffer for this frame: the draw list's Backdrop/Content/Ghost layers
	/// (crosshair, HUD, screens, menu — all engine-emitted), plus the legacy direct-push surfaces
	/// (debug panel, feed, chat, nametags), which draw UNDER the Content layer so screens can dim
	/// them. The two derived splits bracket the icon pipeline's main and ghost stages.
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn prepare(
		&mut self,
		gpu: &Gpu,
		size: (u32, u32),
		ui_scale: f32,
		ui: &crate::ui::DrawList,
		stats: Option<(&OverlayStats, &[f32])>,
		feed: &[(crate::voice::Msg, f32, u32)],
		chat: Option<&crate::chat::ChatView<'_>>,
		nametags: &[crate::renderer::NameTag],
		binds: &crate::bindings::Bindings,
	) {
		self.scale = ui_scale;
		self.glyph = glyph_px(ui_scale);
		// Declare the sizes this frame will ask for BEFORE anything emits:
		// the atlas wipes and repacks when the set changes, and doing that
		// mid-frame would invalidate UVs already in the vertex buffer.
		self.atlas.begin_frame(&[
			(font::FaceId::Ui, font::ui().size_key(self.glyph)),
			(font::FaceId::Ui, font::ui().size_key(FIXED_GLYPH_PX)),
			(font::FaceId::Mono, font::mono().size_key(FIXED_GLYPH_PX)),
		]);
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
			self.push_feed(size, feed, feed_top, chat.is_some(), binds);
		}
		self.run_cmds(ui, crate::ui::Layer::Content);
		self.splits[1] = self.verts.len() as u32;
		self.run_cmds(ui, crate::ui::Layer::Ghost);
		self.vert_count = self.verts.len() as u32;
		// After every emitter has interned its glyphs, before the pass
		// runs. write_texture is queue-ordered against the submit, so the
		// rows are in place by the time anything samples them.
		if let Some((y0, y1, rows)) = self.atlas.take_dirty() {
			gpu.queue.write_texture(
				wgpu::TexelCopyTextureInfo {
					texture: &self.atlas_tex,
					mip_level: 0,
					origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
					aspect: wgpu::TextureAspect::All,
				},
				rows,
				wgpu::TexelCopyBufferLayout {
					offset: 0,
					bytes_per_row: Some(font::ATLAS_W),
					rows_per_image: None,
				},
				wgpu::Extent3d {
					width: font::ATLAS_W,
					height: y1 - y0,
					depth_or_array_layers: 1,
				},
			);
		}
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
		let mut lines: [(u32, String); DEBUG_LINES] = Default::default();
		let mut scratch = std::mem::take(&mut self.line_scratch);
		debug_lines(stats, &mut scratch, &mut lines);
		self.line_scratch = scratch;

		// Width comes from the fixed column budget, NOT from this frame's longest line. The
		// panel is right-anchored, so sizing it to live content resizes and slides it on
		// every frame that changes a digit count. Every field above is padded to a fixed
		// width; DEBUG_COLS is the belt to that's braces, and
		// `debug_lines_fit_the_column_budget` pins both.
		let text_w = DEBUG_COLS as f32 * font::mono().advance(' ', FIXED_GLYPH_PX);
		let graph_w = GRAPH_FRAMES as f32 * BAR_W;
		let panel_w = text_w.max(graph_w) + PAD * 2.0;
		let line_px = debug_line_px();
		let panel_h = lines.len() as f32 * line_px + GRAPH_H + PAD * 3.0;
		// Right-anchored: the top-left corner belongs to the chat column.
		let panel_x = (size.0 as f32 - MARGIN - panel_w).max(MARGIN);
		self.push_quad(panel_x, MARGIN, panel_w, panel_h, BG);

		let (tx, mut ty) = (panel_x + PAD, MARGIN + PAD);
		for entry in &mut lines {
			let (color, text) = std::mem::take(entry);
			self.push_text(
				font::FaceId::Mono,
				tx,
				ty,
				FIXED_GLYPH_PX,
				color,
				truncate_cols(&text, DEBUG_COLS),
			);
			ty += line_px;
		}

		// Frame-time graph: newest bar on the right, green inside the
		// frame budget, yellow inside twice it, red above; faint
		// reference line at the budget itself.
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

	/// Width available to a wrapped chat-column line: the column starts at
	/// the left margin and wraps when it reaches the midpoint of the
	/// screen. Floored so a very narrow window still gets a usable column
	/// instead of one character per row.
	fn wrap_w(&self, size: (u32, u32)) -> f32 {
		let usable = size.0 as f32 / 2.0 - (MARGIN + PAD * 2.0) * self.scale;
		usable.max(self.glyph * 4.0)
	}

	/// Server-message feed (system notifications + chat): top-left under the input line, NEWEST at
	/// the top so the latest message sits next to where the player types; long lines wrap at the
	/// screen midpoint; each entry fades out over its final second in its own color (chat lines
	/// are tinted per sender). `top` is where the block starts. With the box open, faded-out
	/// messages come back dimly — see [`visible_feed`].
	fn push_feed(
		&mut self,
		size: (u32, u32),
		feed: &[(crate::voice::Msg, f32, u32)],
		top: f32,
		chat_open: bool,
		binds: &crate::bindings::Bindings,
	) {
		let wrap_w = self.wrap_w(size);
		let mut y = top;
		for (msg, a, color) in visible_feed(feed, chat_open) {
			// Rendered HERE, not where it was pushed: a message keeps its
			// arguments until the frame that draws it, which is what lets
			// a language change re-word lines already on screen.
			y = self.push_column_line(&crate::voice::render(msg, binds), wrap_w, y, color, a);
		}
	}

	/// One spoken line into the chat column: wrapped, each row backed by
	/// its own panel and drawn one run per ink, the pen carried across
	/// them by the advance `push_text` returns — never a width guessed
	/// from the piece before it. Returns the y below the last row.
	fn push_column_line(
		&mut self,
		spoken: &crate::voice::Line,
		wrap_w: f32,
		top: f32,
		own: u32,
		a: f32,
	) -> f32 {
		let s = self.scale;
		let (line, margin, pad, glyph) = (LINE_PX * s, MARGIN * s, PAD * s, self.glyph);
		let mut scratch = std::mem::take(&mut self.wrap_scratch);
		font::wrap(font::ui(), &spoken.text, glyph, wrap_w, &mut scratch);
		let mut y = top;
		for r in &scratch {
			let w = font::ui().measure(&spoken.text[r.clone()], glyph) + pad * 2.0;
			self.push_quad(margin, y, w, line, fade(BG, a));
			let ty = y + (line - glyph) / 2.0;
			let mut x = margin + pad;
			for (piece, ink) in spoken.row(r.clone()) {
				let c = fade(ink_color(ink, own), a);
				x += self.push_text(font::FaceId::Ui, x, ty, glyph, c, piece);
			}
			y += line;
		}
		self.wrap_scratch = scratch;
		y
	}

	/// Chat input: pinned to the top-left corner, wrapping at the screen
	/// midpoint, with a blinking block caret riding the end of the text
	/// (same blink clock as the menu's name field). Returns the y where
	/// the feed continues below.
	fn push_chat_input(&mut self, size: (u32, u32), view: &crate::chat::ChatView<'_>) -> f32 {
		let s = self.scale;
		let (line, margin, pad, glyph) = (LINE_PX * s, MARGIN * s, PAD * s, self.glyph);
		let wrap_w = self.wrap_w(size);
		let caret_w = glyph * CARET_W;
		// "> " prefix + text, wrapped as a single run. The caret rides the
		// end and can spill onto a row of its own when the text fills the
		// last row exactly — wrap against a width that reserves room for
		// it so it never hangs outside the backdrop.
		let content = format!("> {}", view.line);
		let mut scratch = std::mem::take(&mut self.wrap_scratch);
		font::wrap(
			font::ui(),
			&content,
			glyph,
			(wrap_w - caret_w).max(glyph),
			&mut scratch,
		);
		let mut y = margin;
		for (i, r) in scratch.iter().enumerate() {
			let chunk = &content[r.clone()];
			let last = i == scratch.len() - 1;
			let text_w = font::ui().measure(chunk, glyph);
			let min_w = if i == 0 { 240.0 * s } else { 0.0 };
			let w = (text_w + if last { caret_w } else { 0.0 } + pad * 2.0).max(min_w);
			self.push_quad(margin, y, w, line, 0xC0000000);
			let ty = y + (line - glyph) / 2.0;
			self.push_text(font::FaceId::Ui, margin + pad, ty, glyph, WHITE, chunk);
			if last && view.caret {
				self.push_quad(margin + pad + text_w, ty, caret_w, glyph, WHITE);
			}
			y += line;
		}
		self.wrap_scratch = scratch;
		// Commands still matching the stem, listed under the box as it is
		// typed: the same rows `/help` speaks and the same three inks, so
		// what Tab would cycle is on screen before anyone presses it.
		for (usage, desc) in view.matches.iter().take(CHAT_SUGGESTIONS) {
			y = self.push_column_line(
				&crate::voice::command_hint(usage, desc),
				wrap_w,
				y,
				WHITE,
				1.0,
			);
		}
		y
	}

	/// Name tags: centered text with a dim backdrop at pre-projected
	/// screen positions. No depth — tags read through walls, which keeps
	/// them useful as a "someone is over there" cue.
	fn push_nametags(&mut self, size: (u32, u32), tags: &[crate::renderer::NameTag]) {
		use digblock_core::skills::Skill;
		let (pad, glyph) = (PAD * self.scale, self.glyph);
		// Sized to the LINE box, not the nominal one: accents reach above
		// the box and descenders below it, so a backdrop the height of the
		// box would clip the `g` in someone's name.
		let back_h = font::ui().line_h(glyph).max(glyph);
		let back_dy = (back_h - glyph) / 2.0;
		// The mastery glyphs: one square per mastered skill after the name,
		// in the skill's emblem colour — the same colour its essence chip
		// is painted in, so the two read as one language. A square rather
		// than a character, since the UI face promises no symbol glyphs.
		let mark = (glyph * 0.5).round().max(2.0);
		let mark_gap = (glyph * 0.2).round().max(1.0);
		for tag in tags {
			let marks = Skill::from_mastery(tag.mastery).count() as f32;
			let text_w = font::ui().measure(&tag.text, glyph);
			let w = text_w + marks * (mark + mark_gap);
			let x = (tag.x - w / 2.0).clamp(0.0, (size.0 as f32 - w).max(0.0));
			let y = tag.y.clamp(0.0, size.1 as f32 - glyph);
			self.push_quad(x - pad / 2.0, y - back_dy, w + pad, back_h, 0x60000000);
			self.push_text(font::FaceId::Ui, x, y, glyph, tag.color, &tag.text);
			let mut mx = x + text_w + mark_gap;
			for skill in Skill::from_mastery(tag.mastery) {
				let (core, _) = skill.emblem().palette();
				let color = 0xFF00_0000
					| (u32::from(core[2]) << 16)
					| (u32::from(core[1]) << 8)
					| u32::from(core[0]);
				self.push_quad(mx, y + (glyph - mark) / 2.0, mark, mark, color);
				mx += mark + mark_gap;
			}
		}
	}

	/// The range under the main icons (crosshair, slot/panel backdrops).
	pub(crate) fn draw_below(&self, pass: &mut wgpu::RenderPass<'_>) {
		self.draw_range(pass, 0, self.splits[0]);
	}

	/// The content band over the main icons (counts, labels, panel,
	/// feed, menu) but under the ghost icon.
	pub(crate) fn draw_mid(&self, pass: &mut wgpu::RenderPass<'_>) {
		self.draw_range(pass, self.splits[0], self.splits[1]);
	}

	/// The topmost range (ghost-layer quads/text, over the ghost icon).
	pub(crate) fn draw_above(&self, pass: &mut wgpu::RenderPass<'_>) {
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

#[cfg(test)]
mod tests {
	use super::*;

	fn feed(ttls: &[f32]) -> Vec<(String, f32, u32)> {
		ttls.iter()
			.enumerate()
			.map(|(i, t)| (format!("m{i}"), *t, 0xFFFF_FFFF))
			.collect()
	}

	fn texts(feed: &[(String, f32, u32)], open: bool) -> Vec<&str> {
		visible_feed(feed, open)
			.map(|(t, _, _)| t.as_str())
			.collect()
	}

	/// With the box shut the feed looks exactly as it always did: live
	/// messages only, newest first. Expired entries linger in the vec now
	/// (they are the scrollback) and must stay invisible.
	#[test]
	fn a_closed_chat_box_shows_only_live_messages() {
		let f = feed(&[-1.0, -1.0, 3.0, 8.0]);
		assert_eq!(texts(&f, false), vec!["m3", "m2"]);
	}

	/// Typing brings back what just scrolled off — the point of keeping
	/// expired entries at all — but only a bounded number of them.
	#[test]
	fn an_open_chat_box_recalls_bounded_history() {
		let f = feed(&[-1.0, -1.0, 4.0]);
		assert_eq!(texts(&f, true), vec!["m2", "m1", "m0"]);

		let mut ttls = vec![-1.0; CHAT_HISTORY_LINES + 4];
		*ttls.last_mut().unwrap() = 5.0;
		let f = feed(&ttls);
		assert_eq!(texts(&f, true).len(), CHAT_HISTORY_LINES + 1);
	}

	/// A message's final second fades it out; recalled history is drawn
	/// at a flat, clearly-past alpha instead.
	#[test]
	fn alpha_fades_live_messages_and_flattens_history() {
		let f = feed(&[-1.0, 0.25, 9.0]);
		let alphas: Vec<f32> = visible_feed(&f, true).map(|(_, a, _)| a).collect();
		assert_eq!(alphas, vec![1.0, 0.25, HISTORY_ALPHA]);
	}

	// The panel is right-anchored and sized from DEBUG_COLS, so a line
	// that outgrows the budget would be clipped rather than push the
	// panel around. Saturate every field and confirm none of them do —
	// this is what keeps the padding above honest.
	#[test]
	fn debug_lines_fit_the_column_budget() {
		let mut stats = OverlayStats {
			fps: 9999.9,
			avg_ms: 999.99,
			p95_ms: 999.99,
			p99_ms: 999.99,
			worst_ms: 9999.99,
			hitches_window: 999,
			hitches_total: 99999,
			dropped_window: 999,
			dropped_total: 99999,
			refresh_ms: 999.99,
			uploaded: 9999,
			upload_kb: 999_999,
			stale_dropped: 9999,
			requeued: 9999,
			evicted: 9999,
			upload_saturated: 9999,
			workers: 99,
			gpu_mem: digblock_core::metrics::GpuMemory {
				live: 9999 * 1024 * 1024,
				mesh: 9999 * 1024 * 1024,
				gi: 9999 * 1024 * 1024,
				targets: 9999 * 1024 * 1024,
				retired: 9999 * 1024 * 1024,
			},
			host_mem: digblock_core::metrics::HostMemory {
				wasm_bytes: 9999 * 1024 * 1024,
				js_heap_bytes: 9999 * 1024 * 1024,
			},
			stream_budget_ms: 999.9,
			..Default::default()
		};
		stats.gpu = Some(digblock_core::metrics::GpuPassMs {
			prepass_ms: 999.99,
			main_ms: 999.99,
			water_ms: 999.99,
			overlay_ms: 999.99,
			gi_ms: 999.99,
			gi_trace_ms: 999.99,
			gi_denoise_ms: 999.99,
			depth_blit_ms: 999.99,
			viewmodel_ms: 999.99,
			upscale_ms: 999.99,
		});
		stats.gauges.gen_queue = 9999;
		stats.gauges.mesh_queue = 9999;
		stats.gauges.in_flight = 999;
		stats.gauges.resident = 99999;
		stats.gauges.upload_bytes = 99999 * 1024;
		stats.gauges.gi_backlog = u32::MAX;
		stats.gen_jobs.count = 9999;
		stats.mesh_jobs.count = 9999;
		stats.render.chunks_drawn = 99999;
		stats.render.chunks_culled = 99999;
		stats.render.water_draws = 9999;
		stats.render.draw_calls = 99999;
		stats.render.chunks_resident = 99999;
		// The GI window's full-height ceiling and a saturated emitter map.
		stats.render.sky_y = 255.0;
		stats.render.emitters = 9999;
		// Connected, saturated. Without this the net line under test would
		// be the short "offline" string and the budget would go unchecked
		// on the one line that has the most fields.
		stats.net = Some(digblock_core::metrics::NetStats {
			rtt_ms: 999.9,
			wire_rtt_ms: 999.9,
			jitter_ms: 999.9,
			rtt_last_ms: 999.9,
			down_kbps: 9999.9,
			up_kbps: 9999.9,
			snapshots_per_sec: 99.9,
			replays_per_sec: 99.9,
			pending_edits: u32::MAX,
			remotes: u32::MAX,
			tick_lead: u32::MAX,
		});
		// The world line's own worst case. `MIN_POSITIVE` is the value that
		// costs `seed_text` the most: the plain form runs to hundreds of
		// characters of leading zeros, so this is the case the exponent
		// form exists for, and its width is what the budget below is
		// really checking.
		stats.world = digblock_core::metrics::WorldStats {
			pos: [-999_999.9, -999_999.9, -999_999.9],
			yaw_deg: -359.9,
			pitch_deg: -89.9,
			tod_deg: 359.9,
			seed: f64::MIN_POSITIVE,
		};
		assert!(
			seed_text(f64::MIN_POSITIVE).len() <= 24,
			"the longest seed an f64 can need"
		);
		assert_eq!(
			seed_text(f64::MIN_POSITIVE).parse::<f64>(),
			Ok(f64::MIN_POSITIVE),
			"and it round-trips — a rounded seed is a different world"
		);
		assert_eq!(seed_text(42.0), "42", "an ordinary seed reads plainly");

		let mut scratch = String::new();
		let mut lines: [(u32, String); DEBUG_LINES] = Default::default();
		debug_lines(&stats, &mut scratch, &mut lines);
		for (i, (_, l)) in lines.iter().enumerate() {
			let n = l.chars().count();
			assert!(
				n <= DEBUG_COLS,
				"line {i} needs {n} cols (budget {DEBUG_COLS}):\n{l}"
			);
			assert!(l.is_ascii(), "line {i} must be mono-face ASCII");
		}
		// And the budget isn't wildly generous — a budget far past the
		// longest line just wastes screen.
		let widest = lines.iter().map(|(_, l)| l.chars().count()).max().unwrap();
		assert!(
			DEBUG_COLS - widest <= 8,
			"DEBUG_COLS {DEBUG_COLS} is {} past the widest line",
			DEBUG_COLS - widest
		);
	}

	// The whole point of the fixed budget: streaming numbers must not
	// move the panel. Two very different stat snapshots have to produce
	// identical line widths.
	#[test]
	fn panel_width_is_independent_of_the_numbers() {
		let mut scratch = String::new();
		let widths = |s: &OverlayStats, scratch: &mut String| {
			let mut lines: [(u32, String); DEBUG_LINES] = Default::default();
			debug_lines(s, scratch, &mut lines);
			lines.map(|(_, l)| l.chars().count())
		};
		let quiet = OverlayStats {
			gpu: Some(Default::default()),
			net: Some(Default::default()),
			..Default::default()
		};
		let mut busy = quiet.clone();
		busy.net = Some(digblock_core::metrics::NetStats {
			rtt_ms: 812.5,
			wire_rtt_ms: 780.0,
			jitter_ms: 91.25,
			rtt_last_ms: 777.5,
			down_kbps: 1234.5,
			up_kbps: 987.5,
			snapshots_per_sec: 20.0,
			replays_per_sec: 12.5,
			pending_edits: 999,
			remotes: 42,
			tick_lead: 37,
		});
		busy.uploaded = 4321;
		busy.upload_kb = 98765;
		busy.gauges.gen_queue = 812;
		busy.gauges.mesh_queue = 1904;
		busy.gauges.resident = 40321;
		busy.render.chunks_drawn = 8123;
		busy.render.draw_calls = 12345;
		busy.hitches_total = 61234;
		// The world line streams too — a walking player moves every field
		// on it EXCEPT the seed, which is fixed for the session and is the
		// one field left unpadded (see `debug_lines`). So both snapshots
		// carry the same seed and every other field differs.
		busy.world = digblock_core::metrics::WorldStats {
			pos: [-1234.5, 71.25, 8888.75],
			yaw_deg: -179.5,
			pitch_deg: 42.5,
			tod_deg: 271.25,
			seed: quiet.world.seed,
		};
		assert_eq!(widths(&quiet, &mut scratch), widths(&busy, &mut scratch));
	}
}
