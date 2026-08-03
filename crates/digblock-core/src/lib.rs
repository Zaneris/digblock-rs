//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod block;
pub mod chunk;
pub mod decoration;
pub mod dir;
pub mod fluid;
pub mod mesher;
pub mod metrics;
pub mod pipeline;
pub mod player;
pub mod raycast;
pub mod simplex;
pub mod sky;
pub mod sun;
pub mod touch;
pub mod worldgen;

// Root re-exports: only what consumers actually reach through the crate
// root (everything stays reachable at its module path).
pub use block::BlockType;
pub use chunk::{ChunkData, ChunkPos, CHUNK_SIZE};

/// Version of the behavior client and server execute INDEPENDENTLY and
/// must agree on bit-for-bit: `Player::tick` physics (prediction + the
/// adoption envelope replay the same code) and worldgen (terrain is never
/// sent — join sync is seed + edits, so divergent worldgen means silently
/// different worlds). Exchanged in the Hello/Welcome handshake; a
/// mismatch is refused.
///
/// Bump on ANY behavior change to player physics or worldgen output. The
/// golden tests (`player::tests::golden_trajectory_pins_sim_version`,
/// `worldgen_golden_hash_pins_sim_version` in tests/worldgen.rs) fail
/// when behavior drifts and name this constant in their message.
pub const SIM_VERSION: u16 = 2;
