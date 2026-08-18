//! Pure game logic: no GPU, no windowing. Everything here is testable
//! headless on any machine.

pub mod alchemy;
pub mod block;
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
pub mod material;
pub mod mesher;
pub mod metrics;
pub mod overlay;
pub mod pick;
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
pub mod unlock;
pub mod worldgen;

// Root re-exports: only what consumers actually reach through the crate
// root (everything stays reachable at its module path).
pub use block::BlockType;
pub use chunk::{CHUNK_SIZE, ChunkData, ChunkPos};

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
/// v5: worldgen variety — region spread (the control-field wavelengths
/// widened), flat steppes, tree variants + forest clustering.
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
/// `AUTO_STEP_MAX` of the feet (the slab family's stacking rise) when
/// there's capsule headroom; ramps are excluded (their own branch
/// lifts). Also shifts apex-phase jumps against a one-block step: a
/// mount whose apex brings the feet inside that window now steps
/// instead of scraping the wall for the last centimeters.
/// v10: climbing is one continuous motion, stairs and steps alike.
/// Stairs: a ramp cell no longer reports its floor as support when the
/// axis is still short of its low front edge (that floor is the NEXT
/// step's, one tread up, so every step popped early and the
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
/// `skills::dig_required(hardness, mining_level)`: slower than the flat
/// hardness table at the bottom of the curve, faster at the top, with
/// that table reproduced exactly at the mid-curve anchor (the golden).
/// Shared dig behavior both sides time independently, so a v10 client's
/// holds would complete early or late against a v11 server at every
/// level but that anchor.
/// v12: per-material dig curves + the rock gate — `dig_required` keys on
/// the block's `DigFamily` (Fixed flat, every other family its own
/// level-1 speed, all of them still meeting the hardness table at the
/// mid-curve anchor), and `unlock::mine` refuses rock digs below a
/// per-material Mining level (client prediction + the server's DigStart
/// backstop). A v11 peer times non-Soft holds differently at every level
/// but that anchor and would let a fresh player open a hold on gated
/// rock.
/// v13: the craft gate — `unlock::pickup` refuses a dig whose
/// drop is craft-only (`block::crafted`) below that item's own
/// `unlock::craft` level, so a torch or a lumber stair can't be picked
/// up by someone who couldn't have made it. Client prediction plus the
/// server's DigStart backstop, exactly as v12's rock gate; a v12 peer
/// would open holds the new server refuses.
/// v14: the material × shape grid — `unlock::craft` is DERIVED as
/// `max(shape level, material level)` (SKILLS-DESIGN §7.2, `material.rs`)
/// instead of hand-listed, which moves five thresholds: Wood and the
/// soils down to their materials' levels, the stone slab and stair up to
/// stone's. Because `unlock::pickup` derives from `craft`, those are
/// v13's craft gate answering differently — a v13 peer would open a hold
/// on a stone slab the new server refuses below stone's Alchemy level,
/// and refuse one on a log the new server allows. Worldgen and physics
/// are untouched, so both goldens hold and no world is staled.
/// v15: ore (SKILLS-DESIGN §5.3). Veins are DERIVED — `worldgen::ore` is
/// a pure function both ends evaluate, `build_column` is byte-identical
/// and the journal still carries Stone — but what a dig DOES is now
/// shared behaviour: a revealed vein with the Essence Siphon in hand is
/// an extraction that writes `DepletedStone` and credits the ore, one
/// without it is refused at `DigStart`, and an extraction is TIMED as
/// the ore rather than as the stone the cell holds. A v14 peer would
/// break a seam into rubble at stone speed and lose it.
/// v16: the Resonant Pick (SKILLS-DESIGN §5.6). One hold breaks the
/// PLANE of the face it hits — a square that widens with Mining — and
/// the pick multiplies the hold time by a LEVEL-only factor
/// (`skills::dig_required_with`). Both are shared behaviour derived from
/// data each side already has (`pick::cells` over its own world, the
/// multiplier over the level and the active tool), so a v15 peer times
/// every pick hold differently and breaks one cell where the new server
/// breaks the whole plane. Mining's mastery bonus ore rides here too.
/// v17: §7.2's later materials and §7.3's lamps — twenty-one block rows
/// (dressed stone, glass, metal, crystal, the two lamps, the sigil). No
/// existing behaviour changes, but the ids do: a v16 peer decodes every
/// one of them as Air, so a metal wall would simply not be there. Metal
/// and Crystal also pick up `unlock::mine` gates of their own, which is
/// v12's rock gate answering for substances that had no blocks until
/// now.
/// v18: the dig curve rebuilt end to end (SKILLS-DESIGN §5.1). Three
/// things change together. **The shape**: a family is now just two
/// endpoints — its first-level speed and its mastery speed — with one
/// shared `DIG_TAPER` deciding how the journey between them is spent,
/// replacing v12's two linear segments hinged on a mid anchor. **The
/// numbers**: every mastery endpoint is exactly what it was and every
/// first-level one is half again as slow, so the early game is a longer
/// climb and mastery lands where it already did. **The grid**: the curve
/// is SAMPLED rather than continuous (`skills::dig_speed_step`), a
/// handful of jumps in a life rather than one sub-tick jump per level,
/// each strictly smaller than the one before so the first is the biggest
/// speedup a player ever feels. The load-bearing casualty is that **the
/// mid-curve anchor is no longer parity with the shipped hardness
/// table** — that parity was the shared golden from v11 through v17, and
/// `hardness_ticks` is now a base the curve scales at every level. Both
/// sides time holds independently, so a v17 peer's completions arrive
/// early against a v18 server at every level but the last.
///
/// v18 also re-keys the STRENGTH WALL (`unlock::mine`) off the substance
/// and onto the same `DigFamily`: Fixed/Soft from the first swing, then
/// Wood, then Rock — soil → timber → rock, every block of a family
/// opening together. v12's per-material numbers survive only as a SECOND
/// gate for substances that must also be earned (the ores at their own
/// reveal, Metal and Crystal at theirs), and `mine` takes the later of
/// the two. Sandstone and stone therefore move up onto the rock family's
/// level, and gravel and timber stop being ungated. Emberore's reveal
/// moves later with it, so the first vein is no longer visible in the
/// first stone a player can break. Same shape as v12: client prediction
/// plus the server's DigStart backstop, so a v17 peer opens holds the new
/// server refuses.
/// v19: what a dig CREDITS depends on the cell (SKILLS-DESIGN §7.1). The
/// rock rule is now conditional — the world's own stone and sandstone
/// crumble to Rubble, and one somebody PLACED hands itself back to any
/// digger who could have made one (`BlockType::dig_credit`, over the same
/// "has this cell ever been written" bit the ore reveal already reads).
/// That is what lets clean rock stay craftable while nothing else the
/// world hands you does, and what stops a made block laundering itself
/// back into a world-given one for the extraction XP. Shared behaviour
/// derived from data both ends already have, exactly like v15's veins, so
/// a v18 peer credits rubble where the new server credits the wall — and
/// its pick's plane picks a different set, since the extras filter now
/// matches on the credit rather than the block id.
/// v20: two changes, both to questions the two ends answer separately.
/// **A tool only counts while the hand is ON it** (`Hotbar::tool_in_hand`).
/// Holding a tool was two facts — which one the tool slot holds, and
/// whether that slot is selected — and everything but the bucket asked
/// only the first, so a player with dirt in hand drew veins with the
/// siphon and paid the Resonant Pick's hold multiplier while breaking its
/// plane. Both `ore::dig_mode` and `skills::dig_required_with` take the
/// answer, so a v19 peer calls a dig an extraction where the new server
/// calls it a dig, and times it by a multiplier the server never applied.
/// **And the dig curve is slower at both ends** (SKILLS-DESIGN §5.1), the
/// mastery endpoint by more than the opening — the shape, the taper and
/// the sampling grid are untouched, but every family's two endpoints
/// moved, so a v19 peer's completions arrive early against a v20 server
/// at every level.
pub const SIM_VERSION: u16 = 20;
