//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod alchemy;
pub mod block;
pub mod budget;
pub mod camera_probe;
pub mod causeway;
pub mod chunk;
pub mod compass;
pub mod decoration;
pub mod dir;
pub mod ease;
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
pub mod sphere;
pub mod stream_budget;
pub mod sun;
pub mod touch;
pub mod unlock;
pub mod waystones;
pub mod worldgen;
pub mod worldjob;

// Root re-exports: only what consumers actually reach through the crate
// root (everything stays reachable at its module path).
pub use block::BlockType;
pub use chunk::{CHUNK_SIZE, ChunkData, ChunkPos};

/// Version of the behavior client and server execute INDEPENDENTLY and must agree on bit-for-bit:
/// `Player::tick` physics, the dig tables, and the BODY of every terrain generator this build
/// carries. Exchanged in the Hello/Welcome handshake; a mismatch is refused, and the golden tests
/// name this constant when they fail. A NEW `worldgen::TerrainVersion` row needs no bump — a world
/// names its generator — but editing a row in place does.
pub const SIM_VERSION: u16 = 28;
