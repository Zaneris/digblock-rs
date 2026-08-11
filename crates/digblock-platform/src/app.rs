//! The lifecycle state machine. Every entry into / exit from the
//! live-surface state goes through exactly two functions
//! (`enter_suspend` / `resume_from`), shared by: initial launch, iOS
//! background/foreground, future Android surface destruction, and the F9
//! desktop drill. `Suspended` has no surface field, so rendering without a
//! surface is unrepresentable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

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
	/// Window exists, async adapter/device request in flight (wasm only —
	/// the browser main thread cannot block on GPU init). Completed by
	/// `user_event`.
	#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
	WaitingForGpu {
		window: Arc<Window>,
	},
	Running(Running),
	Suspended(Suspended),
}

/// Posted back into the event loop by the async GPU init on wasm; native
/// init is synchronous and never sends one.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
struct GpuReady {
	gpu: Gpu,
	surface: wgpu::Surface<'static>,
	config: wgpu::SurfaceConfiguration,
	/// Device-loss recovery (rebuild path) vs first init.
	reinit: bool,
}

/// Events posted back into the winit loop from async/browser contexts
/// (wasm only; native never constructs one).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
enum UserEvent {
	/// Async GPU init handing back its result.
	GpuReady(GpuReady),
	/// Browser pointer-lock truth from the pointerlockchange listener.
	PointerLock { locked: bool },
}

struct App<G: Game> {
	state: AppState,
	/// Route back into the event loop from async contexts (wasm GPU init;
	/// unused native — init is synchronous there).
	#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
	proxy: winit::event_loop::EventLoopProxy<UserEvent>,
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
	/// Last frame's `wants_cursor_grab`, for edge-triggered reconcile:
	/// starting at `true` means a game that never opens a menu produces no
	/// edge, keeping the classic click-to-grab behavior bit-identical.
	prev_wants_grab: bool,
	frames_rendered: u64,
	/// Read only by the native screenshot path.
	#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
	screenshot_pending: bool,
	launched_at: Instant,
	/// Set only by the drill's final step: the app rendered on the
	/// recovered device and exited cleanly. `run` turns its absence into
	/// a hard error so `--drill` cannot green-light a broken recovery.
	/// (Read post-loop — native only; the web loop never returns.)
	#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
	drill_ok: bool,
	/// Next drill step to fire (see `auto_drill`).
	drill_step: u8,
}

/// Watchdog for a stalled drill: without it a drill that stops
/// requesting redraws sits in `ControlFlow::Wait` forever and would look
/// hung instead of failed.
const DRILL_WATCHDOG: Duration = Duration::from_secs(60);

pub fn run(game: impl Game, options: RunOptions) -> Result<(), crate::RunError> {
	let event_loop = EventLoop::<UserEvent>::with_user_event()
		.build()
		.expect("event loop");
	event_loop.set_control_flow(ControlFlow::Wait);
	let auto_drill = options.auto_drill;
	let app = App {
		state: AppState::Uninitialized,
		proxy: event_loop.create_proxy(),
		lifecycle: Lifecycle::new(),
		device_lost: Arc::new(AtomicBool::new(false)),
		game,
		clock: GameClock::new(),
		options,
		cursor_grabbed: false,
		prev_wants_grab: true,
		frames_rendered: 0,
		screenshot_pending: false,
		launched_at: Instant::now(),
		drill_ok: false,
		drill_step: 0,
	};
	#[cfg(not(target_arch = "wasm32"))]
	{
		let mut app = app;
		event_loop.run_app(&mut app).expect("event loop run");
		if auto_drill && !app.drill_ok {
			return Err(crate::RunError::DrillIncomplete {
				frames_rendered: app.frames_rendered,
			});
		}
		Ok(())
	}
	#[cfg(target_arch = "wasm32")]
	{
		// The browser event loop never returns to the caller: spawn_app
		// hands control to the JS runtime and this Ok is immediate. The
		// drill has no wasm mode, so the post-loop check has no analogue.
		let _ = auto_drill;
		use winit::platform::web::EventLoopExtWebSys;
		event_loop.spawn_app(app);
		Ok(())
	}
}

fn window_attrs(title: &str, size: Option<(u32, u32)>) -> winit::window::WindowAttributes {
	let attrs = Window::default_attributes().with_title(title.to_owned());
	// LOGICAL, so a retina display doubles it to the same physical size a
	// browser canvas reports at devicePixelRatio 2 — which is the whole
	// point of being able to ask for one.
	let attrs = match size {
		Some((w, h)) => attrs.with_inner_size(winit::dpi::LogicalSize::new(w, h)),
		None => attrs,
	};
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
	#[cfg(target_arch = "wasm32")]
	let attrs = {
		use wasm_bindgen::JsCast;
		use winit::platform::web::WindowAttributesExtWebSys;
		// Fixed id contract with the apps/web page; a missing canvas is a
		// packaging bug, not a runtime condition.
		let canvas = web_sys::window()
			.and_then(|w| w.document())
			.and_then(|d| d.get_element_by_id("digblock-canvas"))
			.and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
			.expect("canvas #digblock-canvas missing");
		attrs.with_canvas(Some(canvas)).with_prevent_default(true)
	};
	attrs
}

/// Adapter + device + configured surface. Async because the browser main
/// thread cannot block; native callers wrap it in `pollster::block_on`
/// (macOS requires window/surface creation on the main thread anyway).
async fn init_gpu(
	window: Arc<Window>,
	no_vsync: bool,
) -> (Gpu, wgpu::Surface<'static>, wgpu::SurfaceConfiguration) {
	let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
	let surface = instance
		.create_surface(window.clone())
		.expect("create surface");
	let adapter = instance
		.request_adapter(&wgpu::RequestAdapterOptions {
			power_preference: wgpu::PowerPreference::HighPerformance,
			compatible_surface: Some(&surface),
			force_fallback_adapter: false,
		})
		.await
		.expect("no compatible GPU adapter");
	log::info!("adapter: {:?}", adapter.get_info());
	// GPU pass timing (debug/metrics facility) when the adapter has it.
	let required_features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
	log::info!(
		"gpu timestamps: {}",
		required_features.contains(wgpu::Features::TIMESTAMP_QUERY)
	);
	let (device, queue) = adapter
		.request_device(&wgpu::DeviceDescriptor {
			label: Some("digblock"),
			required_features,
			..Default::default()
		})
		.await
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
		// COPY_SRC exists purely for native screenshots; browser canvas
		// textures don't reliably support it.
		usage: if cfg!(target_arch = "wasm32") {
			wgpu::TextureUsages::RENDER_ATTACHMENT
		} else {
			wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
		},
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
				match std::mem::replace(&mut self.state, AppState::Uninitialized) {
					AppState::Suspended(s) => self.resume_from(s, el),
					other => {
						// The FSM only emits this from Suspended; never
						// drop live resources if that invariant breaks.
						log::error!("RecreateSurface outside Suspended — ignoring");
						self.state = other;
					}
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
			// An in-flight init's result will be dropped by user_event
			// (state is no longer WaitingForGpu); start recovery fresh.
			AppState::WaitingForGpu { window } => Some(window),
			AppState::Uninitialized => None,
		};
		let Some(window) = window else {
			log::error!("device lost before initialization — exiting");
			el.exit();
			return;
		};
		log::warn!("lifecycle: device lost — recovering with a fresh adapter/device");
		self.device_lost.store(false, Ordering::Release);
		#[cfg(not(target_arch = "wasm32"))]
		{
			let (gpu, surface, config) =
				pollster::block_on(init_gpu(window.clone(), self.options.no_vsync));
			self.finish_reinit(window, gpu, surface, config);
		}
		#[cfg(target_arch = "wasm32")]
		self.spawn_gpu_init(window, true);
	}

	fn init_all(&mut self, el: &ActiveEventLoop) {
		log::info!("lifecycle: initial launch");
		let window = Arc::new(
			el.create_window(window_attrs(&self.options.title, self.options.window_size))
				.unwrap(),
		);
		#[cfg(not(target_arch = "wasm32"))]
		{
			let (gpu, surface, config) =
				pollster::block_on(init_gpu(window.clone(), self.options.no_vsync));
			self.finish_init(window, gpu, surface, config, el);
		}
		#[cfg(target_arch = "wasm32")]
		{
			let _ = el;
			// Once per launch (reinit_device bypasses init_all, so no
			// double install).
			install_pointer_lock_listeners(&self.proxy);
			self.spawn_gpu_init(window, false);
		}
	}

	/// Kick the async adapter/device request; `user_event` completes it.
	#[cfg(target_arch = "wasm32")]
	fn spawn_gpu_init(&mut self, window: Arc<Window>, reinit: bool) {
		let proxy = self.proxy.clone();
		let no_vsync = self.options.no_vsync;
		let w = window.clone();
		wasm_bindgen_futures::spawn_local(async move {
			let (gpu, surface, config) = init_gpu(w, no_vsync).await;
			// Send fails only if the loop is gone — nothing to do then.
			let _ = proxy.send_event(UserEvent::GpuReady(GpuReady {
				gpu,
				surface,
				config,
				reinit,
			}));
		});
		self.state = AppState::WaitingForGpu { window };
	}

	/// Shared post-GPU tail of the first init: native `init_all` calls it
	/// synchronously, the wasm path from `user_event`.
	fn finish_init(
		&mut self,
		window: Arc<Window>,
		gpu: Gpu,
		surface: wgpu::Surface<'static>,
		config: wgpu::SurfaceConfiguration,
		el: &ActiveEventLoop,
	) {
		Self::register_device_callbacks(&gpu.device, &self.device_lost);
		self.game.set_scale_factor(window.scale_factor() as f32);
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

	/// Shared post-GPU tail of device-loss recovery (rebuild, not init;
	/// no SurfaceFormatPicked — the decision core already ordered this).
	fn finish_reinit(
		&mut self,
		window: Arc<Window>,
		gpu: Gpu,
		surface: wgpu::Surface<'static>,
		config: wgpu::SurfaceConfiguration,
	) {
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
			Arc::new(
				el.create_window(window_attrs(&self.options.title, self.options.window_size))
					.unwrap(),
			)
		});
		let (surface, config) = create_surface(&s.gpu, window.clone(), self.options.no_vsync);
		// Engine pipelines/buffers are untouched — the Device survived.
		// Only notify about a possible size/scale change while backgrounded.
		self.game.set_scale_factor(window.scale_factor() as f32);
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
		// Opens the occupancy span; the game closes it inside `render`, so
		// every scrap of main-thread work lands inside it — not just the
		// phases that happen to be timed individually.
		self.game.frame_started();

		// Cursor-grab reconcile (edge-triggered, one frame behind update —
		// imperceptible): menu opening releases the cursor; menu closing
		// re-grabs it. Level-holding at `true` never grabs on its own —
		// that stays with the click-to-grab path. On the web the rising
		// edge is suppressed: a lock request from rAF context carries no
		// user gesture and Chrome rejects it — re-grabs happen only from
		// the gesture paths (clicks, keydowns).
		let wants = self.game.wants_cursor_grab();
		if self.cursor_grabbed && !wants {
			self.grab_cursor(false);
		} else if !self.cursor_grabbed
			&& wants && !self.prev_wants_grab
			&& cfg!(not(target_arch = "wasm32"))
		{
			self.grab_cursor(true);
		}
		self.prev_wants_grab = wants;

		let AppState::Running(r) = &mut self.state else {
			return;
		};

		let t_acquire = web_time::Instant::now();
		let frame = match r.surface.get_current_texture() {
			Ok(f) => f,
			Err(e) => {
				let ev = match e {
					wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => LcEvent::SurfaceLost,
					wgpu::SurfaceError::Timeout => LcEvent::SurfaceTimeout,
					other => {
						// wgpu never fires the lost callback from the loss
						// itself: it fires from device maintenance once the
						// queue is EMPTY. A dead device lands here as a
						// generic acquire error first, usually with the last
						// frame's work still in flight — pump maintenance,
						// give the queue a bounded moment to drain (error
						// path; a stall is already certain), and re-check
						// the flag before declaring the error fatal.
						// PollType::Wait is deliberately avoided: it can
						// block 60s on a wedged queue.
						#[cfg(not(target_arch = "wasm32"))]
						let lost = {
							let mut lost = false;
							for _ in 0..5 {
								let _ = r.gpu.device.poll(wgpu::PollType::Poll);
								if self.device_lost.swap(false, Ordering::Acquire) {
									lost = true;
									break;
								}
								std::thread::sleep(Duration::from_millis(5));
							}
							lost
						};
						// No blocking waits on the web: one maintenance pump,
						// one flag check.
						#[cfg(target_arch = "wasm32")]
						let lost = {
							let _ = r.gpu.device.poll(wgpu::PollType::Poll);
							self.device_lost.swap(false, Ordering::Acquire)
						};
						if lost {
							self.dispatch(LcEvent::DeviceLost, el);
							return;
						}
						log::error!("surface acquire failed, no device loss reported: {other}");
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
		// Screenshots are native-only: capture needs a blocking map wait
		// and a filesystem, neither of which exists on the web.
		#[cfg(not(target_arch = "wasm32"))]
		{
			let want_shot = self.screenshot_pending
				|| (self.options.screenshot.is_some()
					&& self.frames_rendered
						== u64::from(self.options.screenshot_after_frames.max(1)));
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
		}

		frame.present();
		// First frame is on screen: tell the page to drop its loading
		// overlay (fixed contract with apps/web's index.html, like the
		// canvas id).
		#[cfg(target_arch = "wasm32")]
		if self.frames_rendered == 1 {
			if let (Some(w), Ok(ev)) = (
				web_sys::window(),
				web_sys::Event::new("digblock-first-frame"),
			) {
				let _ = w.dispatch_event(&ev);
			}
		}
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
	/// same transitions the OS lifecycle events use. Steps advance on a
	/// counter, never on exact frame equality: failed frames don't
	/// increment `frames_rendered`, so an `== 60` match would re-destroy
	/// the freshly recovered device every retry frame.
	fn auto_drill(&mut self, el: &ActiveEventLoop) {
		match (self.drill_step, self.frames_rendered) {
			(0, 20..) => {
				log::info!("auto-drill: suspend + immediate resume");
				self.drill_step = 1;
				self.dispatch(LcEvent::ToggleDrillSuspend, el);
				self.dispatch(LcEvent::ToggleDrillSuspend, el);
			}
			(1, 40..) => {
				log::info!("auto-drill: device-loss rebuild");
				self.drill_step = 2;
				self.dispatch(LcEvent::DrillDeviceRebuild, el);
			}
			(2, 60..) => {
				// REAL device loss. destroy() does NOT fire the lost
				// callback — it only invalidates the device; the closure
				// fires from device maintenance (the poll in the acquire-
				// error arm pumps it) and ReinitDevice recovers. Reaching
				// frame 100 means the app rendered on the recovered device.
				log::info!("auto-drill: destroying the device (real loss)");
				self.drill_step = 3;
				if let AppState::Running(r) = &self.state {
					r.gpu.device.destroy();
				}
			}
			(3, 100..) => {
				log::info!("auto-drill: complete, exiting");
				self.drill_step = 4;
				self.drill_ok = true;
				el.exit();
			}
			_ => {}
		}
	}

	fn grab_cursor(&mut self, grab: bool) {
		if let AppState::Running(r) = &self.state {
			#[cfg(target_arch = "wasm32")]
			{
				if grab {
					// Async request: winit's Ok does not mean the lock
					// happened — only the pointerlockchange listener flips
					// `cursor_grabbed` and hides the cursor.
					let _ = r
						.window
						.set_cursor_grab(winit::window::CursorGrabMode::Locked);
				} else {
					// exitPointerLock never fails: apply immediately; the
					// echoing pointerlockchange dedupes in the handler.
					let _ = r
						.window
						.set_cursor_grab(winit::window::CursorGrabMode::None);
					r.window.set_cursor_visible(true);
					self.cursor_grabbed = false;
				}
			}
			#[cfg(not(target_arch = "wasm32"))]
			{
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

	/// Apply browser pointer-lock truth (wasm pointerlockchange). Our own
	/// exit calls flip `cursor_grabbed` synchronously, so the echoing event
	/// arrives with `locked == cursor_grabbed` and is a no-op.
	fn on_pointer_lock_change(&mut self, locked: bool) {
		if locked == self.cursor_grabbed {
			return;
		}
		self.cursor_grabbed = locked;
		if let AppState::Running(r) = &self.state {
			r.window.set_cursor_visible(!locked);
			if !locked {
				// Keep winit's internal grab intent in sync with the browser
				// so it doesn't re-request a stale Locked mode on its own.
				let _ = r
					.window
					.set_cursor_grab(winit::window::CursorGrabMode::None);
			}
		}
		if !locked {
			// Browser-initiated unlock (Escape, tab switch): clear held keys
			// and let the engine pause into its menu.
			self.game.input(InputEvent::PointerLockLost);
		}
	}

	/// Re-grab from inside DOM event dispatch (a genuine user gesture, the
	/// only context Chrome honors) when this input just closed the menu.
	#[cfg(target_arch = "wasm32")]
	fn gesture_regrab(&mut self, wants_before: bool) {
		if !wants_before && self.game.wants_cursor_grab() {
			self.grab_cursor(true);
		}
	}
}

/// Browser truth for pointer lock, routed into the winit loop as user
/// events. Page-lifetime listeners: leaking the closures is deliberate.
#[cfg(target_arch = "wasm32")]
fn install_pointer_lock_listeners(proxy: &winit::event_loop::EventLoopProxy<UserEvent>) {
	use wasm_bindgen::closure::Closure;
	use wasm_bindgen::JsCast;
	let document = web_sys::window()
		.and_then(|w| w.document())
		.expect("document");
	let doc = document.clone();
	let p = proxy.clone();
	let on_change = Closure::<dyn FnMut()>::new(move || {
		// Fixed id contract with the apps/web page (same as window_attrs).
		let locked = doc
			.pointer_lock_element()
			.is_some_and(|e| e.id() == "digblock-canvas");
		let _ = p.send_event(UserEvent::PointerLock { locked });
	});
	document
		.add_event_listener_with_callback("pointerlockchange", on_change.as_ref().unchecked_ref())
		.expect("pointerlockchange listener");
	on_change.forget();
	let on_error = Closure::<dyn FnMut()>::new(move || {
		// Expected after Escape (Chrome's relock cooldown) or gesture-less
		// requests; state stays truthful via pointerlockchange — log only.
		log::warn!("pointer lock request rejected by the browser");
	});
	document
		.add_event_listener_with_callback("pointerlockerror", on_error.as_ref().unchecked_ref())
		.expect("pointerlockerror listener");
	on_error.forget();
}

impl<G: Game> ApplicationHandler<UserEvent> for App<G> {
	fn resumed(&mut self, el: &ActiveEventLoop) {
		// The decision core owns idempotence (macOS fires Resumed once at
		// startup after init) and the deferred device-loss recovery.
		self.dispatch(LcEvent::OsResumed, el);
	}

	/// Wasm-only arrivals: async GPU init results and pointer-lock truth
	/// (never fires native).
	fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
		match ev {
			UserEvent::GpuReady(ev) => {
				match std::mem::replace(&mut self.state, AppState::Uninitialized) {
					AppState::WaitingForGpu { window } => {
						if ev.reinit {
							self.finish_reinit(window, ev.gpu, ev.surface, ev.config);
						} else {
							self.finish_init(window, ev.gpu, ev.surface, ev.config, el);
						}
					}
					other => {
						log::error!("GpuReady outside WaitingForGpu — dropping");
						self.state = other;
					}
				}
			}
			UserEvent::PointerLock { locked } => self.on_pointer_lock_change(locked),
		}
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
			// A Resized with the new physical size follows when the size
			// actually changed; UI scale is recomputed per frame, so no
			// reconfigure is needed here.
			WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
				self.game.set_scale_factor(scale_factor as f32);
			}
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
				} else if self.game.wants_cursor_grab() {
					// First click (re-)grabs the cursor and is swallowed so
					// it doesn't also dig.
					if state == ElementState::Pressed {
						self.grab_cursor(true);
					}
				} else {
					// Menu open: UI click, paired with the last CursorMoved.
					self.game.input(InputEvent::MouseButton {
						button: map_button(button),
						pressed: state.is_pressed(),
					});
					// A click that just closed the menu (e.g. "Back to
					// Game") re-locks from inside its own gesture.
					#[cfg(target_arch = "wasm32")]
					self.gesture_regrab(false);
				}
			}
			WindowEvent::CursorMoved { position, .. } => {
				// Only meaningful ungrabbed — while locked the position is
				// frozen and gameplay look comes from MouseDelta.
				if !self.cursor_grabbed {
					self.game.input(InputEvent::CursorMoved {
						x: position.x as f32,
						y: position.y as f32,
					});
				}
			}
			WindowEvent::MouseWheel { delta, .. } => {
				// Gameplay-only (hotbar selection), gated like MouseInput; the
				// menu has no scroll region today.
				if self.cursor_grabbed {
					use winit::event::MouseScrollDelta;
					let dy = match delta {
						MouseScrollDelta::LineDelta(_, y) => y,
						MouseScrollDelta::PixelDelta(p) => (p.y / 50.0) as f32,
					};
					if dy != 0.0 {
						self.game.input(InputEvent::Scroll { dy });
					}
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
					// (F9/F10 stay live on the web — surface recreate and
					// device rebuild are real paths there; F8 needs the
					// native-only capture module.)
					PhysicalKey::Code(KeyCode::F8)
						if pressed
							&& cfg!(all(not(target_os = "ios"), not(target_arch = "wasm32"))) =>
					{
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
					// Escape reaches the game like any key: the menu owns the
					// policy (open/back out). Quitting is window close/Cmd-Q.
					PhysicalKey::Code(code) => {
						#[cfg(target_arch = "wasm32")]
						let wants_before = self.game.wants_cursor_grab();
						self.game.input(InputEvent::Key {
							key: map_key(code),
							pressed,
						});
						// Produced text for menu fields while the cursor is
						// released (a menu is up), or for an in-game text
						// field (chat) that keeps the grab — otherwise
						// gameplay keys must not double as text.
						if pressed
							&& (!self.game.wants_cursor_grab() || self.game.wants_text_input())
						{
							if let Some(text) = &k.text {
								for ch in text.chars().filter(|c| !c.is_control()) {
									self.game.input(InputEvent::Text { ch });
								}
							}
						}
						// A key that just closed the menu (Enter, hotkeys)
						// re-locks from inside its own gesture — except
						// Escape: Chrome excludes it from user activation,
						// so the lock request fails (or is granted and
						// instantly revoked), and the resulting unlock
						// event would bounce the player straight into the
						// pause menu. Leave the cursor free instead; the
						// first click re-grabs and is swallowed.
						#[cfg(target_arch = "wasm32")]
						if code != KeyCode::Escape {
							self.gesture_regrab(wants_before);
						}
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

	fn about_to_wait(&mut self, el: &ActiveEventLoop) {
		// Drill watchdog: a drill that stops requesting redraws would
		// otherwise park in ControlFlow::Wait forever. Keep the loop
		// waking; abort (drill_ok stays false ⇒ non-zero exit) on expiry.
		if self.options.auto_drill && !self.drill_ok {
			if self.launched_at.elapsed() > DRILL_WATCHDOG {
				log::error!(
					"auto-drill: watchdog expired after {}s at frame {} — aborting",
					DRILL_WATCHDOG.as_secs(),
					self.frames_rendered
				);
				el.exit();
			} else {
				// WaitUntil takes a std Instant, which has no wasm clock —
				// but the drill never runs on the web.
				#[cfg(not(target_arch = "wasm32"))]
				el.set_control_flow(ControlFlow::WaitUntil(
					Instant::now() + Duration::from_secs(1),
				));
			}
		}
	}
}
