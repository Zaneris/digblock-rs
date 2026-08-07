//! The engine: world state + wgpu renderer. Implements the platform `Game`
//! trait. Never touches winit or the surface — it renders into borrowed
//! `TextureView`s.

pub mod audio;
mod camera;
mod chat;
mod engine;
mod font;
mod gpu_timer;
mod highlight;
pub mod hotbar;
pub mod menu;
mod overlay;
mod pipeline_builder;
mod players;
mod renderer;
pub mod settings;
mod shadow;
mod sky;
mod smoothing;
mod spray;
mod texture;
pub mod ui;
mod water;
mod water_audio;

pub use engine::{DebugScreen, Engine, EngineConfig};
