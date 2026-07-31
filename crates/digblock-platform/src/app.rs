//! The lifecycle state machine. Every entry into / exit from the
//! live-surface state goes through exactly two functions
//! (`enter_suspend` / `resume_from`), shared by: initial launch, iOS
//! background/foreground, future Android surface destruction, and the F9
//! desktop drill. `Suspended` has no surface field, so rendering without a
//! surface is unrepresentable.

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::clock::GameClock;
use crate::input::{map_button, map_key, map_touch_phase, InputEvent};
use crate::{Game, Gpu, RunOptions};

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

struct Running {
	gpu: Arc<Gpu>,
	window: Arc<Window>,
	surface: wgpu::Surface<'static>,
	config: wgpu::SurfaceConfiguration,
	depth: wgpu::TextureView,
}

struct Suspended {
	gpu: Arc<Gpu>,
	// Some on iOS/desktop (window object survives backgrounding); the
	// Option is the pre-built escape hatch for Android, where the window
	// itself can die and must be recreated on resume.
	window: Option<Arc<Window>>,
}

// One instance per process — variant size difference is irrelevant.
#[allow(clippy::large_enum_variant)]
enum AppState {
	Uninitialized,
	Running(Running),
	Suspended(Suspended),
}

struct App<G: Game> {
	state: AppState,
	game: G,
	clock: GameClock,
	options: RunOptions,
	cursor_grabbed: bool,
	frames_rendered: u64,
	screenshot_pending: bool,
	launched_at: Instant,
}

pub fn run(game: impl Game, options: RunOptions) {
	let event_loop = EventLoop::new().expect("event loop");
	event_loop.set_control_flow(ControlFlow::Wait);
	let mut app = App {
		state: AppState::Uninitialized,
		game,
		clock: GameClock::new(),
		options,
		cursor_grabbed: false,
		frames_rendered: 0,
		screenshot_pending: false,
		launched_at: Instant::now(),
	};
	event_loop.run_app(&mut app).expect("event loop run");
}

fn window_attrs(title: &str) -> winit::window::WindowAttributes {
	let attrs = Window::default_attributes().with_title(title.to_owned());
	#[cfg(target_os = "ios")]
	let attrs = {
		use winit::platform::ios::WindowAttributesExtIOS;
		attrs
			.with_prefers_home_indicator_hidden(true)
			.with_prefers_status_bar_hidden(true)
			.with_preferred_screen_edges_deferring_system_gestures(
				winit::platform::ios::ScreenEdge::ALL,
			)
	};
	attrs
}

fn create_surface(
	gpu: &Gpu,
	window: Arc<Window>,
	no_vsync: bool,
) -> (wgpu::Surface<'static>, wgpu::SurfaceConfiguration) {
	let size = window.inner_size();
	let surface = gpu.instance.create_surface(window).expect("create surface");
	let config = surface_config(
		gpu,
		&surface,
		size.width.max(1),
		size.height.max(1),
		no_vsync,
	);
	surface.configure(&gpu.device, &config);
	(surface, config)
}

fn surface_config(
	gpu: &Gpu,
	surface: &wgpu::Surface<'_>,
	width: u32,
	height: u32,
	no_vsync: bool,
) -> wgpu::SurfaceConfiguration {
	let caps = surface.get_capabilities(&gpu.adapter);
	// Gamma-space rendering is deliberate (Java look parity): pick a
	// non-sRGB format. Bgra8Unorm is universal on Metal.
	let format = caps
		.formats
		.iter()
		.copied()
		.find(|f| !f.is_srgb())
		.unwrap_or(caps.formats[0]);
	wgpu::SurfaceConfiguration {
		usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
		format,
		width,
		height,
		present_mode: if no_vsync {
			// Falls back to a supported uncapped (or Fifo) mode per-platform.
			wgpu::PresentMode::AutoNoVsync
		} else {
			wgpu::PresentMode::Fifo
		},
		desired_maximum_frame_latency: 2,
		alpha_mode: caps.alpha_modes[0],
		view_formats: vec![],
	}
}

fn create_depth(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
	device
		.create_texture(&wgpu::TextureDescriptor {
			label: Some("surface depth"),
			size: wgpu::Extent3d {
				width,
				height,
				depth_or_array_layers: 1,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: DEPTH_FORMAT,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
			view_formats: &[],
		})
		.create_view(&wgpu::TextureViewDescriptor::default())
}

impl<G: Game> App<G> {
	fn init_gpu(
		window: Arc<Window>,
		no_vsync: bool,
	) -> (Gpu, wgpu::Surface<'static>, wgpu::SurfaceConfiguration) {
		let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
		let surface = instance
			.create_surface(window.clone())
			.expect("create surface");
		let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
			power_preference: wgpu::PowerPreference::HighPerformance,
			compatible_surface: Some(&surface),
			force_fallback_adapter: false,
		}))
		.expect("no compatible GPU adapter");
		log::info!("adapter: {:?}", adapter.get_info());
		// GPU pass timing (debug/metrics facility) when the adapter has it.
		let required_features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
		log::info!(
			"gpu timestamps: {}",
			required_features.contains(wgpu::Features::TIMESTAMP_QUERY)
		);
		let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
			label: Some("digblock"),
			required_features,
			..Default::default()
		}))
		.expect("request device");
		let gpu = Gpu {
			instance,
			adapter,
			device,
			queue,
		};
		let size = window.inner_size();
		let config = surface_config(
			&gpu,
			&surface,
			size.width.max(1),
			size.height.max(1),
			no_vsync,
		);
		surface.configure(&gpu.device, &config);
		(gpu, surface, config)
	}

	fn enter_suspend(&mut self) {
		match std::mem::replace(&mut self.state, AppState::Uninitialized) {
			AppState::Running(r) => {
				log::info!("lifecycle: suspend (surface dropped, device retained)");
				self.clock.pause();
				self.game.input(InputEvent::FocusLost);
				self.game.on_suspend();
				// r.surface and r.depth drop here; gpu + window retained.
				self.state = AppState::Suspended(Suspended {
					gpu: r.gpu,
					window: Some(r.window),
				});
			}
			other => self.state = other, // idempotent
		}
	}

	fn resume_from(&mut self, s: Suspended, el: &ActiveEventLoop) {
		log::info!("lifecycle: resume (recreating surface)");
		let window = s.window.unwrap_or_else(|| {
			Arc::new(el.create_window(window_attrs(&self.options.title)).unwrap())
		});
		let (surface, config) = create_surface(&s.gpu, window.clone(), self.options.no_vsync);
		let depth = create_depth(&s.gpu.device, config.width, config.height);
		// Engine pipelines/buffers are untouched — the Device survived.
		// Only notify about a possible size change while backgrounded.
		self.game.resize(&s.gpu, (config.width, config.height));
		self.game.on_resume();
		self.state = AppState::Running(Running {
			gpu: s.gpu,
			window,
			surface,
			config,
			depth,
		});
		if let AppState::Running(r) = &self.state {
			r.window.request_redraw();
		}
	}

	fn reconfigure(&mut self, width: u32, height: u32) {
		if let AppState::Running(r) = &mut self.state {
			if width == 0 || height == 0 {
				return;
			}
			r.config.width = width;
			r.config.height = height;
			r.surface.configure(&r.gpu.device, &r.config);
			r.depth = create_depth(&r.gpu.device, width, height);
			self.game.resize(&r.gpu, (width, height));
		}
	}

	fn render_frame(&mut self, el: &ActiveEventLoop) {
		let AppState::Running(r) = &mut self.state else {
			return;
		};

		let t_acquire = std::time::Instant::now();
		let frame = match r.surface.get_current_texture() {
			Ok(f) => f,
			Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
				log::warn!("surface lost/outdated — reconfiguring");
				r.surface.configure(&r.gpu.device, &r.config);
				r.window.request_redraw();
				return;
			}
			Err(wgpu::SurfaceError::Timeout) => {
				r.window.request_redraw();
				return;
			}
			Err(e) => {
				log::error!("unrecoverable surface error: {e}");
				el.exit();
				return;
			}
		};

		self.game.frame_acquired(t_acquire.elapsed());
		let dt = self.clock.tick();
		self.game.update(dt);

		let view = frame
			.texture
			.create_view(&wgpu::TextureViewDescriptor::default());
		self.game
			.render(&r.gpu, &view, &r.depth, (r.config.width, r.config.height));

		self.frames_rendered += 1;
		let want_shot = self.screenshot_pending
			|| (self.options.screenshot.is_some()
				&& self.frames_rendered == u64::from(self.options.screenshot_after_frames.max(1)));
		if want_shot {
			if let Some(path) = self.options.screenshot.clone().or_else(|| {
				Some(std::path::PathBuf::from(format!(
					"shots/f8-{}.png",
					self.launched_at.elapsed().as_secs()
				)))
			}) {
				crate::screenshot::capture(&r.gpu, &frame.texture, r.config.format, &path);
			}
			self.screenshot_pending = false;
			if self.options.exit_after_screenshot && self.options.screenshot.is_some() {
				frame.present();
				el.exit();
				return;
			}
		}

		frame.present();
		if self
			.options
			.exit_after_frames
			.is_some_and(|n| self.frames_rendered >= n)
		{
			el.exit();
			return;
		}
		// Redraw-chained loop: pacing comes from present (Fifo blocks at
		// vsync; no-vsync chains back-to-back), and this pattern avoids
		// winit's iOS busy-spin issue with poll loops.
		r.window.request_redraw();
	}

	/// Automated lifecycle drill (`--drill`). Exercises byte-for-byte the
	/// same transitions the OS lifecycle events use.
	fn auto_drill(&mut self, el: &ActiveEventLoop) {
		match self.frames_rendered {
			20 => {
				log::info!("auto-drill: suspend + immediate resume");
				self.enter_suspend();
				if let AppState::Suspended(s) =
					std::mem::replace(&mut self.state, AppState::Uninitialized)
				{
					self.resume_from(s, el);
				}
			}
			40 => {
				if let AppState::Running(r) = &self.state {
					log::info!("auto-drill: device-loss rebuild");
					self.game.rebuild_gpu_resources(&r.gpu, r.config.format);
				}
			}
			60 => {
				log::info!("auto-drill: complete, exiting");
				el.exit();
			}
			_ => {}
		}
	}

	fn grab_cursor(&mut self, grab: bool) {
		if let AppState::Running(r) = &self.state {
			let mode = if grab {
				winit::window::CursorGrabMode::Locked
			} else {
				winit::window::CursorGrabMode::None
			};
			let mode_ok = r
				.window
				.set_cursor_grab(mode)
				.or_else(|_| {
					r.window.set_cursor_grab(if grab {
						winit::window::CursorGrabMode::Confined
					} else {
						winit::window::CursorGrabMode::None
					})
				})
				.is_ok();
			if mode_ok {
				r.window.set_cursor_visible(!grab);
				self.cursor_grabbed = grab;
			}
		}
	}
}

impl<G: Game> ApplicationHandler for App<G> {
	fn resumed(&mut self, el: &ActiveEventLoop) {
		match std::mem::replace(&mut self.state, AppState::Uninitialized) {
			AppState::Uninitialized => {
				log::info!("lifecycle: initial launch");
				let window = Arc::new(el.create_window(window_attrs(&self.options.title)).unwrap());
				let (gpu, surface, config) = Self::init_gpu(window.clone(), self.options.no_vsync);
				let depth = create_depth(&gpu.device, config.width, config.height);
				self.game
					.init(&gpu, config.format, (config.width, config.height));
				self.state = AppState::Running(Running {
					gpu: Arc::new(gpu),
					window,
					surface,
					config,
					depth,
				});
				if let AppState::Running(r) = &self.state {
					r.window.request_redraw();
				}
			}
			AppState::Suspended(s) => self.resume_from(s, el),
			running => {
				// e.g. macOS fires Resumed once at startup after we already
				// initialized; idempotent by design.
				self.state = running;
			}
		}
	}

	fn suspended(&mut self, _el: &ActiveEventLoop) {
		self.enter_suspend();
	}

	fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
		match event {
			WindowEvent::RedrawRequested => {
				self.render_frame(el);
				if self.options.auto_drill {
					self.auto_drill(el);
				}
			}
			WindowEvent::Resized(size) => self.reconfigure(size.width, size.height),
			WindowEvent::CloseRequested => {
				self.game.on_suspend();
				el.exit();
			}
			WindowEvent::Focused(false) => {
				self.game.input(InputEvent::FocusLost);
			}
			WindowEvent::MouseInput { state, button, .. } => {
				if self.cursor_grabbed {
					self.game.input(InputEvent::MouseButton {
						button: map_button(button),
						pressed: state.is_pressed(),
					});
				} else if state == ElementState::Pressed {
					// First click (re-)grabs the cursor and is swallowed so
					// it doesn't also dig.
					self.grab_cursor(true);
				}
			}
			WindowEvent::Touch(t) => {
				if let AppState::Running(r) = &self.state {
					let size = r.window.inner_size();
					self.game.input(InputEvent::Touch {
						id: t.id,
						phase: map_touch_phase(t.phase),
						x: (t.location.x / f64::from(size.width.max(1))) as f32,
						y: (t.location.y / f64::from(size.height.max(1))) as f32,
					});
				}
			}
			WindowEvent::KeyboardInput { event: k, .. } => {
				use winit::keyboard::{KeyCode, PhysicalKey};
				let pressed = k.state.is_pressed();
				match k.physical_key {
					// Debug drills + screenshot — desktop only, press only.
					PhysicalKey::Code(KeyCode::F8) if pressed && cfg!(not(target_os = "ios")) => {
						self.screenshot_pending = true;
					}
					PhysicalKey::Code(KeyCode::F9) if pressed && cfg!(not(target_os = "ios")) => {
						match self.state {
							AppState::Running(_) => {
								log::info!("F9 drill: simulating OS suspend");
								self.enter_suspend();
							}
							AppState::Suspended(_) => {
								log::info!("F9 drill: simulating OS resume");
								if let AppState::Suspended(s) =
									std::mem::replace(&mut self.state, AppState::Uninitialized)
								{
									self.resume_from(s, el);
								}
							}
							AppState::Uninitialized => {}
						}
					}
					PhysicalKey::Code(KeyCode::F10) if pressed && cfg!(not(target_os = "ios")) => {
						if let AppState::Running(r) = &self.state {
							log::info!("F10 drill: simulating device loss (renderer rebuild)");
							self.game.rebuild_gpu_resources(&r.gpu, r.config.format);
						}
					}
					PhysicalKey::Code(KeyCode::Escape) if pressed => {
						if self.cursor_grabbed {
							self.grab_cursor(false);
						} else {
							self.game.on_suspend();
							el.exit();
						}
					}
					PhysicalKey::Code(code) => {
						self.game.input(InputEvent::Key {
							key: map_key(code),
							pressed,
						});
					}
					PhysicalKey::Unidentified(_) => {}
				}
			}
			_ => {}
		}
	}

	fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
		if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
			if self.cursor_grabbed && matches!(self.state, AppState::Running(_)) {
				self.game.input(InputEvent::MouseDelta { dx, dy });
			}
		}
	}
}
