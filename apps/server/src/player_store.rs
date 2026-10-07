//! Persistent per-player state (inventory counts + item layout), keyed by the player's master
//! account id. NOT part of the block journal — the journal is cell-keyed and its compaction
//! discards authorship.
//!
//! Records are tiny, so the format is a whole-file snapshot rewritten atomically at most once per
//! [`FLUSH_INTERVAL`]; a hard crash loses at most that interval, deliberate since blocks
//! themselves are journaled. The rewrite runs on a dedicated writer thread ([`StoreWriter`])
//! because on a container or network volume the fsync is several dropped ticks every interval.
//! Unlike the journal, a store that cannot write does NOT refuse edits.
//!
//! **Every version this project has written still loads**, the archive-and-start-fresh path being
//! reserved for actual corruption. Two shapes keep that true: the file has TWO sections — records
//! keyed by account, and [`Self::legacy`] records keyed by the pre-account device hash, in a
//! quarantine that only ever SHRINKS and so needs no cap; and the layout blob is SELF-DESCRIBING,
//! a leading shape tag, not a present/absent flag, so a `GRID_SLOTS` change needs no version bump.

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
const VERSION: u16 = 12;

/// The version from which the bucket is a CRAFTED tool. Every record below
/// it was written when the bucket was owned by everyone — the count machinery just never stored
/// one — so the reader grants it, writing down the ownership the old build implied. Never a
/// windfall: records at this version and above own exactly what their counts say. The record BODY
/// is unchanged; the version is the grant's whole trigger.
const FIRST_CRAFTED_BUCKET_VERSION: u16 = 7;

/// The version from which essence is FOUND, not ground. Every record below it
/// holds balances earned against the unbounded bench the redesign removed, so the reader clears
/// them — all but Aether, which was always the scarce one. The record BODY is unchanged; the
/// version is the reset's whole trigger, so it runs at most once per record and a fresh account
/// never sees it.
const FIRST_FOUND_ESSENCE_VERSION: u16 = 8;

/// The window in which a record carried the waystones its account had arrived at, for a milestone
/// that no longer exists: the field is gone from the BODY, so a reader in the window has bytes to
/// skip and one above it has none. Frozen — they describe bytes on disk. Below the upper bound an
/// Arcana total may hold milestone XP that is not earnable now, so the reader winds Arcana, and
/// only Arcana, back to nothing; reading rewrites at [`VERSION`], so it runs at most once.
const FIRST_ARRIVALS_VERSION: u16 = 9;
const FIRST_VERSION_WITHOUT_ARRIVALS: u16 = 10;

/// The version whose records are keyed by ACCOUNT rather than by a device
/// hash. Anything below reads wholly into the quarantine; v6 and up say
/// which section each record is in. Frozen: it describes bytes on disk.
const FIRST_ACCOUNT_VERSION: u16 = 5;

/// The version from which a record carries the spell-bar bytes — opaque preference data the sim
/// stores verbatim (the client validates on read). Records below it read an empty bar.
const FIRST_SPELLS_VERSION: u16 = 11;

/// The version from which the raw shapes hold the retired crests' block ids
/// (`BlockType::REUSED_CREST_IDS`). Every record below it carries those ids as crests, which
/// dropped on decode, so the reader strikes them out before decoding — the record BODY is
/// unchanged; the version is the purge's whole trigger, so it runs at most once per record and
/// a fresh account never sees it.
const FIRST_RAW_SHAPES_VERSION: u16 = 12;

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

/// The hotbar width [`LAYOUT_LEGACY`] blobs were written at. FROZEN for exactly the same reason as
/// [`LEGACY_GRID_SLOTS`], and split out because the legacy arm originally read the LIVE
/// `HOTBAR_SLOTS`. Grow the bar and every tag-`1` record over-reads, desyncs into the next
/// record's key, and `PlayerStore::open` takes that for corruption and archives everybody's
/// inventory — a regression test built from the same live constant would move with the break.
const LEGACY_HOTBAR_SLOTS: usize = 9;

/// Both frozen widths describe bytes on disk, so this asserts the FORMAT,
/// not the constants: if today's hotbar ever differs, the legacy arm must
/// keep reading the LEGACY width, and this line is the reminder that it
/// does.
const _: () = assert!(
	LEGACY_HOTBAR_SLOTS == 9 && LEGACY_GRID_SLOTS == 36,
	"legacy layout widths describe bytes already written — never retune them"
);

/// The sized blob spends one byte per length, which is the whole reason growing the grid costs
/// nothing here. Fail the BUILD rather than truncating somebody's arrangement. Two asserts, not
/// one conjunction: today's values make either side imply the other, which clippy reads as a
/// redundant comparison rather than as the guard on future growth it is.
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

/// Account cap: beyond this, the least-recently-USED record is evicted on insert. Bounds the file;
/// an evicted player's state is lost. Least-recently-used, not first-inserted — a policy that
/// never refreshed a key's position evicts in join order and throws out the longest-standing
/// players first, online ones included. It bounds the file, not the ABUSE; that is closed
/// upstream, a record needing a master account and `/api/join` being rate-limited per session.
const MAX_IDENTITIES: usize = 10_000;

/// The stored key: the player's master account id, sixteen bytes. Its byte order is a FILE FORMAT
/// — every record is filed under it, so a parser reading the id differently would orphan every
/// player silently, the store still loading and never finding anybody; pinned by
/// `protocol::account_ids_round_trip_through_their_canonical_text`.
// A `use` alias, not a `type` alias, so the tuple constructor comes with it.
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
	/// Eye position at last disconnect. `None` = pre-v3 data or a first join; the sim starts those
	/// players at the resolved spawn. Resuming here is what closes the rejoin exploit: without it,
	/// reconnecting was a free trip to spawn that ignored the return-to-spawn cooldown entirely.
	pub pos: Option<[f32; 3]>,
	/// Server tick of the player's last teleport of any kind — one clock for Return Home and every
	/// port — so the cooldown survives a reconnect; resuming in place would otherwise still leave
	/// "rejoin to reset the timer" open. Ticks are per-run and reset on restart, so a stored tick
	/// from a previous run reads as long expired (`wrapping_sub` yields a huge interval), failing
	/// open across restarts, the safe direction for a convenience cooldown.
	pub last_teleport_tick: Option<u32>,
	/// Per-skill XP totals. Pre-v4 records read as empty with their
	/// inventories intact — existing players keep their stuff and start the
	/// skill system at the bottom like everyone else.
	pub skills: SkillSet,
	/// Spell-bar bytes, stored VERBATIM (opaque preference data; the
	/// client validates on read). Empty = pre-v11 record or never set.
	pub spells: Vec<u8>,
}

impl PlayerRecord {
	/// This record with `old` folded in — the legacy-character migration's whole definition of what
	/// a player gets back, each rule the conservative reading of "the same person owned both".
	/// Stuff ADDS, saturating at every cap ([`Inventory::merge_from`]); XP takes the HIGHER total,
	/// never the sum; position and spawn cooldown stay where the ACCOUNT left them, the old values
	/// used only when it has none; the arrangement likewise, `reconcile_layout` re-filing after.
	fn merged_with(&self, old: &Self) -> Self {
		let mut inventory = self.inventory.clone();
		inventory.merge_from(&old.inventory);
		let mut skills = self.skills.clone();
		skills.merge_from(&old.skills);
		Self {
			inventory,
			layout: self.layout.clone().or_else(|| old.layout.clone()),
			pos: self.pos.or(old.pos),
			last_teleport_tick: self.last_teleport_tick.or(old.last_teleport_tick),
			skills,
			// The account's own bar when it has one; a legacy character
			// predates the bar, so "old" only ever contributes emptiness.
			spells: if self.spells.is_empty() {
				old.spells.clone()
			} else {
				self.spells.clone()
			},
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
	/// Where a change of skills is also announced, when this server reports
	/// to the board. `put` is the one write every such change goes through,
	/// which is why the hook lives here and not at the award chokepoints.
	outbox: Option<std::sync::Arc<crate::hiscores::SkillOutbox>>,
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
			outbox: None,
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

	/// Fold this server's pre-account character for `legacy` into `account`, and delete the
	/// quarantined copy; `None` when nothing is held under that key. **Deleting the source IS the
	/// one-time-ness** — no "claimed" flag to keep in step, a second attempt looks like a first for
	/// a key that was never here, and two accounts racing one resolve first-through-takes-it. A
	/// merge ([`PlayerRecord::merged_with`]), not an overwrite: the account may have played here.
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

	/// Remove a record outright; `None` when nothing is held under `id`.
	fn take(&mut self, id: &IdentityKey) -> Option<PlayerRecord> {
		self.used.remove(id);
		let rec = self.map.remove(id)?;
		self.dirty = true;
		Some(rec)
	}

	/// A player's persisted record; default (empty, no layout) when
	/// unknown.
	pub fn get(&self, id: &IdentityKey) -> PlayerRecord {
		self.map.get(id).cloned().unwrap_or_default()
	}

	/// Report every stored record's skills now and every later change as
	/// it lands. Called once at boot, so a board that has never heard of
	/// this server catches up without anyone having to play first.
	pub fn attach_outbox(&mut self, outbox: std::sync::Arc<crate::hiscores::SkillOutbox>) {
		for (id, rec) in &self.map {
			if rec.skills != SkillSet::default() {
				outbox.push(*id, &rec.skills);
			}
		}
		self.outbox = Some(outbox);
	}

	/// Write-through to memory; durable at the next flush. Touching a key
	/// refreshes its LRU stamp, so an active player is never the eviction
	/// candidate.
	pub fn put(&mut self, id: IdentityKey, rec: PlayerRecord) {
		if let Some(outbox) = &self.outbox
			&& self.map.get(&id).is_none_or(|old| old.skills != rec.skills)
			&& rec.skills != SkillSet::default()
		{
			outbox.push(id, &rec.skills);
		}
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

	/// Move the save under `from` onto `to`, emptying `from`; `None` when nothing is there. The
	/// OLD character wins where only one can (position, layout), with anything `to` already
	/// gathered merged in: `to` is the same player under a new key, not a second character.
	pub fn claim_account(&mut self, from: IdentityKey, to: IdentityKey) -> Option<PlayerRecord> {
		let old = self.take(&from)?;
		let moved = old.merged_with(&self.get(&to));
		self.put(to, moved.clone());
		Some(moved)
	}

	/// [`PlayerStore::claim_account`] on the store in `dir`, offline (`--move-save`): a running
	/// server's next flush would overwrite it. `Ok(false)` when nothing is saved under `from`.
	pub fn move_save(dir: &Path, from: IdentityKey, to: IdentityKey) -> io::Result<bool> {
		let mut store = Self::open(dir)?;
		if store.claim_account(from, to).is_none() {
			return Ok(false);
		}
		store.sync();
		if store.writer.write_failed() {
			return Err(io::Error::other("the player store could not be written"));
		}
		Ok(true)
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

	/// Coalesced flush for a disconnect: writes only if the interval has elapsed, exactly like
	/// `maintain`. Forcing one unconditionally is wrong — every write rebuilds and fsyncs the WHOLE
	/// file (see `write_snapshot`) on the sim thread, so a crowd dropping in one tick pays a
	/// full-file rewrite and an fsync each inside a single tick's budget. The module already
	/// accepts losing up to `FLUSH_INTERVAL` to a hard crash; a disconnect need not be stricter.
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

	/// Encode into a buffer borrowed from the writer's free list. Two sections: the account-keyed
	/// records, then the quarantined pre-account ones behind their own count. The quarantine is
	/// written back on every flush precisely because it is not otherwise touched — a format that
	/// only wrote what changed would drop it on the first save after boot, which is the whole
	/// point of keeping it.
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
	match &rec.last_teleport_tick {
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
	// v11: the spell-bar bytes — u8 length + bytes, the layout blob's shape.
	let n = rec.spells.len().min(u8::MAX as usize);
	buf.push(n as u8);
	buf.extend_from_slice(&rec.spells[..n]);
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

/// The background writer: owns the fsync, so the sim thread does not. A flush rebuilds the WHOLE
/// file and fsyncs it — milliseconds on local SSD, tens on a container or network volume, several
/// dropped sim ticks every `FLUSH_INTERVAL`. Encoding stays on the sim thread (it needs the live
/// map); only the I/O moves. Submissions COALESCE — the channel holds one buffer and a newer
/// snapshot replaces an unstarted older one — so falling behind costs staleness, never queueing.
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

/// Decode a layout blob of `hotbar_len + grid_len` words into the CURRENT shape, padding short
/// halves with `WIRE_EMPTY` and dropping any beyond today's constants. It always consumes exactly
/// the words the writer declared, whatever the constants are now — a reader that stopped early
/// would leave the remainder to be read as the next field and desync the whole record stream.
/// Truncation is lossy, but `reconcile_layout` re-files anything that lost its slot after load.
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

/// One record: the 16-byte key, then whichever sections `version` wrote. The `version >=` guards
/// carry real weight: reading v1-v4 forward into the quarantine is exactly what the migration
/// needs. A v1 record ends after its inventory, a v2 after its layout, a v3 after its position —
/// each absent section reads as the empty value it always did.
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
	let (pos, last_teleport_tick) = if version >= 3 {
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
	// Records in the window where a first arrival paid a milestone carry
	// its memory. Read past it: nothing pays a milestone any more, but a
	// trailing field a reader skips is still a field a reader must read.
	if (FIRST_ARRIVALS_VERSION..FIRST_VERSION_WITHOUT_ARRIVALS).contains(&version) {
		let mut n = [0u8; 1];
		r.read_exact(&mut n).map_err(|_| bad("short record"))?;
		let mut v = vec![0u8; n[0] as usize];
		r.read_exact(&mut v).map_err(|_| bad("short record"))?;
	}
	// v1-v10 records end here: the spell bar reads empty (unassigned).
	let spells = if version >= FIRST_SPELLS_VERSION {
		let mut n = [0u8; 1];
		r.read_exact(&mut n).map_err(|_| bad("short record"))?;
		let mut v = vec![0u8; n[0] as usize];
		r.read_exact(&mut v).map_err(|_| bad("short record"))?;
		v
	} else {
		Vec::new()
	};
	let pairs = if version < FIRST_RAW_SHAPES_VERSION {
		Inventory::without_reused_crests(&pairs)
	} else {
		pairs
	};
	let mut inventory = Inventory::from_wire(&pairs);
	if version < FIRST_CRAFTED_BUCKET_VERSION {
		inventory.add(digblock_core::block::BlockType::Bucket, 1);
	}
	if version < FIRST_FOUND_ESSENCE_VERSION {
		inventory.clear_pre_redesign_essence();
	}
	let mut skills = skills;
	if version < FIRST_VERSION_WITHOUT_ARRIVALS {
		skills.clear(digblock_core::skills::Skill::Arcana);
	}
	Ok((
		key,
		PlayerRecord {
			inventory,
			layout,
			pos,
			last_teleport_tick,
			skills,
			spells,
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
