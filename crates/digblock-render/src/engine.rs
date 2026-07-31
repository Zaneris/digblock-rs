use std::time::Duration;

use digblock_core::chunk::ChunkPos;
use digblock_core::metrics::{FrameTimings, Metrics, PipelineGauges};
use digblock_core::pipeline::Pipeline;
use digblock_core::player::{Player, PlayerIntent, TICK};
use digblock_core::raycast::{raycast, REACH};
use digblock_core::sun::Sun;
use digblock_core::touch::{Phase, TouchDriver};
use digblock_core::BlockType;
use digblock_platform::{Game, Gpu, InputEvent, Key, MouseButton, TouchPhase};
use glam::{Vec2, Vec3};

use crate::camera::FlyCamera;
use crate::renderer::{FrameUniforms, Renderer};

pub struct EngineConfig {
	/// Draw distance in chunks: 20 desktop, 10 mobile (Java Config).
	pub draw_dist: i32,
	/// Shadow map resolution: 2048 desktop, 1024 mobile.
	pub shadow_size: u32,
	/// Chunk worker threads.
	pub workers: usize,
	pub seed: Option<f64>,
	/// Fixed sun angle in degrees for deterministic captures.
	pub tod: Option<f32>,
	pub metrics_csv: Option<std::path::PathBuf>,
	/// Start with the debug overlay visible (` toggles at runtime).
	pub overlay: bool,
	/// Benchmark mode: inputs ignored, deterministic scripted camera
	/// flight, run summary logged on exit.
	pub bench: bool,
}

impl EngineConfig {
	pub fn desktop() -> Self {
		let workers = std::thread::available_parallelism().map_or(4, |n| (n.get() - 2).clamp(2, 4));
		Self {
			draw_dist: 20,
			shadow_size: 2048,
			workers,
			seed: None,
			tod: None,
			metrics_csv: None,
			overlay: false,
			bench: false,
		}
	}

	pub fn mobile() -> Self {
		Self {
			draw_dist: 10,
			shadow_size: 1024,
			workers: 2,
			seed: None,
			tod: None,
			metrics_csv: None,
			// iOS has no keyboard toggle; flip this for device profiling.
			overlay: false,
			bench: false,
		}
	}
}

#[derive(Default)]
struct HeldKeys {
	w: bool,
	a: bool,
	s: bool,
	d: bool,
	up: bool,
	down: bool,
}

/// Per-frame wall-clock budget for main-thread streaming — the modern form
/// of the Java one-queue-item-per-frame rule. The first half bounds the
/// result drain (chunk inserts + neighbor cascades); the rest bounds mesh
/// uploads, with leftovers carried in `pending_uploads` to later frames.
/// Workers keep producing at full speed — undrained results wait in the
/// channel — so this trades a longer fill tail for even frame pacing:
/// streaming + render must fit a 120Hz vsync frame (8.3ms).
const STREAM_BUDGET_MS: f32 = 3.0;
/// Uploads always make this much progress per frame even with the budget
/// exhausted, so a saturated drain can never starve the upload queue.
const UPLOAD_MIN: usize = 4;

pub struct Engine {
	config: EngineConfig,
	/// Resolved world seed (config.seed or a random one).
	seed: f64,
	renderer: Option<Renderer>,
	pipeline: Pipeline,
	camera: FlyCamera,
	player: Player,
	touch: TouchDriver,
	/// Walk (player physics) vs debug fly camera; F toggles.
	walking: bool,
	fly_key_down: bool,
	overlay_key_down: bool,
	tick_accum: f32,
	sun: Sun,
	wide_due: bool,
	keys: HeldKeys,
	look_accum: (f32, f32),
	/// Click edges pending for the next update (left = dig, right = place).
	click_dig: bool,
	click_place: bool,
	/// What right-click places: the last block dug (default dirt).
	held_block: BlockType,
	metrics: Metrics,
	/// Phase timings for the in-progress frame, handed to metrics at the
	/// end of render().
	pending: FrameTimings,
	overlay_visible: bool,
	history_scratch: Vec<f32>,
	/// Scripted-flight clock (bench mode), in fixed 60Hz ticks.
	bench_t: f32,
	/// Meshes drained but not yet uploaded (upload budget carry-over).
	pending_uploads: std::collections::VecDeque<digblock_core::pipeline::MeshOutput>,
	size: (u32, u32),
}

fn ms_since(t: std::time::Instant) -> f32 {
	t.elapsed().as_secs_f32() * 1000.0
}

impl Engine {
	pub fn new(config: EngineConfig) -> Self {
		let mut metrics = Metrics::new(config.metrics_csv.as_deref());
		metrics.workers = config.workers as u32;
		if config.bench {
			metrics.record_run_history();
		}
		// Java: seed = Math.random() * 10000, a domain offset for the noise.
		let seed = config
			.seed
			.unwrap_or_else(|| rand::random::<f64>() * 10000.0);
		let far = (config.draw_dist * 32) as f32;
		log::info!("world seed: {seed} | {} chunk workers", config.workers);
		let pipeline = Pipeline::new(seed, config.draw_dist, config.workers);
		let sun = config.tod.map_or_else(Sun::new, Sun::with_tod);
		let overlay_visible = config.overlay;
		let spawn = digblock_core::player::spawn(seed);
		Self {
			config,
			seed,
			renderer: None,
			pipeline,
			camera: FlyCamera::new(far, spawn),
			player: Player::new(spawn),
			touch: TouchDriver::new(),
			walking: true,
			fly_key_down: false,
			overlay_key_down: false,
			tick_accum: 0.0,
			sun,
			wide_due: false,
			keys: HeldKeys::default(),
			look_accum: (0.0, 0.0),
			click_dig: false,
			click_place: false,
			held_block: BlockType::Dirt,
			metrics,
			pending: FrameTimings::default(),
			overlay_visible,
			history_scratch: Vec::new(),
			bench_t: 0.0,
			pending_uploads: std::collections::VecDeque::new(),
			size: (1, 1),
		}
	}

	/// Camera far plane = light rig depth = draw_dist × 32 (Java World.updateDepth).
	fn depth(&self) -> f32 {
		(self.config.draw_dist * 32) as f32
	}

	fn camera_chunk(&self) -> ChunkPos {
		ChunkPos::new(
			(self.camera.pos.x / 16.0).floor() as i32,
			0,
			(self.camera.pos.z / 16.0).floor() as i32,
		)
	}

	fn pump_pipeline(&mut self, gpu: &Gpu) {
		let center = self.camera_chunk();
		let Some(renderer) = self.renderer.as_mut() else {
			return;
		};
		let t_maintain = std::time::Instant::now();
		let full_deadline = t_maintain + Duration::from_secs_f32(STREAM_BUDGET_MS / 1000.0);
		let drain_deadline = t_maintain + Duration::from_secs_f32(STREAM_BUDGET_MS / 2000.0);
		let m = self
			.pipeline
			.maintain(center, usize::MAX, Some(drain_deadline));
		self.pending.maintain_ms = ms_since(t_maintain);
		let t_upload = std::time::Instant::now();
		self.metrics.evicted += m.evicted.len() as u32;
		if !m.evicted.is_empty() {
			self.pending_uploads
				.retain(|out| !m.evicted.contains(&out.pos));
			for pos in m.evicted {
				renderer.remove_chunk(pos);
			}
		}
		self.pending_uploads.extend(m.meshed);
		// Nearest-first: on fast travel the queue backs up, and the chunks
		// that matter are the ones at and around the player. UPLOAD_MIN ≥
		// stack height, so the column the player is in/above always lands
		// the frame it drains — only the far ring is allowed to lag.
		if self.pending_uploads.len() > 1 {
			self.pending_uploads.make_contiguous().sort_by_key(|out| {
				let dx = out.pos.x - center.x;
				let dz = out.pos.z - center.z;
				dx * dx + dz * dz
			});
		}
		let mut uploads = 0u32;
		while let Some(out) = self.pending_uploads.pop_front() {
			renderer.upload_chunk(gpu, out.pos, &out.mesh);
			uploads += 1;
			if uploads as usize >= UPLOAD_MIN && std::time::Instant::now() >= full_deadline {
				break;
			}
		}
		self.pending.upload_ms = ms_since(t_upload);
		self.metrics.chunks_generated += m.generated;
		self.metrics.meshes_uploaded += uploads;
		self.metrics.gen_jobs.merge(m.gen_jobs);
		self.metrics.mesh_jobs.merge(m.mesh_jobs);
		self.metrics.stale_dropped += m.stale_dropped;
		self.metrics.requeued += m.requeued;
		self.metrics.upload_saturated += m.saturated as u32;
		self.metrics.gauges = PipelineGauges {
			gen_queue: m.gen_queue,
			mesh_queue: m.mesh_queue,
			in_flight: m.in_flight,
			resident: self.pipeline.resident() as u32,
		};
		// First full quiesce = initial world fill complete (the bench's
		// primary streaming-throughput metric).
		if m.gen_queue == 0
			&& m.mesh_queue == 0
			&& m.in_flight == 0
			&& self.pending_uploads.is_empty()
		{
			self.metrics.mark_streamed();
		}
	}

	/// Resolve pending dig/place clicks against the world. Runs in update,
	/// outside the physics `block_at` borrow: raycast (immutable) →
	/// set_block (mutable) → player tick (immutable), strictly sequential.
	fn apply_clicks(&mut self) {
		let dig = std::mem::take(&mut self.click_dig);
		let place = std::mem::take(&mut self.click_place);
		if !dig && !place {
			return;
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let hit = {
			let pipeline = &self.pipeline;
			raycast(origin, dir, REACH, &|x, y, z| pipeline.block_at(x, y, z))
		};
		let Some(hit) = hit else { return };
		if dig {
			if let Some(prev) =
				self.pipeline
					.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Air)
			{
				self.held_block = prev;
			}
		} else if hit.normal != (0, 0, 0) {
			let target = (
				hit.cell.0 + hit.normal.0,
				hit.cell.1 + hit.normal.1,
				hit.cell.2 + hit.normal.2,
			);
			// Never place inside the player: the eye cell and the body cell
			// (the −1.25 probe the point collider uses).
			let cell = |v: Vec3| (v.x.floor() as i32, v.y.floor() as i32, v.z.floor() as i32);
			let eye = cell(self.camera.pos);
			let body = cell(self.camera.pos - Vec3::new(0.0, 1.25, 0.0));
			if target != eye && target != body {
				self.pipeline
					.set_block(target.0, target.1, target.2, self.held_block);
			}
		}
	}

	fn frame_uniforms(&self, near_vp: glam::Mat4, wide_vp: glam::Mat4) -> FrameUniforms {
		let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
		let vp = self.camera.view_proj(aspect);
		let caster = self.sun.caster();
		let atmos = digblock_core::sky::Atmosphere::sample(self.sun.elevation());
		// Directions toward the bodies in the sky (opposite the light).
		let sun = -self.sun.sun_dir();
		let moon = -self.sun.moon_dir();
		let far = self.depth();
		let (fog_start, fog_end) = (0.70 * far, 0.95 * far);
		let cam = self.camera.pos;
		FrameUniforms {
			view_proj: vp.to_cols_array_2d(),
			inv_view_proj: vp.inverse().to_cols_array_2d(),
			light_near: near_vp.to_cols_array_2d(),
			light_wide: wide_vp.to_cols_array_2d(),
			light_dir: [caster.dir.x, caster.dir.y, caster.dir.z, caster.strength],
			light_color: [
				atmos.light_color[0],
				atmos.light_color[1],
				atmos.light_color[2],
				0.0,
			],
			ambient: [atmos.ambient[0], atmos.ambient[1], atmos.ambient[2], 0.0],
			fog_color: [
				atmos.horizon[0],
				atmos.horizon[1],
				atmos.horizon[2],
				fog_start,
			],
			cam_pos: [cam.x, cam.y, cam.z, 1.0 / (fog_end - fog_start)],
			sun_dir: [sun.x, sun.y, sun.z, atmos.star_intensity],
			moon_dir: [moon.x, moon.y, moon.z, atmos.glow_strength],
			zenith: [atmos.zenith[0], atmos.zenith[1], atmos.zenith[2], 0.0],
			horizon: [atmos.horizon[0], atmos.horizon[1], atmos.horizon[2], 0.0],
			misc: [
				1.0,
				1.0,
				1.0 / self.config.shadow_size as f32,
				// Wide-cascade texel size in world units (extent / resolution),
				// the chunk shader's normal-offset distance.
				far / self.config.shadow_size as f32,
			],
		}
	}
}

impl Drop for Engine {
	fn drop(&mut self) {
		if self.config.bench {
			if let Some(line) = self.metrics.bench_summary() {
				log::info!("{line}");
			}
		}
	}
}

impl Game for Engine {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, size: (u32, u32)) {
		self.size = size;
		self.renderer = Some(Renderer::new(
			gpu,
			surface_format,
			self.config.shadow_size,
			self.config.draw_dist,
		));
		log::info!(
			"engine init: draw_dist={} shadow={}px format={surface_format:?}",
			self.config.draw_dist,
			self.config.shadow_size
		);
	}

	fn resize(&mut self, _gpu: &Gpu, size: (u32, u32)) {
		self.size = size;
	}

	fn input(&mut self, event: InputEvent) {
		// Benchmark runs must be input-independent to stay comparable.
		if self.config.bench {
			return;
		}
		match event {
			InputEvent::Key { key, pressed } => match key {
				Key::W | Key::Up => self.keys.w = pressed,
				Key::S | Key::Down => self.keys.s = pressed,
				Key::A | Key::Left => self.keys.a = pressed,
				Key::D | Key::Right => self.keys.d = pressed,
				Key::Space => self.keys.up = pressed,
				Key::Shift => self.keys.down = pressed,
				Key::F => {
					if pressed && !self.fly_key_down {
						self.walking = !self.walking;
						if !self.walking {
							// Enter fly where the player stands.
							self.camera.pos = self.player.pos;
						} else {
							// Re-enter walk at the camera position.
							self.player.pos = self.camera.pos;
						}
						log::info!("mode: {}", if self.walking { "walk" } else { "fly" });
					}
					self.fly_key_down = pressed;
				}
				Key::Backquote => {
					if pressed && !self.overlay_key_down {
						self.overlay_visible = !self.overlay_visible;
					}
					self.overlay_key_down = pressed;
				}
				_ => {}
			},
			InputEvent::MouseDelta { dx, dy } => {
				self.look_accum.0 += dx as f32;
				self.look_accum.1 += dy as f32;
			}
			InputEvent::MouseButton { button, pressed } => {
				if pressed {
					match button {
						MouseButton::Left => self.click_dig = true,
						MouseButton::Right => self.click_place = true,
						MouseButton::Other => {}
					}
				}
			}
			InputEvent::FocusLost => {
				self.keys = HeldKeys::default();
				self.touch.clear();
			}
			InputEvent::Touch { id, phase, x, y } => {
				let phase = match phase {
					TouchPhase::Started => Phase::Started,
					TouchPhase::Moved => Phase::Moved,
					TouchPhase::Ended => Phase::Ended,
					TouchPhase::Cancelled => Phase::Cancelled,
				};
				self.touch.handle(id, phase, x, y);
			}
		}
	}

	fn frame_acquired(&mut self, acquire_wait: Duration) {
		self.pending.acquire_ms = acquire_wait.as_secs_f32() * 1000.0;
	}

	fn update(&mut self, dt: Duration) {
		let t_update = std::time::Instant::now();
		let dt = dt.as_secs_f32();
		// A fixed --tod freezes the sun angle but keeps the wide-cascade
		// refresh cadence (the map must still track streamed-in chunks).
		let fixed_tod = self.config.tod;
		self.wide_due |= self.sun.update(dt, self.camera.pos);
		if let Some(tod) = fixed_tod {
			self.sun.tod = tod;
		}
		// Scripted benchmark flight: constant-speed line with a slow yaw
		// sweep, advanced one fixed 60Hz tick per frame so the path depends
		// only on the frame index — identical workload run-to-run.
		if self.config.bench {
			const BENCH_SPEED: f32 = 30.0; // blocks per path-second
			const BENCH_YAW: f32 = 12.0; // degrees per path-second
								// Altitude follows the terrain so worldgen retunes can't put the
								// flight underground; still a pure function of the frame index.
			self.bench_t += TICK;
			let x = BENCH_SPEED * self.bench_t;
			let h = digblock_core::worldgen::terrain_height(x as i32, 8, self.seed);
			self.camera.pos = Vec3::new(x, h as f32 + 12.0, 8.0);
			self.camera.look(BENCH_YAW * TICK, 0.0);
			self.pending.update_ms = ms_since(t_update);
			return;
		}

		const LOOK_SENS: f32 = 0.12; // degrees per raw mouse unit
		let (lx, ly) = std::mem::take(&mut self.look_accum);
		let touch_look = self.touch.take_look();
		self.camera
			.look(lx * LOOK_SENS + touch_look.x, ly * LOOK_SENS + touch_look.y);

		self.apply_clicks();

		if self.walking {
			// Fixed 60 Hz player simulation (Java feel was tuned per-frame
			// at 60fps; the accumulator preserves it at any display rate).
			let touch_jump = self.touch.take_jump();
			let key_axis = Vec2::new(
				(self.keys.d as i32 - self.keys.a as i32) as f32,
				(self.keys.w as i32 - self.keys.s as i32) as f32,
			);
			let intent = PlayerIntent {
				move_axis: key_axis + self.touch.move_axis(),
				jump: self.keys.up || touch_jump,
			};
			let dir = self.camera.dir();
			self.tick_accum = (self.tick_accum + dt).min(0.25);
			let pipeline = &self.pipeline;
			let block_at = |x: i32, y: i32, z: i32| pipeline.block_at(x, y, z);
			while self.tick_accum >= TICK {
				self.tick_accum -= TICK;
				self.player.tick(TICK, dir, intent, &block_at);
			}
			self.camera.pos = self.player.pos;
		} else {
			// Debug fly camera.
			let speed = 24.0 * dt;
			let dir = self.camera.dir();
			let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
			let right = flat.cross(Vec3::Y);
			let mut mv = Vec3::ZERO;
			if self.keys.w {
				mv += flat;
			}
			if self.keys.s {
				mv -= flat;
			}
			if self.keys.d {
				mv += right;
			}
			if self.keys.a {
				mv -= right;
			}
			if self.keys.up {
				mv += Vec3::Y;
			}
			if self.keys.down {
				mv -= Vec3::Y;
			}
			self.camera.pos += mv.normalize_or_zero() * speed;
		}
		self.pending.update_ms = ms_since(t_update);
	}

	fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		depth: &wgpu::TextureView,
		size: (u32, u32),
	) {
		self.size = size;
		// Streaming happens here (not update) because uploads need the GPU.
		self.pump_pipeline(gpu);
		let d = self.depth();
		let texels = self.config.shadow_size as f32;
		let near_vp = self.sun.near_vp(self.camera.pos, d, texels);
		let wide_vp = self.sun.wide_vp(d, texels);
		let frame = self.frame_uniforms(near_vp, wide_vp);
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		let view_proj = self.camera.view_proj(aspect);
		let wide_due = std::mem::take(&mut self.wide_due);
		// Overlay data is last frame's numbers (metrics.frame() runs at the
		// end of this fn) and GPU ms lags 2-3 frames — fine for a debug HUD.
		let overlay_stats = if self.overlay_visible {
			self.metrics
				.history_tail(crate::overlay::GRAPH_FRAMES, &mut self.history_scratch);
			Some(self.metrics.overlay_stats())
		} else {
			None
		};
		if let Some(r) = self.renderer.as_mut() {
			let t_encode = std::time::Instant::now();
			let stats = r.render(
				gpu,
				target,
				depth,
				&frame,
				view_proj,
				near_vp,
				wide_vp,
				wide_due,
				self.camera.pos,
				size,
				overlay_stats
					.as_ref()
					.map(|s| (s, self.history_scratch.as_slice())),
			);
			self.pending.encode_ms = ms_since(t_encode);
			self.metrics.render = stats;
			self.metrics.gpu = r.gpu_pass_ms();
			self.metrics.gpu_buffer_bytes = r.gpu_buffer_bytes;
		}
		// total_ms is measured inside frame() from the frame-to-frame delta.
		self.metrics.frame(std::mem::take(&mut self.pending));
	}

	fn on_suspend(&mut self) {
		log::info!("engine: on_suspend (state save point)");
		self.metrics.pause();
	}

	fn on_resume(&mut self) {
		log::info!("engine: on_resume");
	}

	fn rebuild_gpu_resources(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat) {
		// Device-loss drill: rebuild every device-lifetime object; meshes
		// stream back in from retained chunk data via the pipeline.
		self.renderer = Some(Renderer::new(
			gpu,
			surface_format,
			self.config.shadow_size,
			self.config.draw_dist,
		));
		self.pipeline.remesh_all();
		log::info!("engine: renderer rebuilt (device-loss drill), re-mesh queued");
	}
}
