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
// The web soft-keyboard bridge (a hidden <input> driven from app.rs).
#[cfg(target_arch = "wasm32")]
mod webtext;
// Private: only app.rs consumes the decision core, and crate-private
// visibility lets -D warnings give it real dead-code coverage.
mod lifecycle;
// Blocking map wait + filesystem — no wasm analogue.
#[cfg(not(target_arch = "wasm32"))]
mod screenshot;

pub use input::{InputEvent, Key, MouseButton, TouchPhase};
// For launcher boot code that must let the page paint before a long
// synchronous stretch (the loading overlay's whole reason to exist).
#[cfg(target_arch = "wasm32")]
pub use app::yield_to_browser;
// Whether the menu should offer its Fullscreen row. The shell owns the
// answer because the shell is what would have to act on it — the two
// spellings of the Fullscreen API are checked once, here, rather than
// probed by the launcher and toggled by the shell against different names.
#[cfg(target_arch = "wasm32")]
pub use app::fullscreen_available;

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
	/// Called at redraw-handler entry, before the swapchain acquire, so the game can time its own
	/// main-thread occupancy: the span from here to the end of `render()`, minus the acquire wait.
	/// That is the only honest occupancy signal across both platforms — natively the vsync wait
	/// blocks inside the acquire, while on the web the idle sits BETWEEN frames. Deriving busy time
	/// from the frame-to-frame delta instead reads as "always full" on the web.
	fn frame_started(&mut self) {}
	/// What the host process holds, where the shell can read it — on the web the wasm linear
	/// memory and Chrome's JS heap, sampled every few dozen frames. Never called natively.
	/// Metrics facility; default ignores it.
	fn set_host_memory(&mut self, _wasm_bytes: u64, _js_heap_bytes: u64) {}
	/// Never called while suspended.
	fn update(&mut self, dt: std::time::Duration);
	/// The game renders into the borrowed swapchain view; every other
	/// target (depth, offscreen scene) is game-owned.
	fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32));
	/// Save state NOW — on iOS/Android the process may be killed with no
	/// further callbacks. Must be cheap and synchronous.
	fn on_suspend(&mut self) {}
	/// We are going away for good: the window closed, the page unloading, a capture out of frames.
	/// Follows [`Game::on_suspend`], so this is for the goodbyes state-saving cannot do — telling a
	/// server the player has left, emitting a bench summary. Not a `Drop` impl, which is no
	/// portable exit hook: on wasm, destructors do not reliably run. Kept apart from `on_suspend`,
	/// which a backgrounded phone may come back from. The only callback here allowed to BLOCK.
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
	/// Drained right after each input dispatch: the game asked for a
	/// browser-fullscreen toggle from inside the gesture still on the
	/// stack (the only context the Fullscreen API honours). Only the web
	/// shell acts on it; the default never asks.
	fn take_fullscreen_request(&mut self) -> bool {
		false
	}
	/// Polled after input dispatches on touch devices: some text field currently accepts produced
	/// text (the menu's name entry, chat). The web shell focuses its hidden input on the rising
	/// edge — inside the tap's own dispatch, which is what raises the soft keyboard — and blurs it
	/// on the falling edge. A native shell may drive its own keyboard bridge from the same signal.
	fn text_entry_active(&self) -> bool {
		false
	}
	/// Polled by the keyboard bridge as it (re)takes focus: the active
	/// field's current contents, seeding the hidden DOM input so
	/// deletion can reach characters typed before the keyboard was last
	/// dismissed. Meaningless while `text_entry_active` is false.
	fn text_field_contents(&self) -> String {
		String::new()
	}
	/// The bridge's DOM buffer changed: replace the active field with
	/// this text, re-applying the field's own charset and length rules.
	/// Always the whole value, never a delta — so IME composition, paste
	/// and caret edits arrive by one path, and a character the field
	/// refused simply stays refused on every later restatement.
	fn text_replace(&mut self, _text: &str) {}
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
	/// Automated lifecycle drill, a step every few frames: suspend+resume (surface dropped and
	/// recreated through the production transitions), a simulated device loss (renderer rebuild), a
	/// real device destroy (full ReinitDevice recovery), then exit once frames are landing again on
	/// the recovered device. An incomplete drill makes `run` return an error. Manual equivalents:
	/// F9 (hold suspended) and F10.
	pub auto_drill: bool,
	/// Exit after this many presented frames (benchmark runs).
	pub exit_after_frames: Option<u64>,
	/// Present with `AutoNoVsync` instead of `Fifo` — frames run uncapped
	/// so benchmarks measure real frame cost, not vsync residue.
	pub no_vsync: bool,
	/// Draw into a texture of our own instead of the swapchain, never acquiring or presenting: what
	/// makes a capture independent of the DESKTOP, since macOS hands a covered window no surface
	/// texture at all and an unattended run simply stops when the screen locks. Deliberately NOT
	/// implied by `--screenshot`, because dropping the swapchain drops the present wait and every
	/// BENCH.md baseline was taken through the surface. Composes with [`Self::auto_drill`].
	pub offscreen: bool,
	/// Logical window size, when a run needs a SPECIFIC one rather than
	/// winit's default. The GI resolve is a fraction of the surface, so
	/// its cost scales with this directly — which makes it the one knob a
	/// desktop-vs-web comparison has to hold equal before any of its
	/// numbers mean anything.
	pub window_size: Option<(u32, u32)>,
	/// Open the window on this monitor (index into winit's enumeration) instead of wherever the
	/// desktop last had focus. A MEASUREMENT knob, not a convenience: a window landing wherever the
	/// user last clicked picks up a different refresh rate and backing scale run to run, and both
	/// are load-bearing — the framebuffer follows the scale, so the GI resolve's cost moves with
	/// it, and a display-paced frame lets the GPU downclock. A bracket across screens measures it.
	pub display: Option<usize>,
	/// Open borderless fullscreen on the chosen monitor (`--fullscreen`). A MEASUREMENT knob like
	/// `display`: a bench's resolve cost is per pixel, so a windowed run measures whatever size
	/// the window happened to open at; fullscreen is the display's own, and the same every run.
	pub fullscreen: bool,
	/// Step the world by exactly this much per frame instead of by measured wall time
	/// (`--fixed-dt`). Everything animated rides dt — water ripple phase, the fluid sim, the spawn
	/// settle — so without it two builds of DIFFERENT SPEED reach `--frames N` in different world
	/// states and a parity capture measures the speed difference instead of the change.
	pub fixed_dt: Option<std::time::Duration>,
	/// The primary pointer is a finger. The shell then never requests pointer lock (iOS Safari
	/// lacks the API; elsewhere every tap's re-grab would be refused without a real pointer) and,
	/// on the web, raises the soft keyboard while the game reports a text field open. Set by the
	/// launcher from its own device detection — the shell has no view of that.
	pub touch: bool,
	/// Do not request `TIMESTAMP_QUERY` even where the adapter offers it: the overlay's gpu line
	/// reads n/a. The web launcher sets this by default — Chrome's pass-boundary counter
	/// sampling makes macOS's GPU governor downclock a busy GPU (`EngineConfig::web`).
	pub no_gpu_timer: bool,
	/// Present at most this many frames a second. On the web a redraw arriving early is skipped
	/// and the next animation frame asked for; natively the remainder is slept. `None` is the
	/// display's pace.
	pub fps_cap: Option<f32>,
}

/// The wait a cap of `hz` puts between presents.
pub fn frame_interval(hz: f32) -> std::time::Duration {
	std::time::Duration::from_secs_f64(1.0 / f64::from(hz.max(1.0)))
}

/// Whether a redraw at `now` is too early for a frame due at `due`. Half a millisecond of slack
/// keeps a 120 Hz animation frame from missing a 60 Hz cap by rounding.
pub fn cap_holds(now: web_time::Instant, due: web_time::Instant) -> bool {
	now + std::time::Duration::from_micros(500) < due
}

/// When the frame after one that passed at `now` against `due` is due: one interval on from the
/// DEADLINE, not from now, so the frame's own cost never stretches the period — measured from
/// the present, a 30 Hz cap paced a 6 ms frame at 25. A frame more than an interval late
/// restarts the schedule from now instead of trying to catch up.
pub fn next_deadline(
	now: web_time::Instant,
	due: web_time::Instant,
	interval: std::time::Duration,
) -> web_time::Instant {
	let from = if now > due + interval { now } else { due };
	from + interval
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;
	use web_time::Instant;

	#[test]
	fn a_cap_of_60_presents_every_other_120hz_frame() {
		let interval = frame_interval(60.0);
		let vsync = Duration::from_secs_f64(1.0 / 120.0);
		let t0 = Instant::now();
		let due = next_deadline(t0, t0, interval);
		assert!(
			cap_holds(t0 + vsync, due),
			"the first 120 Hz frame is early"
		);
		assert!(!cap_holds(t0 + vsync * 2, due), "the second lands");
		assert!(!cap_holds(t0 + vsync * 3, due));
	}

	#[test]
	fn the_schedule_runs_from_the_deadline_until_a_frame_falls_a_whole_interval_behind() {
		let interval = frame_interval(30.0);
		let t0 = Instant::now();
		let due = t0 + interval;
		// Six milliseconds of frame after the deadline: the next is still one interval on.
		let late = due + Duration::from_millis(6);
		assert_eq!(next_deadline(late, due, interval), due + interval);
		assert!(!cap_holds(due, due));
		assert!(cap_holds(t0 + interval / 2, due));
		// A stall past a whole interval restarts from now.
		let stalled = due + interval + Duration::from_millis(1);
		assert_eq!(next_deadline(stalled, due, interval), stalled + interval);
	}
}

/// Why `run` came back with a failure the process should report.
#[derive(Debug)]
pub enum RunError {
	/// `--drill` exited before completing every step, the destroy-step recovery included.
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
