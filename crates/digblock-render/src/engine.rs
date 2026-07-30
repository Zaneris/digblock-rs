use std::time::Duration;

use digblock_core::chunk::ChunkPos;
use digblock_core::metrics::Metrics;
use digblock_core::pipeline::Pipeline;
use digblock_core::sun::Sun;
use digblock_platform::{Game, Gpu, InputEvent, Key};
use glam::Vec3;

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

/// Per-frame mesh-upload budget: keeps main-thread GPU upload cost bounded
/// so streaming can never hitch a frame (the modern form of the Java
/// one-queue-item-per-frame rule).
const UPLOAD_BUDGET: usize = 4;

pub struct Engine {
	config: EngineConfig,
	renderer: Option<Renderer>,
	pipeline: Pipeline,
	camera: FlyCamera,
	sun: Sun,
	wide_due: bool,
	keys: HeldKeys,
	look_accum: (f32, f32),
	metrics: Metrics,
	size: (u32, u32),
}

impl Engine {
	pub fn new(config: EngineConfig) -> Self {
		let metrics = Metrics::new(config.metrics_csv.as_deref());
		// Java: seed = Math.random() * 10000, a domain offset for the noise.
		let seed = config
			.seed
			.unwrap_or_else(|| rand::random::<f64>() * 10000.0);
		let far = (config.draw_dist * 32) as f32;
		log::info!("world seed: {seed} | {} chunk workers", config.workers);
		let pipeline = Pipeline::new(seed, config.draw_dist, config.workers);
		let sun = config.tod.map_or_else(Sun::new, Sun::with_tod);
		Self {
			config,
			renderer: None,
			pipeline,
			camera: FlyCamera::new(far),
			sun,
			wide_due: false,
			keys: HeldKeys::default(),
			look_accum: (0.0, 0.0),
			metrics,
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
		let m = self.pipeline.maintain(center, UPLOAD_BUDGET);
		for pos in m.evicted {
			renderer.remove_chunk(pos);
		}
		let uploads = m.meshed.len() as u32;
		for out in m.meshed {
			renderer.upload_chunk(gpu, out.pos, &out.mesh);
		}
		self.metrics.chunks_generated += m.generated;
		self.metrics.chunks_meshed += uploads;
		self.metrics.meshes_uploaded += uploads;
		self.metrics.queue_depth = m.queue_depth;
	}

	fn frame_uniforms(&self, near_vp: glam::Mat4, wide_vp: glam::Mat4) -> FrameUniforms {
		let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
		let light_dir = self.sun.dir();
		FrameUniforms {
			view_proj: self.camera.view_proj(aspect).to_cols_array_2d(),
			light_near: near_vp.to_cols_array_2d(),
			light_wide: wide_vp.to_cols_array_2d(),
			light_dir: [light_dir.x, light_dir.y, light_dir.z, 0.0],
			misc: [1.0, 1.0, 1.0 / self.config.shadow_size as f32, 0.0],
		}
	}
}

impl Game for Engine {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, size: (u32, u32)) {
		self.size = size;
		self.renderer = Some(Renderer::new(gpu, surface_format, self.config.shadow_size));
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
		match event {
			InputEvent::Key { key, pressed } => match key {
				Key::W | Key::Up => self.keys.w = pressed,
				Key::S | Key::Down => self.keys.s = pressed,
				Key::A | Key::Left => self.keys.a = pressed,
				Key::D | Key::Right => self.keys.d = pressed,
				Key::Space => self.keys.up = pressed,
				Key::Shift => self.keys.down = pressed,
				_ => {}
			},
			InputEvent::MouseDelta { dx, dy } => {
				self.look_accum.0 += dx as f32;
				self.look_accum.1 += dy as f32;
			}
			InputEvent::FocusLost => self.keys = HeldKeys::default(),
			InputEvent::Touch { .. } => {} // M8 touch drivers
		}
	}

	fn update(&mut self, dt: Duration) {
		let dt = dt.as_secs_f32();
		// A fixed --tod freezes the sun angle but keeps the wide-cascade
		// refresh cadence (the map must still track streamed-in chunks).
		let fixed_tod = self.config.tod;
		self.wide_due |= self.sun.update(dt, self.camera.pos);
		if let Some(tod) = fixed_tod {
			self.sun.tod = tod;
		}
		const LOOK_SENS: f32 = 0.12; // degrees per raw mouse unit
		let (lx, ly) = std::mem::take(&mut self.look_accum);
		self.camera.look(lx * LOOK_SENS, ly * LOOK_SENS);

		// Fly movement (debug camera; replaced by Player in M8).
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
		let near_vp = self.sun.near_vp(self.camera.pos, d);
		let wide_vp = self.sun.wide_vp(d);
		let frame = self.frame_uniforms(near_vp, wide_vp);
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		let view_proj = self.camera.view_proj(aspect);
		let wide_due = std::mem::take(&mut self.wide_due);
		if let Some(r) = self.renderer.as_mut() {
			r.render(
				gpu,
				target,
				depth,
				&frame,
				view_proj,
				near_vp,
				wide_vp,
				wide_due,
				self.camera.pos,
			);
			self.metrics.resident_chunks = r.resident_chunks() as u32;
			self.metrics.gpu_buffer_bytes = r.gpu_buffer_bytes;
		}
		self.metrics.frame();
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
		self.renderer = Some(Renderer::new(gpu, surface_format, self.config.shadow_size));
		self.pipeline.remesh_all();
		log::info!("engine: renderer rebuilt (device-loss drill), re-mesh queued");
	}
}
