//! The engine: world state + wgpu renderer. Implements the platform `Game`
//! trait. Never touches winit or the surface — it renders into borrowed
//! `TextureView`s.

mod camera;
mod engine;
mod font;
mod gpu_timer;
mod overlay;
mod renderer;
mod spray;
mod texture;

pub use engine::{Engine, EngineConfig};
