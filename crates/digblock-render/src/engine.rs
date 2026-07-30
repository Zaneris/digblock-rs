use digblock_core::metrics::Metrics;
use digblock_platform::{Game, Gpu, InputEvent};

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

/// Device-lifetime renderer state. `Renderer::new` is a pure constructor so
/// device-loss recovery (F10 drill) is just "build a new one".
struct Renderer {
	// M4+: pipelines, bind group layouts, texture array, shadow maps,
	// chunk buffers. M1: nothing yet — the clear pass needs no resources.
}

impl Renderer {
	fn new(_gpu: &Gpu, _surface_format: wgpu::TextureFormat, _cfg: &EngineConfig) -> Self {
		Self {}
	}
}

pub struct Engine {
	config: EngineConfig,
	renderer: Option<Renderer>,
	metrics: Metrics,
}

impl Engine {
	pub fn new(config: EngineConfig) -> Self {
		let metrics = Metrics::new(config.metrics_csv.as_deref());
		Self {
			config,
			renderer: None,
			metrics,
		}
	}
}

impl Game for Engine {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, _size: (u32, u32)) {
		self.renderer = Some(Renderer::new(gpu, surface_format, &self.config));
		log::info!(
			"engine init: draw_dist={} shadow={}px format={surface_format:?}",
			self.config.draw_dist,
			self.config.shadow_size
		);
	}

	fn resize(&mut self, _gpu: &Gpu, size: (u32, u32)) {
		log::info!("resize: {}x{}", size.0, size.1);
	}

	fn input(&mut self, _event: InputEvent) {
		// M8: feed input drivers → PlayerIntent.
	}

	fn update(&mut self, _dt: std::time::Duration) {
		// M5: pump chunk pipeline; M8: fixed-tick player sim.
	}

	fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		depth: &wgpu::TextureView,
		_size: (u32, u32),
	) {
		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor {
				label: Some("frame"),
			});
		{
			let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label: Some("main"),
				color_attachments: &[Some(wgpu::RenderPassColorAttachment {
					view: target,
					resolve_target: None,
					ops: wgpu::Operations {
						load: wgpu::LoadOp::Clear(crate::SKY),
						store: wgpu::StoreOp::Store,
					},
					depth_slice: None,
				})],
				depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
					view: depth,
					depth_ops: Some(wgpu::Operations {
						load: wgpu::LoadOp::Clear(1.0),
						store: wgpu::StoreOp::Store,
					}),
					stencil_ops: None,
				}),
				timestamp_writes: None,
				occlusion_query_set: None,
			});
			// M4: solid pass; M7: water pass.
		}
		gpu.queue.submit([encoder.finish()]);
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
		// Device-loss drill: throw away every device-lifetime object and
		// rebuild from CPU-side state. M5+: also bump all chunk mesh_gen.
		self.renderer = Some(Renderer::new(gpu, surface_format, &self.config));
		log::info!("engine: renderer rebuilt (device-loss drill)");
	}
}
