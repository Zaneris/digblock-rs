//! Protocol constants and versioning rules.
//!
//! Wire evolution is by discipline, not self-description: postcard encodes
//! enum discriminants by declaration order, so variants and fields are
//! APPEND-ONLY — never reorder, never remove. Any breaking change bumps
//! [`PROTOCOL_VERSION`]; a mismatch is refused at the Hello handshake.

pub type PlayerId = u8;

/// Author id for server-originated edits (fluid simulation).
pub const SERVER_ID: PlayerId = u8::MAX;

/// The production master server. Launchers and the dedicated server fall
/// back to this when no master is specified; an explicitly empty override
/// (`--master-url ""`, `?master=`) disables master-backed features (menu
/// multiplayer, music) instead.
pub const DEFAULT_MASTER_URL: &str = "https://master.digblock.net";

/// v6: return-to-spawn (ClientMsg::ReturnToSpawn / ServerMsg::Teleported).
pub const PROTOCOL_VERSION: u16 = 6;
/// Oldest protocol this build still speaks. The handshake refuses peers
/// outside `MIN_PROTOCOL..=PROTOCOL_VERSION`; from v2 on, `Hello` and
/// `Welcome` are append-only across the supported range so a decoder for
/// version N can read any peer ≥ N (postcard tolerates trailing bytes).
pub const MIN_PROTOCOL: u16 = 6;

// ---- handshake preamble ----------------------------------------------
//
// Both directions of the control stream start with this fixed 8-byte
// preamble, sent before any length-prefixed postcard frame. FROZEN
// FOREVER — never versioned, never extended — so any two builds can
// always exchange versions and refuse each other cleanly instead of
// hitting a postcard decode error on a differently-shaped Hello.

/// Preamble bytes 0-3.
pub const PREAMBLE_MAGIC: [u8; 4] = *b"DIGB";
/// magic (4) + protocol u16 LE (2) + reserved zeros (2).
pub const PREAMBLE_LEN: usize = 8;

pub fn encode_preamble(protocol: u16) -> [u8; PREAMBLE_LEN] {
	let v = protocol.to_le_bytes();
	[
		PREAMBLE_MAGIC[0],
		PREAMBLE_MAGIC[1],
		PREAMBLE_MAGIC[2],
		PREAMBLE_MAGIC[3],
		v[0],
		v[1],
		0,
		0,
	]
}

/// `None` = bad magic (not a DigBlock peer). Reserved bytes are ignored.
pub fn decode_preamble(bytes: &[u8; PREAMBLE_LEN]) -> Option<u16> {
	(bytes[..4] == PREAMBLE_MAGIC).then(|| u16::from_le_bytes([bytes[4], bytes[5]]))
}

/// Player-name cap, chars. Applied on both sides of the handshake.
pub const MAX_NAME_LEN: usize = 24;

/// Sanitize a player name: strip control characters, truncate to
/// [`MAX_NAME_LEN`] chars, empty → "player".
pub fn sanitize_name(name: &str) -> String {
	let clean: String = name
		.chars()
		.filter(|c| !c.is_control())
		.take(MAX_NAME_LEN)
		.collect();
	if clean.is_empty() {
		"player".into()
	} else {
		clean
	}
}

/// Display-name rule: 3-10 chars of `a-z A-Z 0-9 - _`. One source of
/// truth for the menu's typing filter and join gate, the launchers'
/// name arguments, and (mirrored as a regex) the master's /api/join
/// backstop. Distinct from [`MAX_NAME_LEN`], which stays the wire-level
/// sanitization cap.
pub const MIN_DISPLAY_NAME: usize = 3;
pub const MAX_DISPLAY_NAME: usize = 10;

/// The display-name alphabet. ASCII-only on purpose: the client font
/// renders 95 printable-ASCII glyphs.
pub fn is_name_char(c: char) -> bool {
	c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Validate a display name against the 3-10 char rule. The `Err` string
/// is shown verbatim as the menu's status-line alert.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
	let n = name.chars().count();
	if !(MIN_DISPLAY_NAME..=MAX_DISPLAY_NAME).contains(&n) || !name.chars().all(is_name_char) {
		return Err("name must be 3-10 chars: a-z 0-9 - _");
	}
	Ok(())
}

/// Chat message cap, chars (also bytes — the text is ASCII-only).
pub const MAX_CHAT_LEN: usize = 120;

/// Chat token bucket: sustained rate (burst [`CHAT_BURST`]).
pub const CHAT_RATE_PER_SEC: u32 = 1;
pub const CHAT_BURST: f32 = 3.0;

/// Sanitize a chat line: keep printable ASCII only (the font has no
/// other glyphs), truncate to [`MAX_CHAT_LEN`], trim; `None` when
/// nothing survives (the server drops the message silently).
pub fn sanitize_chat(text: &str) -> Option<String> {
	let clean: String = text
		.chars()
		.filter(|c| matches!(c, ' '..='~'))
		.take(MAX_CHAT_LEN)
		.collect();
	let trimmed = clean.trim();
	(!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Return-to-spawn cooldown, seconds. The menu greys the row with a
/// countdown; the server enforces it authoritatively per client (a
/// request inside the window is silently ignored — honest clients gate
/// locally, so only a tampered client ever sends one).
pub const RETURN_SPAWN_COOLDOWN_SECS: u32 = 60;

/// Give up on a transport stuck in `Connecting` after this long.
pub const CONNECT_TIMEOUT_SECS: f32 = 10.0;
/// Connected but neither `Welcome` nor `Refused` after this long →
/// treat as dead (an old server that couldn't decode our Hello would
/// otherwise leave the client in connecting-limbo forever).
pub const HANDSHAKE_TIMEOUT_SECS: f32 = 5.0;

/// Simulation rate. Fixed: `Player::tick` air control hardcodes /60.
pub const TICK_RATE: u32 = 60;

// The adoption envelope replays the identical `Player::tick` on client and
// server; that only holds if this rate and core's fixed tick describe the
// same clock. Compile-time tripwire, not a convention.
const _: () = assert!(digblock_core::player::TICK == 1.0 / TICK_RATE as f32);

/// Snapshots go out every Nth sim tick (3 → 20 Hz).
pub const SNAPSHOT_DIVISOR: u32 = 3;

/// Each input datagram repeats the last N unacked ticks so isolated
/// datagram loss never costs the server an input.
pub const INPUT_REDUNDANCY: usize = 4;

/// Send an input datagram at most once per this many predicted ticks
/// (30 Hz at stride 2). The redundancy window covers a lost datagram at
/// this spacing with 2× margin; input→adoption latency grows with the
/// stride, so don't raise it casually. High-refresh displays used to
/// send per FRAME (120+/s, half of them duplicates).
pub const INPUT_SEND_STRIDE: u32 = 2;

/// Datagram payload budget, safely under the QUIC minimum MTU.
pub const MAX_DATAGRAM: usize = 1150;

/// Reliable-stream frame cap (journal batches are the big ones).
pub const MAX_FRAME: usize = 1 << 20;

/// Dig/place: claimed eye origin may differ from the server's simulated
/// position by at most this many blocks. Sized for honest divergence:
/// the client predicts [`CLIENT_TICK_LEAD`] ticks ahead of the sim
/// state it is compared against (~0.4 blocks at walk speed, more when
/// falling) and aims from a camera still carrying the decaying
/// reconciliation offset (up to [`POS_TOLERANCE`]) — at 1.0 the sum
/// occasionally rejected legitimate digs made while moving.
pub const EDIT_POS_TOLERANCE: f32 = 1.5;

/// Timed digging: a completing Dig/Collect is accepted when the server
/// has counted at least `hardness - dig_grace(hardness)` aim-verified
/// ticks on the hold. The grace is what a cheat gains per dig, so it is
/// kept small — but it must cover the honest shortfall, which has two
/// parts: a CONSTANT pipeline lag every dig pays regardless of hardness
/// (the first verify lands a tick after DigStart and the server's look
/// trails the client's by the input jitter buffer — ~3 ticks even on
/// localhost), plus transient aim misses that scale with how long the
/// hold runs. The old `clamp(4, 12)` folded both into one floor, which
/// left cheap blocks (leaves, water) a single tick of real slack — any
/// one late input datagram bounced an honest dig.
pub const DIG_GRACE_BASE: u32 = 4;
/// Cap of the proportional (hardness/5) component of [`dig_grace`]
/// (~200 ms) — a long dig doesn't hand a cheat proportionally more.
pub const DIG_GRACE_PROP_MAX: u32 = 12;

/// Completion grace in ticks for a block of the given hardness.
pub fn dig_grace(hardness_ticks: u32) -> u32 {
	DIG_GRACE_BASE + (hardness_ticks / 5).min(DIG_GRACE_PROP_MAX)
}

/// The server re-raycasts each digging client's aim every tick from its
/// own simulated state. A missed tick just doesn't count toward the
/// hold (a pause, not a reset — transient misses are expected from
/// honest clients: look lag, reconciliation camera offsets); only this
/// many CONSECUTIVE misses abandons the hold outright (~500 ms).
pub const DIG_OFF_TARGET_RESET: u32 = 30;

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

/// Interest management: at most this many players per snapshot — self
/// first (the reconciliation anchor), then nearest remotes by column
/// distance. Sized so a full snapshot always fits one datagram.
pub const MAX_SNAPSHOT_PLAYERS: usize = 24;
// NetPlayerState is 37 wire bytes (id + 9×f32); ≤13 bytes of snapshot
// overhead (variant, two varint ticks, vec length).
const _: () = assert!(MAX_SNAPSHOT_PLAYERS * 37 + 13 <= MAX_DATAGRAM);

/// A remote whose newest snapshot lags the estimated clock by this many
/// ticks (2 s) left our interest radius — drop it client-side so it
/// vanishes instead of ghosting at the extrapolation cap. `PlayerLeft`
/// still removes eagerly.
pub const REMOTE_EXPIRE_TICKS: f64 = 120.0;

#[cfg(test)]
mod tests {
	use super::*;

	/// Constant pipeline-lag base plus a hardness-proportional slice for
	/// transient misses, capped so long digs don't hand a cheat more.
	#[test]
	fn dig_grace_scales_with_hardness() {
		assert_eq!(dig_grace(10), 6); // leaves: base + 2
		assert_eq!(dig_grace(20), 8); // water: base + 4
		assert_eq!(dig_grace(30), 10); // dirt: base + 6
		assert_eq!(dig_grace(60), DIG_GRACE_BASE + DIG_GRACE_PROP_MAX); // wood: cap
		assert_eq!(dig_grace(90), DIG_GRACE_BASE + DIG_GRACE_PROP_MAX); // stone: cap
	}

	#[test]
	fn name_rule_is_3_to_10_of_the_alphabet() {
		assert!(validate_name("ted").is_ok());
		assert!(validate_name("Ted_L-99").is_ok());
		assert!(validate_name("abcdefghij").is_ok()); // 10: at the cap
		assert!(validate_name("").is_err());
		assert!(validate_name("ab").is_err()); // too short
		assert!(validate_name("abcdefghijk").is_err()); // 11: too long
		assert!(validate_name("has space").is_err());
		assert!(validate_name("bang!").is_err());
		assert!(validate_name("émile").is_err()); // non-ASCII
	}

	#[test]
	fn chat_sanitizer_keeps_printable_ascii_only() {
		assert_eq!(sanitize_chat("hi there").as_deref(), Some("hi there"));
		// Control chars and non-ASCII are stripped, not replaced.
		assert_eq!(sanitize_chat("a\u{7}b\u{1F600}c").as_deref(), Some("abc"));
		// Whitespace-only collapses to nothing.
		assert_eq!(sanitize_chat("   \t  "), None);
		assert_eq!(sanitize_chat(""), None);
		// Overlong input truncates to the cap (then trims).
		let long = "x".repeat(500);
		assert_eq!(sanitize_chat(&long).unwrap().len(), MAX_CHAT_LEN);
	}
}
