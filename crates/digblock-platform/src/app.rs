//! The lifecycle state machine. Every entry into / exit from the
//! live-surface state goes through exactly two functions
//! (`enter_suspend` / `resume_from`), shared by: initial launch, iOS
//! background/foreground, future Android surface destruction, and the F9
//! desktop drill. `Suspended` has no surface field, so rendering without a
//! surface is unrepresentable.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use web_time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::clock::GameClock;
use crate::input::{InputEvent, map_button, map_key, map_touch_phase};
use crate::lifecycle::{Action, Event as LcEvent, Lifecycle};
use crate::{Game, Gpu, RunOptions};

struct Running {
	gpu: Arc<Gpu>,
	window: Arc<Window>,
	surface: wgpu::Surface<'static>,
	config: wgpu::SurfaceConfiguration,
	/// `Some` under `RunOptions::offscreen`: the frame target the engine
	/// draws into when the swapchain is deliberately left out. Sized and
	/// formatted exactly like the surface would be, so a capture taken
	/// here is the same pixels a windowed one would produce.
	offscreen: Option<Offscreen>,
}

/// A frame target off the swapchain. `COPY_SRC` because the whole point
/// of drawing here is to read it back.
struct Offscreen {
	texture: wgpu::Texture,
	view: wgpu::TextureView,
}

impl Offscreen {
	fn new(gpu: &Gpu, config: &wgpu::SurfaceConfiguration) -> Self {
		let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("offscreen frame"),
			size: wgpu::Extent3d {
				width: config.width.max(1),
				height: config.height.max(1),
				depth_or_array_layers: 1,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: config.format,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
			view_formats: &[],
		});
		let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
		Self { texture, view }
	}
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
	/// Window exists, async GPU init in flight (wasm only); completed by `user_event`.
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
pub(crate) struct GpuReady {
	gpu: Gpu,
	surface: wgpu::Surface<'static>,
	config: wgpu::SurfaceConfiguration,
	/// Device-loss recovery (rebuild path) vs first init.
	reinit: bool,
}

/// Events posted back into the winit loop from async/browser contexts (wasm only; native never
/// constructs one). One `GpuReady` is posted per GPU init — twice in a long session, if a device is
/// lost — so the variant size spread costs nothing, the same reason [`AppState`] carries the allow.
#[allow(clippy::large_enum_variant)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) enum UserEvent {
	/// Async GPU init handing back its result.
	GpuReady(GpuReady),
	/// Browser pointer-lock truth from the pointerlockchange listener.
	PointerLock { locked: bool },
	/// The soft-keyboard bridge's whole DOM value after an edit — a restatement, not a delta.
	#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
	TextValue(String),
	/// A named key from the soft-keyboard bridge, delivered as a press+release pair.
	#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
	TextKey(crate::input::Key),
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
	/// When the next frame is due under `RunOptions::fps_cap`.
	next_frame_due: Option<Instant>,
	/// The goodbye has run. There are three doors out — the window's
	/// close button, a capture exhausting its frames, and the event
	/// loop's own `exiting` — and the first two are followed by the
	/// third, so without this the game would be told twice.
	shut_down: bool,
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
	/// The soft-keyboard bridge (webtext.rs). `None` only if the DOM
	/// refused its setup.
	#[cfg(target_arch = "wasm32")]
	text_bridge: Option<crate::webtext::TextBridge>,
}

/// Watchdog for a stalled drill: without it a drill that stops
/// requesting redraws sits in `ControlFlow::Wait` forever and would look
/// hung instead of failed.
const DRILL_WATCHDOG: Duration = Duration::from_secs(60);

/// How long to idle before asking a fully-covered window for a texture
/// again. Roughly a frame: long enough that a window nobody can see costs
/// nothing, short enough that uncovering one is indistinguishable from
/// never having covered it.
#[cfg(not(target_arch = "wasm32"))]
const OCCLUDED_RETRY: Duration = Duration::from_millis(16);

/// The one place an `Arc<Gpu>` is minted. On the web wgpu's handles are JS objects carrying neither
/// `Send` nor `Sync`, which clippy reads as an `Rc` spelled wrong. It isn't: the same `Arc<Gpu>`
/// type serves every platform — the engine borrows it, and `Running`/`Suspended` share it across a
/// suspend — and forking it per target would fork both states with it. Nothing sends one anywhere;
/// the browser's chunk workers take bytes.
#[allow(clippy::arc_with_non_send_sync)]
fn share_gpu(gpu: Gpu) -> Arc<Gpu> {
	Arc::new(gpu)
}

pub(crate) fn run(game: impl Game, options: RunOptions) -> Result<(), crate::RunError> {
	let event_loop = EventLoop::<UserEvent>::with_user_event()
		.build()
		.expect("event loop");
	event_loop.set_control_flow(ControlFlow::Wait);
	// winit's web backend drops device events (mouse look) whenever the
	// canvas lacks DOM focus, and the text bridge holds that focus for the
	// whole of a chat line. Pointer lock already bounds where motion comes
	// from, so the focus gate buys nothing here.
	#[cfg(target_arch = "wasm32")]
	event_loop.listen_device_events(winit::event_loop::DeviceEvents::Always);
	let auto_drill = options.auto_drill;
	let app = App {
		state: AppState::Uninitialized,
		proxy: event_loop.create_proxy(),
		lifecycle: Lifecycle::new(),
		device_lost: Arc::new(AtomicBool::new(false)),
		game,
		clock: match options.fixed_dt {
			Some(step) => GameClock::with_fixed_step(step),
			None => GameClock::new(),
		},
		options,
		cursor_grabbed: false,
		prev_wants_grab: true,
		frames_rendered: 0,
		next_frame_due: None,
		shut_down: false,
		screenshot_pending: false,
		launched_at: Instant::now(),
		drill_ok: false,
		drill_step: 0,
		#[cfg(target_arch = "wasm32")]
		text_bridge: None,
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

fn window_attrs(
	title: &str,
	size: Option<(u32, u32)>,
	display: Option<usize>,
	fullscreen: bool,
	_el: &ActiveEventLoop,
) -> winit::window::WindowAttributes {
	let attrs = Window::default_attributes().with_title(title.to_owned());
	// LOGICAL, so a retina display scales it to the same physical size a
	// browser canvas reports at the matching devicePixelRatio — which is
	// the whole point of being able to ask for one.
	let attrs = match size {
		Some((w, h)) => attrs.with_inner_size(winit::dpi::LogicalSize::new(w, h)),
		None => attrs,
	};
	// Pin the window to one monitor. Without it the window opens wherever the desktop last had
	// focus, so a capture run silently changes BACKING SCALE and REFRESH RATE between runs, and an
	// interleaved before/after bracket taken across two screens measures the screens. Positioned
	// at the monitor's own origin rather than centred, because the point is repeatability: winit's
	// coordinates are global desktop physical pixels, so `position()` is that screen's corner.
	#[cfg(not(any(target_arch = "wasm32", target_os = "ios")))]
	let attrs = match display {
		None => attrs,
		Some(i) => {
			let mons: Vec<_> = _el.available_monitors().collect();
			match mons.get(i) {
				Some(m) => {
					let p = m.position();
					let hz = m.refresh_rate_millihertz().unwrap_or(0) as f32 / 1000.0;
					log::info!(
						"display {i}: {:?} {}x{} @{hz:.2}Hz scale {:.2} origin {},{}",
						m.name().unwrap_or_default(),
						m.size().width,
						m.size().height,
						m.scale_factor(),
						p.x,
						p.y,
					);
					attrs.with_position(winit::dpi::PhysicalPosition::new(p.x, p.y))
				}
				None => {
					// Fatal, like every other bad capture flag: a run that
					// silently fell back to "wherever" is exactly the
					// non-repeatable measurement this exists to prevent.
					log::error!(
						"--display {i}: only {} monitor(s) — {:?}",
						mons.len(),
						mons.iter().map(|m| m.name()).collect::<Vec<_>>()
					);
					std::process::exit(2);
				}
			}
		}
	};
	// Borderless fullscreen on the pinned monitor (or the current one). Borderless rather than
	// exclusive: the display keeps its own mode and refresh rate, which is what the bench wants
	// to measure against.
	#[cfg(not(any(target_arch = "wasm32", target_os = "ios")))]
	let attrs = if fullscreen {
		let monitor = display.and_then(|i| _el.available_monitors().nth(i));
		attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(monitor)))
	} else {
		attrs
	};
	#[cfg(any(target_arch = "wasm32", target_os = "ios"))]
	let _ = (display, fullscreen);
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

/// Adapter + device + configured surface. Async because the browser main thread cannot block;
/// native callers wrap it in `pollster::block_on` (macOS requires window/surface creation on the
/// main thread anyway). Fallible because on the web "no adapter" is an ordinary runtime condition
/// that must reach the page's error UI; native callers still treat any Err as fatal.
async fn init_gpu(
	window: Arc<Window>,
	no_vsync: bool,
	no_gpu_timer: bool,
) -> Result<(Gpu, wgpu::Surface<'static>, wgpu::SurfaceConfiguration), String> {
	// No display handle at the instance: the window carries its own, and
	// `create_surface` is the only thing here that needs one.
	let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
	let surface = instance
		.create_surface(window.clone())
		.map_err(|e| format!("create surface: {e}"))?;
	let adapter = instance
		.request_adapter(&wgpu::RequestAdapterOptions {
			power_preference: wgpu::PowerPreference::HighPerformance,
			compatible_surface: Some(&surface),
			force_fallback_adapter: false,
			// Take the adapter's real limits rather than rounding them
			// down to a portability tier — every limit this engine cares
			// about is already asked for explicitly.
			apply_limit_buckets: false,
		})
		.await
		.map_err(|e| format!("no compatible GPU adapter: {e}"))?;
	log::info!("adapter: {:?}", adapter.get_info());
	// GPU pass timing (debug/metrics facility) when the adapter has it,
	// unless the run opted out (`DIG_NO_GPU_TIMER` / `?nogputimer`).
	let required_features = if no_gpu_timer {
		wgpu::Features::empty()
	} else {
		adapter.features() & wgpu::Features::TIMESTAMP_QUERY
	};
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
		.map_err(|e| format!("request device: {e}"))?;
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
	Ok((gpu, surface, config))
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
		// Gamma-space rendering all the way to the surface (see the format
		// pick above); `Auto` is wgpu's own "whatever this format has
		// always meant" and never widens the gamut behind our back.
		color_space: wgpu::SurfaceColorSpace::Auto,
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
		device.on_uncaptured_error(Arc::new(move |e| {
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
			Action::IdleThenRetry => {
				if let AppState::Running(r) = &self.state {
					// Sleeping in the event loop is normally wrong, and is
					// right here: an occluded window acquires nothing, so
					// the redraw chain has no `present` left to pace it and
					// would spin a core. The wait is one frame's worth, so
					// the window returning is still felt as immediate.
					#[cfg(not(target_arch = "wasm32"))]
					std::thread::sleep(OCCLUDED_RETRY);
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
			let (gpu, surface, config) = pollster::block_on(init_gpu(
				window.clone(),
				self.options.no_vsync,
				self.options.no_gpu_timer,
			))
			.expect("gpu init");
			self.finish_reinit(window, gpu, surface, config);
		}
		#[cfg(target_arch = "wasm32")]
		self.spawn_gpu_init(window, true);
	}

	fn init_all(&mut self, el: &ActiveEventLoop) {
		log::info!("lifecycle: initial launch");
		let window = Arc::new(
			el.create_window(window_attrs(
				&self.options.title,
				self.options.window_size,
				self.options.display,
				self.options.fullscreen,
				el,
			))
			.unwrap(),
		);
		#[cfg(not(target_arch = "wasm32"))]
		{
			let (gpu, surface, config) = pollster::block_on(init_gpu(
				window.clone(),
				self.options.no_vsync,
				self.options.no_gpu_timer,
			))
			.expect("gpu init");
			self.finish_init(window, gpu, surface, config, el);
		}
		#[cfg(target_arch = "wasm32")]
		{
			let _ = el;
			// Once per launch (reinit_device bypasses init_all, so no
			// double install).
			install_pointer_lock_listeners(&self.proxy);
			// Unconditional: gating on device detection is how Brave's
			// fingerprinting shields once left a phone with no keyboard
			// at all. Existing is free — only `sync`'s focus costs.
			self.text_bridge = crate::webtext::TextBridge::new(self.proxy.clone());
			self.spawn_gpu_init(window, false);
		}
	}

	/// Kick the async adapter/device request; `user_event` completes it.
	#[cfg(target_arch = "wasm32")]
	fn spawn_gpu_init(&mut self, window: Arc<Window>, reinit: bool) {
		let proxy = self.proxy.clone();
		let no_vsync = self.options.no_vsync;
		let no_gpu_timer = self.options.no_gpu_timer;
		let w = window.clone();
		wasm_bindgen_futures::spawn_local(async move {
			match init_gpu(w, no_vsync, no_gpu_timer).await {
				Ok((gpu, surface, config)) => {
					// Fixed contract with the apps/web page (like the canvas
					// id): the device is in hand, and everything left before
					// the first frame is one long synchronous engine init —
					// yield the browser a frame so the page's status update
					// for this stage actually paints before that stretch.
					dispatch_page_event("digblock-gpu-ready");
					yield_to_browser().await;
					// Send fails only if the loop is gone — nothing to do
					// then.
					let _ = proxy.send_event(UserEvent::GpuReady(GpuReady {
						gpu,
						surface,
						config,
						reinit,
					}));
				}
				Err(reason) => {
					// The page owns the failure UI (fixed contract, again) —
					// a panic in a detached future may never reach it. The
					// app parks in WaitingForGpu behind the page's message.
					log::error!("gpu init failed: {reason}");
					dispatch_page_failure(&reason);
				}
			}
		});
		self.state = AppState::WaitingForGpu { window };
	}

	/// Re-read the window and reconfigure if it has resized since [`init_gpu`] measured it. Runs at
	/// every hand-off from an async GPU init, the whole window in which that can happen. On the web
	/// the two routinely disagree: a freshly created canvas reports 0×0 until its `ResizeObserver`
	/// fires, so an async init that beats the callback configures at the `max(1)` floor, and the
	/// `Resized` that follows is dropped in `WaitingForGpu` — winit only sends one on a CHANGE.
	fn adopt_window_size(
		window: &Window,
		gpu: &Gpu,
		surface: &wgpu::Surface<'static>,
		config: &mut wgpu::SurfaceConfiguration,
	) {
		let size = window.inner_size();
		let (width, height) = (size.width.max(1), size.height.max(1));
		if (width, height) == (config.width, config.height) {
			return;
		}
		log::info!(
			"window resized during gpu init ({}x{} -> {width}x{height}) — reconfiguring",
			config.width,
			config.height,
		);
		config.width = width;
		config.height = height;
		surface.configure(&gpu.device, config);
	}

	/// Shared post-GPU tail of the first init: native `init_all` calls it
	/// synchronously, the wasm path from `user_event`.
	fn finish_init(
		&mut self,
		window: Arc<Window>,
		gpu: Gpu,
		surface: wgpu::Surface<'static>,
		mut config: wgpu::SurfaceConfiguration,
		el: &ActiveEventLoop,
	) {
		Self::register_device_callbacks(&gpu.device, &self.device_lost);
		Self::adopt_window_size(&window, &gpu, &surface, &mut config);
		self.game.set_scale_factor(window.scale_factor() as f32);
		self.game
			.init(&gpu, config.format, (config.width, config.height));
		let format = config.format;
		self.state = AppState::Running(Running {
			offscreen: self
				.options
				.offscreen
				.then(|| Offscreen::new(&gpu, &config)),
			gpu: share_gpu(gpu),
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
		mut config: wgpu::SurfaceConfiguration,
	) {
		Self::register_device_callbacks(&gpu.device, &self.device_lost);
		Self::adopt_window_size(&window, &gpu, &surface, &mut config);
		self.game.rebuild_gpu_resources(&gpu, config.format);
		self.game.resize(&gpu, (config.width, config.height));
		self.lifecycle.note_rebuilt(config.format);
		self.state = AppState::Running(Running {
			offscreen: self
				.options
				.offscreen
				.then(|| Offscreen::new(&gpu, &config)),
			gpu: share_gpu(gpu),
			window,
			surface,
			config,
		});
		if let AppState::Running(r) = &self.state {
			r.window.request_redraw();
		}
	}

	/// Tell the game we are going, exactly once. See `shut_down`.
	fn exit_once(&mut self) {
		if !std::mem::replace(&mut self.shut_down, true) {
			self.game.on_exit();
		}
	}

	fn enter_suspend(&mut self) {
		match std::mem::replace(&mut self.state, AppState::Uninitialized) {
			AppState::Running(r) => {
				log::info!("lifecycle: suspend (surface dropped, device retained)");
				self.clock.pause();
				self.game.input(InputEvent::FocusLost);
				self.game.on_suspend();
				// On the web this arm is reached from `pagehide` with `persisted` set — the page
				// went into the back/forward cache. It may be restored, but the connection will
				// not be, so say goodbye rather than leave the server holding a dead session. The
				// OTHER pagehide reaches winit as `LoopExiting`. Native keeps suspend and exit
				// apart, because a backgrounded phone may well come back.
				#[cfg(target_arch = "wasm32")]
				self.exit_once();
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
				el.create_window(window_attrs(
					&self.options.title,
					self.options.window_size,
					self.options.display,
					self.options.fullscreen,
					el,
				))
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
			offscreen: self
				.options
				.offscreen
				.then(|| Offscreen::new(&s.gpu, &config)),
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
			if r.offscreen.is_some() {
				r.offscreen = Some(Offscreen::new(&r.gpu, &r.config));
			}
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
		// The frame cap, before the occupancy span opens so a held frame
		// is not a frame: on the web the redraw is declined and the next
		// animation frame asked for; natively the remainder is slept.
		if let Some(hz) = self.options.fps_cap {
			let interval = crate::frame_interval(hz);
			let now = Instant::now();
			let due = self.next_frame_due.unwrap_or(now);
			if crate::cap_holds(now, due) {
				#[cfg(target_arch = "wasm32")]
				{
					if let AppState::Running(r) = &self.state {
						r.window.request_redraw();
					}
					return;
				}
				#[cfg(not(target_arch = "wasm32"))]
				std::thread::sleep(due - now);
			}
			self.next_frame_due = Some(crate::next_deadline(Instant::now(), due, interval));
		}
		// Opens the occupancy span; the game closes it inside `render`, so
		// every scrap of main-thread work lands inside it — not just the
		// phases that happen to be timed individually.
		self.game.frame_started();

		// Cursor-grab reconcile, edge-triggered: menu opening releases the cursor, menu closing
		// re-grabs it. Level-holding at `true` never grabs on its own — that stays with the
		// click-to-grab path. On the web the rising edge is suppressed: a lock request from rAF
		// context carries no user gesture and Chrome rejects it, so re-grabs happen only from the
		// gesture paths.
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
		use wgpu::CurrentSurfaceTexture as Acquired;
		// Off the swapchain there is nothing to acquire and nothing that
		// can refuse: the target is a texture this shell owns, so none of
		// the triage below — occlusion included — can apply.
		let frame = if r.offscreen.is_some() {
			None
		} else {
			Some(match r.surface.get_current_texture() {
				// A suboptimal texture is still a texture. It rode an `Ok`
				// beside a `suboptimal` flag before wgpu split it out, and
				// this loop has always drawn it; the size change that makes
				// it suboptimal reaches us as a winit resize anyway.
				Acquired::Success(f) | Acquired::Suboptimal(f) => f,
				outcome => {
					let ev = match outcome {
						Acquired::Lost | Acquired::Outdated => LcEvent::SurfaceLost,
						Acquired::Timeout => LcEvent::SurfaceTimeout,
						Acquired::Occluded => LcEvent::SurfaceOccluded,
						_ => {
							// wgpu fires the lost callback from device
							// maintenance once the queue is EMPTY, never from
							// the loss itself, so a dead device lands here as a
							// generic acquire error with work still in flight.
							// A waiting poll would block its whole timeout.
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
							log::error!(
								"surface acquire failed, no device loss reported: {outcome:?}"
							);
							LcEvent::SurfaceFatal
						}
					};
					self.dispatch(ev, el);
					return;
				}
			})
		};

		self.game.frame_acquired(t_acquire.elapsed());
		let dt = self.clock.tick();
		self.game.update(dt);

		// One of the two is always present; which one is the whole
		// difference between a windowed run and an offscreen capture. The
		// swapchain's view is built per frame, so it needs a local to
		// outlive the borrow.
		let swap_view;
		// `shot` feeds the screenshot path, which is native-only.
		#[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
		let (view, shot) = match (&frame, &r.offscreen) {
			(Some(f), _) => {
				swap_view = f
					.texture
					.create_view(&wgpu::TextureViewDescriptor::default());
				(&swap_view, &f.texture)
			}
			(None, Some(o)) => (&o.view, &o.texture),
			(None, None) => unreachable!("a frame target is either acquired or owned"),
		};
		self.game
			.render(&r.gpu, view, (r.config.width, r.config.height));

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
					crate::screenshot::capture(&r.gpu, shot, r.config.format, &path);
				}
				self.screenshot_pending = false;
				if self.options.exit_after_screenshot && self.options.screenshot.is_some() {
					if let Some(frame) = frame {
						r.gpu.queue.present(frame);
					}
					el.exit();
					return;
				}
			}
		}

		if let Some(frame) = frame {
			r.gpu.queue.present(frame);
		}
		// The host's memory, for the overlay's `mem` line: a JS call, so
		// not every frame.
		#[cfg(target_arch = "wasm32")]
		if self.frames_rendered.is_multiple_of(60) {
			let (wasm, js) = host_memory();
			self.game.set_host_memory(wasm, js);
		}
		// First frame is on screen: tell the page to drop its loading
		// overlay (fixed contract with apps/web's index.html, like the
		// canvas id).
		#[cfg(target_arch = "wasm32")]
		if self.frames_rendered == 1 {
			dispatch_page_event("digblock-first-frame");
		}
		if self
			.options
			.exit_after_frames
			.is_some_and(|n| self.frames_rendered >= n)
		{
			self.exit_once();
			// The last frame is presented and the run is over: the page
			// contract's closing event, which is what a harness waits
			// for before it photographs the canvas or reads the summary.
			#[cfg(target_arch = "wasm32")]
			dispatch_page_event("digblock-exit");
			el.exit();
			return;
		}
		// Redraw-chained loop: pacing comes from present, and this pattern
		// avoids winit's iOS busy-spin issue with poll loops.
		r.window.request_redraw();
	}

	/// Automated lifecycle drill (`--drill`), exercising byte-for-byte the same transitions the OS
	/// lifecycle events use. Steps advance on a counter, never on exact frame equality: failed
	/// frames don't increment `frames_rendered`, so an equality match on the step's frame would
	/// re-destroy the freshly recovered device every retry frame.
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
				// the final step means the app rendered on the recovered
				// device.
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

	/// Runs right after a user-gesture input dispatch (click, tap, key):
	/// requests the game raised that can only be honoured while that
	/// gesture is still on the stack — the fullscreen toggle, and the
	/// text bridge's focus edge (focusing inside the tap's dispatch is
	/// what raises the soft keyboard).
	fn after_game_input(&mut self) {
		let fullscreen = self.game.take_fullscreen_request();
		let screenshot = self.game.take_screenshot_request();
		#[cfg(target_arch = "wasm32")]
		{
			if fullscreen {
				toggle_browser_fullscreen();
			}
			// The canvas image is the last PRESENTED frame — one frame old,
			// invisible in a screenshot — and snapshotting it here keeps the
			// anchor click inside the keypress's own task (the gesture rule).
			if screenshot
				&& let AppState::Running(r) = &self.state
				&& let Some(canvas) = winit::platform::web::WindowExtWebSys::canvas(&*r.window)
			{
				crate::webshot::download_canvas_png(&canvas, self.launched_at.elapsed().as_secs());
			}
			let game = &self.game;
			if let Some(bridge) = &mut self.text_bridge
				&& let AppState::Running(r) = &self.state
				&& let Some(canvas) = winit::platform::web::WindowExtWebSys::canvas(&*r.window)
			{
				bridge.sync(
					game.text_entry_active(),
					|| game.text_field_contents(),
					&canvas,
				);
			}
		}
		#[cfg(not(target_arch = "wasm32"))]
		{
			let _ = fullscreen;
			// The native capture rides the existing render_frame path
			// (blocking map wait + PNG to shots/); iOS has no keyboard to
			// fire it, and the flag is simply never read there.
			self.screenshot_pending |= screenshot;
		}
	}

	fn grab_cursor(&mut self, grab: bool) {
		// A touch device has no pointer to lock: iOS Safari lacks the API
		// entirely, and every tap's re-grab attempt would be refused and
		// logged. One gate here covers the reconcile, click-to-grab and
		// gesture re-grab paths alike; releases stay allowed.
		if grab && self.options.touch {
			return;
		}
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

/// The wasm linear memory's size and, on Chrome, the JS heap in use (`performance.memory`, a
/// non-standard field read reflectively; 0 where absent).
#[cfg(target_arch = "wasm32")]
fn host_memory() -> (u64, u64) {
	use wasm_bindgen::JsCast;
	let wasm = wasm_bindgen::memory()
		.dyn_into::<js_sys::WebAssembly::Memory>()
		.ok()
		.and_then(|m| m.buffer().dyn_into::<js_sys::ArrayBuffer>().ok())
		.map_or(0, |b| u64::from(b.byte_length()));
	let js = web_sys::window()
		.and_then(|w| w.performance())
		.and_then(|p| js_sys::Reflect::get(&p, &"memory".into()).ok())
		.and_then(|m| js_sys::Reflect::get(&m, &"usedJSHeapSize".into()).ok())
		.and_then(|v| v.as_f64())
		.map_or(0, |v| v as u64);
	(wasm, js)
}

/// One side of the fixed contract with the apps/web page (like the canvas
/// id): named events on the window are how the platform narrates startup
/// to the page's loading overlay — `digblock-gpu-ready`, then
/// `digblock-first-frame`, and `digblock-exit` when a `?frames=` run ends;
/// `digblock-fatal` when startup cannot continue.
#[cfg(target_arch = "wasm32")]
fn dispatch_page_event(name: &str) {
	if let (Some(w), Ok(ev)) = (web_sys::window(), web_sys::Event::new(name)) {
		let _ = w.dispatch_event(&ev);
	}
}

/// The fatal half of the page contract: a `digblock-fatal` CustomEvent
/// whose detail is the reason, shown by the page's failure UI. A panic in
/// a detached wasm future may or may not reach the page's error listener,
/// so anything that must be seen goes through here instead.
#[cfg(target_arch = "wasm32")]
fn dispatch_page_failure(reason: &str) {
	let init = web_sys::CustomEventInit::new();
	init.set_detail(&wasm_bindgen::JsValue::from_str(reason));
	if let (Some(w), Ok(ev)) = (
		web_sys::window(),
		web_sys::CustomEvent::new_with_event_init_dict("digblock-fatal", &init),
	) {
		let _ = w.dispatch_event(&ev);
	}
}

/// Resume only after the browser has actually presented a frame. Awaiting a rAF alone is not
/// enough: the await's continuation runs as a microtask INSIDE the rendering task, before its
/// paint, so heavy synchronous work scheduled there still starves the very update the caller wanted
/// shown. The zero timer queues the continuation behind the rendering steps after the callbacks.
#[cfg(target_arch = "wasm32")]
pub async fn yield_to_browser() {
	let Some(w) = web_sys::window() else { return };
	let raf = js_sys::Promise::new(&mut |resolve, _| {
		let _ = w.request_animation_frame(&resolve);
	});
	let _ = wasm_bindgen_futures::JsFuture::from(raf).await;
	let timer = js_sys::Promise::new(&mut |resolve, _| {
		let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
	});
	let _ = wasm_bindgen_futures::JsFuture::from(timer).await;
}

/// A property that is present and not nullish, or `None`. Every reach into
/// a vendor-prefixed API goes through this: the prefixed names simply do
/// not exist on the browsers that have the standard ones, and a typed
/// getter would throw rather than answer.
#[cfg(target_arch = "wasm32")]
fn prop(target: &wasm_bindgen::JsValue, name: &str) -> Option<wasm_bindgen::JsValue> {
	js_sys::Reflect::get(target, &wasm_bindgen::JsValue::from_str(name))
		.ok()
		.filter(|v| !v.is_undefined() && !v.is_null())
}

/// Call a zero-argument method by name if the object has one; `false` if
/// it does not, or if the call threw.
#[cfg(target_arch = "wasm32")]
fn call_method(target: &wasm_bindgen::JsValue, name: &str) -> bool {
	use wasm_bindgen::JsCast;
	prop(target, name)
		.and_then(|f| f.dyn_into::<js_sys::Function>().ok())
		.is_some_and(|f| f.call0(target).is_ok())
}

/// Can this browser put the page fullscreen at all? The unprefixed Fullscreen API first, then
/// WebKit's prefixed one, still what answers on some iPadOS versions. iPhone Safari has neither for
/// most of its history — there the honest answer is `false` and the menu does not offer the row.
/// The launcher asks this to decide whether to offer it, and [`toggle_browser_fullscreen`] uses
/// whichever half answered, so the question and the action cannot disagree about what exists.
#[cfg(target_arch = "wasm32")]
pub fn fullscreen_available() -> bool {
	let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
		return false;
	};
	if doc.fullscreen_enabled() {
		return true;
	}
	prop(&doc, "webkitFullscreenEnabled").is_some_and(|v| v.as_bool().unwrap_or(false))
}

/// Toggle browser fullscreen on the whole page. Must be called inside a user gesture's own event
/// dispatch (see `after_game_input`). Entering also tries a landscape orientation lock — honoured
/// by Android Chrome once fullscreen, rejected everywhere else, and the rejection IS the answer, so
/// both calls are fire-and-forget. The lock waits a frame, since one requested before the
/// fullscreen transition applies is refused even where it is supported.
#[cfg(target_arch = "wasm32")]
fn toggle_browser_fullscreen() {
	let Some(window) = web_sys::window() else {
		return;
	};
	let Some(doc) = window.document() else {
		return;
	};
	let orientation = window.screen().ok().map(|s| s.orientation());
	// Both spellings on both sides of the toggle: a browser answering the
	// prefixed `fullscreen_available` also reports its state and takes its
	// requests under the prefixed names, and asking the standard ones
	// there reads as "not fullscreen" forever.
	let entered =
		doc.fullscreen_element().is_some() || prop(&doc, "webkitFullscreenElement").is_some();
	if entered {
		if let Some(o) = orientation {
			let _ = o.unlock();
		}
		if doc.fullscreen_enabled() {
			doc.exit_fullscreen();
		} else {
			call_method(&doc, "webkitExitFullscreen");
		}
	} else if let Some(root) = doc.document_element() {
		if doc.fullscreen_enabled() {
			let _ = root.request_fullscreen();
		} else {
			call_method(&root, "webkitRequestFullscreen");
		}
		if let Some(o) = orientation {
			wasm_bindgen_futures::spawn_local(async move {
				yield_to_browser().await;
				if let Ok(p) = o.lock(web_sys::OrientationLockType::Landscape) {
					let f = wasm_bindgen_futures::JsFuture::from(p);
					let _ = f.await;
				}
			});
		}
	}
}

/// Browser truth for pointer lock, routed into the winit loop as user
/// events. Page-lifetime listeners: leaking the closures is deliberate.
#[cfg(target_arch = "wasm32")]
fn install_pointer_lock_listeners(proxy: &winit::event_loop::EventLoopProxy<UserEvent>) {
	use wasm_bindgen::JsCast;
	use wasm_bindgen::closure::Closure;
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
			UserEvent::TextValue(v) => self.game.text_replace(&v),
			UserEvent::TextKey(key) => {
				self.game.input(InputEvent::Key { key, pressed: true });
				self.game.input(InputEvent::Key {
					key,
					pressed: false,
				});
				// Enter may have submitted and closed the field: let the
				// bridge blur so the keyboard follows it down.
				self.after_game_input();
				// A named key can rewrite the field engine-side (Tab
				// completion); restate the DOM buffer, or the next
				// keystroke resends the text it replaced.
				#[cfg(target_arch = "wasm32")]
				if self.game.text_entry_active()
					&& let Some(bridge) = &self.text_bridge
				{
					bridge.reseed(self.game.text_field_contents());
				}
			}
		}
	}

	fn suspended(&mut self, el: &ActiveEventLoop) {
		self.dispatch(LcEvent::OsSuspended, el);
	}

	/// The loop is ending. On desktop this follows the close button's own `el.exit()`, so the
	/// goodbye has already run and this is a no-op. On the WEB it is the only notice we get: winit
	/// splits `pagehide` by `persisted`, sending the back/forward-cache case to `suspended` and a
	/// real close or navigation HERE. Without this hook, closing a tab — the most common way a
	/// browser player leaves — would say nothing to the server at all.
	fn exiting(&mut self, _el: &ActiveEventLoop) {
		self.exit_once();
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
				self.exit_once();
				el.exit();
			}
			WindowEvent::Focused(false) => {
				// The text bridge taking focus blurs the canvas too; that
				// is the game's own field, not a loss — forwarding it
				// closed the chat on the very keypress that opened it.
				#[cfg(target_arch = "wasm32")]
				if self.text_bridge.as_ref().is_some_and(|b| b.owns_focus()) {
					return;
				}
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
				self.after_game_input();
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
					self.after_game_input();
				}
			}
			WindowEvent::KeyboardInput { event: k, .. } => {
				use winit::keyboard::{KeyCode, PhysicalKey};
				let pressed = k.state.is_pressed();
				match k.physical_key {
					// Debug drills — press only. (F9/F10 stay live on the web:
					// surface recreate and device rebuild are real paths there.
					// F8 is no longer shell business — the screenshot bind
					// flows through the engine's key table like any key and
					// comes back via `take_screenshot_request`.)
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
							&& let Some(text) = &k.text
						{
							for ch in text.chars().filter(|c| !c.is_control()) {
								self.game.input(InputEvent::Text { ch });
							}
						}
						// A key that just closed the menu re-locks from
						// inside its own gesture — except Escape: Chrome
						// excludes it from user activation, so the lock
						// fails and the unlock event would bounce the
						// player straight back into the pause menu.
						#[cfg(target_arch = "wasm32")]
						if code != KeyCode::Escape {
							self.gesture_regrab(wants_before);
						}
						self.after_game_input();
					}
					PhysicalKey::Unidentified(_) => {}
				}
			}
			_ => {}
		}
	}

	fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
		if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
			&& self.cursor_grabbed
			&& matches!(self.state, AppState::Running(_))
		{
			self.game.input(InputEvent::MouseDelta { dx, dy });
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
