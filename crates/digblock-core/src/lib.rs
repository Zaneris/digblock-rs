//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod alchemy;
pub mod block;
pub mod chunk;
pub mod decoration;
pub mod dir;
pub mod fluid;
pub mod hotbar;
pub mod inv_grid;
pub mod inventory;
pub mod mesher;
pub mod metrics;
pub mod overlay;
pub mod pipeline;
pub mod player;
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
///
/// v3: hold-to-dig hardness table, the water-regen fluid rule, and the
/// fluid-aware picking ray — all behavior both sides run independently.
/// v4: swimming — buoyant water physics in `Player::tick` (slow sink,
/// held-jump swim-up, entry damping, surface breach); pinned by the
/// second golden, `golden_water_trajectory_pins_sim_version`.
/// v5: worldgen variety — region spread (2–3× control-field wavelengths),
/// flat steppes, tree variants + forest clustering.
/// v6: bedrock floor — cells at y = `block::BEDROCK_Y` are undiggable
/// (`BlockType::dig_hardness`); shared dig behavior, so prediction on an
/// old client would silently open holes a new server refuses.
/// v7: sub-block shapes — stair ramp collision (diagonal support +
/// downhill glue), door leaf boxes with the open/closed state bit, and
/// the shape-aware occupancy probes; pinned by the third golden,
/// `golden_stair_trajectory_pins_sim_version`. Cube-only worlds are
/// bit-identical to v6 (the older goldens did not move), but shaped
/// cells exist only from v7 on.
/// v8: ramp run-out — a cube the supporting stair's diagonal tops out
/// against no longer presents its side face (mounting a flush landing
/// was a seconds-long corner grind); the exemption is mirrored in
/// `overlaps_solid` so mid-mount claims stay adoptable. Worlds where no
/// capsule is ever ramp-supported are bit-identical to v7.
/// v9: auto-step — a descending walk mounts any cube/box top within
/// 4/12 of the feet (`AUTO_STEP_MAX`; the slab family's stacking rise)
/// when there's capsule headroom; ramps are excluded (their own branch
/// lifts). Also shifts apex-phase jumps against a one-block step: a
/// mount whose apex brings the feet within 4/12 of the top now steps
/// instead of scraping the wall for the last centimeters.
/// v10: climbing is one continuous motion, stairs and steps alike.
/// Stairs: a ramp cell no longer reports its floor as support when the
/// axis is still short of its low front edge (that floor is the NEXT
/// step's, a third of a block up, so every step popped early and the
/// downhill glue then yanked the feet back); the glue now takes the
/// highest ramp surface underfoot and only when it is below the feet;
/// and the run-out no longer counts the next tread of the same flight as
/// blocked headroom, so a filled-in staircase stops grinding against
/// each riser. Steps: an auto-step is now the same mount a ramp gives,
/// rising at the rate a 45° diagonal would carry the walk
/// (`MOUNT_RATE`) instead of teleporting the whole rise in one tick —
/// so the camera smoothing that already covers stairs covers steps too,
/// with no presentation special case. The mount window (`step_mount`) is
/// mirrored in `overlaps_solid`, replacing v9's "the step needs no probe
/// mirror" property: a rate-limited mount spends a few ticks with the
/// capsule inside the side of the box it climbs.
/// v11: level-scaled dig speed — hold-to-dig duration is
/// `skills::dig_required(hardness, mining_level)`: 1.25× today's times
/// at level 1, parity at 30 (the legacy table is the mid-curve golden),
/// ~0.42× at 99. Shared dig behavior both sides time independently, so
/// a v10 client's holds would complete early or late against a v11
/// server at any level but 30.
pub const SIM_VERSION: u16 = 11;
