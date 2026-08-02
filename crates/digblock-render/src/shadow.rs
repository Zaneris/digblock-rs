//! The two-cascade shadow system: near map (whole caster set, every
//! frame) plus the persistent wide cascade, re-rendered a slice per
//! frame into a ping-ponged back map so a refresh can never blow the
//! vsync budget. Owns the shadow pipeline, both depth maps, the
//! comparison sampler, the light-VP uniforms, and the sampling bind
//! groups (the main passes' group 2).

use digblock_core::chunk::ChunkPos;
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_SHADOW_NEAR, Q_SHADOW_WIDE};
use crate::renderer::{side_planes, ChunkGpu, CHUNK_STRIDE};

/// Wide-cascade refresh in progress: the resident set snapshotted at start,
/// re-rendered into the back map `wide_slice_draws` chunks per frame.
/// Chunks uploaded mid-job appear at the next refresh (same as before the
/// split); evicted ones are skipped at encode time.
struct WideJob {
	vp: glam::Mat4,
	list: Vec<ChunkPos>,
	next: usize,
}

/// The pure ping-pong bookkeeping — no wgpu types, so the invariant
/// below is unit-testable. The contract: the sampled (map index, VP)
/// pair changes only together, on the frame AFTER a refresh job
/// completes — never mid-job — or far shadows mis-project for one
/// frame (a visible dark flash at every refresh).
struct WidePingPong {
	/// Index of the wide map currently sampled; 1-front is the render
	/// target of the in-progress refresh.
	front: usize,
	/// The view-proj the front wide map was rendered with — what the main
	/// pass must project with until the next flip, regardless of how far
	/// the sun has moved since.
	active_vp: glam::Mat4,
	/// False until the first frame bootstraps a clear of the front map
	/// (it is uninitialized at construction / device rebuild).
	front_valid: bool,
	/// In-progress wide refresh, encoded a slice per frame.
	job: Option<WideJob>,
	/// Set at construction: the persistent wide cascade has no contents
	/// yet (also the device-loss recovery path).
	refresh_needed: bool,
}

impl WidePingPong {
	fn new() -> Self {
		Self {
			front: 0,
			active_vp: glam::Mat4::IDENTITY,
			front_valid: false,
			job: None,
			refresh_needed: true,
		}
	}

	fn mark_refresh_needed(&mut self) {
		self.refresh_needed = true;
	}

	fn refresh_pending(&self, wide_due: bool) -> bool {
		wide_due || self.refresh_needed
	}

	/// Kick off (or restart) a refresh when due; a restart re-targets
	/// the same back map from slice 0. `resident` is only called on a
	/// kickoff. Returns whether one happened.
	fn begin_frame(
		&mut self,
		wide_vp: glam::Mat4,
		wide_due: bool,
		resident: impl FnOnce() -> Vec<ChunkPos>,
	) -> bool {
		let due = self.refresh_pending(wide_due);
		self.refresh_needed = false;
		if due {
			self.job = Some(WideJob {
				vp: wide_vp,
				list: resident(),
				next: 0,
			});
		}
		due
	}

	/// The (map index, VP) pair the main pass samples this frame.
	/// Capture BEFORE `take_job`/`finish_slice` can flip `front` — a
	/// just-completed map is first sampled the following frame, paired
	/// with its VP.
	fn sample(&self) -> (usize, glam::Mat4) {
		(self.front, self.active_vp)
	}

	fn back(&self) -> usize {
		1 - self.front
	}

	/// True exactly once after construction: the front map needs its
	/// bootstrap clear.
	fn take_bootstrap_clear(&mut self) -> bool {
		!std::mem::replace(&mut self.front_valid, true)
	}

	fn take_job(&mut self) -> Option<WideJob> {
		self.job.take()
	}

	/// Return a job after encoding a slice; a completed job flips the
	/// ping-pong and adopts its VP (sampled from the next frame on).
	fn finish_slice(&mut self, job: WideJob, done: bool) {
		if done {
			self.front = 1 - self.front;
			self.active_vp = job.vp;
		} else {
			self.job = Some(job);
		}
	}
}

/// The sampled pair for one frame, captured by `begin_frame` before any
/// flip. `active_vp` is what the frame UBO's light_wide must carry.
pub(crate) struct WideSample {
	pub index: usize,
	pub active_vp: glam::Mat4,
}

#[derive(Default)]
pub(crate) struct ShadowPassStats {
	pub casters: u32,
	pub draw_calls: u32,
	pub triangles: u32,
}

pub(crate) struct ShadowCascades {
	pipeline: wgpu::RenderPipeline,
	/// Sampling layout (near, wide, comparison sampler) — the main
	/// passes' group 2.
	bgl: wgpu::BindGroupLayout,
	/// One per wide map (binding 1 differs); `WideSample::index` picks
	/// the one whose wide map is complete and safe to sample.
	bind_groups: [wgpu::BindGroup; 2],
	near_view: wgpu::TextureView,
	/// Two wide maps, ping-ponged: one is sampled while the other is
	/// re-rendered a slice per frame (the refresh would otherwise encode
	/// every resident chunk in one frame and blow the vsync budget).
	wide_views: [wgpu::TextureView; 2],
	light_near_buf: wgpu::Buffer,
	light_near_bg: wgpu::BindGroup,
	light_wide_buf: wgpu::Buffer,
	light_wide_bg: wgpu::BindGroup,
	pingpong: WidePingPong,
	/// This frame's near-cascade VP (set by `begin_frame`, culled
	/// against in `encode`).
	near_vp: glam::Mat4,
	/// Per-frame wide-refresh encode cap (RenderConfig).
	wide_slice_draws: u32,
}

impl ShadowCascades {
	pub fn new(
		gpu: &Gpu,
		shadow_size: u32,
		wide_slice_draws: u32,
		chunk_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;

		// Shadow maps: real render targets, cleared to depth 1.0 (fully
		// lit) until rendered.
		let make_shadow = |label: &str| {
			device
				.create_texture(&wgpu::TextureDescriptor {
					label: Some(label),
					size: wgpu::Extent3d {
						width: shadow_size,
						height: shadow_size,
						depth_or_array_layers: 1,
					},
					mip_level_count: 1,
					sample_count: 1,
					dimension: wgpu::TextureDimension::D2,
					format: wgpu::TextureFormat::Depth32Float,
					usage: wgpu::TextureUsages::RENDER_ATTACHMENT
						| wgpu::TextureUsages::TEXTURE_BINDING,
					view_formats: &[],
				})
				.create_view(&wgpu::TextureViewDescriptor::default())
		};
		let near_view = make_shadow("shadow near");
		let wide_views = [make_shadow("shadow wide A"), make_shadow("shadow wide B")];

		let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("shadow comparison"),
			address_mode_u: wgpu::AddressMode::ClampToEdge,
			address_mode_v: wgpu::AddressMode::ClampToEdge,
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			compare: Some(wgpu::CompareFunction::LessEqual),
			..Default::default()
		});

		// Group 2 of the solid/water pipelines: near map, wide map,
		// comparison sampler. Two bind groups exist (one per wide map);
		// the ping-pong picks one.
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("shadows"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Depth,
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						sample_type: wgpu::TextureSampleType::Depth,
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled: false,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
					count: None,
				},
			],
		});
		let bind_groups = [0, 1].map(|i| {
			device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("shadows"),
				layout: &bgl,
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: wgpu::BindingResource::TextureView(&near_view),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&wide_views[i]),
					},
					wgpu::BindGroupEntry {
						binding: 2,
						resource: wgpu::BindingResource::Sampler(&sampler),
					},
				],
			})
		});

		// Depth-only shadow pass: group0 = light VP, group1 = chunk origins.
		let light_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("light"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::VERTEX,
				ty: wgpu::BindingType::Buffer {
					ty: wgpu::BufferBindingType::Uniform,
					has_dynamic_offset: false,
					min_binding_size: None,
				},
				count: None,
			}],
		});
		let shadow_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("shadow"),
			bind_group_layouts: &[&light_bgl, chunk_bgl],
			push_constant_ranges: &[],
		});
		let shadow_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("shadow"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SHADOW.into()),
		});
		// Depth-only (color_format None, no fragment stage).
		let pipeline = crate::pipeline_builder::build(
			device,
			None,
			&crate::pipeline_builder::PipelineDesc {
				label: "shadow",
				layout: &shadow_layout,
				module: &shadow_shader,
				vs_entry: "vs",
				fs_entry: None,
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: 8,
					step_mode: wgpu::VertexStepMode::Vertex,
					attributes: &[wgpu::VertexAttribute {
						format: wgpu::VertexFormat::Uint32x2,
						offset: 0,
						shader_location: 0,
					}],
				}],
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				// Replaces the Java heightmap biases (0.0005/0.0008) and
				// the 3° tilt hack. Tunable during M6 A/B.
				depth_bias: wgpu::DepthBiasState {
					constant: 2,
					slope_scale: 2.0,
					clamp: 0.0,
				},
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		let make_light = |label: &str| {
			let buf = device.create_buffer(&wgpu::BufferDescriptor {
				label: Some(label),
				size: 64,
				usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
				mapped_at_creation: false,
			});
			let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some(label),
				layout: &light_bgl,
				entries: &[wgpu::BindGroupEntry {
					binding: 0,
					resource: buf.as_entire_binding(),
				}],
			});
			(buf, bg)
		};
		let (light_near_buf, light_near_bg) = make_light("light near");
		let (light_wide_buf, light_wide_bg) = make_light("light wide");

		Self {
			pipeline,
			bgl,
			bind_groups,
			near_view,
			wide_views,
			light_near_buf,
			light_near_bg,
			light_wide_buf,
			light_wide_bg,
			pingpong: WidePingPong::new(),
			near_vp: glam::Mat4::IDENTITY,
			wide_slice_draws,
		}
	}

	/// The sampling layout — group 2 of the solid/water pipeline layouts.
	pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
		&self.bgl
	}

	/// The sampling bind group for this frame's captured `WideSample`.
	pub fn sample_bind_group(&self, sample: &WideSample) -> &wgpu::BindGroup {
		&self.bind_groups[sample.index]
	}

	/// Force a wide refresh next frame even if the sun hasn't moved
	/// (device rebuild: the maps are new and empty).
	#[allow(dead_code)]
	pub fn mark_refresh_needed(&mut self) {
		self.pingpong.mark_refresh_needed();
	}

	/// Whether this frame will run wide-refresh work (for stats; call
	/// before `begin_frame`, which consumes the pending flag).
	pub fn refresh_pending(&self, wide_due: bool) -> bool {
		self.pingpong.refresh_pending(wide_due)
	}

	/// Per-frame kickoff: uploads the light VPs and captures the sample
	/// pair BEFORE any flip the encode below may perform. When a
	/// refresh is due, light_wide_buf takes the new VP immediately — it
	/// feeds only the shadow-pass encode — while the main pass keeps
	/// projecting with the returned `active_vp` until the flip.
	pub fn begin_frame(
		&mut self,
		queue: &wgpu::Queue,
		near_vp: glam::Mat4,
		wide_vp: glam::Mat4,
		wide_due: bool,
		chunks: &FxHashMap<ChunkPos, ChunkGpu>,
	) -> WideSample {
		self.near_vp = near_vp;
		queue.write_buffer(
			&self.light_near_buf,
			0,
			bytemuck::bytes_of(&near_vp.to_cols_array_2d()),
		);
		if self
			.pingpong
			.begin_frame(wide_vp, wide_due, || chunks.keys().copied().collect())
		{
			queue.write_buffer(
				&self.light_wide_buf,
				0,
				bytemuck::bytes_of(&wide_vp.to_cols_array_2d()),
			);
		}
		let (index, active_vp) = self.pingpong.sample();
		WideSample { index, active_vp }
	}

	/// Encode this frame's shadow work, fixed order: bootstrap clear
	/// (first frame only) → wide slice (when a job is running; flips on
	/// completion) → near pass (whole caster set, every frame).
	pub fn encode(
		&mut self,
		encoder: &mut wgpu::CommandEncoder,
		chunks: &FxHashMap<ChunkPos, ChunkGpu>,
		chunk_bg: &wgpu::BindGroup,
		index_buf: &wgpu::Buffer,
		timer: Option<&GpuTimer>,
	) -> ShadowPassStats {
		let mut stats = ShadowPassStats::default();
		if self.pingpong.take_bootstrap_clear() {
			// Startup / device rebuild: the front map is uninitialized;
			// clear it to depth 1.0 (fully lit) so sampling is sane until
			// the first refresh completes and flips.
			encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label: Some("shadow wide bootstrap clear"),
				color_attachments: &[],
				depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
					view: &self.wide_views[self.pingpong.sample().0],
					depth_ops: Some(wgpu::Operations {
						load: wgpu::LoadOp::Clear(1.0),
						store: wgpu::StoreOp::Store,
					}),
					stencil_ops: None,
				}),
				timestamp_writes: None,
				occlusion_query_set: None,
			});
		}
		if let Some(mut job) = self.pingpong.take_job() {
			let (draws, tris, done) =
				self.encode_wide_slice(encoder, &mut job, chunks, chunk_bg, index_buf, timer);
			stats.draw_calls += draws;
			stats.triangles += tris;
			self.pingpong.finish_slice(job, done);
		}
		let (casters, tris) = self.encode_near(encoder, chunks, chunk_bg, index_buf, timer);
		stats.casters = casters;
		stats.draw_calls += casters;
		stats.triangles += tris;
		stats
	}

	/// Near-cascade depth render (whole caster set, every frame).
	/// Solid meshes only — water never casts shadows (Java depth passes
	/// drew solidMesh exclusively). Casters are culled against the
	/// light's own frustum. Returns (draw calls, triangles) encoded.
	fn encode_near(
		&self,
		encoder: &mut wgpu::CommandEncoder,
		chunks: &FxHashMap<ChunkPos, ChunkGpu>,
		chunk_bg: &wgpu::BindGroup,
		index_buf: &wgpu::Buffer,
		timer: Option<&GpuTimer>,
	) -> (u32, u32) {
		let planes = side_planes(self.near_vp);
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("shadow near"),
			color_attachments: &[],
			depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
				view: &self.near_view,
				depth_ops: Some(wgpu::Operations {
					load: wgpu::LoadOp::Clear(1.0),
					store: wgpu::StoreOp::Store,
				}),
				stencil_ops: None,
			}),
			timestamp_writes: timer.and_then(|t| t.writes(Q_SHADOW_NEAR)),
			occlusion_query_set: None,
		});
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.light_near_bg, &[]);
		pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
		let (mut draws, mut tris) = (0u32, 0u32);
		for (pos, chunk) in chunks {
			if let Some(d) = Self::shadow_draw(&mut pass, chunk_bg, planes, pos, chunk) {
				draws += 1;
				tris += d / 3;
			}
		}
		(draws, tris)
	}

	/// Encode the next slice of an in-progress wide refresh into the back
	/// map (cleared on the first slice, appended thereafter). Returns
	/// (draws, tris, done).
	fn encode_wide_slice(
		&self,
		encoder: &mut wgpu::CommandEncoder,
		job: &mut WideJob,
		chunks: &FxHashMap<ChunkPos, ChunkGpu>,
		chunk_bg: &wgpu::BindGroup,
		index_buf: &wgpu::Buffer,
		timer: Option<&GpuTimer>,
	) -> (u32, u32, bool) {
		let planes = side_planes(job.vp);
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("shadow wide slice"),
			color_attachments: &[],
			depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
				view: &self.wide_views[self.pingpong.back()],
				depth_ops: Some(wgpu::Operations {
					load: if job.next == 0 {
						wgpu::LoadOp::Clear(1.0)
					} else {
						wgpu::LoadOp::Load
					},
					store: wgpu::StoreOp::Store,
				}),
				stencil_ops: None,
			}),
			timestamp_writes: timer.and_then(|t| t.writes(Q_SHADOW_WIDE)),
			occlusion_query_set: None,
		});
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.light_wide_bg, &[]);
		pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
		let (mut draws, mut tris) = (0u32, 0u32);
		while job.next < job.list.len() && draws < self.wide_slice_draws {
			let pos = job.list[job.next];
			job.next += 1;
			// Evicted mid-job: skip.
			let Some(chunk) = chunks.get(&pos) else {
				continue;
			};
			if let Some(d) = Self::shadow_draw(&mut pass, chunk_bg, planes, &pos, chunk) {
				draws += 1;
				tris += d / 3;
			}
		}
		(draws, tris, job.next >= job.list.len())
	}

	/// Cull-and-draw one chunk into a depth pass; returns the index count
	/// if drawn. Solid meshes only — water never casts shadows.
	fn shadow_draw(
		pass: &mut wgpu::RenderPass<'_>,
		chunk_bg: &wgpu::BindGroup,
		planes: [glam::Vec4; 4],
		pos: &ChunkPos,
		chunk: &ChunkGpu,
	) -> Option<u32> {
		let c = glam::Vec3::new(
			(pos.x * 16 + 8) as f32,
			(pos.y * 16 + 8) as f32,
			(pos.z * 16 + 8) as f32,
		);
		const RADIUS: f32 = 13.86;
		if !planes.iter().all(|p| p.truncate().dot(c) + p.w > -RADIUS) {
			return None;
		}
		let (buf, index_count) = chunk.solid.as_ref()?;
		pass.set_bind_group(
			1,
			chunk_bg,
			&[(u64::from(chunk.slot) * CHUNK_STRIDE) as u32],
		);
		pass.set_vertex_buffer(0, buf.slice(..));
		pass.draw_indexed(0..*index_count, 0, 0..1);
		Some(*index_count)
	}
}

#[cfg(test)]
mod tests {
	use super::WidePingPong;
	use digblock_core::chunk::ChunkPos;

	fn resident() -> Vec<ChunkPos> {
		vec![ChunkPos::new(0, 0, 0), ChunkPos::new(1, 0, 0)]
	}

	/// One frame of the pure bookkeeping: kickoff decision, sample
	/// capture (before any flip), then `slices` worth of encode with
	/// `done` on the last. Returns the sampled pair.
	fn frame(pp: &mut WidePingPong, vp: glam::Mat4, due: bool, done: bool) -> (usize, glam::Mat4) {
		pp.begin_frame(vp, due, resident);
		let sample = pp.sample();
		let _ = pp.take_bootstrap_clear();
		if let Some(job) = pp.take_job() {
			pp.finish_slice(job, done);
		}
		sample
	}

	#[test]
	fn sample_changes_only_after_job_completes_never_mid_job() {
		let mut pp = WidePingPong::new();
		let vp1 = glam::Mat4::from_translation(glam::Vec3::X);
		// Frame 1 kicks off (refresh_needed at construction) but the job
		// spans frames: the sample must stay (0, IDENTITY) throughout.
		let s1 = frame(&mut pp, vp1, false, false);
		let s2 = frame(&mut pp, vp1, false, false);
		assert_eq!((s1.0, s2.0), (0, 0));
		assert_eq!(s1.1, glam::Mat4::IDENTITY);
		assert_eq!(s2.1, glam::Mat4::IDENTITY);
		// Completing frame: the flip happens AFTER this frame's sample
		// was captured — index and VP still the old pair.
		let s3 = frame(&mut pp, vp1, false, true);
		assert_eq!(s3.0, 0);
		assert_eq!(s3.1, glam::Mat4::IDENTITY);
		// The completed map is first sampled the following frame, index
		// and VP changed together.
		let s4 = frame(&mut pp, vp1, false, false);
		assert_eq!(s4.0, 1);
		assert_eq!(s4.1, vp1);
	}

	#[test]
	fn restart_mid_job_retargets_same_back_map_from_slice_zero() {
		let mut pp = WidePingPong::new();
		let vp1 = glam::Mat4::from_translation(glam::Vec3::X);
		let vp2 = glam::Mat4::from_translation(glam::Vec3::Y);
		pp.begin_frame(vp1, false, resident);
		let back_before = pp.back();
		// Advance the job partway.
		let mut job = pp.take_job().unwrap();
		job.next = 1;
		pp.finish_slice(job, false);
		// Restart while in progress: same back map, job restarts at 0
		// with the new VP.
		assert!(pp.begin_frame(vp2, true, resident));
		assert_eq!(pp.back(), back_before);
		let job = pp.take_job().unwrap();
		assert_eq!(job.next, 0);
		assert_eq!(job.vp, vp2);
		pp.finish_slice(job, true);
		// The flip makes the just-rendered back map the sampled front,
		// adopting the restarted job's VP.
		assert_eq!(pp.sample(), (back_before, vp2));
	}

	#[test]
	fn refresh_needed_forces_job_even_without_wide_due() {
		// Construction (= device rebuild) marks a refresh; the first
		// frame must kick off a job even with wide_due false.
		let mut pp = WidePingPong::new();
		assert!(pp.refresh_pending(false));
		assert!(pp.begin_frame(glam::Mat4::IDENTITY, false, resident));
		// Consumed: without wide_due, no further kickoff...
		pp.take_job();
		assert!(!pp.begin_frame(glam::Mat4::IDENTITY, false, resident));
		// ...until marked again (device-loss recovery path).
		pp.mark_refresh_needed();
		assert!(pp.begin_frame(glam::Mat4::IDENTITY, false, resident));
	}

	#[test]
	fn bootstrap_clear_fires_exactly_once() {
		let mut pp = WidePingPong::new();
		assert!(pp.take_bootstrap_clear());
		assert!(!pp.take_bootstrap_clear());
	}
}
