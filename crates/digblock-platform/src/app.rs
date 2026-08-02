//! The lifecycle state machine. Every entry into / exit from the
//! live-surface state goes through exactly two functions
//! (`enter_suspend` / `resume_from`), shared by: initial launch, iOS
//! background/foreground, future Android surface destruction, and the F9
//! desktop drill. `Suspended` has no surface field, so rendering without a
//! surface is unrepresentable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::clock::GameClock;
use crate::input::{map_button, map_key, map_touch_phase, InputEvent};
use crate::lifecycle::{Action, Event as LcEvent, Lifecycle};
use crate::{Game, Gpu, RunOptions};

struct Running {
	gpu: Arc<Gpu>,
	window: Arc<Window>,
	surface: wgpu::Surface<'static>,
	config: wgpu::SurfaceConfiguration,
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
	/// The pure decision core (lifecycle.rs); `state` holds the resources
	/// its Phase mirrors. All transitions route through `dispatch`.
	lifecycle: Lifecycle,
	/// Set by the wgpu device-lost callback (may fire off-thread — never
	/// touch winit from it); polled at the top of each frame.
	device_lost: Arc<AtomicBool>,
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
		lifecycle: Lifecycle::new(),
		device_lost: Arc::new(AtomicBool::new(false)),
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

impl<G: Game> App<G> {
	/// Register the loss/error hooks on a fresh device. The callbacks may
	/// fire on any thread: they only flip the shared flag and log.
	fn register_device_callbacks(device: &wgpu::Device, lost: &Arc<AtomicBool>) {
		let flag = lost.clone();
		device.set_device_lost_callback(Box::new(move |reason, message| {
			log::error!("device lost ({reason:?}): {message}");
			flag.store(true, Ordering::Release);
		}));
		let flag = lost.clone();
		device.on_uncaptured_error(Box::new(move |e| {
			log::error!("uncaptured wgpu error: {e}");
			if matches!(e, wgpu::Error::OutOfMemory { .. }) {
				// OOM devices are lost in all but name.
				flag.store(true, Ordering::Release);
			}
		}));
	}

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

	/// Route an event through the decision core and execute its actions.
	fn dispatch(&mut self, ev: LcEvent, el: &ActiveEventLoop) {
		for action in self.lifecycle.on(ev) {
			self.perform(action, el);
		}
	}

	fn perform(&mut self, action: Action, el: &ActiveEventLoop) {
		match action {
			Action::InitAll => self.init_all(el),
			Action::SuspendGame => self.enter_suspend(),
			Action::RecreateSurface => {
				if let AppState::Suspended(s) =
					std::mem::replace(&mut self.state, AppState::Uninitialized)
				{
					self.resume_from(s, el);
				}
			}
			Action::RebuildRenderer => {
				if let AppState::Running(r) = &self.state {
					log::info!("lifecycle: renderer rebuild (format {:?})", r.config.format);
					self.game.rebuild_gpu_resources(&r.gpu, r.config.format);
					self.lifecycle.note_rebuilt(r.config.format);
				}
			}
			Action::ReinitDevice => self.reinit_device(el),
			Action::ReconfigureSurface => {
				if let AppState::Running(r) = &mut self.state {
					log::warn!("surface lost/outdated — reconfiguring");
					r.surface.configure(&r.gpu.device, &r.config);
					r.window.request_redraw();
				}
			}
			Action::RetryNextFrame => {
				if let AppState::Running(r) = &self.state {
					r.window.request_redraw();
				}
			}
			Action::Exit => el.exit(),
		}
	}

	/// Full device-loss recovery: a NEW adapter/device/surface on the
	/// surviving window, then the existing rebuild path re-uploads
	/// everything. Nothing else holds a Gpu clone (the engine borrows
	/// per call, by design), so swapping the Arc is safe.
	fn reinit_device(&mut self, el: &ActiveEventLoop) {
		let window = match std::mem::replace(&mut self.state, AppState::Uninitialized) {
			AppState::Running(r) => Some(r.window), // surface + old gpu drop
			AppState::Suspended(s) => s.window,
			AppState::Uninitialized => None,
		};
		let Some(window) = window else {
			log::error!("device lost before initialization — exiting");
			el.exit();
			return;
		};
		log::warn!("lifecycle: device lost — recovering with a fresh adapter/device");
		self.device_lost.store(false, Ordering::Release);
		let (gpu, surface, config) = Self::init_gpu(window.clone(), self.options.no_vsync);
		Self::register_device_callbacks(&gpu.device, &self.device_lost);
		self.game.rebuild_gpu_resources(&gpu, config.format);
		self.game.resize(&gpu, (config.width, config.height));
		self.lifecycle.note_rebuilt(config.format);
		self.state = AppState::Running(Running {
			gpu: Arc::new(gpu),
			window,
			surface,
			config,
		});
		if let AppState::Running(r) = &self.state {
			r.window.request_redraw();
		}
	}

	fn init_all(&mut self, el: &ActiveEventLoop) {
		log::info!("lifecycle: initial launch");
		let window = Arc::new(el.create_window(window_attrs(&self.options.title)).unwrap());
		let (gpu, surface, config) = Self::init_gpu(window.clone(), self.options.no_vsync);
		Self::register_device_callbacks(&gpu.device, &self.device_lost);
		self.game
			.init(&gpu, config.format, (config.width, config.height));
		let format = config.format;
		self.state = AppState::Running(Running {
			gpu: Arc::new(gpu),
			window,
			surface,
			config,
		});
		self.dispatch(LcEvent::SurfaceFormatPicked(format), el);
		if let AppState::Running(r) = &self.state {
			r.window.request_redraw();
		}
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
		// Engine pipelines/buffers are untouched — the Device survived.
		// Only notify about a possible size change while backgrounded.
		self.game.resize(&s.gpu, (config.width, config.height));
		self.game.on_resume();
		let format = config.format;
		self.state = AppState::Running(Running {
			gpu: s.gpu,
			window,
			surface,
			config,
		});
		// A resume can pick a different surface format (monitor/GPU swap
		// while backgrounded); the decision core orders a rebuild then.
		self.dispatch(LcEvent::SurfaceFormatPicked(format), el);
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
			self.game.resize(&r.gpu, (width, height));
		}
	}

	fn render_frame(&mut self, el: &ActiveEventLoop) {
		// The loss callback fires off-thread; recover on the main thread
		// before touching the (dead) surface.
		if self.device_lost.swap(false, Ordering::Acquire) {
			self.dispatch(LcEvent::DeviceLost, el);
			return;
		}
		let AppState::Running(r) = &mut self.state else {
			return;
		};

		let t_acquire = std::time::Instant::now();
		let frame = match r.surface.get_current_texture() {
			Ok(f) => f,
			Err(e) => {
				let ev = match e {
					wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => LcEvent::SurfaceLost,
					wgpu::SurfaceError::Timeout => LcEvent::SurfaceTimeout,
					other => {
						log::error!("unrecoverable surface error: {other}");
						LcEvent::SurfaceFatal
					}
				};
				self.dispatch(ev, el);
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
			.render(&r.gpu, &view, (r.config.width, r.config.height));

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
				self.dispatch(LcEvent::ToggleDrillSuspend, el);
				self.dispatch(LcEvent::ToggleDrillSuspend, el);
			}
			40 => {
				log::info!("auto-drill: device-loss rebuild");
				self.dispatch(LcEvent::DrillDeviceRebuild, el);
			}
			60 => {
				// REAL device loss: destroy() guarantees the lost callback
				// fires; the poll at the top of render_frame then drives
				// the full ReinitDevice recovery. Reaching frame 100 means
				// the app rendered on the recovered device.
				log::info!("auto-drill: destroying the device (real loss)");
				if let AppState::Running(r) = &self.state {
					r.gpu.device.destroy();
				}
			}
			100 => {
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
		// The decision core owns idempotence (macOS fires Resumed once at
		// startup after init) and the deferred device-loss recovery.
		self.dispatch(LcEvent::OsResumed, el);
	}

	fn suspended(&mut self, el: &ActiveEventLoop) {
		self.dispatch(LcEvent::OsSuspended, el);
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
						log::info!("F9 drill: toggling suspend/resume");
						self.dispatch(LcEvent::ToggleDrillSuspend, el);
					}
					PhysicalKey::Code(KeyCode::F10) if pressed && cfg!(not(target_os = "ios")) => {
						log::info!("F10 drill: simulating device loss (renderer rebuild)");
						self.dispatch(LcEvent::DrillDeviceRebuild, el);
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
