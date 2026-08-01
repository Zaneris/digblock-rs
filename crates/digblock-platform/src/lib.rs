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
	fn input(&mut self, event: InputEvent);
	/// Reports how long the swapchain acquire (vsync wait) took, just
	/// before `update()`. Metrics facility; default ignores it.
	fn frame_acquired(&mut self, _acquire_wait: std::time::Duration) {}
	/// Never called while suspended.
	fn update(&mut self, dt: std::time::Duration);
	/// The game renders into the borrowed swapchain view; every other
	/// target (depth, offscreen scene) is game-owned.
	fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32));
	/// Save state NOW — on iOS/Android the process may be killed with no
	/// further callbacks. Must be cheap and synchronous.
	fn on_suspend(&mut self) {}
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
	/// 40 simulate device loss (renderer rebuild), exit at frame 60.
	/// Manual equivalents: F9 (hold suspended) and F10.
	pub auto_drill: bool,
	/// Exit after this many presented frames (benchmark runs).
	pub exit_after_frames: Option<u64>,
	/// Present with `AutoNoVsync` instead of `Fifo` — frames run uncapped
	/// so benchmarks measure real frame cost, not vsync residue.
	pub no_vsync: bool,
}

/// Run the game. Never returns on iOS.
pub fn run(game: impl Game, options: RunOptions) {
	app::run(game, options)
}
