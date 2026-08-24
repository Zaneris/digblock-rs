//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod alchemy;
pub mod block;
pub mod causeway;
pub mod chunk;
pub mod compass;
pub mod decoration;
pub mod dir;
pub mod fluid;
pub mod gi;
pub mod hotbar;
pub mod inv_grid;
pub mod inventory;
pub mod job_wire;
pub mod kindle;
pub mod material;
pub mod mesher;
pub mod metrics;
pub mod overlay;
pub mod pick;
pub mod pipeline;
pub mod player;
pub mod presence;
pub mod protect;
pub mod raycast;
pub mod shapes;
pub mod simplex;
pub mod skills;
pub mod sky;
pub mod spawn;
pub mod stream_budget;
pub mod sun;
pub mod touch;
pub mod unlock;
pub mod ward;
pub mod waystones;
pub mod worldgen;
pub mod worldjob;

// Root re-exports: only what consumers actually reach through the crate
// root (everything stays reachable at its module path).
pub use block::BlockType;
pub use chunk::{CHUNK_SIZE, ChunkData, ChunkPos};

/// Version of the behavior client and server execute INDEPENDENTLY and must agree on bit-for-bit:
/// `Player::tick` physics, worldgen (terrain is never sent — join sync is seed + edits, so
/// divergent worldgen means silently different worlds), and the tables both ends time a dig
/// against. Exchanged in the Hello/Welcome handshake; a mismatch is refused. Bump on ANY behavior
/// change to those — the golden tests fail on drift and name this constant in their message.
pub const SIM_VERSION: u16 = 24;
