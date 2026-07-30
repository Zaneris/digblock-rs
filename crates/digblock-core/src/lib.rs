//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod block;
pub mod chunk;
pub mod metrics;
pub mod simplex;
pub mod worldgen;

pub use block::BlockType;
pub use chunk::{ChunkData, ChunkPos, CHUNK_SIZE};
