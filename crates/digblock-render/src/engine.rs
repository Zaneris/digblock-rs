use std::sync::Arc;
use std::time::Duration;

use digblock_core::chunk::{ChunkData, ChunkPos, WORLD_VCHUNK};
use digblock_core::mesher::{border_plane, mesh_chunk, MeshData, NeighborPlane, DIRS};
use digblock_core::metrics::Metrics;
use digblock_core::worldgen::build_chunk;
use digblock_platform::{Game, Gpu, InputEvent, Key};
use glam::Vec3;
use rustc_hash::FxHashMap;

use crate::camera::FlyCamera;
use crate::renderer::{FrameUniforms, Renderer};

pub struct EngineConfig {
	/// Draw distance in chunks: 20 desktop, 10 mobile (Java Config).
	pub draw_dist: i32,
	/// Shadow map resolution: 2048 desktop, 1024 mobile.
	pub shadow_size: u32,
	pub seed: Option<f64>,
	pub metrics_csv: Option<std::path::PathBuf>,
}

impl EngineConfig {
	pub fn desktop() -> Self {
		Self {
			draw_dist: 20,
			shadow_size: 2048,
			seed: None,
			metrics_csv: None,
		}
	}

	pub fn mobile() -> Self {
		Self {
			draw_dist: 10,
			shadow_size: 1024,
			seed: None,
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

pub struct Engine {
	config: EngineConfig,
	seed: f64,
	renderer: Option<Renderer>,
	world: FxHashMap<ChunkPos, Arc<ChunkData>>,
	camera: FlyCamera,
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
		log::info!("world seed: {seed}");
		Self {
			config,
			seed,
			renderer: None,
			world: FxHashMap::default(),
			camera: FlyCamera::new(far),
			keys: HeldKeys::default(),
			look_accum: (0.0, 0.0),
			metrics,
			size: (1, 1),
		}
	}

	fn neighbor_planes(&self, pos: ChunkPos) -> [NeighborPlane; 6] {
		DIRS.map(|dir| {
			let (ox, oy, oz) = dir.offset();
			let np = ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz);
			if np.y >= WORLD_VCHUNK {
				return NeighborPlane::Air; // Java airBlock sentinel above the world
			}
			if np.y < 0 {
				return NeighborPlane::Missing; // Java null below the world
			}
			match self.world.get(&np) {
				Some(data) => NeighborPlane::Blocks(border_plane(data, dir.opposite())),
				None => NeighborPlane::Missing,
			}
		})
	}

	fn mesh_one(&self, pos: ChunkPos) -> Option<MeshData> {
		let data = self.world.get(&pos)?;
		Some(mesh_chunk(data, &self.neighbor_planes(pos)))
	}

	/// M4 interim: generate + mesh a static grid synchronously at init.
	/// M5 replaces this with the threaded streaming pipeline.
	fn build_static_world(&mut self, gpu: &Gpu) {
		const R: i32 = 3;
		for x in -R..=R {
			for z in -R..=R {
				for y in 0..WORLD_VCHUNK {
					let pos = ChunkPos::new(x, y, z);
					self.world
						.insert(pos, Arc::new(build_chunk(pos, self.seed)));
				}
			}
		}
		let positions: Vec<ChunkPos> = self.world.keys().copied().collect();
		for pos in positions {
			if let Some(mesh) = self.mesh_one(pos) {
				self.renderer
					.as_mut()
					.expect("renderer")
					.upload_chunk(gpu, pos, &mesh);
			}
		}
		let resident = self.renderer.as_ref().expect("renderer").resident_chunks();
		log::info!("static world: {resident} chunks resident");
	}

	fn frame_uniforms(&self) -> FrameUniforms {
		let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
		// Placeholder sun until M6: slightly tilted noon light.
		let light_dir = Vec3::new(-0.25, -1.0, -0.15).normalize();
		FrameUniforms {
			view_proj: self.camera.view_proj(aspect).to_cols_array_2d(),
			light_near: glam::Mat4::IDENTITY.to_cols_array_2d(),
			light_wide: glam::Mat4::IDENTITY.to_cols_array_2d(),
			light_dir: [light_dir.x, light_dir.y, light_dir.z, 0.0],
			misc: [1.0, 0.0, 1.0 / self.config.shadow_size as f32, 0.0],
		}
	}
}

impl Game for Engine {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, size: (u32, u32)) {
		self.size = size;
		self.renderer = Some(Renderer::new(gpu, surface_format, self.config.shadow_size));
		self.build_static_world(gpu);
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
		let frame = self.frame_uniforms();
		if let Some(r) = self.renderer.as_mut() {
			r.render(gpu, target, depth, &frame);
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
		// Device-loss drill: rebuild every device-lifetime object from
		// CPU-side state (world data survives; meshes regenerate).
		self.renderer = Some(Renderer::new(gpu, surface_format, self.config.shadow_size));
		let positions: Vec<ChunkPos> = self.world.keys().copied().collect();
		for pos in positions {
			if let Some(mesh) = self.mesh_one(pos) {
				self.renderer
					.as_mut()
					.unwrap()
					.upload_chunk(gpu, pos, &mesh);
			}
		}
		log::info!("engine: renderer rebuilt (device-loss drill)");
	}
}
