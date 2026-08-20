//! The engine: world state + wgpu renderer. Implements the platform `Game`
//! trait. Never touches winit or the surface — it renders into borrowed
//! `TextureView`s.

pub mod audio;
mod camera;
mod chat;
mod confetti;
mod engine;
mod font;
mod gi;
mod glass;
mod gpu_timer;
mod highlight;
pub mod hotbar;
mod item_geom;
mod menu;
mod overlay;
mod pipeline_builder;
mod players;
mod renderer;
pub mod settings;
mod siphon;
mod sky;
mod smoothing;
mod spray;
mod texture;
pub mod ui;
mod viewmodel;
mod voice;
mod water;
mod water_audio;

pub use engine::{DebugScreen, Engine, EngineConfig, parse_held_item, parse_node_kind};
/// The five types a launcher needs to drive the menu. Everything else
/// the menu owns is the engine's business.
pub use menu::{AccountView, MenuCommand, MenuResponse, MenuServices, ServerEntry};
/// Cost-stub names for the launcher's `DIG_STUB` parse (the mask itself
/// travels as `EngineConfig::stub`).
pub use renderer::stub;
