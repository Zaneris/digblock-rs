//! Persistent per-player state (inventory counts + item layout), keyed by
//! the player's master account id. NOT part of the block journal — the
//! journal is cell-keyed and its compaction discards authorship, so
//! player-keyed state lives in this separate, much smaller file.
//!
//! The key used to be a hash of a secret the CLIENT chose and sent in its
//! Hello. It is now the account id the master returns from
//! `/api/tokens/verify`, which changes two things beyond the bytes: a
//! player's character follows their account rather than their machine, so
//! signing in on a second device finds it — and a record can no longer be
//! created, claimed, or flooded by anyone who simply picks a number.
//!
//! Records are tiny, so the format is a whole-file snapshot rewritten
//! atomically (tmp + fsync + rename) at most once per [`FLUSH_INTERVAL`].
//! A hard crash loses at most the last interval of changes — deliberate:
//! blocks themselves are journaled, counts are low-stakes.
//!
//! The rewrite happens on a dedicated writer thread ([`StoreWriter`]).
//! Encoding stays on the caller's thread — it needs the live map — but the
//! fsync does not, and that is the expensive half: milliseconds on local
//! SSD, tens of milliseconds on a container or network volume, which on the
//! sim thread is several dropped ticks every interval. Disconnects go through
//! [`PlayerStore::sync_soon`], which coalesces onto the same cadence
//! instead of forcing a write each; only shutdown
//! ([`PlayerStore::sync`]) waits for the writer.
//!
//! Unlike the journal, a store that cannot write does NOT refuse edits:
//! the world stays consistent either way, only persistence degrades. It
//! logs loudly and keeps serving from memory.
//!
//! Format v2 appended an optional item layout (hotbar + grid wire
//! arrangements) per record; v3 the player's last position and
//! return-to-spawn cooldown; v4 per-skill XP totals. Each was an append,
//! read behind a `version >=` guard, because the archive-and-start-fresh
//! path is reserved for actual corruption and never a routine upgrade —
//! it would wipe every player's inventory.
//!
//! **v5 changed what the 16-byte key MEANS** — from a hash of a device
//! secret to a master account id — without changing a single byte of a
//! record's body. A v1–v4 record is therefore structurally fine and
//! semantically orphaned: nothing on this server can say which account
//! held that device.
//!
//! **v6 is the answer to that**, and it is why the archive path went back
//! to being corruption-only. The file now has TWO sections — records
//! keyed by account, and [`Self::legacy`] records still keyed by the old
//! device hash, held in quarantine. A quarantined record is inert: it
//! belongs to nobody, no join reads it, and the only thing that can
//! happen to it is [`PlayerStore::claim_legacy`], where the player who
//! still holds that device secret folds it into their account and the
//! quarantined copy is deleted. Every version is read: v1–v4 land wholly
//! in quarantine, v5 wholly in the account map, v6 carries both.
//!
//! The quarantine only ever SHRINKS. Nothing writes into it after load —
//! there is no path that creates a device-keyed record any more — so it
//! needs no cap of its own beyond the one the file it came from already
//! had, and every claim is one entry gone for good.
//!
//! The layout blob is SELF-DESCRIBING, which is what keeps growing the
//! inventory grid out of the version field entirely. Its leading byte is
//! a shape tag, not a bare present/absent flag: [`LAYOUT_ABSENT`], or
//! [`LAYOUT_SIZED`] followed by a length-prefixed pair the reader pads or
//! truncates into whatever the current constants are. So a `GRID_SLOTS`
//! change is a no-op here — no version bump, no archive path, and no
//! silent re-interpretation of somebody's stored arrangement, which is
//! the failure this shape exists to make impossible. That property is why
//! the tag survived the key change: the reasons the two exist are
//! unrelated, and only one of them was ever about the version field.

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
const VERSION: u16 = 7;

/// The version from which the bucket is a CRAFTED tool (SKILLS-DESIGN
/// §13.5). Every record below it was written when the bucket was owned by
/// everyone — the count machinery just never stored one — so the reader
/// grants it, writing down the ownership the old build implied. Never a
/// windfall: records at this version and above own exactly what their
/// counts say, and a brand-new account starts with none. The record BODY
/// is unchanged; the version is the grant's whole trigger.
const FIRST_CRAFTED_BUCKET_VERSION: u16 = 7;

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

/// The hotbar width [`LAYOUT_LEGACY`] blobs were written at. FROZEN for
/// exactly the same reason as [`LEGACY_GRID_SLOTS`], and split out because
/// the legacy arm originally read the LIVE `HOTBAR_SLOTS` — the one half of
/// the blob still described by a constant free to move.
///
/// It happens to equal `HOTBAR_SLOTS` today, so nothing was wrong; it was
/// wrong in waiting. Grow the bar and every tag-`1` record over-reads by two
/// bytes a slot, desyncs into the next record's key, and `PlayerStore::open`
/// takes that for corruption and archives everybody's inventory — precisely
/// the failure the shape tag was introduced to make impossible,
/// reintroduced through the half of the blob nobody froze. The regression
/// test could not have caught it either, because its fixture was built from
/// the same live constant and would have moved with the break.
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
/// insert. Bounds the file; an evicted (absent) player's state is lost.
///
/// Least-recently-used, not first-inserted. The old policy never refreshed
/// a key's position, so eviction walked strictly in join order and threw
/// out the longest-standing players first — including ones currently online
/// and actively writing.
///
/// This used to bound the file without bounding the ABUSE: the key was
/// client-chosen and join tokens were free, so a stream of join/disconnect
/// cycles evicted every real record (NET-AUDIT-2.md §1.5). That is closed
/// upstream now rather than here — a record needs a master account, an
/// account needs a real Google or Facebook login, and `/api/join` is
/// rate-limited per session. The cap is back to being what it says it is.
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
}

impl PlayerRecord {
	/// This record with `old` folded in — the legacy-character migration's
	/// whole definition of what a player gets back.
	///
	/// Four rules, and each is the conservative reading of "the same person
	/// owned both":
	///
	/// - **Stuff adds**, saturating at every cap, so owning a tool on both
	///   sides still means owning one ([`Inventory::merge_from`]).
	/// - **XP takes the higher total, never the sum** — one career, walked
	///   on two keys ([`SkillSet::merge_from`]).
	/// - **Where you are, and your spawn cooldown, stay where the ACCOUNT
	///   left them.** A claim must not teleport a player who is standing
	///   somewhere, and inheriting a stale cooldown tick would hand them a
	///   free return; the old values are used only when the account has
	///   none, which is the first-join case the migration is actually for.
	/// - **The arrangement likewise**, with one difference that matters:
	///   whichever layout survives is about to be run through
	///   `reconcile_layout` against the MERGED counts, so items that gained
	///   a slot get filed rather than lost.
	fn merged_with(&self, old: &Self) -> Self {
		let mut inventory = self.inventory.clone();
		inventory.merge_from(&old.inventory);
		let mut skills = self.skills.clone();
		skills.merge_from(&old.skills);
		Self {
			inventory,
			layout: self.layout.clone().or_else(|| old.layout.clone()),
			pos: self.pos.or(old.pos),
			last_return_tick: self.last_return_tick.or(old.last_return_tick),
			skills,
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

	/// Fold this server's pre-account character for `legacy` into
	/// `account`, and delete the quarantined copy. `None` when nothing is
	/// held under that key — already claimed, never played here, or simply
	/// not this player's device.
	///
	/// **Deleting the source IS the one-time-ness.** There is no "claimed"
	/// flag to keep in step with anything: a second attempt looks exactly
	/// like a first attempt for a key that was never here, which is also
	/// what every client that keeps its old identity file will send on
	/// every join forever. Two accounts racing the same key resolve the
	/// same way — the first one through takes it.
	///
	/// The merge is defined by [`PlayerRecord::merged_with`]. It is a merge
	/// rather than an overwrite because the account may already have played
	/// here, and a player who owns both piles should not have to choose.
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

/// Decode a layout blob of `hotbar_len + grid_len` words into the CURRENT
/// shape, padding short halves with `WIRE_EMPTY` and dropping any beyond
/// today's constants.
///
/// It always consumes exactly the words the writer declared, whatever the
/// constants are now — that is the part that matters. A reader that
/// stopped early would leave the remainder to be read as the next field
/// and desync the whole record stream, which is precisely how a grid
/// resize turns into "archive the file and start fresh".
///
/// Truncation is lossy by construction (a shrinking grid has nowhere to
/// put the tail), but `reconcile_layout` re-files anything that lost its
/// slot on the first pass after load, so no item is lost — only its
/// placement.
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
	let mut inventory = Inventory::from_wire(&pairs);
	if version < FIRST_CRAFTED_BUCKET_VERSION {
		inventory.add(digblock_core::block::BlockType::Bucket, 1);
	}
	Ok((
		key,
		PlayerRecord {
			inventory,
			layout,
			pos,
			last_return_tick,
			skills,
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
mod tests {
	use super::*;
	use digblock_core::block::BlockType;
	use digblock_core::hotbar::WIRE_TOOL;

	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("dbinv-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	/// A distinct account id per test player. The value is arbitrary — what
	/// matters is that two different `n` never collide.
	fn key(n: u8) -> IdentityKey {
		IdentityKey([n; 16])
	}

	/// A distinct pre-account device hash. Same shape as [`key`] and
	/// deliberately a different function: the two are the same 16 bytes and
	/// mean entirely different things, and a test that used one for the
	/// other would read as passing.
	fn device(n: u8) -> [u8; 16] {
		[0x80 | n; 16]
	}

	/// A hand-built v1 file — inventory-only records under device hashes,
	/// which is what a server that ran before accounts has on disk.
	///
	/// Hand-built rather than round-tripped through the writer on purpose:
	/// the writer only emits the current version, so a fixture derived from
	/// it could never stand in for the format this migration exists to read.
	fn legacy_v1_file(players: &[([u8; 16], u32)]) -> Vec<u8> {
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&1u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&(players.len() as u32).to_le_bytes());
		for (hash, dirt) in players {
			buf.extend_from_slice(hash);
			buf.push(1);
			buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
			buf.extend_from_slice(&dirt.to_le_bytes());
		}
		buf
	}

	/// A v4 file of whole records under device hashes — the richest thing
	/// a pre-account server could have written, for the merge rules that
	/// need a position and skills on the old side.
	fn legacy_file(players: &[([u8; 16], PlayerRecord)]) -> Vec<u8> {
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&4u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&(players.len() as u32).to_le_bytes());
		let mut pairs = Vec::new();
		for (hash, rec) in players {
			write_record(&mut buf, hash, rec, &mut pairs);
		}
		buf
	}

	fn layout() -> PlayerLayout {
		let mut hotbar = [WIRE_EMPTY; HOTBAR_SLOTS];
		hotbar[0] = BlockType::Dirt as u16;
		hotbar[8] = WIRE_TOOL;
		let mut grid = [WIRE_EMPTY; GRID_SLOTS];
		grid[17] = BlockType::Stone as u16;
		PlayerLayout { hotbar, grid }
	}

	/// A sized layout blob written at explicit widths, so a fixture can
	/// stand for a file some OTHER build wrote — narrower or wider than
	/// today's grid.
	///
	/// The widths are arguments rather than read from the current
	/// constants on purpose: a fixture built from the live constants moves
	/// with them, so it can never fail on the resize it exists to guard
	/// against. Words past the caller's own layout are `WIRE_EMPTY`.
	fn push_sized_layout(buf: &mut Vec<u8>, l: &PlayerLayout, hotbar_len: usize, grid_len: usize) {
		buf.push(LAYOUT_SIZED);
		buf.push(hotbar_len as u8);
		buf.push(grid_len as u8);
		for i in 0..hotbar_len {
			let w = l.hotbar.get(i).copied().unwrap_or(WIRE_EMPTY);
			buf.extend_from_slice(&w.to_le_bytes());
		}
		for i in 0..grid_len {
			let w = l.grid.get(i).copied().unwrap_or(WIRE_EMPTY);
			buf.extend_from_slice(&w.to_le_bytes());
		}
	}

	/// Eviction is least-recently-USED, not first-inserted. The old policy
	/// never refreshed a key on re-`put`, so it walked strictly in join
	/// order and threw out the longest-standing players first — including
	/// ones online and actively writing, which is exactly who `put` is
	/// being called for.
	#[test]
	fn eviction_spares_the_recently_used() {
		let dir = temp_dir("lru");
		let mut s = PlayerStore::open(&dir).unwrap();
		let veteran = key(1);
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 7);
		s.put(
			veteran,
			PlayerRecord {
				inventory: inv,
				layout: None,
				pos: None,
				last_return_tick: None,
				skills: SkillSet::default(),
			},
		);
		// Fill to the cap with strangers, touching the veteran throughout
		// the way an online player's edits would.
		for i in 0..MAX_IDENTITIES {
			let mut secret = [0u8; 16];
			secret[..8].copy_from_slice(&(i as u64).to_le_bytes());
			secret[8] = 0xff; // never collides with the veteran's [1; 16]
			s.put(IdentityKey(secret), PlayerRecord::default());
			if i % 100 == 0 {
				s.put(veteran, s.get(&veteran));
			}
		}
		assert_eq!(s.map.len(), MAX_IDENTITIES, "cap still bounds the file");
		assert_eq!(
			s.get(&veteran).inventory.count(BlockType::Dirt),
			7,
			"the identity that kept being touched survived the churn"
		);
	}

	/// The interval flush hands off to the writer thread and returns; the
	/// bytes land on disk without the caller ever touching the filesystem.
	/// That is the whole point — a whole-file fsync is milliseconds, and
	/// this used to happen inside `tick_once`.
	#[test]
	fn the_interval_flush_persists_without_blocking_the_caller() {
		let dir = temp_dir("asyncwrite");
		let id = key(1);
		let path = dir.join("inventories.bin");
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			let mut inv = Inventory::default();
			inv.add(BlockType::Dirt, 7);
			s.put(
				id,
				PlayerRecord {
					inventory: inv,
					layout: None,
					pos: None,
					last_return_tick: None,
					skills: SkillSet::default(),
				},
			);
			// Age the flush clock so `maintain` fires this tick.
			s.last_flush = Instant::now() - FLUSH_INTERVAL;
			s.maintain();
			assert!(!s.dirty, "the snapshot was handed to the writer");

			// The write is on another thread — it appears shortly after.
			let deadline = Instant::now() + Duration::from_secs(10);
			while !path.exists() {
				assert!(Instant::now() < deadline, "the writer never wrote");
				std::thread::sleep(Duration::from_millis(5));
			}
		} // drop joins the writer

		let reopened = PlayerStore::open(&dir).unwrap();
		assert_eq!(reopened.get(&id).inventory.count(BlockType::Dirt), 7);
	}

	/// File order is the LRU ranking, so a restart resumes it instead of
	/// resetting to whatever the hash map happens to iterate.
	#[test]
	fn lru_order_survives_a_reopen() {
		let dir = temp_dir("lruorder");
		let old = key(1);
		let new = key(2);
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			s.put(old, PlayerRecord::default());
			s.put(new, PlayerRecord::default());
			s.sync();
		}
		let s = PlayerStore::open(&dir).unwrap();
		assert!(
			s.used[&old] < s.used[&new],
			"the earlier-used identity is still the older one after reload"
		);
	}

	#[test]
	fn round_trips_across_reopen() {
		let dir = temp_dir("roundtrip");
		let id_a = key(1);
		let id_b = key(2);
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			let mut inv = Inventory::default();
			inv.add(BlockType::Dirt, 7);
			inv.add(BlockType::Water, 2); // the bucket's charge: caps at one
			s.put(
				id_a,
				PlayerRecord {
					inventory: inv,
					layout: Some(layout()),
					pos: None,
					last_return_tick: None,
					skills: SkillSet::default(),
				},
			);
			s.put(id_b, PlayerRecord::default());
			s.sync();
		}
		let s = PlayerStore::open(&dir).unwrap();
		let a = s.get(&id_a);
		assert_eq!(a.inventory.count(BlockType::Dirt), 7);
		assert!(a.inventory.bucket_full());
		assert_eq!(a.inventory.count(BlockType::Water), 1);
		assert_eq!(a.layout, Some(layout()));
		let b = s.get(&id_b);
		assert_eq!(b.inventory, Inventory::default());
		assert_eq!(b.layout, None);
		assert!(s.get(&key(3)).layout.is_none());
	}

	/// A pre-account file loads into QUARANTINE, not into the account map
	/// and not into the archive.
	///
	/// Both halves matter. Reading those keys as account ids would hand a
	/// stranger's inventory to whichever account id happened to share the
	/// bytes; archiving them — which is what v5 did — deletes every
	/// pre-account character on the server. The quarantine is the third
	/// option: the records are there, owned by nobody, waiting for the one
	/// player who can still prove they owned that device.
	#[test]
	fn a_pre_account_file_lands_in_quarantine() {
		let dir = temp_dir("v4upgrade");
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("inventories.bin");
		std::fs::write(&path, legacy_v1_file(&[(device(5), 7)])).unwrap();

		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.legacy_len(), 1, "held for a claim");
		assert_eq!(
			s.get(&IdentityKey(device(5))).inventory,
			Inventory::default(),
			"an old record must not be handed to the account whose id shares its bytes"
		);
		assert!(
			!path.with_extension(format!("bin.v{VERSION}.bak")).exists(),
			"an upgrade must never take the archive path"
		);
	}

	/// EVERY version this project has written still reads. v5 briefly
	/// refused the older ones, which would have deleted every pre-account
	/// character the first time a server restarted; the quarantine exists
	/// so it does not have to.
	#[test]
	fn every_format_version_still_reads() {
		for version in 1u16..=VERSION {
			let mut bytes = Vec::new();
			bytes.extend_from_slice(&MAGIC);
			bytes.extend_from_slice(&version.to_le_bytes());
			bytes.extend_from_slice(&[0, 0]);
			bytes.extend_from_slice(&1u32.to_le_bytes());
			bytes.extend_from_slice(&device(7));
			bytes.push(0); // no inventory pairs
			if version >= 2 {
				bytes.push(LAYOUT_ABSENT);
			}
			if version >= 3 {
				bytes.push(0); // no position
				bytes.push(0); // no return tick
			}
			if version >= 4 {
				bytes.push(0); // no skills
			}
			if version >= FIRST_TWO_SECTION_VERSION {
				bytes.extend_from_slice(&0u32.to_le_bytes()); // empty quarantine
			}
			let file = parse(&bytes).unwrap_or_else(|e| panic!("v{version} must read: {e}"));
			// And it lands on the right side of the split, which is the
			// whole reason the version is still consulted.
			let (accounts, legacy) = (file.accounts.len(), file.legacy.len());
			if version >= FIRST_ACCOUNT_VERSION {
				assert_eq!((accounts, legacy), (1, 0), "v{version} is account-keyed");
			} else {
				assert_eq!((accounts, legacy), (0, 1), "v{version} is device-keyed");
			}
			// The bucket migration: a record from before the vessel was
			// crafted owned one implicitly, and the reader writes that
			// down; a current record owns exactly what its counts say —
			// this fixture's says nothing.
			let record = file
				.accounts
				.iter()
				.map(|(_, r)| r)
				.chain(file.legacy.iter().map(|(_, r)| r));
			for r in record {
				let owned = r.inventory.count(digblock_core::block::BlockType::Bucket);
				if version < FIRST_CRAFTED_BUCKET_VERSION {
					assert_eq!(owned, 1, "v{version} records keep their bucket");
				} else {
					assert_eq!(owned, 0, "v{version} records own what they stored");
				}
			}
		}
	}

	/// The claim: one player's old character folded into their account, and
	/// the source gone. Doing it twice is not an error and not a duplicate
	/// — the second attempt finds nothing, exactly as it would on a server
	/// this player never played on.
	#[test]
	fn a_claim_moves_the_character_once_and_only_once() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("claim");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(
			dir.join("inventories.bin"),
			legacy_v1_file(&[(device(1), 7), (device(2), 3)]),
		)
		.unwrap();
		let mut s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.legacy_len(), 2);

		let account = key(40);
		let got = s.claim_legacy(device(1), account).expect("claimed");
		assert_eq!(got.inventory.count(BlockType::Dirt), 7, "the stuff moved");
		assert_eq!(s.get(&account).inventory.count(BlockType::Dirt), 7);
		assert_eq!(s.legacy_len(), 1, "the source is gone, not copied");

		// Again, with the same key: nothing left to take, and the account's
		// stock is unchanged rather than doubled.
		assert!(s.claim_legacy(device(1), account).is_none());
		assert_eq!(s.get(&account).inventory.count(BlockType::Dirt), 7);

		// A key that was never here answers the same way — which is what
		// every client that keeps its old identity file sends forever.
		assert!(s.claim_legacy(device(99), account).is_none());

		// The OTHER player's character is untouched by all of that, and is
		// still claimable by whoever holds its device.
		let other = key(41);
		assert_eq!(
			s.claim_legacy(device(2), other)
				.expect("still there")
				.inventory
				.count(BlockType::Dirt),
			3
		);
		assert_eq!(s.legacy_len(), 0);
		assert_eq!(s.get(&other).skills.xp(Skill::Mining), 0);
	}

	/// A claim MERGES rather than overwrites, because the account may
	/// already have played here and a player who owns both piles should not
	/// have to choose. Each rule is asserted on the side it decides.
	#[test]
	fn a_claim_merges_into_what_the_account_already_had() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("claimmerge");
		std::fs::create_dir_all(&dir).unwrap();
		let mut old = PlayerRecord {
			inventory: Inventory::default(),
			layout: Some(layout()),
			pos: Some([1.0, 2.0, 3.0]),
			last_return_tick: Some(11),
			skills: SkillSet::default(),
		};
		old.inventory.add(BlockType::Dirt, 5);
		old.inventory.add(BlockType::Stone, 9);
		old.skills.add_xp(Skill::Mining, 500);
		old.skills.add_xp(Skill::Alchemy, 10);
		std::fs::write(
			dir.join("inventories.bin"),
			legacy_file(&[(device(3), old)]),
		)
		.unwrap();
		let mut s = PlayerStore::open(&dir).unwrap();

		// The account has been played on this server already.
		let account = key(42);
		let mut mine = PlayerRecord {
			pos: Some([40.0, 50.0, 60.0]),
			last_return_tick: Some(77),
			..Default::default()
		};
		mine.inventory.add(BlockType::Dirt, 2);
		mine.skills.add_xp(Skill::Mining, 100);
		s.put(account, mine);

		let got = s.claim_legacy(device(3), account).expect("claimed");
		assert_eq!(got.inventory.count(BlockType::Dirt), 7, "stuff adds up");
		assert_eq!(got.inventory.count(BlockType::Stone), 9, "and carries over");
		assert_eq!(
			got.skills.xp(Skill::Mining),
			500,
			"XP takes the higher total, never the sum"
		);
		assert_eq!(got.skills.xp(Skill::Alchemy), 10, "and the other side too");
		assert_eq!(
			got.pos,
			Some([40.0, 50.0, 60.0]),
			"a claim must not teleport a player who is standing somewhere"
		);
		assert_eq!(
			got.last_return_tick,
			Some(77),
			"nor hand them a fresh return by inheriting a stale cooldown"
		);
	}

	/// The quarantine survives a flush. It is the half nothing touches, so
	/// a writer that only wrote the live map would drop every unclaimed
	/// character on the first save after boot — silently, and for good.
	#[test]
	fn the_quarantine_survives_a_save_and_reopen() {
		let dir = temp_dir("quarantineround");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(
			dir.join("inventories.bin"),
			legacy_v1_file(&[(device(1), 7), (device(2), 3)]),
		)
		.unwrap();
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			// An ordinary write by an unrelated player: this is what
			// rewrites the file, and what used to lose the quarantine.
			s.put(key(50), PlayerRecord::default());
			s.sync();
		}
		let mut s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.legacy_len(), 2, "both still claimable after a rewrite");
		assert_eq!(
			s.claim_legacy(device(2), key(51))
				.expect("survived as itself")
				.inventory
				.count(BlockType::Dirt),
			3
		);

		// And a claim is durable across a restart, which is the other half
		// of one-time-ness: a server bounce must not resurrect the source.
		s.sync();
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.legacy_len(), 1);
		assert_eq!(s.get(&key(51)).inventory.count(BlockType::Dirt), 3);
	}

	#[test]
	fn corrupt_file_is_archived_and_store_starts_fresh() {
		let dir = temp_dir("corrupt");
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("inventories.bin");
		std::fs::write(&path, b"garbage not a store").unwrap();
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&key(0)).inventory, Inventory::default());
		let bak = path.with_extension(format!("bin.v{VERSION}.bak"));
		assert_eq!(
			std::fs::read(&bak).unwrap(),
			b"garbage not a store",
			"archived byte-intact"
		);
	}

	/// The position and cooldown tick round-trip.
	#[test]
	fn position_and_cooldown_round_trip() {
		let dir = temp_dir("v3pos");
		let id = key(9);
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			s.put(
				id,
				PlayerRecord {
					inventory: Inventory::default(),
					layout: Some(layout()),
					pos: Some([12.5, 130.5, -7.5]),
					last_return_tick: Some(4242),
					skills: SkillSet::default(),
				},
			);
			s.sync();
		}
		let s = PlayerStore::open(&dir).unwrap();
		let rec = s.get(&id);
		assert_eq!(rec.pos, Some([12.5, 130.5, -7.5]));
		assert_eq!(rec.last_return_tick, Some(4242));
	}

	/// A record with no position stored (a player who never disconnected)
	/// reads back absent rather than as some default coordinate.
	#[test]
	fn absent_position_stays_absent() {
		let dir = temp_dir("v3nopos");
		let id = key(10);
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			s.put(
				id,
				PlayerRecord {
					inventory: Inventory::default(),
					layout: None,
					pos: None,
					last_return_tick: None,
					skills: SkillSet::default(),
				},
			);
			s.sync();
		}
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&id).pos, None);
		assert_eq!(s.get(&id).last_return_tick, None);
	}

	/// THE grid-growth test: a complete file written when the grid was
	/// narrower still reads, with every player's items, arrangement,
	/// position and XP intact — no version bump, no archive path.
	///
	/// Growing `GRID_SLOTS` broke exactly this once. The layout blob was a
	/// fixed, length-free run, so a wider reader over-read into the pos
	/// flag, the return tick and the skills blob, desynced into the next
	/// record's key, and `open` took the resulting parse error for
	/// corruption — archiving the file and wiping every player on the first
	/// restart after deploy. Nothing caught it because the other round-trip
	/// fixtures are all built from the CURRENT constants, which move
	/// together. This one declares its widths and does not.
	#[test]
	fn a_file_written_at_the_old_grid_width_still_reads() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("gridgrowth");
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("inventories.bin");
		let id_a = key(21);
		let id_b = key(22);
		let mut skills = SkillSet::default();
		skills.add_xp(Skill::Mining, 146_751);
		skills.add_xp(Skill::Alchemy, 394);
		// A grid narrower than today's, standing for the build that wrote
		// the file. Derived from the live constant so it stays narrow as the
		// grid grows, and asserted to still contain the slot under test —
		// a fixture that quietly stopped covering its own case would pass
		// forever.
		let narrow = GRID_SLOTS - 9;
		assert!(narrow > 17, "the fixture's occupied slot must be in range");

		// Two records, so an over-read desyncs into the second one's key the
		// way it would on a real server.
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&VERSION.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&2u32.to_le_bytes());
		for (id, dirt) in [(id_a, 12u32), (id_b, 5)] {
			buf.extend_from_slice(&id.0);
			buf.push(1);
			buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
			buf.extend_from_slice(&dirt.to_le_bytes());
			push_sized_layout(&mut buf, &layout(), HOTBAR_SLOTS, narrow);
			buf.push(1); // position
			for c in [8.5f32, 133.0, -4.5] {
				buf.extend_from_slice(&c.to_le_bytes());
			}
			buf.push(1); // return tick
			buf.extend_from_slice(&99u32.to_le_bytes());
			let wire = skills.to_wire();
			buf.push(wire.len() as u8);
			for (skill, total) in &wire {
				buf.push(*skill);
				buf.extend_from_slice(&total.to_le_bytes());
			}
		}
		buf.extend_from_slice(&0u32.to_le_bytes()); // empty quarantine
		std::fs::write(&path, &buf).unwrap();

		let mut s = PlayerStore::open(&dir).unwrap();
		assert!(
			!path.with_extension(format!("bin.v{VERSION}.bak")).exists(),
			"a routine grid resize must never take the archive path"
		);
		let a = s.get(&id_a);
		assert_eq!(a.inventory.count(BlockType::Dirt), 12, "items survive");
		assert_eq!(a.pos, Some([8.5, 133.0, -4.5]), "position survives");
		assert_eq!(a.last_return_tick, Some(99));
		assert_eq!(a.skills, skills, "XP survives");
		// The stored arrangement lands on the SAME slots in the wider grid,
		// with the new rows empty — a migration, not a reshuffle.
		let l = a.layout.clone().expect("layout survives");
		assert_eq!(l.hotbar, layout().hotbar);
		assert_eq!(l.grid[17], BlockType::Stone as u16);
		assert!(
			l.grid[narrow..].iter().all(|&w| w == WIRE_EMPTY),
			"the rows the old file never had come back empty"
		);
		// The record behind it parsed too — an over-read would have eaten
		// its key and lost it entirely.
		assert_eq!(s.get(&id_b).inventory.count(BlockType::Dirt), 5);

		// And the next write stores the sized form, which round-trips at
		// the full width.
		s.put(id_a, PlayerRecord { ..a });
		s.sync();
		let s = PlayerStore::open(&dir).unwrap();
		let back = s.get(&id_a).layout.expect("still there");
		assert_eq!(back.grid[17], BlockType::Stone as u16);
		assert_eq!(back.grid.len(), GRID_SLOTS);
		assert_eq!(s.get(&id_a).skills, skills);
	}

	/// The sized blob is read against the lengths it declares, not the
	/// constants of the day, so a file from a FUTURE build (a wider grid)
	/// truncates cleanly instead of desyncing the record stream.
	#[test]
	fn a_wider_stored_grid_truncates_without_desyncing() {
		let dir = temp_dir("widergrid");
		std::fs::create_dir_all(&dir).unwrap();
		let id_a = key(23);
		let id_b = key(24);
		let wide = GRID_SLOTS + 9;
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&VERSION.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&2u32.to_le_bytes());
		for (id, dirt) in [(id_a, 3u32), (id_b, 4)] {
			buf.extend_from_slice(&id.0);
			buf.push(1);
			buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
			buf.extend_from_slice(&dirt.to_le_bytes());
			buf.push(LAYOUT_SIZED);
			buf.push(HOTBAR_SLOTS as u8);
			buf.push(wide as u8);
			for w in layout().hotbar.iter() {
				buf.extend_from_slice(&w.to_le_bytes());
			}
			for i in 0..wide {
				let w = if i == 17 {
					BlockType::Stone as u16
				} else {
					WIRE_EMPTY
				};
				buf.extend_from_slice(&w.to_le_bytes());
			}
			buf.push(0); // no position
			buf.push(0); // no return tick
			buf.push(0); // no skills
		}
		buf.extend_from_slice(&0u32.to_le_bytes()); // empty quarantine
		std::fs::write(dir.join("inventories.bin"), &buf).unwrap();

		let s = PlayerStore::open(&dir).unwrap();
		let a = s.get(&id_a).layout.expect("layout survives");
		assert_eq!(a.grid[17], BlockType::Stone as u16);
		assert_eq!(
			s.get(&id_b).inventory.count(BlockType::Dirt),
			4,
			"the extra words were consumed, not read as the next field"
		);
	}

	/// Skills round-trip alongside everything else.
	#[test]
	fn skills_round_trip() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("v4skills");
		let id = key(12);
		let mut skills = SkillSet::default();
		skills.add_xp(Skill::Mining, 146_751);
		skills.add_xp(Skill::Alchemy, 394);
		{
			let mut s = PlayerStore::open(&dir).unwrap();
			s.put(
				id,
				PlayerRecord {
					inventory: Inventory::default(),
					layout: Some(layout()),
					pos: Some([1.0, 140.0, 2.0]),
					last_return_tick: Some(60),
					skills: skills.clone(),
				},
			);
			s.sync();
		}
		let s = PlayerStore::open(&dir).unwrap();
		let rec = s.get(&id);
		assert_eq!(rec.skills, skills);
		assert_eq!(rec.skills.level(Skill::Mining), 70);
		assert_eq!(rec.layout, Some(layout()));
	}
}
