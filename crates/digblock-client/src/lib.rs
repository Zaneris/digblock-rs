//! The engine: world state + wgpu renderer. Implements the platform `Game`
//! trait. Never touches winit or the surface — it renders into borrowed
//! `TextureView`s.

mod anim;
pub mod audio;
pub mod bindings;
mod bursts;
mod camera;
mod camera_fx;
mod camrig;
mod chat;
mod cloud_noise;
mod confetti;
mod dusk;
mod engine;
pub mod events;
mod flame;
mod font;
mod gi;
mod glass;
mod gpu_ledger;
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
pub mod spellbar;
mod spray;
mod teleport;
mod texture;
pub mod tutorial;
pub mod ui;
mod viewmodel;
mod voice;
mod water;
mod water_audio;
mod waystone_fx;

/// The shape editor's launcher-facing IO seam (see `Engine::set_shape_services`).
pub use engine::editor::io::{ShapeIoCmd, ShapeIoResp, ShapeServices};
pub use engine::{
	DebugScreen, Engine, EngineConfig, STARE_FRAMES, knobs, parse_held_item, parse_node_kind,
};
/// The five types a launcher needs to drive the menu. Everything else
/// the menu owns is the engine's business.
pub use menu::{
	AccountView, BOARD_PAGE_ROWS, BoardRow, BoardView, MenuCommand, MenuResponse, MenuServices,
	RankView, ServerEntry,
};
/// Cost-stub names for the launcher's `DIG_STUB` parse (the mask itself
/// travels as `EngineConfig::stub`).
pub use renderer::stub;
