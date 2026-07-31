//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod block;
pub mod chunk;
pub mod mesher;
pub mod metrics;
pub mod pipeline;
pub mod player;
pub mod simplex;
pub mod sky;
pub mod sun;
pub mod touch;
pub mod worldgen;

pub use block::{BlockProps, BlockType, RenderClass};
pub use chunk::{ChunkData, ChunkPos, CHUNK_SIZE};
