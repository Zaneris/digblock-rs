//! Platform shell: winit event loop, app lifecycle state machine, and
//! surface ownership.
//!
//! Load-bearing rule: this is the ONLY crate that touches `winit` or
//! `wgpu::Surface`. The engine implements [`Game`] and receives a
//! `&TextureView` borrowed for one `render()` call — it can never hold a
//! stale swapchain texture across a suspend, on any platform.

mod app;
mod clock;
pub mod input;
// Private: only app.rs consumes the decision core, and crate-private
// visibility lets -D warnings give it real dead-code coverage.
mod lifecycle;
// Blocking map wait + filesystem — no wasm analogue.
#[cfg(not(target_arch = "wasm32"))]
mod screenshot;

pub use input::{InputEvent, Key, MouseButton, TouchPhase};

use std::path::PathBuf;

/// Process-lifetime GPU objects. Survives suspend/resume; only the surface
/// (owned by the shell, invisible to the engine) dies with the window.
pub struct Gpu {
	pub instance: wgpu::Instance,
	pub adapter: wgpu::Adapter,
	pub device: wgpu::Device,
	pub queue: wgpu::Queue,
}

pub trait Game: 'static {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, size: (u32, u32));
	fn resize(&mut self, gpu: &Gpu, size: (u32, u32));
	/// Device pixels per logical pixel, for UI scaling. Reported once
	/// after window creation and again on every OS change (monitor hop,
	/// browser zoom). Sizes stay physical everywhere; default ignores it
	/// (bench/screenshot/drill paths are unaffected).
	fn set_scale_factor(&mut self, _scale_factor: f32) {}
	fn input(&mut self, event: InputEvent);
	/// Reports how long the swapchain acquire (vsync wait) took, just
	/// before `update()`. Metrics facility; default ignores it.
	fn frame_acquired(&mut self, _acquire_wait: std::time::Duration) {}
	/// Called at redraw-handler entry, before the swapchain acquire, so the
	/// game can time its own main-thread occupancy for this frame: the span
	/// from here to the end of `render()`, minus the acquire wait.
	///
	/// That span is the only honest occupancy signal across both platforms.
	/// Natively the vsync wait blocks inside the acquire, so subtracting it
	/// yields real work; on the web the acquire cannot block at all and the
	/// idle sits BETWEEN frames, outside the span entirely. Deriving busy
	/// time from the frame-to-frame delta instead reads as "always full" on
	/// the web and silently disables anything that feeds on it.
	fn frame_started(&mut self) {}
	/// Never called while suspended.
	fn update(&mut self, dt: std::time::Duration);
	/// The game renders into the borrowed swapchain view; every other
	/// target (depth, offscreen scene) is game-owned.
	fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32));
	/// Save state NOW — on iOS/Android the process may be killed with no
	/// further callbacks. Must be cheap and synchronous.
	fn on_suspend(&mut self) {}
	/// The run is ending on its own terms (`exit_after_frames`), before
	/// the event loop is told to exit.
	///
	/// Exists because a `Drop` impl is not a portable exit hook: on wasm,
	/// winit's exit unwinds through JS and destructors do not reliably
	/// run, so anything a capture is FOR — the bench summary — would be
	/// emitted on native and silently lost on the web, which is the
	/// platform with no other way to report it.
	fn on_exit(&mut self) {}
	/// Polled once per frame. `false` (menu open) releases the cursor and
	/// routes clicks/positions to the game as UI input; the shell re-grabs
	/// on the rising edge. Defaults to the classic always-grabbed FPS
	/// behavior (bench/screenshot/drill paths and iOS are unaffected).
	fn wants_cursor_grab(&self) -> bool {
		true
	}
	/// Polled on every keypress. `true` makes the shell deliver produced
	/// `Text` events even while the cursor stays grabbed — an in-game
	/// text field (chat) without releasing pointer lock.
	fn wants_text_input(&self) -> bool {
		false
	}
	fn on_resume(&mut self) {}
	/// F10 drill: rebuild all device-lifetime resources from scratch, as a
	/// real device loss would require. The standing regression test for the
	/// future Android port.
	fn rebuild_gpu_resources(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat);
}

#[derive(Default)]
pub struct RunOptions {
	pub title: String,
	/// Capture a PNG after `screenshot_after_frames` frames, then exit if
	/// `exit_after_screenshot`.
	pub screenshot: Option<PathBuf>,
	pub screenshot_after_frames: u32,
	pub exit_after_screenshot: bool,
	/// Automated lifecycle drill: at frame 20 suspend+resume (surface
	/// dropped and recreated through the production transitions), at frame
	/// 40 simulate device loss (renderer rebuild), at frame 60 destroy the
	/// device for real (full ReinitDevice recovery), complete and exit at
	/// frame 100. An incomplete drill makes `run` return an error.
	/// Manual equivalents: F9 (hold suspended) and F10.
	pub auto_drill: bool,
	/// Exit after this many presented frames (benchmark runs).
	pub exit_after_frames: Option<u64>,
	/// Present with `AutoNoVsync` instead of `Fifo` — frames run uncapped
	/// so benchmarks measure real frame cost, not vsync residue.
	pub no_vsync: bool,
	/// Logical window size, when a run needs a SPECIFIC one rather than
	/// winit's 800x600 default. The GI resolve is half-res off the
	/// surface, so its cost scales with this directly — which makes it the
	/// one knob a desktop-vs-web comparison has to hold equal before any
	/// of its numbers mean anything.
	pub window_size: Option<(u32, u32)>,
	/// Open the window on this monitor (index into winit's enumeration)
	/// instead of wherever the desktop last had focus.
	///
	/// This is a MEASUREMENT knob, not a convenience. A window that opens
	/// on whichever screen the user most recently clicked lands on a
	/// different refresh rate and a different backing scale run to run,
	/// and both are load-bearing: the physical framebuffer follows the
	/// scale, so the GI resolve's cost changes with it, and a
	/// display-paced frame lets the GPU downclock, which inflates every
	/// timestamp in the CSV. A bracket taken across two screens is
	/// measuring the screens. `--display` pins it; the chosen monitor's
	/// name, size and refresh are logged so a capture records which one
	/// it ran on.
	pub display: Option<usize>,
	/// Step the world by exactly this much per frame instead of by
	/// measured wall time (`--fixed-dt`). Everything animated rides dt —
	/// water ripple phase, the fluid sim, the spawn settle — so without
	/// it two builds of DIFFERENT SPEED reach `--frames N` in different
	/// world states and a parity capture measures the speed difference
	/// instead of the change. See `GameClock`.
	pub fixed_dt: Option<std::time::Duration>,
}

/// Why `run` came back with a failure the process should report.
#[derive(Debug)]
pub enum RunError {
	/// `--drill` exited before completing every step (the destroy-step
	/// recovery must render through frame 100).
	DrillIncomplete { frames_rendered: u64 },
}

impl std::fmt::Display for RunError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			RunError::DrillIncomplete { frames_rendered } => write!(
				f,
				"lifecycle drill incomplete: exited at frame {frames_rendered} before the \
				 frame-100 completion step"
			),
		}
	}
}

impl std::error::Error for RunError {}

/// Run the game. Never returns on iOS.
pub fn run(game: impl Game, options: RunOptions) -> Result<(), RunError> {
	app::run(game, options)
}
