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

/// The wire contract both ends must agree on. Exchanged in the handshake
/// preamble; a peer outside `MIN_PROTOCOL..=PROTOCOL_VERSION` is refused.
///
/// Bump on ANY change to a message's shape or meaning. Postcard encodes enum
/// discriminants by declaration order, so variants and fields are
/// APPEND-ONLY — never reorder, never remove. `git log -S PROTOCOL_VERSION`
/// is the history of what each bump carried.
pub const PROTOCOL_VERSION: u16 = 31;

/// Chunk-column radius around an arrival — a join or a teleport — whose
/// journaled columns must reach the client before it may render or
/// simulate. A small radius is the bounded choice: at
/// `JOIN_SYNC_PER_TICK` the near set clears in a handful of ticks on any
/// real world, while the rest keeps streaming in behind the player as it
/// always has.
///
/// Sized in COLUMNS rather than blocks because that is the unit the
/// journal is indexed and shipped in ([`ServerMsg::JournalColumn`]), so
/// the gate can be answered exactly rather than estimated.
pub const AREA_SYNC_RADIUS: i32 = 3;

/// How long the client will wait for [`ServerMsg::AreaReady`] before
/// giving up and entering the world anyway.
///
/// A hold that can hang is worse than the artifact it removes: the marker
/// rides the reliable stream, but a server wedged mid-sync, or one whose
/// outbound queue never opens the room join sync reserves, would strand
/// the player on a loading screen with no way out. The wait is far past
/// the few ticks the radius needs on a healthy link, and the fallback is
/// exactly today's behaviour.
pub const AREA_SYNC_TIMEOUT_SECS: f32 = 5.0;
/// Oldest protocol this build still speaks. From v2 on, `Hello` and
/// `Welcome` are append-only across the supported range, so a decoder for
/// version N can read any peer >= N (postcard tolerates trailing bytes).
///
/// Two kinds of break raise this floor, and only one of them is about
/// decoding. A field REMOVED from, or added INSIDE, an existing variant
/// makes an older decoder run off the end of the frame. The subtler kind is
/// a peer that decodes everything perfectly and finds its buttons no longer
/// buy anything — a client whose bulk craft silently does nothing, or whose
/// fresh character holds a phantom bucket. Refusing the handshake is the
/// honest answer to both: a range that admits a peer whose UI lies is worse
/// than a narrow one.
///
/// v31 is the floor for the second reason. A cell is a raw id on the wire
/// and `BlockType::from_id` is the FORGIVING decode world data goes
/// through, so an id past an older peer's `COUNT` reads as Air: it would
/// render a stained pane as a hole, walk through it, and have every dig
/// near it refused, while the server holds the cell solid. Adding block
/// rows therefore raises the floor as surely as moving a field does.
pub const MIN_PROTOCOL: u16 = 31;

// ---- handshake preamble ----------------------------------------------
//
// Both directions of the control stream start with this one fixed-length
// preamble, sent before any length-prefixed postcard frame. FROZEN
// FOREVER — never versioned, never extended — so any two builds can
// always exchange versions and refuse each other cleanly instead of
// hitting a postcard decode error on a differently-shaped Hello.

/// The preamble opens with this magic; anything else is not a peer.
pub const PREAMBLE_MAGIC: [u8; 4] = *b"DIGB";
/// [`PREAMBLE_MAGIC`], then the protocol version as u16 LE, then the
/// reserved zeros.
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

/// Display-name rule: a bounded run of `a-z A-Z 0-9 - _`. One source of
/// truth for the menu's typing filter and join gate, the launchers'
/// name arguments, and (mirrored as a regex) the master's /api/join
/// backstop. Distinct from [`MAX_NAME_LEN`], which stays the wire-level
/// sanitization cap.
pub const MIN_DISPLAY_NAME: usize = 3;
pub const MAX_DISPLAY_NAME: usize = 10;

/// The display-name alphabet. ASCII-only on purpose, and NOT for want of
/// glyphs — the client draws the whole Latin repertoire (see
/// [`is_text_char`]). Names are mirrored as a regex in the master's
/// /api/join backstop, so widening this means changing the validator,
/// the master, and the desktop services filler in lockstep, then
/// redeploying the master ahead of any server release.
pub fn is_name_char(c: char) -> bool {
	c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// The free-text alphabet, shared by chat and local world names:
/// printable ASCII, Latin-1 Supplement, Latin Extended-A, and the
/// punctuation people paste. Covers the Western European, Nordic, Baltic
/// and Central European languages.
///
/// This is the wire authority — the server sanitizes to it. The client's
/// embedded face is required to draw every character in it
/// (`font::ui_face_covers_the_whole_repertoire` pins that), so a
/// player's accents can never arrive as `?`.
const TEXT_RANGES: &[(u32, u32)] = &[
	(0x0020, 0x007E),
	(0x00A0, 0x00FF),
	(0x0100, 0x017F),
	(0x2013, 0x2014),
	(0x2018, 0x201D),
	(0x20AC, 0x20AC),
];

/// Holes in [`TEXT_RANGES`]. `U+00AD` is an invisible formatting
/// character — accepting it would let someone "say" nothing at all —
/// and `U+0149` is a deprecated letter our face doesn't carry.
const TEXT_EXCLUDED: &[u32] = &[0x00AD, 0x0149];

pub fn is_text_char(c: char) -> bool {
	let u = c as u32;
	!TEXT_EXCLUDED.contains(&u) && TEXT_RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&u))
}

/// Every character in the free-text alphabet, in code point order.
/// Client-side font coverage tests walk this.
pub fn text_charset() -> impl Iterator<Item = char> {
	TEXT_RANGES
		.iter()
		.flat_map(|&(lo, hi)| lo..=hi)
		.filter_map(char::from_u32)
		.filter(|&c| is_text_char(c))
}

/// Validate a display name against [`MIN_DISPLAY_NAME`]..=
/// [`MAX_DISPLAY_NAME`] and [`is_name_char`]. The `Err` string is shown
/// verbatim as the menu's status-line alert, so it spells the rule out.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
	let n = name.chars().count();
	if !(MIN_DISPLAY_NAME..=MAX_DISPLAY_NAME).contains(&n) || !name.chars().all(is_name_char) {
		return Err("name must be 3-10 chars: a-z 0-9 - _");
	}
	Ok(())
}

// ---- account identity ------------------------------------------------

/// A master-issued account id: the UUID that identifies a player everywhere, and
/// the one thing in this system that is not self-asserted.
///
/// It reaches a game server ONLY from the master, as the answer to
/// `/api/tokens/verify` — never from the client, which is exactly what makes it
/// worth keying on. Server-side it is the persistence key for inventories and
/// skills, the value `--admin` compares against, and what a ban records.
///
/// It is public, not secret: knowing someone's account id lets you name them,
/// not be them.
///
/// Lives here because this crate owns the master contract and is wasm-clean, so
/// the browser client and the dedicated server share one parser. Parsed by hand
/// rather than pulling in a UUID crate — this crate stays dependency-light.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct AccountId(pub [u8; 16]);

/// Where the dashes go in the canonical textual form.
const UUID_GROUPS: [usize; 5] = [4, 2, 2, 2, 6];

impl AccountId {
	/// All zeros — "no account". Offline play and the loopback tests have
	/// no master to ask, so they carry this.
	pub const NONE: Self = Self([0; 16]);

	pub fn is_none(&self) -> bool {
		self.0 == [0; 16]
	}

	/// Parse the canonical hyphenated form, and also the bare 32-hex form
	/// that .NET's "N" format and a hand-typed id can produce. Case
	/// insensitive; anything else is `None`.
	pub fn parse(s: &str) -> Option<Self> {
		// Dashes are allowed but not arbitrary: either none, or exactly the
		// canonical five groups at exactly the canonical offsets.
		//
		// The offsets are the load-bearing half. Counting dashes and checking
		// the total length is not enough — 32 hex digits plus four dashes is
		// the same total WHEREVER the dashes fall, so an id grouped 8-4-4-8-8
		// passed a length-only rule while reading as well-formed everywhere it
		// was later printed.
		let b = s.as_bytes();
		if b.contains(&b'-') {
			if b.len() != 32 + UUID_GROUPS.len() - 1 {
				return None;
			}
			let mut at = 0;
			for group in &UUID_GROUPS[..UUID_GROUPS.len() - 1] {
				at += group * 2;
				if b[at] != b'-' {
					return None;
				}
				at += 1;
			}
		}
		let mut bytes = [0u8; 16];
		let mut hex = s.bytes().filter(|&b| b != b'-');
		for byte in &mut bytes {
			let hi = from_hex(hex.next()?)?;
			let lo = from_hex(hex.next()?)?;
			*byte = hi << 4 | lo;
		}
		if hex.next().is_some() {
			return None;
		}
		Some(Self(bytes))
	}
}

fn from_hex(b: u8) -> Option<u8> {
	match b {
		b'0'..=b'9' => Some(b - b'0'),
		b'a'..=b'f' => Some(b - b'a' + 10),
		b'A'..=b'F' => Some(b - b'A' + 10),
		_ => None,
	}
}

impl core::fmt::Display for AccountId {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		let mut i = 0;
		for (g, len) in UUID_GROUPS.iter().enumerate() {
			if g != 0 {
				f.write_str("-")?;
			}
			for _ in 0..*len {
				write!(f, "{:02x}", self.0[i])?;
				i += 1;
			}
		}
		Ok(())
	}
}

impl core::fmt::Debug for AccountId {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "AccountId({self})")
	}
}

/// The key a pre-account character is filed under: truncated SHA-256 of the
/// device secret that owned it. What `ClientMsg::ClaimLegacy` carries.
///
/// **This is a FILE FORMAT, not a choice.** It reproduces exactly what
/// pre-account servers computed over the client's identity secret, because that
/// is what their `inventories.bin` records are keyed by. Move it by a byte and
/// every migration silently finds nothing — the store still loads and simply
/// never matches anybody. Pinned against an external vector by
/// `the_legacy_key_still_matches_what_old_servers_stored`.
///
/// Lives here, and not in either launcher, because BOTH must agree with it and
/// with each other: a player who used the desktop and the browser has one old
/// character per server.
pub fn legacy_identity_hash(secret: &[u8; 16]) -> [u8; 16] {
	use sha2::{Digest, Sha256};
	Sha256::digest(secret)[..16]
		.try_into()
		.expect("digest is 32 bytes")
}

/// The same key, from the form the BROWSER stored its secret in: 32
/// lowercase hex characters in `localStorage["digblock.identity"]`.
/// `None` for anything that is not exactly that, including the all-zero
/// secret a blocked-storage session used to fall back to.
///
/// Here rather than in the web launcher because it is the only step of the
/// client's half that can silently differ from what the old client WROTE —
/// a slicing slip decodes a different secret, hashes to a key no server
/// holds, and the migration quietly finds nothing on every server forever.
/// In the launcher it is unreachable from a test; here
/// `the_two_launchers_derive_one_key_from_one_secret` covers it.
pub fn legacy_identity_from_hex(hex: &str) -> Option<[u8; 16]> {
	let b = hex.as_bytes();
	if b.len() != 32 {
		return None;
	}
	let mut secret = [0u8; 16];
	for (i, out) in secret.iter_mut().enumerate() {
		*out = from_hex(b[i * 2])? << 4 | from_hex(b[i * 2 + 1])?;
	}
	(secret != [0; 16]).then(|| legacy_identity_hash(&secret))
}

/// Chat message cap, in CHARS. Not bytes: accented characters are two
/// bytes each in UTF-8, so a full line can encode to twice the cap —
/// still nowhere near [`MAX_FRAME`].
pub const MAX_CHAT_LEN: usize = 120;

/// Chat token bucket: sustained rate (burst [`CHAT_BURST`]).
pub const CHAT_RATE_PER_SEC: u32 = 1;
pub const CHAT_BURST: f32 = 3.0;

/// Sanitize a chat line: keep [`is_text_char`] only (which strips every
/// control and formatting character), truncate to [`MAX_CHAT_LEN`],
/// trim; `None` when nothing survives (the server drops the message
/// silently).
pub fn sanitize_chat(text: &str) -> Option<String> {
	let clean: String = text
		.chars()
		.filter(|&c| is_text_char(c))
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

/// Nothing at all received for this long AFTER the handshake → treat the session
/// as dead.
///
/// A backstop, not the primary mechanism: normally the transport reports the
/// disconnect and this never fires. It covers the case where the transport
/// CANNOT report — a native background thread that died without reaching its
/// `disconnect` call, a WebTransport session wedged rather than closed — which
/// otherwise leaves the engine in a multiplayer session with no snapshots, no
/// error and no way back.
///
/// Sized off [`TOD_SYNC_TICKS`]: the server broadcasts time-of-day to EVERY
/// client, live or not, so even a player still waiting for their spawn column
/// has a heartbeat. The margin past the last missed beat is the point, since
/// firing exactly on it would make ordinary scheduling jitter enough to drop a
/// healthy session.
pub const RECV_TIMEOUT_SECS: f32 = 35.0;
const _: () = assert!(RECV_TIMEOUT_SECS > 3.0 * (TOD_SYNC_TICKS / TICK_RATE) as f32);

/// Simulation rate. Fixed: `Player::tick` air control hardcodes the
/// matching divisor, which is what the tripwire below pins.
pub const TICK_RATE: u32 = 60;

// The adoption envelope replays the identical `Player::tick` on client and
// server; that only holds if this rate and core's fixed tick describe the
// same clock. Compile-time tripwire, not a convention.
const _: () = assert!(digblock_core::player::TICK == 1.0 / TICK_RATE as f32);

/// Snapshots go out every Nth sim tick, so the snapshot rate is
/// [`TICK_RATE`] over this.
pub const SNAPSHOT_DIVISOR: u32 = 3;

/// Time-of-day heartbeat cadence in sim ticks. The clients'
/// frame-integrated suns drift microscopically between beats; each
/// `ServerMsg::TimeOfDay` re-anchors them, and it also catches clients
/// whose clock paused entirely (backgrounded tab, suspended laptop).
pub const TOD_SYNC_TICKS: u32 = 10 * TICK_RATE;

/// Each input datagram repeats the last N unacked ticks so isolated
/// datagram loss never costs the server an input.
pub const INPUT_REDUNDANCY: usize = 4;

/// Send an input datagram at most once per this many predicted ticks, so
/// the send rate is [`TICK_RATE`] over this. [`INPUT_REDUNDANCY`] covers
/// a lost datagram at this spacing with margin to spare; input→adoption
/// latency grows with the stride, so don't raise it casually.
/// High-refresh displays used to send per FRAME, most of them duplicates.
pub const INPUT_SEND_STRIDE: u32 = 2;

/// Datagram payload budget, safely under the QUIC minimum MTU.
pub const MAX_DATAGRAM: usize = 1150;

/// Reliable-stream frame cap for SERVER→client frames. Journal batches
/// are the big ones: a densely edited column is thousands of cells.
pub const MAX_FRAME: usize = 1 << 20;

/// Reliable-stream frame cap for CLIENT→server frames.
///
/// Nothing a client legitimately sends is large — the widest is a `Layout`
/// at a few hundred bytes, and `max_client_frame_fits_every_client_message`
/// pins that. Sharing the server's [`MAX_FRAME`] cap meant a client could
/// push `Layout` frames of that size (a `Vec<u16>` truncated only later,
/// during sanitization) straight into the sim's action queue:
/// amplification, not a heap bomb, since `MSG_BUDGET` bounds the rate —
/// but a cap orders of magnitude past any real message is not a cap.
pub const MAX_CLIENT_FRAME: usize = 4096;
const _: () = assert!(MAX_CLIENT_FRAME < MAX_FRAME);

/// Dig/place: claimed eye origin may differ from the server's simulated
/// position by at most this many blocks. Sized for honest divergence:
/// the client predicts [`CLIENT_TICK_LEAD`] ticks ahead of the sim state
/// it is compared against (a fraction of a block at walk speed, more when
/// falling) and aims from a camera still carrying the decaying
/// reconciliation offset (up to [`POS_TOLERANCE`]) — at the tighter value
/// this replaced, the sum occasionally rejected legitimate digs made
/// while moving.
pub const EDIT_POS_TOLERANCE: f32 = 1.5;

/// Timed digging: a completing Dig/Collect is accepted when the server
/// has counted at least `hardness - dig_grace(hardness)` aim-verified
/// ticks on the hold. The grace is what a cheat gains per dig, so it is
/// kept small — but it must cover the honest shortfall, which has two
/// parts: a CONSTANT pipeline lag every dig pays regardless of hardness
/// (the first verify lands a tick after DigStart and the server's look
/// trails the client's by the input jitter buffer — a few ticks even on
/// localhost), plus transient aim misses that scale with how long the
/// hold runs. The old single `clamp` folded both into one floor, which
/// left cheap blocks (leaves, water) barely any real slack — any one late
/// input datagram bounced an honest dig.
pub const DIG_GRACE_BASE: u32 = 4;
/// Cap on [`dig_grace`]'s hardness-proportional component — a long dig
/// doesn't hand a cheat proportionally more.
pub const DIG_GRACE_PROP_MAX: u32 = 12;

/// Completion grace in ticks for a block of the given hardness.
pub fn dig_grace(hardness_ticks: u32) -> u32 {
	DIG_GRACE_BASE + (hardness_ticks / 5).min(DIG_GRACE_PROP_MAX)
}

/// The server re-raycasts each digging client's aim every tick from its
/// own simulated state. A missed tick just doesn't count toward the
/// hold (a pause, not a reset — transient misses are expected from
/// honest clients: look lag, reconciliation camera offsets); only this
/// many CONSECUTIVE misses ends it.
///
/// "Ends" no longer means "discards": the hold then bleeds away over
/// `skills::DIG_DECAY_TICKS` exactly as a released one does, since a
/// client that has been off this cell for half a second has been
/// decaying its own copy the whole time.
pub const DIG_OFF_TARGET_RESET: u32 = 30;

/// Movement adoption envelope: a claimed post-tick position within this
/// distance of the server's own simulation is adopted as authoritative,
/// re-zeroing drift so honest clients never rubber-band.
pub const POS_TOLERANCE: f32 = 0.75;

/// Sanity clamp on per-tick displacement, blocks/s — set clear of
/// terminal velocity, which is itself well clear of a walk. Claims that
/// move faster than this are never adopted.
pub const MAX_SPEED: f32 = 25.0;

/// Drift EWMA smoothing; the averaging window is its reciprocal, in
/// ticks, which is a couple of seconds' worth. The server keeps a moving
/// average of each adopted claim's (claim − sim) delta VECTOR. Honest
/// deltas are zero-mean float noise and cancel; riding the envelope edge
/// requires a persistent directional bias that accumulates instead.
pub const DRIFT_ALPHA: f32 = 1.0 / 120.0;

/// Sustained-drift ceiling, blocks per tick of EWMA magnitude. Claims
/// stop being adopted while the average sits above this, capping the
/// exploitable envelope gain (this times [`TICK_RATE`]) at a fraction of
/// walking pace — honest noise sits orders of magnitude below.
pub const DRIFT_TOLERANCE: f32 = 0.02;

/// Edit token bucket: sustained rate (burst [`EDIT_BURST`]).
pub const EDIT_RATE_PER_SEC: u32 = 10;
pub const EDIT_BURST: f32 = 5.0;

/// Catch-up threshold for the server's per-client input drain, ticks.
///
/// Despite the name this was never a buffer depth — it is only the point
/// above which the drain takes TWO inputs in a tick, which is what pays back
/// the delay a hold introduces. The depth itself is set by
/// [`INPUT_HOLD_MAX`]; raising this number alone does nothing, as the
/// netbench's `depth` column showed — barely over a tick even on a clean
/// LAN, and that remainder was entirely [`INPUT_SEND_STRIDE`] burstiness.
pub const JITTER_BUFFER_TICKS: u32 = 2;

/// How long the input drain will hold a HOLE in the tick sequence open, waiting
/// for the missing tick to arrive, before giving up and skipping it.
///
/// This is the server's actual reorder tolerance. `handle_inputs` refuses
/// anything at or below the consumed high-water mark, so an input that arrives
/// after the drain has stepped past its tick is lost for good. Heavy jitter
/// reorders datagrams, which is exactly how a bad link loses a far larger share
/// of its INPUTS than of its packets — redundancy cannot help, because every
/// copy of a tick is equally old.
///
/// A hold costs latency, but only ever converts DIVERGENCE into it: held inputs
/// are applied in order, just later, so the server still lands on the claim the
/// client sent. A link with no holes pays literally zero.
///
/// The cap is set by measurement, not taste (`just bench-net` swept it from no
/// hold up past this depth). Up to this depth the claimed edit origin sits
/// CLOSER to the server's own position than with no hold at all, on every lossy
/// link measured, because the alternative to holding is coasting on stale intent
/// and a coast diverges faster than a delay does. Deeper, it climbs, and the
/// deepest rungs put it OVER [`EDIT_POS_TOLERANCE`], where honest edits begin
/// bouncing. Delivery is flat past here too; the worst link keeps improving
/// deeper still but buys it with a replay rate above the no-hold baseline, which
/// is the symptom this was meant to remove.
///
/// RULE: [`EDIT_POS_TOLERANCE`] and [`dig_grace`] must never be scaled by the
/// observed depth. A deeper buffer SPENDS those fixed budgets; making them
/// elastic would hand anyone sending deliberately gappy input ticks a
/// proportionally wider envelope — `verify_digs` casts
/// `REACH + EDIT_POS_TOLERANCE`, so that is extra reach, bought by pretending to
/// have a bad connection. Their fixedness is what keeps this free of cheat
/// surface.
pub const INPUT_HOLD_MAX: u32 = 4;

/// How far ahead of the server clock a fresh client starts predicting.
///
/// This sets the client's starting tick LABEL and nothing else: the server
/// never compares `input.tick` against its own clock, only against the
/// per-client high-water mark, and `Predictor::tick_lead` has no consumer
/// outside stats and the debug overlay. Buffer depth is set by arrival
/// timing, which tick numbering cannot influence — so the arithmetic
/// relationship to [`JITTER_BUFFER_TICKS`] below is a historical
/// coincidence, not a coupling. Its only real dependants are the loopback
/// tests that do this same arithmetic.
pub const CLIENT_TICK_LEAD: u32 = JITTER_BUFFER_TICKS + 3;

/// Remote players render this many ticks behind the estimated server
/// clock, interpolating between snapshots.
pub const INTERP_DELAY_TICKS: f64 = 6.0;

/// Max extrapolation past the newest snapshot, in ticks, before a remote
/// player holds position.
pub const EXTRAP_CAP_TICKS: f64 = 15.0;

/// Interest management: at most this many players per snapshot — self
/// (the reconciliation anchor) plus the nearest remotes by column
/// distance. Sized so a full snapshot always fits one datagram.
///
/// v14's narrower `RemoteState` is what bought the last rise: the same
/// datagram now carries substantially more players than it used to.
///
/// This must hold for the WORST-CASE encoding, not the typical one — an
/// oversize snapshot does not truncate, it fails to encode, and that
/// client gets no snapshot at all. (Exactly the cliff interest management
/// was added to close.)
pub const MAX_SNAPSHOT_PLAYERS: usize = 40;
// The assert below sizes the WORST-case encoding of each part. A
// RemoteState is its id, a fixed-width f32 position, then velocity, yaw
// and held as postcard VARINTS — which take their widest encoding at the
// extremes, not their typical one. A real entry is far smaller (a walking
// velocity encodes short), but the cap has to survive the extremes.
// SelfState adds its Option tag on top; the snapshot itself carries the
// datagram variant, two varint ticks and the vec length.
const _: () = assert!(25 + (MAX_SNAPSHOT_PLAYERS - 1) * 28 + 13 <= MAX_DATAGRAM);

/// A remote whose newest snapshot lags the estimated clock by this many
/// ticks left our interest radius — drop it client-side so it vanishes
/// instead of ghosting at the extrapolation cap. `PlayerLeft` still
/// removes eagerly.
///
/// Must stay comfortably above `SNAPSHOT_DIVISOR * FAR_PLAYER_STRIDE`
/// ticks, or the distance tiering below would expire the very players it
/// is only sending slowly.
pub const REMOTE_EXPIRE_TICKS: f64 = 120.0;

/// Column radius inside which a remote player rides EVERY snapshot.
///
/// Beyond it they ride one snapshot in [`FAR_PLAYER_STRIDE`] instead of
/// being dropped, because "far" here is not "invisible": the selection
/// radius is the client's own draw distance, at which a player still
/// subtends a few pixels on an ordinary display and FOV. Cutting the
/// radius instead would delete people you can plainly see. This nearer
/// radius is where a player grows big enough on screen for smooth motion
/// to be worth full rate.
pub const PLAYER_NEAR_COLS: i32 = 8;

/// How often a remote beyond [`PLAYER_NEAR_COLS`] is included: every Nth
/// snapshot, so their rate is the snapshot rate over this.
///
/// The client interpolates [`INTERP_DELAY_TICKS`] behind and extrapolates
/// along velocity up to [`EXTRAP_CAP_TICKS`], so this wider spacing
/// degrades into mostly-extrapolation for these players — worth a
/// fraction of a block of positional error at a walk, on a figure that
/// small on screen. Which player falls in which snapshot is phased by id,
/// so far players spread across the cycle rather than all landing in one
/// datagram.
pub const FAR_PLAYER_STRIDE: u32 = 4;
const _: () = assert!(
	REMOTE_EXPIRE_TICKS as u32 > SNAPSHOT_DIVISOR * FAR_PLAYER_STRIDE * 4,
	"a far player's send interval must sit far inside the client's expiry"
);

#[cfg(test)]
mod tests {
	use super::*;

	/// The migration key must still be the one pre-account servers filed
	/// their records under, or every claim quietly matches nothing.
	///
	/// This vector is the one the old server-side `hash_identity` was
	/// pinned to, carried over verbatim when that function was deleted —
	/// which is the point: the value is fixed by bytes already on players'
	/// disks, and this crate is now the only place it is computed. A
	/// swapped digest crate, a different truncation, an accidental hash of
	/// the hex rather than the bytes: each would still pass a test that
	/// only checked the function agreed with itself.
	#[test]
	fn the_legacy_key_still_matches_what_old_servers_stored() {
		let want = [
			0xd7, 0x61, 0xd4, 0x06, 0xaf, 0x2a, 0x4a, 0x5a, 0x15, 0xf6, 0x7c, 0x92, 0x43, 0x78,
			0xed, 0x88,
		];
		assert_eq!(legacy_identity_hash(&[7; 16]), want);
		// And it is the hash, never the secret: a launcher that shipped the
		// raw file contents would hand every server the old bearer
		// credential, which is the thing v23 removed.
		assert_ne!(legacy_identity_hash(&[7; 16]), [7; 16]);
	}

	/// Desktop reads 16 raw bytes from a file; the browser reads 32 hex
	/// characters out of localStorage. Same player, same secret, so they
	/// must reach the same key — a player who used both has ONE old
	/// character per server, and two decodings would give them one key that
	/// works and one that silently matches nothing.
	///
	/// The hex here is written the way the old web client wrote it
	/// (`format!("{b:02x}")` over the secret), which is what is sitting in
	/// browsers now.
	#[test]
	fn the_two_launchers_derive_one_key_from_one_secret() {
		let secret: [u8; 16] = [
			0x00, 0x01, 0x0f, 0x10, 0x7f, 0x80, 0xa5, 0xff, 0x3c, 0x42, 0x99, 0xde, 0x01, 0x02,
			0x03, 0x04,
		];
		let stored: String = secret.iter().map(|b| format!("{b:02x}")).collect();
		assert_eq!(stored.len(), 32, "the shape the old client wrote");
		assert_eq!(
			legacy_identity_from_hex(&stored),
			Some(legacy_identity_hash(&secret)),
			"the browser's stored form and the file's raw bytes are one key"
		);

		// Uppercase is not what the old client wrote, but costs nothing to
		// accept and would otherwise strand a hand-edited entry.
		assert_eq!(
			legacy_identity_from_hex(&stored.to_uppercase()),
			Some(legacy_identity_hash(&secret))
		);

		// Anything else is "nothing to claim", never a wrong key.
		assert_eq!(legacy_identity_from_hex(""), None);
		assert_eq!(legacy_identity_from_hex(&stored[..30]), None, "too short");
		assert_eq!(
			legacy_identity_from_hex(&format!("{stored}00")),
			None,
			"too long"
		);
		assert_eq!(legacy_identity_from_hex(&"g".repeat(32)), None, "not hex");
		// The all-zero secret was the blocked-storage fallback; it names no
		// character, and hashing it would produce a real-looking key that
		// every such browser would send.
		assert_eq!(legacy_identity_from_hex(&"0".repeat(32)), None);
	}

	/// The id the master prints and the id we parse have to be the same
	/// string: an operator copies one out of the game and pastes it into
	/// `--admin`, so a byte-order slip here would silently grant admin to
	/// nobody.
	#[test]
	fn account_ids_round_trip_through_their_canonical_text() {
		// Taken verbatim from the master's dev sign-in for subject "admin".
		let text = "b2379408-3102-8b68-b56b-5774dec269b8";
		let id = AccountId::parse(text).expect("canonical form parses");
		assert_eq!(id.to_string(), text);
		assert_eq!(id.0[0], 0xb2);
		assert_eq!(id.0[15], 0xb8);

		// The bare 32-hex form (.NET's "N") is the same id.
		assert_eq!(AccountId::parse(&text.replace('-', "")), Some(id));
		// Case-insensitive.
		assert_eq!(AccountId::parse(&text.to_uppercase()), Some(id));
	}

	#[test]
	fn a_malformed_account_id_is_refused_rather_than_truncated() {
		assert_eq!(AccountId::parse(""), None);
		assert_eq!(AccountId::parse("not-a-uuid"), None);
		// Too short, too long, and a non-hex digit.
		assert_eq!(
			AccountId::parse("b2379408-3102-8b68-b56b-5774dec269b"),
			None
		);
		assert_eq!(
			AccountId::parse("b2379408-3102-8b68-b56b-5774dec269b8f"),
			None
		);
		assert_eq!(
			AccountId::parse("g2379408-3102-8b68-b56b-5774dec269b8"),
			None
		);
		// Dashes in the wrong places: the right hex, the wrong shape.
		assert_eq!(
			AccountId::parse("b2-379408-3102-8b68-b56b-5774dec269b8"),
			None
		);
		// The same, at the canonical LENGTH — 32 hex plus four dashes,
		// grouped 8-4-4-8-8. A length-only rule accepted this, so an id
		// that never round-trips to itself was read as well-formed.
		assert_eq!(
			AccountId::parse("b2379408-3102-8b68-b56b5774-dec269b8"),
			None
		);
		// And the degenerate case the length rule was written for.
		assert_eq!(AccountId::parse("0-0-0-0-0"), None);
		assert!(AccountId::NONE.is_none());
		assert_eq!(
			AccountId::NONE.to_string(),
			"00000000-0000-0000-0000-000000000000"
		);
	}

	/// Constant pipeline-lag base plus a hardness-proportional slice for
	/// transient misses, capped so long digs don't hand a cheat more.
	///
	/// Asserted over the real catalog's hardnesses as the three
	/// properties the anti-cheat rests on — never under the base, never
	/// over the cap, never falling as a dig gets longer. The individual
	/// products are the function's own arithmetic, and the hold lengths
	/// that used to be its inputs were the hardness table restated in a
	/// crate that cannot see it.
	#[test]
	fn dig_grace_scales_with_hardness() {
		let mut prev = 0;
		let mut capped = false;
		for h in 0..=200 {
			let g = dig_grace(h);
			assert!(g >= DIG_GRACE_BASE, "grace {g} under the base at {h}");
			assert!(
				g <= DIG_GRACE_BASE + DIG_GRACE_PROP_MAX,
				"grace {g} over the cap at {h}"
			);
			assert!(g >= prev, "grace fell from {prev} to {g} at {h}");
			capped |= g == DIG_GRACE_BASE + DIG_GRACE_PROP_MAX;
			prev = g;
		}
		assert_eq!(
			dig_grace(0),
			DIG_GRACE_BASE,
			"a free dig gets the base alone"
		);
		assert!(
			capped,
			"the cap is never reached, so it is not being tested"
		);
	}

	/// The bounds come from the constants the validator itself reads, so
	/// widening the rule is one edit and not two — the old name of this
	/// test carried the numbers in it, which is the same restatement one
	/// level further out.
	#[test]
	fn a_name_must_sit_inside_its_length_bounds_and_alphabet() {
		let a = |n: usize| "a".repeat(n);
		assert!(validate_name(&a(MIN_DISPLAY_NAME)).is_ok());
		assert!(validate_name(&a(MAX_DISPLAY_NAME)).is_ok());
		assert!(
			validate_name(&a(MIN_DISPLAY_NAME - 1)).is_err(),
			"too short"
		);
		assert!(
			validate_name(&a(MAX_DISPLAY_NAME + 1)).is_err(),
			"past the cap"
		);
		assert!(validate_name("").is_err());
		// Every length in range is fine as long as the charset is.
		for n in MIN_DISPLAY_NAME..=MAX_DISPLAY_NAME {
			assert!(validate_name(&a(n)).is_ok(), "length {n}");
		}
		// The alphabet, independent of length.
		assert!(validate_name("Ted_L-99").is_ok());
		assert!(validate_name("has space").is_err());
		assert!(validate_name("bang!").is_err());
		assert!(validate_name("émile").is_err(), "non-ASCII");
	}

	#[test]
	fn chat_sanitizer_keeps_the_latin_repertoire() {
		assert_eq!(sanitize_chat("hi there").as_deref(), Some("hi there"));
		// European text survives intact — the whole point of the wider
		// alphabet.
		assert_eq!(
			sanitize_chat("Grüße aus Łódź — čau!").as_deref(),
			Some("Grüße aus Łódź — čau!")
		);
		// Control chars, emoji and other scripts are stripped, not
		// replaced. So is the soft hyphen, which is invisible.
		assert_eq!(
			sanitize_chat("a\u{7}b\u{1F600}c\u{00AD}d").as_deref(),
			Some("abcd")
		);
		assert_eq!(sanitize_chat("Ω あ").as_deref(), None);
		// Whitespace-only collapses to nothing.
		assert_eq!(sanitize_chat("   \t  "), None);
		assert_eq!(sanitize_chat(""), None);
		// Overlong input truncates to the cap in CHARS, not bytes — an
		// accented line is two bytes per char.
		let long = "x".repeat(500);
		assert_eq!(sanitize_chat(&long).unwrap().chars().count(), MAX_CHAT_LEN);
		let accented = "é".repeat(500);
		let out = sanitize_chat(&accented).unwrap();
		assert_eq!(out.chars().count(), MAX_CHAT_LEN);
		assert_eq!(out.len(), MAX_CHAT_LEN * 2);
	}

	#[test]
	fn text_charset_agrees_with_its_predicate() {
		assert!(text_charset().all(is_text_char));
		assert!(text_charset().count() > 300);
		// Names stay a strict subset — widening them is a cross-service
		// change (see is_name_char).
		assert!(
			"abcABC019-_"
				.chars()
				.all(|c| is_name_char(c) && is_text_char(c))
		);
		assert!(!is_name_char('é') && is_text_char('é'));
	}
}
