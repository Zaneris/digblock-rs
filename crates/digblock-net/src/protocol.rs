//! Protocol constants and versioning rules.
//!
//! Wire evolution is by discipline, not self-description: postcard encodes
//! enum discriminants by declaration order, so variants and fields are
//! APPEND-ONLY — never reorder, never remove. Any breaking change bumps
//! [`PROTOCOL_VERSION`]; a mismatch is refused at the Hello handshake.

pub type PlayerId = u8;

/// Author id for server-originated edits (fluid simulation).
pub const SERVER_ID: PlayerId = u8::MAX;

pub const PROTOCOL_VERSION: u16 = 1;

/// Simulation rate. Fixed: `Player::tick` air control hardcodes /60.
pub const TICK_RATE: u32 = 60;

/// Snapshots go out every Nth sim tick (3 → 20 Hz).
pub const SNAPSHOT_DIVISOR: u32 = 3;

/// Each input datagram repeats the last N unacked ticks so isolated
/// datagram loss never costs the server an input.
pub const INPUT_REDUNDANCY: usize = 4;

/// Datagram payload budget, safely under the QUIC minimum MTU.
pub const MAX_DATAGRAM: usize = 1150;

/// Reliable-stream frame cap (journal batches are the big ones).
pub const MAX_FRAME: usize = 1 << 20;

/// Dig/place: claimed eye origin may differ from the server's simulated
/// position by at most this many blocks.
pub const EDIT_POS_TOLERANCE: f32 = 1.0;

/// Movement adoption envelope: a claimed post-tick position within this
/// distance of the server's own simulation is adopted as authoritative,
/// re-zeroing drift so honest clients never rubber-band.
pub const POS_TOLERANCE: f32 = 0.75;

/// Sanity clamp on per-tick displacement, blocks/s (terminal velocity is
/// 20; walk is 4). Claims that move faster than this are never adopted.
pub const MAX_SPEED: f32 = 25.0;

/// Drift EWMA smoothing: ~2 s window at 60 Hz. The server keeps a moving
/// average of each adopted claim's (claim − sim) delta VECTOR. Honest
/// deltas are zero-mean float noise and cancel; riding the envelope edge
/// requires a persistent directional bias that accumulates instead.
pub const DRIFT_ALPHA: f32 = 1.0 / 120.0;

/// Sustained-drift ceiling, blocks per tick of EWMA magnitude. Claims
/// stop being adopted while the average sits above this, capping the
/// exploitable envelope gain at ~1.2 blocks/s sustained (vs 4 walking) —
/// honest noise sits orders of magnitude below.
pub const DRIFT_TOLERANCE: f32 = 0.02;

/// Edit token bucket: sustained rate (burst [`EDIT_BURST`]).
pub const EDIT_RATE_PER_SEC: u32 = 10;
pub const EDIT_BURST: f32 = 5.0;

/// Target depth of the server's per-client input jitter buffer, ticks.
pub const JITTER_BUFFER_TICKS: u32 = 2;

/// How far ahead of the server clock a fresh client starts predicting:
/// jitter-buffer depth plus a little headroom for the first RTT estimate.
pub const CLIENT_TICK_LEAD: u32 = JITTER_BUFFER_TICKS + 3;

/// Remote players render this many ticks behind the estimated server
/// clock (100 ms), interpolating between snapshots.
pub const INTERP_DELAY_TICKS: f64 = 6.0;

/// Max extrapolation past the newest snapshot before a remote player
/// holds position (250 ms).
pub const EXTRAP_CAP_TICKS: f64 = 15.0;
