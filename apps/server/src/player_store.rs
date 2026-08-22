//! Persistent per-player state (inventory counts + item layout), keyed by the
//! player's master account id. NOT part of the block journal — the journal is
//! cell-keyed and its compaction discards authorship.
//!
//! Records are tiny, so the format is a whole-file snapshot rewritten atomically
//! at most once per [`FLUSH_INTERVAL`]; a hard crash loses at most that interval,
//! which is deliberate since blocks themselves are journaled. The rewrite runs on
//! a dedicated writer thread ([`StoreWriter`]) because on a container or network
//! volume the fsync is several dropped ticks every interval. Unlike the journal,
//! a store that cannot write does NOT refuse edits.
//!
//! **Every version this project has written still loads**, because the
//! archive-and-start-fresh path is reserved for actual corruption. Two shapes
//! keep that true:
//!
//! - **The file has TWO sections**: records keyed by account, and
//!   [`Self::legacy`] records still keyed by the pre-account device hash, held in
//!   quarantine. A quarantined record is inert and the only thing that can happen
//!   to it is [`PlayerStore::claim_legacy`]; the quarantine only ever SHRINKS, so
//!   it needs no cap.
//! - **The layout blob is SELF-DESCRIBING.** Its leading byte is a shape tag, not
//!   a present/absent flag, so a `GRID_SLOTS` change is a no-op here — no version
//!   bump, no archive path, and no silent re-interpretation of somebody's stored
//!   arrangement.

use digblock_core::hotbar::{HOTBAR_SLOTS, WIRE_EMPTY};
use digblock_core::inv_grid::GRID_SLOTS;
use digblock_core::inventory::Inventory;
use digblock_core::skills::SkillSet;
use rustc_hash::FxHashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = *b"DBI1";
const VERSION: u16 = 9;

/// The version from which the bucket is a CRAFTED tool (SKILLS-DESIGN
/// §13.5). Every record below it was written when the bucket was owned by
/// everyone — the count machinery just never stored one — so the reader
/// grants it, writing down the ownership the old build implied. Never a
/// windfall: records at this version and above own exactly what their
/// counts say, and a brand-new account starts with none. The record BODY
/// is unchanged; the version is the grant's whole trigger.
const FIRST_CRAFTED_BUCKET_VERSION: u16 = 7;

/// The version from which essence is FOUND, not ground (SKILLS-DESIGN
/// §14). Every record below it holds balances earned against the
/// unbounded bench the redesign removed, so the reader clears them —
/// all but Aether, which was always the scarce one. The record BODY is
/// unchanged; the version is the reset's whole trigger, so it runs at
/// most once per record and a fresh account never sees it.
const FIRST_FOUND_ESSENCE_VERSION: u16 = 8;

/// The version whose records are keyed by ACCOUNT rather than by a device
/// hash. Anything below reads wholly into the quarantine; v6 and up say
/// which section each record is in. Frozen: it describes bytes on disk.
const FIRST_ACCOUNT_VERSION: u16 = 5;

/// The version that carries a quarantine section at all. Below it the
/// record list simply ends with the account count.
const FIRST_TWO_SECTION_VERSION: u16 = 6;
const HEADER_LEN: usize = 4 + 2 + 2 + 4; // magic | version | pad | count

/// Shape tags for the per-record layout blob. See the module docs: this
/// byte used to be a bare presence flag, and making it a tag is what
/// lets the grid grow without touching [`VERSION`].
const LAYOUT_ABSENT: u8 = 0;
const LAYOUT_LEGACY: u8 = 1;
const LAYOUT_SIZED: u8 = 2;

/// The grid width [`LAYOUT_LEGACY`] blobs were written at. FROZEN: it
/// describes bytes already on disk, so it must never be re-pointed at
/// [`GRID_SLOTS`], which is free to grow.
const LEGACY_GRID_SLOTS: usize = 36;

/// The hotbar width [`LAYOUT_LEGACY`] blobs were written at. FROZEN for exactly
/// the same reason as [`LEGACY_GRID_SLOTS`], and split out because the legacy arm
/// originally read the LIVE `HOTBAR_SLOTS` — the one half of the blob still
/// described by a constant free to move.
///
/// Grow the bar and every tag-`1` record over-reads by two bytes a slot, desyncs
/// into the next record's key, and `PlayerStore::open` takes that for corruption
/// and archives everybody's inventory. The regression test could not have caught
/// it either, because its fixture was built from the same live constant and
/// would have moved with the break.
const LEGACY_HOTBAR_SLOTS: usize = 9;

/// Both frozen widths describe bytes on disk, so this asserts the FORMAT,
/// not the constants: if today's hotbar ever differs, the legacy arm must
/// keep reading the LEGACY width, and this line is the reminder that it
/// does.
const _: () = assert!(
	LEGACY_HOTBAR_SLOTS == 9 && LEGACY_GRID_SLOTS == 36,
	"legacy layout widths describe bytes already written — never retune them"
);

/// The sized blob spends one byte per length, which is the whole reason
/// growing the grid costs nothing here. Fail the BUILD rather than
/// truncating somebody's arrangement to `len & 255`. Two asserts, not one
/// conjunction: today's values make either side imply the other, which
/// clippy reads as a redundant comparison rather than as the guard on
/// future growth it is.
const _: () = assert!(
	HOTBAR_SLOTS <= u8::MAX as usize,
	"the hotbar length is a u8 on disk — widen the blob before growing past 255"
);
const _: () = assert!(
	GRID_SLOTS <= u8::MAX as usize,
	"the grid length is a u8 on disk — widen the blob before growing past 255"
);

/// Dirty-state flush cadence.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Account cap: beyond this, the least-recently-USED record is evicted on
/// insert. Bounds the file; an evicted player's state is lost.
///
/// Least-recently-used, not first-inserted. The old policy never refreshed a
/// key's position, so eviction walked strictly in join order and threw out the
/// longest-standing players first — including ones currently online and actively
/// writing.
///
/// It used to bound the file without bounding the ABUSE, since the key was
/// client-chosen and join tokens were free. That is closed upstream now: a
/// record needs a master account, an account needs a real third-party login, and
/// `/api/join` is rate-limited per session.
const MAX_IDENTITIES: usize = 10_000;

/// The stored key: the player's master account id.
///
/// Sixteen bytes, exactly as before — a UUID is the same width as the
/// truncated hash it replaced, which is why v5 rewrote the meaning of this
/// field and nothing else about the format. Its byte order is a FILE
/// FORMAT: every record is filed under it, so a parser that read the id
/// differently would orphan every player silently, the store still loading
/// and simply never finding anybody. That is pinned where the type lives,
/// by `protocol::account_ids_round_trip_through_their_canonical_text`.
// A `use` alias rather than a `type` alias so the tuple constructor comes
// with it — this module builds keys from raw bytes when it parses a file.
pub use digblock_net::protocol::AccountId as IdentityKey;

/// A player's item arrangement in hotbar wire encoding (`WIRE_EMPTY`/
/// `WIRE_TOOL`/block id), already sanitized by the sim before storage.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlayerLayout {
	pub hotbar: [u16; HOTBAR_SLOTS],
	pub grid: [u16; GRID_SLOTS],
}

#[derive(Clone, Default, Debug)]
pub struct PlayerRecord {
	pub inventory: Inventory,
	/// `None` = never stored (v1 data, or a pre-layout disconnect); the
	/// sim derives the deterministic default instead.
	pub layout: Option<PlayerLayout>,
	/// Eye position at last disconnect. `None` = pre-v3 data or a first
	/// join; the sim starts those players at the resolved spawn.
	///
	/// Resuming here is what closes the rejoin exploit: without it,
	/// reconnecting was a free trip to spawn that ignored the
	/// return-to-spawn cooldown entirely.
	pub pos: Option<[f32; 3]>,
	/// Server tick of the player's last return-to-spawn, so the cooldown
	/// survives a reconnect too — resuming in place would otherwise still
	/// leave "rejoin to reset the timer" open.
	///
	/// Ticks are per-run and reset to 0 on restart, so a stored tick from
	/// a previous run reads as long expired (`wrapping_sub` yields a huge
	/// interval). That fails open across restarts, which is the safe
	/// direction for a convenience cooldown.
	pub last_return_tick: Option<u32>,
	/// Per-skill XP totals. Pre-v4 records read as empty with their
	/// inventories intact — existing players keep their stuff and start the
	/// skill system at the bottom like everyone else.
	pub skills: SkillSet,
	/// Waystones this account has ARRIVED at, as `SurfaceKind` indices —
	/// the once-per-account first-arrival milestone's memory. Pre-v9
	/// records read as empty: the milestone re-pays at most once, which
	/// is the forgiving direction.
	pub visited_waystones: Vec<u8>,
}

impl PlayerRecord {
	/// This record with `old` folded in — the legacy-character migration's whole
	/// definition of what a player gets back. Four rules, each the conservative
	/// reading of "the same person owned both":
	///
	/// - **Stuff adds**, saturating at every cap, so owning a tool on both sides
	///   still means owning one ([`Inventory::merge_from`]).
	/// - **XP takes the higher total, never the sum** — one career, two keys.
	/// - **Where you are, and your spawn cooldown, stay where the ACCOUNT left
	///   them.** A claim must not teleport a player who is standing somewhere,
	///   and inheriting a stale cooldown would hand them a free return; the old
	///   values are used only when the account has none.
	/// - **The arrangement likewise**, with one difference: whichever layout
	///   survives is about to be run through `reconcile_layout` against the
	///   MERGED counts, so items that gained a slot get filed rather than lost.
	fn merged_with(&self, old: &Self) -> Self {
		let mut inventory = self.inventory.clone();
		inventory.merge_from(&old.inventory);
		let mut skills = self.skills.clone();
		skills.merge_from(&old.skills);
		let mut visited_waystones = self.visited_waystones.clone();
		for w in &old.visited_waystones {
			if !visited_waystones.contains(w) {
				visited_waystones.push(*w);
			}
		}
		Self {
			inventory,
			layout: self.layout.clone().or_else(|| old.layout.clone()),
			pos: self.pos.or(old.pos),
			last_return_tick: self.last_return_tick.or(old.last_return_tick),
			skills,
			visited_waystones,
		}
	}
}

pub struct PlayerStore {
	map: FxHashMap<IdentityKey, PlayerRecord>,
	/// Pre-account characters, still keyed by the truncated hash of the
	/// device secret that owned them. See the module docs: inert until
	/// claimed, and delete-only after load — no LRU stamps, because
	/// nothing ever touches an entry except the claim that removes it.
	legacy: FxHashMap<[u8; 16], PlayerRecord>,
	/// LRU stamp per key, from `clock`. Eviction scans for the minimum,
	/// which is O(n) but only runs when the store is at `MAX_IDENTITIES` —
	/// `put` itself stays O(1), and it is called on every inventory-changing
	/// edit, so it has to be.
	used: FxHashMap<IdentityKey, u64>,
	clock: u64,
	dirty: bool,
	last_flush: Instant,
	writer: StoreWriter,
}

impl PlayerStore {
	/// Missing file → empty store. An unreadable file is archived to
	/// `.vN.bak` and the store starts fresh (mirrors the journal's policy
	/// — never crash the server over persisted state), and that path is
	/// for CORRUPTION only: every format version this project has written
	/// is read, pre-account ones into the quarantine.
	pub fn open(dir: &Path) -> io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join("inventories.bin");
		let mut store = Self {
			map: FxHashMap::default(),
			legacy: FxHashMap::default(),
			used: FxHashMap::default(),
			clock: 0,
			dirty: false,
			last_flush: Instant::now(),
			writer: StoreWriter::new(path.clone()),
		};
		let bytes = match std::fs::read(&path) {
			Ok(b) => b,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(store),
			Err(e) => return Err(e),
		};
		match parse(&bytes) {
			Ok(file) => {
				// Stamp in file order, which `write_snapshot` wrote
				// oldest-used first — so the LRU ranking survives a restart
				// instead of resetting to whatever the hash map iterates.
				for (key, rec) in file.accounts {
					store.clock += 1;
					store.used.insert(key, store.clock);
					store.map.insert(key, rec);
				}
				if !file.legacy.is_empty() {
					log::info!(
						"player store: {} pre-account character(s) awaiting a claim",
						file.legacy.len()
					);
				}
				store.legacy = file.legacy.into_iter().collect();
			}
			Err(e) => {
				let bak = bak_path(&path);
				log::warn!(
					"player store unreadable ({e}) — archiving to {} and starting fresh",
					bak.display()
				);
				let _ = std::fs::rename(&path, &bak);
			}
		}
		Ok(store)
	}

	/// Fold this server's pre-account character for `legacy` into `account`,
	/// and delete the quarantined copy. `None` when nothing is held under that
	/// key.
	///
	/// **Deleting the source IS the one-time-ness.** There is no "claimed" flag
	/// to keep in step with anything: a second attempt looks exactly like a
	/// first attempt for a key that was never here, which is also what every
	/// client that keeps its old identity file sends on every join forever. Two
	/// accounts racing the same key resolve the same way — first through takes
	/// it.
	///
	/// The merge is defined by [`PlayerRecord::merged_with`], and it is a merge
	/// rather than an overwrite because the account may already have played
	/// here.
	pub fn claim_legacy(&mut self, legacy: [u8; 16], account: IdentityKey) -> Option<PlayerRecord> {
		let old = self.legacy.remove(&legacy)?;
		let merged = self.get(&account).merged_with(&old);
		self.put(account, merged.clone());
		// `put` already marked the store dirty, which covers the removal
		// too — both halves ride the same snapshot.
		Some(merged)
	}

	/// How many pre-account characters are still unclaimed. Diagnostics
	/// and tests; the sim has no reason to ask.
	pub fn legacy_len(&self) -> usize {
		self.legacy.len()
	}

	/// Plant a quarantined record without going through a v1-v4 file.
	///
	/// Tests only, and deliberately not a general `put_legacy`: nothing in
	/// the running server may add to the quarantine, or "delete-only after
	/// load" — the property that makes a claim one-time — stops holding.
	#[cfg(test)]
	pub fn test_put_legacy(&mut self, legacy: [u8; 16], rec: PlayerRecord) {
		self.legacy.insert(legacy, rec);
		self.dirty = true;
	}

	/// A player's persisted record; default (empty, no layout) when
	/// unknown.
	pub fn get(&self, id: &IdentityKey) -> PlayerRecord {
		self.map.get(id).cloned().unwrap_or_default()
	}

	/// Write-through to memory; durable at the next flush. Touching a key
	/// refreshes its LRU stamp, so an active player is never the eviction
	/// candidate.
	pub fn put(&mut self, id: IdentityKey, rec: PlayerRecord) {
		self.clock += 1;
		self.used.insert(id, self.clock);
		self.map.insert(id, rec);
		if self.map.len() > MAX_IDENTITIES {
			self.evict_lru();
		}
		self.dirty = true;
	}

	fn evict_lru(&mut self) {
		let Some(&victim) = self
			.used
			.iter()
			.min_by_key(|&(_, &stamp)| stamp)
			.map(|(id, _)| id)
		else {
			return;
		};
		self.map.remove(&victim);
		self.used.remove(&victim);
	}

	/// Called once per sim tick: flush if dirty and the interval passed.
	pub fn maintain(&mut self) {
		// A failed background write leaves the file behind the memory it
		// was meant to capture, so re-arm and try again next interval.
		if self.writer.write_failed() {
			self.dirty = true;
		}
		if self.dirty && self.last_flush.elapsed() >= FLUSH_INTERVAL {
			self.flush();
		}
	}

	/// Coalesced flush for a disconnect: writes only if the interval has
	/// elapsed, exactly like `maintain`.
	///
	/// This used to force a flush unconditionally. Every write rebuilds and
	/// fsyncs the WHOLE file (see `write_snapshot`), on the sim thread, so
	/// a server emptying out — a crowd dropping in one tick — paid a
	/// full-file rewrite and an fsync each, inside a single tick's budget.
	/// The module already accepts losing up to `FLUSH_INTERVAL` of changes
	/// to a hard crash; there is no reason a disconnect should be stricter
	/// than a crash.
	pub fn sync_soon(&mut self) {
		self.maintain();
	}

	/// Unconditional flush, then WAIT for it to reach disk. Shutdown only —
	/// see `sync_soon` for the per-disconnect path, which is the one that
	/// must not block the sim thread.
	pub fn sync(&mut self) {
		if self.dirty {
			self.flush();
		}
		self.writer.shutdown();
	}

	/// Hand a fresh snapshot to the writer thread. Returns immediately —
	/// the sim thread never touches the filesystem.
	fn flush(&mut self) {
		self.last_flush = Instant::now();
		let buf = self.encode_snapshot();
		self.writer.submit(buf);
		self.dirty = false;
	}

	/// Encode into a buffer borrowed from the writer's free list.
	///
	/// Two sections: the account-keyed records, then the quarantined
	/// pre-account ones behind their own count. The quarantine is written
	/// back on every flush precisely because it is not otherwise touched —
	/// a snapshot format that only wrote what changed would drop it on the
	/// first save after boot, which is the whole point of keeping it.
	fn encode_snapshot(&mut self) -> Vec<u8> {
		let mut buf = self.writer.take_buffer();
		buf.clear();
		buf.reserve(HEADER_LEN + (self.map.len() + self.legacy.len()) * 128);
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&VERSION.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
		// Oldest-used first, so a reload can rebuild the LRU ranking from
		// file order alone (see `open`).
		let mut keys: Vec<&IdentityKey> = self.map.keys().collect();
		keys.sort_unstable_by_key(|k| self.used.get(*k).copied().unwrap_or(0));
		// One scratch buffer for every record, not one Vec each: this loop
		// runs over the whole store on every flush.
		let mut pairs: Vec<(u16, u32)> = Vec::new();
		for key in keys {
			let Some(rec) = self.map.get(key) else {
				continue;
			};
			write_record(&mut buf, &key.0, rec, &mut pairs);
		}
		// The quarantine. Sorted so a flush is byte-stable across runs —
		// nothing depends on the order, and an unstable one turns every
		// save into a spurious whole-file difference.
		buf.extend_from_slice(&(self.legacy.len() as u32).to_le_bytes());
		let mut legacy: Vec<&[u8; 16]> = self.legacy.keys().collect();
		legacy.sort_unstable();
		for key in legacy {
			let Some(rec) = self.legacy.get(key) else {
				continue;
			};
			write_record(&mut buf, key, rec, &mut pairs);
		}
		buf
	}
}

/// One record: 16-byte key, then the body every version since v4 shares.
/// `pairs` is a caller-owned scratch buffer — this runs over the whole
/// store on every flush, so it must not allocate per record.
fn write_record(
	buf: &mut Vec<u8>,
	key: &[u8; 16],
	rec: &PlayerRecord,
	pairs: &mut Vec<(u16, u32)>,
) {
	rec.inventory.write_wire(pairs);
	buf.extend_from_slice(key);
	buf.push(pairs.len().min(u8::MAX as usize) as u8);
	for (block, count) in pairs.iter().take(u8::MAX as usize) {
		buf.extend_from_slice(&block.to_le_bytes());
		buf.extend_from_slice(&count.to_le_bytes());
	}
	// The layout blob, shape-tagged. Always the sized form: it is the only
	// shape this build has ever written.
	match &rec.layout {
		None => buf.push(LAYOUT_ABSENT),
		Some(layout) => {
			buf.push(LAYOUT_SIZED);
			buf.push(layout.hotbar.len() as u8);
			buf.push(layout.grid.len() as u8);
			for w in layout.hotbar.iter().chain(layout.grid.iter()) {
				buf.extend_from_slice(&w.to_le_bytes());
			}
		}
	}
	// v3: last position and return-to-spawn tick, each flagged.
	match &rec.pos {
		None => buf.push(0),
		Some(pos) => {
			buf.push(1);
			for c in pos {
				buf.extend_from_slice(&c.to_le_bytes());
			}
		}
	}
	match &rec.last_return_tick {
		None => buf.push(0),
		Some(t) => {
			buf.push(1);
			buf.extend_from_slice(&t.to_le_bytes());
		}
	}
	// v4: per-skill XP — u8 pair count, then (u8 skill, u64 LE total)
	// pairs, nonzero totals only (absent = zero).
	let skills = rec.skills.to_wire();
	buf.push(skills.len().min(u8::MAX as usize) as u8);
	for (skill, total) in skills.iter().take(u8::MAX as usize) {
		buf.push(*skill);
		buf.extend_from_slice(&total.to_le_bytes());
	}
	// v9: visited waystones — u8 count, then the kind indices.
	buf.push(rec.visited_waystones.len().min(u8::MAX as usize) as u8);
	for w in rec.visited_waystones.iter().take(u8::MAX as usize) {
		buf.push(*w);
	}
}

/// Atomic whole-file replace: tmp + fsync + rename.
fn write_atomic(path: &Path, buf: &[u8]) -> io::Result<()> {
	let tmp = path.with_extension("bin.tmp");
	{
		let mut f = std::fs::File::create(&tmp)?;
		f.write_all(buf)?;
		f.sync_all()?;
	}
	std::fs::rename(&tmp, path)
}

/// The background writer: owns the fsync, so the sim thread does not.
///
/// A flush rebuilds the WHOLE file (records are tiny; the format is a
/// snapshot) and fsyncs it. That is milliseconds on local SSD and tens of
/// milliseconds on a container or network volume — several dropped sim ticks
/// every `FLUSH_INTERVAL`, taken straight out of the tick budget. Encoding
/// stays on the sim thread (it needs the live map); only the I/O moves.
///
/// Submissions COALESCE: the channel holds one buffer, and a newer
/// snapshot replaces an unstarted older one. Falling behind therefore
/// costs staleness, never unbounded queueing — and the newest state always
/// wins, which is the only thing the format can express anyway.
struct StoreWriter {
	tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
	/// Buffers handed back by the writer, to be filled again.
	spare: crossbeam_channel::Receiver<Vec<u8>>,
	/// Set by the writer when a write fails, cleared when one succeeds.
	/// The sim thread reads it to know the snapshot on disk is stale and
	/// re-arms `dirty` for another attempt.
	failed: Arc<AtomicBool>,
	handle: Option<std::thread::JoinHandle<()>>,
	path: PathBuf,
}

impl StoreWriter {
	fn new(path: PathBuf) -> Self {
		// Shallow by design: a slow disk coalesces rather than queueing.
		let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
		let (spare_tx, spare) = crossbeam_channel::bounded::<Vec<u8>>(2);
		let failed = Arc::new(AtomicBool::new(false));
		let (wpath, wfailed) = (path.clone(), failed.clone());
		let handle = std::thread::Builder::new()
			.name("player-store".into())
			.spawn(move || {
				let mut logged = false;
				while let Ok(buf) = rx.recv() {
					match write_atomic(&wpath, &buf) {
						Ok(()) => {
							wfailed.store(false, Ordering::Relaxed);
							logged = false;
						}
						Err(e) => {
							// Degrade, don't refuse: the world is journaled
							// elsewhere, so keep serving from memory and let
							// the sim re-arm a retry. Log once per streak.
							wfailed.store(true, Ordering::Relaxed);
							if !logged {
								log::error!("player store write failed (serving from memory): {e}");
								logged = true;
							}
						}
					}
					let _ = spare_tx.try_send(buf);
				}
			})
			.expect("spawn player-store writer");
		Self {
			tx: Some(tx),
			spare,
			failed,
			handle: Some(handle),
			path,
		}
	}

	fn take_buffer(&self) -> Vec<u8> {
		self.spare.try_recv().unwrap_or_default()
	}

	fn submit(&self, buf: Vec<u8>) {
		let Some(tx) = &self.tx else {
			// Shut down: write inline so a post-shutdown flush is not lost.
			let _ = write_atomic(&self.path, &buf);
			return;
		};
		if let Err(crossbeam_channel::TrySendError::Full(buf)) = tx.try_send(buf) {
			// Replace the queued-but-unstarted snapshot with this newer one.
			let _ = self.spare.try_recv();
			let _ = tx.try_send(buf);
		}
	}

	fn write_failed(&self) -> bool {
		self.failed.load(Ordering::Relaxed)
	}

	/// Drop the sender and join, so a queued write finishes before the
	/// process exits.
	fn shutdown(&mut self) {
		self.tx = None;
		if let Some(h) = self.handle.take() {
			let _ = h.join();
		}
	}
}

impl Drop for StoreWriter {
	fn drop(&mut self) {
		self.shutdown();
	}
}

/// Decode a layout blob of `hotbar_len + grid_len` words into the CURRENT shape,
/// padding short halves with `WIRE_EMPTY` and dropping any beyond today's
/// constants.
///
/// It always consumes exactly the words the writer declared, whatever the
/// constants are now — that is the part that matters. A reader that stopped
/// early would leave the remainder to be read as the next field and desync the
/// whole record stream, which is precisely how a grid resize turns into "archive
/// the file and start fresh".
///
/// Truncation is lossy by construction, but `reconcile_layout` re-files anything
/// that lost its slot on the first pass after load.
fn read_layout(
	r: &mut &[u8],
	hotbar_len: usize,
	grid_len: usize,
	bad: &dyn Fn(&str) -> io::Error,
) -> io::Result<PlayerLayout> {
	let mut hotbar = [WIRE_EMPTY; HOTBAR_SLOTS];
	let mut grid = [WIRE_EMPTY; GRID_SLOTS];
	for i in 0..hotbar_len + grid_len {
		let mut b = [0u8; 2];
		r.read_exact(&mut b).map_err(|_| bad("short record"))?;
		let w = u16::from_le_bytes(b);
		let slot = if i < hotbar_len {
			hotbar.get_mut(i)
		} else {
			grid.get_mut(i - hotbar_len)
		};
		if let Some(slot) = slot {
			*slot = w;
		}
	}
	Ok(PlayerLayout { hotbar, grid })
}

/// What one file holds: the account-keyed records, and the quarantined
/// pre-account ones still under their device hash.
struct StoreFile {
	accounts: Vec<(IdentityKey, PlayerRecord)>,
	legacy: Vec<([u8; 16], PlayerRecord)>,
}

fn parse(bytes: &[u8]) -> io::Result<StoreFile> {
	let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
	let mut r = bytes;
	let mut header = [0u8; HEADER_LEN];
	r.read_exact(&mut header).map_err(|_| bad("short header"))?;
	if header[..4] != MAGIC {
		return Err(bad("bad magic"));
	}
	let version = u16::from_le_bytes([header[4], header[5]]);
	if !(1..=VERSION).contains(&version) {
		// A version this build has never written: a downgrade from a newer
		// server, or bytes that happen to start with our magic. Both are the
		// corruption case, and the caller archives rather than guessing.
		return Err(bad("unsupported version"));
	}
	let count = u32::from_le_bytes(header[8..12].try_into().unwrap());
	let mut first = Vec::with_capacity(count as usize);
	for _ in 0..count {
		first.push(read_record(&mut r, version, &bad)?);
	}
	// What that first section MEANS is the version's whole job here. Before
	// v5 every key is a device hash and the section is wholly quarantine;
	// from v5 on it is wholly accounts, and v6 adds the quarantine after it.
	let (accounts, mut legacy) = if version >= FIRST_ACCOUNT_VERSION {
		let accounts = first
			.into_iter()
			.map(|(key, rec)| (IdentityKey(key), rec))
			.collect();
		(accounts, Vec::new())
	} else {
		(Vec::new(), first)
	};
	if version >= FIRST_TWO_SECTION_VERSION {
		let mut n = [0u8; 4];
		r.read_exact(&mut n)
			.map_err(|_| bad("short legacy count"))?;
		for _ in 0..u32::from_le_bytes(n) {
			legacy.push(read_record(&mut r, version, &bad)?);
		}
	}
	Ok(StoreFile { accounts, legacy })
}

/// One record: the 16-byte key, then whichever sections `version` wrote.
///
/// The `version >=` guards are back, and they carry real weight again: v5
/// briefly refused everything older, which let them go, and reading v1-v4
/// forward into the quarantine is exactly what the migration needs. A v1
/// record ends after its inventory, a v2 after its layout, a v3 after its
/// position — each absent section reads as the empty value it always did.
fn read_record(
	r: &mut &[u8],
	version: u16,
	bad: &dyn Fn(&str) -> io::Error,
) -> io::Result<([u8; 16], PlayerRecord)> {
	let mut key = [0u8; 16];
	r.read_exact(&mut key).map_err(|_| bad("short record"))?;
	let mut n = [0u8; 1];
	r.read_exact(&mut n).map_err(|_| bad("short record"))?;
	let mut pairs = Vec::with_capacity(n[0] as usize);
	for _ in 0..n[0] {
		let mut pair = [0u8; 6];
		r.read_exact(&mut pair).map_err(|_| bad("short record"))?;
		pairs.push((
			u16::from_le_bytes([pair[0], pair[1]]),
			u32::from_le_bytes(pair[2..6].try_into().unwrap()),
		));
	}
	// v1 records end here (inventory only): layout stays None.
	let layout = if version >= 2 {
		let mut tag = [0u8; 1];
		r.read_exact(&mut tag).map_err(|_| bad("short record"))?;
		match tag[0] {
			LAYOUT_ABSENT => None,
			// Written before the grid grew: a fixed run at the widths of the
			// day, decoded into today's shape.
			LAYOUT_LEGACY => Some(read_layout(r, LEGACY_HOTBAR_SLOTS, LEGACY_GRID_SLOTS, bad)?),
			LAYOUT_SIZED => {
				let mut lens = [0u8; 2];
				r.read_exact(&mut lens).map_err(|_| bad("short record"))?;
				Some(read_layout(r, lens[0] as usize, lens[1] as usize, bad)?)
			}
			_ => return Err(bad("unknown layout tag")),
		}
	} else {
		None
	};
	// v1/v2 records end before these: the player resumes at spawn once,
	// then stores a position from their next disconnect on.
	let (pos, last_return_tick) = if version >= 3 {
		let mut flag = [0u8; 1];
		r.read_exact(&mut flag).map_err(|_| bad("short record"))?;
		let pos = if flag[0] == 1 {
			let mut c = [0f32; 3];
			for v in c.iter_mut() {
				let mut b = [0u8; 4];
				r.read_exact(&mut b).map_err(|_| bad("short record"))?;
				*v = f32::from_le_bytes(b);
			}
			Some(c)
		} else {
			None
		};
		r.read_exact(&mut flag).map_err(|_| bad("short record"))?;
		let tick = if flag[0] == 1 {
			let mut b = [0u8; 4];
			r.read_exact(&mut b).map_err(|_| bad("short record"))?;
			Some(u32::from_le_bytes(b))
		} else {
			None
		};
		(pos, tick)
	} else {
		(None, None)
	};
	// v1-v3 records end here: skills read empty — bottom of the curve, with
	// the inventory intact.
	let skills = if version >= 4 {
		let mut n = [0u8; 1];
		r.read_exact(&mut n).map_err(|_| bad("short record"))?;
		let mut pairs = Vec::with_capacity(n[0] as usize);
		for _ in 0..n[0] {
			let mut b = [0u8; 9];
			r.read_exact(&mut b).map_err(|_| bad("short record"))?;
			pairs.push((b[0], u64::from_le_bytes(b[1..9].try_into().unwrap())));
		}
		SkillSet::from_wire(&pairs)
	} else {
		SkillSet::default()
	};
	// v1-v8 records read as never-arrived; the milestone forgives once.
	let visited_waystones = if version >= 9 {
		let mut n = [0u8; 1];
		r.read_exact(&mut n).map_err(|_| bad("short record"))?;
		let mut v = vec![0u8; n[0] as usize];
		r.read_exact(&mut v).map_err(|_| bad("short record"))?;
		v
	} else {
		Vec::new()
	};
	let mut inventory = Inventory::from_wire(&pairs);
	if version < FIRST_CRAFTED_BUCKET_VERSION {
		inventory.add(digblock_core::block::BlockType::Bucket, 1);
	}
	if version < FIRST_FOUND_ESSENCE_VERSION {
		inventory.clear_pre_redesign_essence();
	}
	Ok((
		key,
		PlayerRecord {
			inventory,
			layout,
			pos,
			last_return_tick,
			skills,
			visited_waystones,
		},
	))
}

/// First free `.vN.bak[M]` slot so repeated breaks never overwrite an
/// earlier archive.
fn bak_path(path: &Path) -> PathBuf {
	for n in 0.. {
		let candidate = if n == 0 {
			path.with_extension(format!("bin.v{VERSION}.bak"))
		} else {
			path.with_extension(format!("bin.v{VERSION}.bak{n}"))
		};
		if !candidate.exists() {
			return candidate;
		}
	}
	unreachable!()
}

#[cfg(test)]
mod tests;
