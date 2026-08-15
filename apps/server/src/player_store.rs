//! Persistent per-player state (inventory counts + item layout), keyed
//! by a hash of the client's identity secret. NOT part of the block
//! journal — the journal is cell-keyed and its compaction discards
//! authorship, so player-keyed state lives in this separate, much
//! smaller file.
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
//! Format v2 appends an optional item layout (hotbar + grid wire
//! arrangements) per record; v3 appends the player's last position and
//! return-to-spawn cooldown; v4 appends per-skill XP totals. v1–v3
//! files are still read — the archive-and-start-fresh path is reserved
//! for actual corruption, never a routine upgrade (it would wipe every
//! player's inventory). An older file simply has no stored position or
//! skills, so those players resume at spawn once / start at the bottom of
//! the skill curve and are stored in the current format from their next
//! write on.
//!
//! The layout blob is SELF-DESCRIBING, which is what keeps growing the
//! inventory grid out of the version field entirely. Its leading byte is
//! a shape tag, not a bare present/absent flag: `0` absent, `1` the
//! legacy fixed run of `HOTBAR_SLOTS + LEGACY_GRID_SLOTS` words that
//! every file written before the grid grew still carries, and `2` a
//! length-prefixed pair the reader pads or truncates into whatever the
//! current constants are. Writes always emit `2`. So a `GRID_SLOTS`
//! change is a no-op here — no version bump, no archive path, and no
//! silent re-interpretation of somebody's stored arrangement, which is
//! the failure this shape exists to make impossible. The one rule:
//! [`LEGACY_GRID_SLOTS`] is frozen at the width tag `1` was written
//! with and must never be re-pointed at `GRID_SLOTS`.

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
const VERSION: u16 = 4;
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
/// exactly the same reason as [`LEGACY_GRID_SLOTS`], and split out
/// because the legacy arm originally read the LIVE `HOTBAR_SLOTS` — the
/// one half of the blob still described by a constant free to move.
///
/// It happens to equal `HOTBAR_SLOTS` today, so nothing was wrong; it
/// was wrong in waiting. Grow the bar and every tag-`1` record
/// over-reads by two bytes a slot, desyncs into the next record's
/// identity hash, and `PlayerStore::open` takes that for corruption and
/// archives everybody's inventory — precisely the failure the shape tag
/// was introduced to make impossible, reintroduced through the half of
/// the blob nobody froze. The regression test could not have caught it
/// either, because `push_legacy_layout` built its fixture from the same
/// live constant and would have moved with the break.
const LEGACY_HOTBAR_SLOTS: usize = 9;

/// Both frozen widths describe bytes on disk, so this asserts the
/// FORMAT, not the constants: if today's hotbar ever differs, the legacy
/// arm must keep reading the LEGACY width, and this line is the reminder
/// that it does.
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

/// Identity cap: beyond this, the least-recently-USED identity is evicted
/// on insert. Bounds the file; an evicted (absent) player's state is lost —
/// acceptable pre-deployment, revisit with real populations.
///
/// Least-recently-used, not first-inserted. The old policy never refreshed
/// a key's position, so eviction walked strictly in join order and threw
/// out the longest-standing players first — including ones currently
/// online and actively writing. Note this bounds the file, not the abuse:
/// `identity` is client-chosen and join tokens are free, so a flood of
/// fresh identities still evicts real records. That root cause needs the
/// master's login flow (see NET-AUDIT-2.md §1.5).
const MAX_IDENTITIES: usize = 10_000;

/// The stored key: SHA-256 of the client's identity secret, truncated.
/// The raw secret is a bearer credential and never touches disk
/// server-side.
pub type IdentityHash = [u8; 16];

pub fn hash_identity(secret: &[u8; 16]) -> IdentityHash {
	use sha2::{Digest, Sha256};
	let digest = Sha256::digest(secret);
	digest[..16].try_into().expect("digest is 32 bytes")
}

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

pub struct PlayerStore {
	map: FxHashMap<IdentityHash, PlayerRecord>,
	/// LRU stamp per key, from `clock`. Eviction scans for the minimum,
	/// which is O(n) but only runs when the store is at `MAX_IDENTITIES` —
	/// `put` itself stays O(1), and it is called on every inventory-changing
	/// edit, so it has to be.
	used: FxHashMap<IdentityHash, u64>,
	clock: u64,
	dirty: bool,
	last_flush: Instant,
	writer: StoreWriter,
}

impl PlayerStore {
	/// Missing file → empty store. An unreadable file is archived to
	/// `.vN.bak` and the store starts fresh (mirrors the journal's policy
	/// — never crash the server over persisted state). v1 files parse
	/// with `layout: None` per record.
	pub fn open(dir: &Path) -> io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join("inventories.bin");
		let mut store = Self {
			map: FxHashMap::default(),
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
			Ok(entries) => {
				// Stamp in file order, which `write_snapshot` wrote
				// oldest-used first — so the LRU ranking survives a restart
				// instead of resetting to whatever the hash map iterates.
				for (hash, rec) in entries {
					store.clock += 1;
					store.used.insert(hash, store.clock);
					store.map.insert(hash, rec);
				}
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

	/// A player's persisted record; default (empty, no layout) when
	/// unknown.
	pub fn get(&self, id: &IdentityHash) -> PlayerRecord {
		self.map.get(id).cloned().unwrap_or_default()
	}

	/// Write-through to memory; durable at the next flush. Touching a key
	/// refreshes its LRU stamp, so an active player is never the eviction
	/// candidate.
	pub fn put(&mut self, id: IdentityHash, rec: PlayerRecord) {
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
	fn encode_snapshot(&mut self) -> Vec<u8> {
		let mut buf = self.writer.take_buffer();
		buf.clear();
		buf.reserve(HEADER_LEN + self.map.len() * 128);
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&VERSION.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
		// Oldest-used first, so a reload can rebuild the LRU ranking from
		// file order alone (see `open`).
		let mut keys: Vec<&IdentityHash> = self.map.keys().collect();
		keys.sort_unstable_by_key(|k| self.used.get(*k).copied().unwrap_or(0));
		// One scratch buffer for every record, not one Vec each: this loop
		// runs over the whole store on every flush.
		let mut pairs: Vec<(u16, u32)> = Vec::new();
		for hash in keys {
			let Some(rec) = self.map.get(hash) else {
				continue;
			};
			rec.inventory.write_wire(&mut pairs);
			buf.extend_from_slice(hash);
			buf.push(pairs.len().min(u8::MAX as usize) as u8);
			for (block, count) in pairs.iter().take(u8::MAX as usize) {
				buf.extend_from_slice(&block.to_le_bytes());
				buf.extend_from_slice(&count.to_le_bytes());
			}
			// v2: the layout blob, shape-tagged. Always the sized form —
			// the legacy tag is read-only, for files already on disk.
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
			// v4: per-skill XP — u8 pair count, then (u8 skill, u64 LE
			// total) pairs, nonzero totals only (absent = zero).
			let skills = rec.skills.to_wire();
			buf.push(skills.len().min(u8::MAX as usize) as u8);
			for (skill, total) in skills.iter().take(u8::MAX as usize) {
				buf.push(*skill);
				buf.extend_from_slice(&total.to_le_bytes());
			}
		}
		buf
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

fn parse(bytes: &[u8]) -> io::Result<Vec<(IdentityHash, PlayerRecord)>> {
	let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
	let mut r = bytes;
	let mut header = [0u8; HEADER_LEN];
	r.read_exact(&mut header).map_err(|_| bad("short header"))?;
	if header[..4] != MAGIC {
		return Err(bad("bad magic"));
	}
	let version = u16::from_le_bytes([header[4], header[5]]);
	if !(1..=VERSION).contains(&version) {
		return Err(bad("unsupported version"));
	}
	let count = u32::from_le_bytes(header[8..12].try_into().unwrap());
	let mut out = Vec::with_capacity(count as usize);
	for _ in 0..count {
		let mut hash = [0u8; 16];
		r.read_exact(&mut hash).map_err(|_| bad("short record"))?;
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
				// Written before the grid grew: a fixed run at the frozen
				// width, decoded into today's shape.
				LAYOUT_LEGACY => Some(read_layout(
					&mut r,
					LEGACY_HOTBAR_SLOTS,
					LEGACY_GRID_SLOTS,
					&bad,
				)?),
				LAYOUT_SIZED => {
					let mut lens = [0u8; 2];
					r.read_exact(&mut lens).map_err(|_| bad("short record"))?;
					Some(read_layout(
						&mut r,
						lens[0] as usize,
						lens[1] as usize,
						&bad,
					)?)
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
		// v1–v3 records end here: skills read empty — bottom of the curve,
		// with the inventory intact. Never the archive path for a routine
		// upgrade.
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
		out.push((
			hash,
			PlayerRecord {
				inventory: Inventory::from_wire(&pairs),
				layout,
				pos,
				last_return_tick,
				skills,
			},
		));
	}
	Ok(out)
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

	fn layout() -> PlayerLayout {
		let mut hotbar = [WIRE_EMPTY; HOTBAR_SLOTS];
		hotbar[0] = BlockType::Dirt as u16;
		hotbar[8] = WIRE_TOOL;
		let mut grid = [WIRE_EMPTY; GRID_SLOTS];
		grid[17] = BlockType::Stone as u16;
		PlayerLayout { hotbar, grid }
	}

	/// Append a layout the way every file written before the grid grew
	/// carries it: the bare `1` tag and a fixed run at the OLD width, with
	/// no lengths of its own. Hand-built on purpose — a fixture derived
	/// from the current constants would move with them and stop testing
	/// the thing that broke.
	/// A tag-`1` blob exactly as it was written to disk. BOTH widths are
	/// the frozen ones on purpose — a fixture built from today's live
	/// constants moves with any change to them, so it can never fail on
	/// the resize it exists to guard against.
	fn push_legacy_layout(buf: &mut Vec<u8>, l: &PlayerLayout) {
		buf.push(LAYOUT_LEGACY);
		for i in 0..LEGACY_HOTBAR_SLOTS {
			let w = l.hotbar.get(i).copied().unwrap_or(WIRE_EMPTY);
			buf.extend_from_slice(&w.to_le_bytes());
		}
		for i in 0..LEGACY_GRID_SLOTS {
			buf.extend_from_slice(&l.grid[i].to_le_bytes());
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
		let veteran = hash_identity(&[1; 16]);
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
			s.put(hash_identity(&secret), PlayerRecord::default());
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
		let id = hash_identity(&[1; 16]);
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
		let old = hash_identity(&[1; 16]);
		let new = hash_identity(&[2; 16]);
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
		let id_a = hash_identity(&[1; 16]);
		let id_b = hash_identity(&[2; 16]);
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
		assert!(s.get(&hash_identity(&[3; 16])).layout.is_none());
	}

	/// The v1 file (inventory-only records) still reads — an upgrade must
	/// NOT trip the archive-and-start-fresh path and wipe every player.
	#[test]
	fn v1_file_reads_with_no_layout() {
		let dir = temp_dir("v1compat");
		std::fs::create_dir_all(&dir).unwrap();
		let id = hash_identity(&[5; 16]);
		// Hand-built v1 snapshot: one record, one (block, count) pair.
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&1u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&1u32.to_le_bytes());
		buf.extend_from_slice(&id);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&7u32.to_le_bytes());
		std::fs::write(dir.join("inventories.bin"), &buf).unwrap();

		let mut s = PlayerStore::open(&dir).unwrap();
		let rec = s.get(&id);
		assert_eq!(rec.inventory.count(BlockType::Dirt), 7, "v1 data survives");
		assert_eq!(rec.layout, None, "v1 records carry no layout");

		// And the next write upgrades the file to v2 losslessly.
		s.put(
			id,
			PlayerRecord {
				layout: Some(layout()),
				..rec
			},
		);
		s.sync();
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&id).layout, Some(layout()));
		assert_eq!(s.get(&id).inventory.count(BlockType::Dirt), 7);
	}

	#[test]
	fn corrupt_file_is_archived_and_store_starts_fresh() {
		let dir = temp_dir("corrupt");
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("inventories.bin");
		std::fs::write(&path, b"garbage not a store").unwrap();
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&[0; 16]).inventory, Inventory::default());
		let bak = path.with_extension(format!("bin.v{VERSION}.bak"));
		assert_eq!(
			std::fs::read(&bak).unwrap(),
			b"garbage not a store",
			"archived byte-intact"
		);
	}

	#[test]
	fn identity_hash_is_stable_and_not_the_secret() {
		let secret = [7; 16];
		let h = hash_identity(&secret);
		assert_eq!(h, hash_identity(&secret), "deterministic");
		assert_ne!(h, secret, "never the raw bearer secret");
		assert_ne!(h, hash_identity(&[8; 16]));
	}

	/// The stored key is the FILE FORMAT: every record in `inventories.bin`
	/// is filed under it, so a hash that moves orphans every player's items
	/// silently — the store still loads, it just never finds anybody. The
	/// test above only proves the hash agrees with itself inside one build,
	/// which a swapped digest crate would satisfy while renaming every key.
	/// Truncated SHA-256 of the secret, against an external vector.
	#[test]
	fn identity_hash_is_a_pinned_wire_format() {
		let want = [
			0xd7, 0x61, 0xd4, 0x06, 0xaf, 0x2a, 0x4a, 0x5a, 0x15, 0xf6, 0x7c, 0x92, 0x43, 0x78,
			0xed, 0x88,
		];
		assert_eq!(hash_identity(&[7; 16]), want);
	}

	/// An existing v2 file (inventory + layout, no position) keeps every
	/// player's items and arrangement and simply reports no stored
	/// position — those players resume at spawn once, then store one.
	/// This must NOT trip the archive-and-start-fresh path.
	#[test]
	fn v2_file_reads_with_no_position() {
		let dir = temp_dir("v2compat");
		std::fs::create_dir_all(&dir).unwrap();
		let id = hash_identity(&[6; 16]);
		// Hand-built v2 snapshot: one record, one (block, count) pair, layout.
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&2u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&1u32.to_le_bytes());
		buf.extend_from_slice(&id);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&3u32.to_le_bytes());
		push_legacy_layout(&mut buf, &layout());
		std::fs::write(dir.join("inventories.bin"), &buf).unwrap();

		let mut s = PlayerStore::open(&dir).unwrap();
		let rec = s.get(&id);
		assert_eq!(rec.inventory.count(BlockType::Dirt), 3, "v2 data survives");
		assert_eq!(rec.layout, Some(layout()), "v2 layout survives");
		assert_eq!(rec.pos, None, "v2 records carry no position");
		assert_eq!(rec.last_return_tick, None);

		// The next write upgrades the file to v3 losslessly.
		s.put(
			id,
			PlayerRecord {
				pos: Some([1.5, 2.5, 3.5]),
				..rec
			},
		);
		s.sync();
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&id).pos, Some([1.5, 2.5, 3.5]));
		assert_eq!(s.get(&id).layout, Some(layout()));
		assert_eq!(s.get(&id).inventory.count(BlockType::Dirt), 3);
	}

	/// v3 round-trips the position and cooldown tick.
	#[test]
	fn position_and_cooldown_round_trip() {
		let dir = temp_dir("v3pos");
		let id = hash_identity(&[9; 16]);
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

	/// A record with no position stored (a player who never disconnected
	/// under v3) reads back absent rather than as some default coordinate.
	#[test]
	fn absent_position_stays_absent() {
		let dir = temp_dir("v3nopos");
		let id = hash_identity(&[10; 16]);
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

	/// An existing v3 file (pre-skills) keeps every player's items,
	/// arrangement and position, and reads back with empty skills —
	/// bottom of the curve, nothing wiped. A routine upgrade must NOT trip
	/// the archive-and-start-fresh path.
	#[test]
	fn v3_file_reads_with_empty_skills() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("v3compat");
		std::fs::create_dir_all(&dir).unwrap();
		let id = hash_identity(&[11; 16]);
		// Hand-built v3 snapshot: one record, one (block, count) pair, no
		// layout, a position, no cooldown tick.
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&3u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&1u32.to_le_bytes());
		buf.extend_from_slice(&id);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&9u32.to_le_bytes());
		buf.push(0); // no layout
		buf.push(1); // position
		for c in [4.5f32, 131.0, -2.5] {
			buf.extend_from_slice(&c.to_le_bytes());
		}
		buf.push(0); // no cooldown tick
		std::fs::write(dir.join("inventories.bin"), &buf).unwrap();

		let mut s = PlayerStore::open(&dir).unwrap();
		let rec = s.get(&id);
		assert_eq!(rec.inventory.count(BlockType::Dirt), 9, "v3 data survives");
		assert_eq!(rec.pos, Some([4.5, 131.0, -2.5]));
		assert_eq!(rec.skills, SkillSet::default(), "pre-skills = level 1");

		// The next write upgrades the file to v4 losslessly.
		let mut skills = SkillSet::default();
		skills.add_xp(Skill::Mining, 1_068);
		s.put(id, PlayerRecord { skills, ..rec });
		s.sync();
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&id).skills.xp(Skill::Mining), 1_068);
		assert_eq!(s.get(&id).inventory.count(BlockType::Dirt), 9);
		assert_eq!(s.get(&id).pos, Some([4.5, 131.0, -2.5]));
	}

	/// THE grid-growth test: a complete v4 file written when the grid was
	/// four rows still reads, with every player's items, arrangement,
	/// position and XP intact — no version bump, no archive path.
	///
	/// Growing `GRID_SLOTS` broke exactly this once. The layout blob was a
	/// fixed, length-free run, so a wider reader over-read into the pos
	/// flag, the return tick and the skills blob, desynced into the next
	/// record's identity hash, and `open` took the resulting parse error
	/// for corruption — archiving the file and wiping every player on the
	/// first restart after deploy. Nothing caught it because the other
	/// round-trip fixtures are all built from the CURRENT constants, which
	/// move together. This one is hand-built at the frozen width and does
	/// not.
	#[test]
	fn a_file_written_at_the_old_grid_width_still_reads() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("gridgrowth");
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("inventories.bin");
		let id_a = hash_identity(&[21; 16]);
		let id_b = hash_identity(&[22; 16]);
		let mut skills = SkillSet::default();
		skills.add_xp(Skill::Mining, 146_751);
		skills.add_xp(Skill::Alchemy, 394);

		// Two records, so an over-read desyncs into the second one's
		// identity hash the way it would on a real server.
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&4u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&2u32.to_le_bytes());
		for (id, dirt) in [(id_a, 12u32), (id_b, 5)] {
			buf.extend_from_slice(&id);
			buf.push(1);
			buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
			buf.extend_from_slice(&dirt.to_le_bytes());
			push_legacy_layout(&mut buf, &layout());
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
			l.grid[LEGACY_GRID_SLOTS..].iter().all(|&w| w == WIRE_EMPTY),
			"the rows the old file never had come back empty"
		);
		// The record behind it parsed too — an over-read would have eaten
		// its hash and lost it entirely.
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
		let id_a = hash_identity(&[23; 16]);
		let id_b = hash_identity(&[24; 16]);
		let wide = GRID_SLOTS + 9;
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&4u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&2u32.to_le_bytes());
		for (id, dirt) in [(id_a, 3u32), (id_b, 4)] {
			buf.extend_from_slice(&id);
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

	/// v4 round-trips skills alongside everything else.
	#[test]
	fn skills_round_trip() {
		use digblock_core::skills::Skill;
		let dir = temp_dir("v4skills");
		let id = hash_identity(&[12; 16]);
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
