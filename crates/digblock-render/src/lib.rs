//! The engine: world state + wgpu renderer. Implements the platform `Game`
//! trait. Never touches winit or the surface — it renders into borrowed
//! `TextureView`s.

pub mod audio;
mod camera;
mod chat;
mod engine;
mod font;
mod gi;
mod gpu_timer;
mod highlight;
pub mod hotbar;
mod item_geom;
pub mod menu;
mod overlay;
mod pipeline_builder;
mod players;
mod renderer;
pub mod settings;
// DORMANT since P4 (RTGI replaced shadow maps); P6 deletes the module.
#[allow(dead_code)]
mod shadow;
mod sky;
mod smoothing;
mod spray;
mod texture;
pub mod ui;
mod viewmodel;
mod water;
mod water_audio;

pub use engine::{DebugScreen, Engine, EngineConfig};
