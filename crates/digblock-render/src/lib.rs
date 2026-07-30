//! The engine: world state + wgpu renderer. Implements the platform `Game`
//! trait. Never touches winit or the surface — it renders into borrowed
//! `TextureView`s.

mod camera;
mod engine;
mod renderer;
mod texture;

pub use engine::{Engine, EngineConfig};

/// Sky clear color, matching the Java Graphics.startSolid clear
/// (0.494, 0.753, 0.93).
pub const SKY: wgpu::Color = wgpu::Color {
	r: 0.494,
	g: 0.753,
	b: 0.93,
	a: 1.0,
};
