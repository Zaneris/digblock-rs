//! Append-only world persistence: every accepted edit is one fixed-size record.
//! Boot loads the whole file into a last-write-wins in-memory index; application
//! to chunks is lazy, so replay never waits on chunk loading. Fixed-size records
//! give trivial torn-tail recovery: truncate to the last whole record.
//!
//! One format, two homes: the dedicated server's world journal AND the client's
//! local saves share this file and its `VERSION`, which means the RECORD FORMAT
//! alone — the header names the generator that made the world, so terrain can
//! change without staling either. The `Journal` writer and its compaction
//! machinery are fs-bound (native only); the index, the record codec and the
//! snapshot halves are pure and compile on wasm.
//!
//! Both homes shard by REGION (see `REGION_COLS`), so persistence work tracks
//! what changed rather than a world's lifetime edit count. A shard is an ordinary
//! journal blob that happens to hold one region's cells, which is why sharding
//! needed no format bump: pre-shard worlds MIGRATE at open instead of staling.

use digblock_core::worldgen::{Seed, TerrainVersion};
use digblock_net::messages::BlockEdit;
use digblock_net::protocol::PlayerId;
#[cfg(not(target_arch = "wasm32"))]
use digblock_net::protocol::SERVER_ID;
use rustc_hash::{FxHashMap, FxHashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::fs::{File, OpenOptions};
#[cfg(not(target_arch = "wasm32"))]
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"DBJ1";
/// A bump means the RECORD FORMAT changed and stored edits cannot be read: `open` REFUSES an
/// old-version world and leaves it standing — starting fresh is [`archive_world`], an operator's
/// explicit call, never a boot side effect. Not a worldgen gate any more: the header records the
/// generator that made the world ([`digblock_core::worldgen::TerrainVersion`]) and that one
/// regenerates it. v2 widened the block field to a u16 CELL; v3 predates the header's slot.
pub const VERSION: u16 = 3;
const HEADER_LEN: u64 = 16;
const RECORD_LEN: u64 = 16;
#[cfg(not(target_arch = "wasm32"))]
const SYNC_INTERVAL_SECS: f32 = 1.0;

/// Final block CELL value per edited cell within one chunk column.
pub type ColumnEdits = FxHashMap<(i32, i32, i32), u16>;

/// Chunk columns per region axis: a region is this many columns square. Regions are the
/// PERSISTENCE granularity, and exist because both homes of this format used to do work
/// proportional to a world's total edit count on every save — the web store re-serialized the
/// whole overlay, the server rewrote the whole file to compact it. The size is a balance: bigger
/// regions mean fewer, larger rewrites and fewer localStorage keys; smaller ones more bookkeeping.
pub const REGION_COLS: i32 = 16;

/// Region owning a chunk column.
pub fn region_of_column(cx: i32, cz: i32) -> (i32, i32) {
	(cx.div_euclid(REGION_COLS), cz.div_euclid(REGION_COLS))
}

/// Region owning a world block position.
pub fn region_of_block(x: i32, z: i32) -> (i32, i32) {
	region_of_column(x.div_euclid(16), z.div_euclid(16))
}

/// Last-write-wins cell state, grouped by chunk column for join sync and
/// by region for persistence.
#[derive(Default, Debug)]
pub struct JournalIndex {
	columns: FxHashMap<(i32, i32), ColumnEdits>,
	/// Region → its edited columns. Lets a region be serialized without
	/// scanning every column in the world, which is the whole point of
	/// sharding.
	regions: FxHashMap<(i32, i32), FxHashSet<(i32, i32)>>,
	/// Regions edited since the last persist. Never cleared implicitly —
	/// a store clears a region only once its bytes are safely written.
	dirty: FxHashSet<(i32, i32)>,
	pub next_seq: u64,
}

impl JournalIndex {
	/// Returns the edit's sequence number and whether this is the column's
	/// FIRST edit ever — the signal residency sync uses to enroll nearby
	/// clients without rescanning the whole edited-column set.
	pub fn record(&mut self, edit: BlockEdit) -> (u64, bool) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		let mut new_column = false;
		self.columns
			.entry(col)
			.or_insert_with(|| {
				new_column = true;
				ColumnEdits::default()
			})
			.insert((edit.x, edit.y, edit.z), edit.block);
		let region = region_of_column(col.0, col.1);
		if new_column {
			self.regions.entry(region).or_default().insert(col);
		}
		self.dirty.insert(region);
		let seq = self.next_seq;
		self.next_seq += 1;
		(seq, new_column)
	}

	pub fn column(&self, cx: i32, cz: i32) -> Option<&ColumnEdits> {
		self.columns.get(&(cx, cz))
	}

	/// Distinct cells with a final value — the compacted size.
	pub fn live_cells(&self) -> u64 {
		self.columns.values().map(|c| c.len() as u64).sum()
	}

	/// Columns that have at least one edit. O(the whole world's edited
	/// columns) — prefer [`Self::columns_near`] for anything that runs per
	/// client or per tick.
	pub fn edited_columns(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
		self.columns.keys().copied()
	}

	/// Edited columns in the REGIONS that a disc of `radius` columns about `center` can touch. A
	/// superset of the columns actually in range — the caller still tests each one — but it walks
	/// only the regions that could matter. This is what keeps residency sync O(active) instead of
	/// O(world age): the late-sync scan runs per client every time their center column moves, and
	/// against `edited_columns` that was a full hash-map walk growing for the life of the world.
	pub fn columns_near(
		&self,
		center: (i32, i32),
		radius: i32,
	) -> impl Iterator<Item = (i32, i32)> + '_ {
		let (r0x, r1x) = (
			(center.0 - radius).div_euclid(REGION_COLS),
			(center.0 + radius).div_euclid(REGION_COLS),
		);
		let (r0z, r1z) = (
			(center.1 - radius).div_euclid(REGION_COLS),
			(center.1 + radius).div_euclid(REGION_COLS),
		);
		(r0x..=r1x)
			.flat_map(move |rx| (r0z..=r1z).map(move |rz| (rx, rz)))
			.filter_map(|r| self.regions.get(&r))
			.flat_map(|cols| cols.iter().copied())
	}

	/// Every edit as `((x, y, z), cell)`, in arbitrary order.
	pub fn iter_edits(&self) -> impl Iterator<Item = ((i32, i32, i32), u16)> + '_ {
		self.columns
			.values()
			.flat_map(|c| c.iter().map(|(&p, &b)| (p, b)))
	}

	/// Regions holding at least one edit.
	pub fn regions(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
		self.regions.keys().copied()
	}

	/// Every edit in one region — O(that region), not O(the world).
	pub fn region_edits(
		&self,
		region: (i32, i32),
	) -> impl Iterator<Item = ((i32, i32, i32), u16)> + '_ {
		self.regions
			.get(&region)
			.into_iter()
			.flat_map(move |cols| cols.iter())
			.filter_map(move |col| self.columns.get(col))
			.flat_map(|cells| cells.iter().map(|(&p, &b)| (p, b)))
	}

	pub fn region_live_cells(&self, region: (i32, i32)) -> u64 {
		self.regions.get(&region).map_or(0, |cols| {
			cols.iter()
				.filter_map(|c| self.columns.get(c))
				.map(|c| c.len() as u64)
				.sum()
		})
	}

	/// Regions changed since they were last persisted.
	pub fn dirty_regions(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
		self.dirty.iter().copied()
	}

	pub fn dirty_count(&self) -> usize {
		self.dirty.len()
	}

	/// Mark a region persisted. Call only AFTER its bytes are committed:
	/// clearing early turns a failed write into silent data loss.
	pub fn clear_dirty(&mut self, region: (i32, i32)) {
		self.dirty.remove(&region);
	}

	pub fn mark_all_dirty(&mut self) {
		self.dirty = self.regions.keys().copied().collect();
	}

	pub fn clear_all_dirty(&mut self) {
		self.dirty.clear();
	}
}

// ---- pure snapshot codec (shared by the file journal and the web
// byte-blob store) --------------------------------------------------------

/// Why a snapshot (or file header) can't be used.
#[derive(Debug, PartialEq)]
pub enum JournalError {
	/// Well-formed header from another RECORD FORMAT — real but stale; don't destroy it.
	WrongVersion(u16),
	SeedMismatch {
		file: f64,
		requested: f64,
	},
	/// Opened under a generator that did not make it. The world is intact and the request wrong.
	TerrainMismatch {
		file: TerrainVersion,
		requested: TerrainVersion,
	},
	/// Made by a generator this build does not carry — the client is too old, the world is fine.
	UnknownTerrain(u16),
	Corrupt(&'static str),
}

impl std::fmt::Display for JournalError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			JournalError::WrongVersion(v) => write!(f, "journal version {v} != {VERSION}"),
			JournalError::SeedMismatch { file, requested } => {
				write!(f, "journal seed {file} != requested {requested}")
			}
			JournalError::TerrainMismatch { file, requested } => {
				write!(f, "journal terrain {file:?} != requested {requested:?}")
			}
			JournalError::UnknownTerrain(v) => {
				write!(
					f,
					"journal terrain {v} is not a generator this build carries"
				)
			}
			JournalError::Corrupt(what) => write!(f, "journal corrupt: {what}"),
		}
	}
}

/// Serialize `index` as a complete journal byte blob: header + one
/// record per live cell (the boot-compaction body as a pure function).
pub fn snapshot_bytes(seed: Seed, index: &JournalIndex) -> Vec<u8> {
	let mut out = Vec::with_capacity((HEADER_LEN + index.live_cells() * RECORD_LEN) as usize);
	out.extend_from_slice(&header_bytes(seed));
	for ((x, y, z), block) in index.iter_edits() {
		out.extend_from_slice(&encode_record(
			x,
			y,
			z,
			block,
			digblock_net::protocol::SERVER_ID,
		));
	}
	out
}

/// Serialize ONE region as a standalone journal blob. The bytes are an ordinary journal — same
/// magic, version and record layout — that happens to contain only this region's cells. That is
/// what makes the sharded layout a pure container change: every reader, including
/// [`parse_snapshot`], is unchanged, and a shard can be merged back into a full index just by
/// replaying it.
pub fn region_snapshot_bytes(seed: Seed, index: &JournalIndex, region: (i32, i32)) -> Vec<u8> {
	let live = index.region_live_cells(region);
	let mut out = Vec::with_capacity((HEADER_LEN + live * RECORD_LEN) as usize);
	out.extend_from_slice(&header_bytes(seed));
	for ((x, y, z), block) in index.region_edits(region) {
		out.extend_from_slice(&encode_record(
			x,
			y,
			z,
			block,
			digblock_net::protocol::SERVER_ID,
		));
	}
	out
}

/// Replay a blob's records INTO an existing index (shard merge). Header
/// checks match [`parse_snapshot`]; a torn tail is ignored.
pub fn merge_snapshot(
	bytes: &[u8],
	expected_seed: Seed,
	index: &mut JournalIndex,
) -> Result<(), JournalError> {
	check_header(bytes, expected_seed)?;
	replay_records(&bytes[HEADER_LEN as usize..], index);
	Ok(())
}

/// Header validation shared by the whole-blob and shard-merge paths.
fn check_header(bytes: &[u8], expected_seed: Seed) -> Result<(), JournalError> {
	if bytes.len() < HEADER_LEN as usize {
		return Err(JournalError::Corrupt("shorter than the header"));
	}
	let header = &bytes[..HEADER_LEN as usize];
	if &header[0..4] != MAGIC {
		return Err(JournalError::Corrupt("magic mismatch"));
	}
	let version = u16::from_le_bytes([header[4], header[5]]);
	if version != VERSION {
		return Err(JournalError::WrongVersion(version));
	}
	let file_seed = f64::from_le_bytes(header[8..16].try_into().unwrap());
	if file_seed.to_bits() != expected_seed.value.to_bits() {
		return Err(JournalError::SeedMismatch {
			file: file_seed,
			requested: expected_seed.value,
		});
	}
	// A world is regenerated by the generator that MADE it, so opening it under any other is a
	// caller bug rather than a stale file — the world is fine, the request is wrong.
	match header_terrain(header) {
		Some(v) if v == expected_seed.version => Ok(()),
		Some(v) => Err(JournalError::TerrainMismatch {
			file: v,
			requested: expected_seed.version,
		}),
		None => Err(JournalError::UnknownTerrain(u16::from_le_bytes([
			header[6], header[7],
		]))),
	}
}

/// Parse a journal byte blob (header checks + LWW replay). A torn tail
/// is truncated, matching the file path's recovery.
pub fn parse_snapshot(bytes: &[u8], expected_seed: Seed) -> Result<JournalIndex, JournalError> {
	let mut index = JournalIndex::default();
	merge_snapshot(bytes, expected_seed, &mut index)?;
	Ok(index)
}

/// LWW-replay whole records into `index`; a torn tail (trailing partial
/// record) is ignored.
fn replay_records(body: &[u8], index: &mut JournalIndex) {
	for rec in body.chunks_exact(RECORD_LEN as usize) {
		let edit = BlockEdit {
			x: i32::from_le_bytes(rec[0..4].try_into().unwrap()),
			y: i32::from_le_bytes(rec[4..8].try_into().unwrap()),
			z: i32::from_le_bytes(rec[8..12].try_into().unwrap()),
			block: u16::from_le_bytes([rec[12], rec[13]]),
		};
		index.record(edit);
	}
}

/// Consecutive write failures before the journal reports unhealthy.
#[cfg(not(target_arch = "wasm32"))]
const MAX_WRITE_FAILURES: u32 = 3;
/// Compact (boot or runtime) when a SHARD holds this many times more
/// records than its region's live cells (and enough of them to matter).
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_RATIO: u64 = 2;
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_MIN_RECORDS: u64 = 10_000;
/// Runtime compaction: copy at least this many records per tick. A whole
/// column always copies atomically (map iteration order is invalidated
/// by mutation), so a single tick can overshoot by one column.
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_RECORDS_PER_TICK: u64 = 4096;
/// Intermediate tmp fsync cadence, in records, so the finishing tick
/// never pays one huge fsync inside the sim's tick budget.
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_RECORDS_PER_SYNC: u64 = 131_072;
/// Cooldown after a completed run (fluid churn re-bloats slowly).
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_COOLDOWN_SECS: u64 = 60;
/// Backoff after a failed run.
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_RETRY_SECS: u64 = 300;
/// Abort if mid-run appends outpace the copy this badly — impossible
/// within the fluid/edit rate budgets, but fail safe.
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_DELTA_MAX: usize = 65_536;
/// Regions examined per `maintain()` when looking for something to compact. The scan is
/// round-robin over the shard set rather than a full pass, so its cost is fixed no matter how
/// many regions a mature world holds — the pre-shard code summed every column's live cells on
/// every tick instead. A sweep of even a few thousand regions completes well inside
/// `COMPACT_COOLDOWN_SECS`.
#[cfg(not(target_arch = "wasm32"))]
const COMPACT_SCAN_PER_TICK: usize = 8;
/// Shard writers kept open at once. Regions are visited without bound — a player walks, a fluid
/// front crosses a border — so this cap is what stops a long session from exhausting file
/// descriptors; the least recently written shard is flushed, fsynced and closed to make room.
/// Sized well past what a full server's players and fluid fronts touch simultaneously, so
/// eviction stays a rare event rather than a per-append fsync treadmill.
#[cfg(not(target_arch = "wasm32"))]
const MAX_OPEN_REGIONS: usize = 64;

/// One open shard file. `unsynced` OUTLIVES `dirty` — `flush_writers` empties the BufWriter into
/// the kernel without making anything durable — and `used` is the LRU stamp (see
/// [`Journal::open_writer`]). The two flags are separate because [`Journal::sync`] fires on a
/// global "something was written" trigger while the writer set is up to `MAX_OPEN_REGIONS` wide:
/// without a per-writer flag it fsynced EVERY open descriptor whenever any one took an append.
#[cfg(not(target_arch = "wasm32"))]
struct RegionWriter {
	file: BufWriter<File>,
	dirty: bool,
	unsynced: bool,
	used: u64,
}

/// In-flight runtime compaction of ONE region, stepped once per `maintain()` call. The invariants
/// correctness rests on (see `append` for the capture rule): the in-memory `index` and `next_seq`
/// are NEVER touched, so join sync and seq monotonicity are safe by construction; live appends
/// keep going to the OLD shard until the atomic rename, so a crash leaves a complete old shard
/// plus a garbage tmp the next boot removes; other regions must stay OUT of the delta.
#[cfg(not(target_arch = "wasm32"))]
struct CompactionRun {
	region: (i32, i32),
	tmp: BufWriter<File>,
	tmp_path: PathBuf,
	/// Snapshot of the region's column keys at start; copied whole, one
	/// cursor step at a time. Columns are never removed from the index, so
	/// every key stays valid for the whole run.
	cols: Vec<(i32, i32)>,
	cursor: usize,
	/// Columns not yet copied. A mid-run append goes to `delta` iff its
	/// column is NOT here: a pending column's copy will read the current
	/// (newer or equal) index value when its turn comes, and a delta
	/// record — written after the base — would resurrect the OLDER value
	/// on last-write-wins replay.
	pending: FxHashSet<(i32, i32)>,
	/// Raw records appended mid-run to already-copied or brand-new
	/// columns of THIS region; written after the base copy, so they win
	/// LWW replay.
	delta: Vec<[u8; RECORD_LEN as usize]>,
	written: u64,
	since_sync: u64,
}

/// The world journal as a DIRECTORY of per-region shards (`r.{rx}.{rz}.journal` under a
/// seed-named directory), each a complete ordinary journal blob holding one region's cells.
/// Sharding keeps persistence work proportional to what changed instead of to the world's
/// lifetime edit count: an append touches one shard, compaction rewrites one region's bounded
/// live set. Pre-shard single-file worlds MIGRATE at open: the layout changed, not the format.
#[cfg(not(target_arch = "wasm32"))]
pub struct Journal {
	/// The shard directory, not the data root the caller passed.
	dir: PathBuf,
	seed: Seed,
	pub index: JournalIndex,
	/// Open shard writers, capped at MAX_OPEN_REGIONS.
	writers: FxHashMap<(i32, i32), RegionWriter>,
	/// Monotonic stamp source for the writer LRU.
	clock: u64,
	/// Records currently in each region's shard — the compaction trigger
	/// input. Every region that has ever been written this boot has an
	/// entry, so this doubles as the compaction scan's key set.
	records: FxHashMap<(i32, i32), u64>,
	/// Remaining regions in the current round-robin compaction sweep.
	scan: Vec<(i32, i32)>,
	wrote_since_sync: bool,
	last_sync: Instant,
	run: Option<CompactionRun>,
	/// Runtime compaction not eligible before this instant (cooldown
	/// after success, backoff after failure). None = eligible now.
	next_compact_at: Option<Instant>,
	/// Consecutive append/flush/fsync failures; reset on success. Past
	/// MAX_WRITE_FAILURES the sim stops accepting edits (rejecting with
	/// ServerError) rather than diverging memory from disk silently.
	write_failures: u32,
	last_health_log: Option<Instant>,
	#[cfg(any(test, feature = "test-hooks"))]
	pub force_write_failure: bool,
	#[cfg(any(test, feature = "test-hooks"))]
	pub force_compact_failure: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl Journal {
	/// Open (or create) the journal for `seed` under `dir`. A seed
	/// mismatch with an existing shard is a hard error — the terrain the
	/// edits apply to would not exist.
	pub fn open(dir: &Path, seed: Seed) -> std::io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let legacy = legacy_path(dir, seed.value);
		let shards = shard_dir(dir, seed.value);
		// Pre-shard world. A version break refuses like any other (below);
		// a CURRENT-version file is migrated, because sharding changed the
		// container and not the format — staling those worlds would throw
		// away every edit over a directory layout.
		if legacy.exists() {
			match file_version(&legacy)? {
				Some(version) => return Err(refuse(&legacy, version)),
				None => migrate_legacy(&legacy, &shards, seed)?,
			}
		}
		// Format break: shards from another version can't be read, and the
		// world is left standing — losing it is [`archive_world`]'s caller's
		// decision, never a boot side effect.
		if let Some(version) = shard_version(&shards)? {
			return Err(refuse(&shards, version));
		}
		std::fs::create_dir_all(&shards)?;
		// A crashed compaction or migration may leave stale tmps behind.
		remove_stale_tmps(&shards);

		let mut index = JournalIndex::default();
		let mut records: FxHashMap<(i32, i32), u64> = FxHashMap::default();
		for region in list_shards(&shards)? {
			let n = load_shard(&shard_path(&shards, region), seed, &mut index)?;
			records.insert(region, n);
		}
		// Boot compaction, per region: fluid appends every cell change, so
		// a shard grows without bound while its live set stays last-write-
		// wins small. Only bloated regions are rewritten (tmp + fsync +
		// atomic rename), so boot cost tracks the churn that actually
		// happened instead of the world's whole history.
		for (&region, count) in records.iter_mut() {
			let live = index.region_live_cells(region);
			if *count > COMPACT_RATIO * live && *count > COMPACT_MIN_RECORDS {
				log::info!(
					"journal boot compaction r.{}.{}: {count} records -> {live} live cells",
					region.0,
					region.1
				);
				*count = write_shard(&shards, seed, &index, region)?;
			}
		}
		// Seq numbers are per-boot monotonic only (clients never persist
		// them), so they simply restart at the record count on disk —
		// replay already left next_seq there unless boot compaction just
		// shrank a shard.
		index.next_seq = records.values().sum();
		Ok(Self {
			dir: shards,
			seed,
			index,
			writers: FxHashMap::default(),
			clock: 0,
			records,
			scan: Vec::new(),
			wrote_since_sync: false,
			last_sync: Instant::now(),
			run: None,
			next_compact_at: None,
			write_failures: 0,
			last_health_log: None,
			#[cfg(any(test, feature = "test-hooks"))]
			force_write_failure: false,
			#[cfg(any(test, feature = "test-hooks"))]
			force_compact_failure: false,
		})
	}

	/// False once writes have failed repeatedly: the world can no longer
	/// be persisted, so the sim must stop mutating it durably.
	pub fn healthy(&self) -> bool {
		self.write_failures < MAX_WRITE_FAILURES
	}

	/// Append one accepted edit; returns its global sequence number and
	/// whether this created a freshly-edited column (see
	/// [`JournalIndex::record`]).
	pub fn append(&mut self, edit: BlockEdit, by: PlayerId) -> (u64, bool) {
		let rec = encode_record(edit.x, edit.y, edit.z, edit.block, by);
		let region = region_of_block(edit.x, edit.z);
		match self.write_record(region, &rec) {
			Ok(()) => {
				self.write_failures = 0;
				*self.records.entry(region).or_insert(0) += 1;
			}
			Err(e) => self.note_write_failure("write", &e),
		}
		// Mid-compaction capture (see CompactionRun::pending for the
		// rule). Tracked off the index like the base copy, so even an
		// append the old shard lost lands in the compacted one. Appends to
		// OTHER regions are already durable in their own shards, which
		// this run does not replace.
		let mut delta_overflow = false;
		if let Some(run) = &mut self.run {
			let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
			if region == run.region && !run.pending.contains(&col) {
				run.delta.push(rec);
				delta_overflow = run.delta.len() > COMPACT_DELTA_MAX;
			}
		}
		if delta_overflow {
			self.abort_compaction(&std::io::Error::other(
				"delta overflow (churn outpaced the copy)",
			));
		}
		self.wrote_since_sync = true;
		self.index.record(edit)
	}

	fn write_record(&mut self, region: (i32, i32), rec: &[u8]) -> std::io::Result<()> {
		#[cfg(any(test, feature = "test-hooks"))]
		if self.force_write_failure {
			return Err(std::io::Error::other("injected write failure"));
		}
		let w = self.open_writer(region)?;
		w.file.write_all(rec)?;
		w.dirty = true;
		w.unsynced = true;
		Ok(())
	}

	/// The region's writer, opening the shard (and creating it, header
	/// first) on demand and evicting the least-recently-written one when
	/// the descriptor budget is full.
	fn open_writer(&mut self, region: (i32, i32)) -> std::io::Result<&mut RegionWriter> {
		if !self.writers.contains_key(&region) {
			if self.writers.len() >= MAX_OPEN_REGIONS {
				self.evict_lru();
			}
			let path = shard_path(&self.dir, region);
			let mut file = OpenOptions::new()
				.read(true)
				.write(true)
				.create(true)
				.truncate(false)
				.open(&path)?;
			if file.metadata()?.len() == 0 {
				file.write_all(&header_bytes(self.seed))?;
				file.sync_data()?;
			}
			file.seek(SeekFrom::End(0))?;
			self.writers.insert(
				region,
				RegionWriter {
					file: BufWriter::new(file),
					dirty: false,
					unsynced: false,
					used: 0,
				},
			);
			self.records.entry(region).or_insert(0);
		}
		self.clock += 1;
		let used = self.clock;
		let w = self.writers.get_mut(&region).expect("inserted above");
		w.used = used;
		Ok(w)
	}

	fn evict_lru(&mut self) {
		let Some(region) = self
			.writers
			.iter()
			.min_by_key(|(_, w)| w.used)
			.map(|(&r, _)| r)
		else {
			return;
		};
		let Some(mut w) = self.writers.remove(&region) else {
			return;
		};
		// fsync on the way out: a shard closed between two periodic syncs
		// would otherwise never get one, quietly breaking the promise that
		// an accepted edit is durable within SYNC_INTERVAL_SECS.
		let closed = w.file.flush().and_then(|()| w.file.get_ref().sync_data());
		if let Err(e) = closed {
			self.note_write_failure("close", &e);
		}
	}

	fn note_write_failure(&mut self, what: &str, e: &std::io::Error) {
		self.write_failures = self.write_failures.saturating_add(1);
		let announce = self.write_failures == MAX_WRITE_FAILURES
			|| self
				.last_health_log
				.is_none_or(|at| at.elapsed().as_secs() >= 60);
		if announce {
			self.last_health_log = Some(Instant::now());
			log::error!(
				"journal {what} failed ({} consecutive): {e}{}",
				self.write_failures,
				if self.healthy() {
					""
				} else {
					" — REJECTING further edits until writes recover"
				}
			);
		}
	}

	/// Flush buffered writes; fsync at most once a second; step (or
	/// start) runtime compaction — exactly one bounded action per call.
	pub fn maintain(&mut self) {
		self.flush_writers();
		if self.wrote_since_sync && self.last_sync.elapsed().as_secs_f32() >= SYNC_INTERVAL_SECS {
			self.sync();
		}
		if self.run.is_some() {
			self.step_compaction();
		} else {
			self.maybe_start_compaction();
		}
	}

	/// Bounded by the open-writer cap, and each flush is a memcpy of one
	/// partial buffer into the kernel.
	fn flush_writers(&mut self) {
		let mut errs = Vec::new();
		for w in self.writers.values_mut() {
			if w.dirty {
				if let Err(e) = w.file.flush() {
					errs.push(e);
				}
				w.dirty = false;
			}
		}
		for e in errs {
			self.note_write_failure("flush", &e);
		}
	}

	fn maybe_start_compaction(&mut self) {
		if !self.healthy() {
			return;
		}
		if self.next_compact_at.is_some_and(|at| Instant::now() < at) {
			return;
		}
		let Some((region, records, live)) = self.next_bloated_region() else {
			return;
		};
		match self.start_compaction(region) {
			Ok(()) => log::info!(
				"journal runtime compaction started r.{}.{}: {records} records, {live} live cells",
				region.0,
				region.1
			),
			Err(e) => self.abort_compaction(&e),
		}
	}

	/// Advance the round-robin sweep by at most COMPACT_SCAN_PER_TICK
	/// regions, returning the first bloated one. `region_live_cells` is
	/// O(the region's edited columns), so the whole check stays a fixed
	/// handful of map lookups per tick.
	fn next_bloated_region(&mut self) -> Option<((i32, i32), u64, u64)> {
		for _ in 0..COMPACT_SCAN_PER_TICK {
			if self.scan.is_empty() {
				self.scan = self.records.keys().copied().collect();
				if self.scan.is_empty() {
					return None;
				}
			}
			let region = self.scan.pop()?;
			let records = self.records.get(&region).copied().unwrap_or(0);
			let live = self.index.region_live_cells(region);
			if records > COMPACT_RATIO * live && records > COMPACT_MIN_RECORDS {
				return Some((region, records, live));
			}
		}
		None
	}

	fn start_compaction(&mut self, region: (i32, i32)) -> std::io::Result<()> {
		let tmp_path = tmp_path(&shard_path(&self.dir, region));
		let mut tmp = BufWriter::new(File::create(&tmp_path)?);
		tmp.write_all(&header_bytes(self.seed))?;
		let cols: Vec<(i32, i32)> = self
			.index
			.regions
			.get(&region)
			.map(|cols| cols.iter().copied().collect())
			.unwrap_or_default();
		let pending = cols.iter().copied().collect();
		self.run = Some(CompactionRun {
			region,
			tmp,
			tmp_path,
			cols,
			cursor: 0,
			pending,
			delta: Vec::new(),
			written: 0,
			since_sync: 0,
		});
		Ok(())
	}

	/// One bounded copy step; finishes the run when the snapshot is
	/// exhausted. Any I/O error aborts the run (the old shard is still the
	/// complete truth).
	fn step_compaction(&mut self) {
		#[cfg(any(test, feature = "test-hooks"))]
		if self.force_compact_failure {
			self.abort_compaction(&std::io::Error::other("injected compaction failure"));
			return;
		}
		let step = {
			let run = self.run.as_mut().expect("active run");
			copy_step(run, &self.index)
		};
		match step {
			Ok(false) => {}
			Ok(true) => {
				if let Err(e) = self.finish_compaction() {
					self.abort_compaction(&e);
				}
			}
			Err(e) => self.abort_compaction(&e),
		}
	}

	/// Append the delta, fsync, atomically swap the shard in.
	/// `index`/`next_seq` are never touched — runtime compaction is
	/// invisible to join sync and seq monotonicity.
	fn finish_compaction(&mut self) -> std::io::Result<()> {
		let mut run = self.run.take().expect("active run");
		for rec in &run.delta {
			run.tmp.write_all(rec)?;
		}
		run.tmp.flush()?;
		run.tmp.get_ref().sync_data()?;
		drop(run.tmp);
		// Close the live writer BEFORE the swap: its descriptor points at
		// the inode the rename unlinks, so anything still buffered there
		// would be written into the void. maintain() flushed it at the top
		// of this very call and every append since the run started is in
		// the delta, so dropping it loses nothing — the next append to
		// this region reopens the compacted shard.
		self.writers.remove(&run.region);
		std::fs::rename(&run.tmp_path, shard_path(&self.dir, run.region))?;
		let now = run.written + run.delta.len() as u64;
		let old = self.records.insert(run.region, now).unwrap_or(0);
		self.next_compact_at = Some(Instant::now() + Duration::from_secs(COMPACT_COOLDOWN_SECS));
		log::info!(
			"journal runtime compaction r.{}.{}: {old} -> {now} records",
			run.region.0,
			run.region.1
		);
		Ok(())
	}

	/// Best-effort cleanup + backoff. Deliberately does NOT touch
	/// `write_failures`: that counter gates edit acceptance, and a failed
	/// compaction leaves a correct (merely bloated) shard — rejecting
	/// edits over it would be worse than the disease.
	fn abort_compaction(&mut self, e: &std::io::Error) {
		if let Some(run) = self.run.take() {
			drop(run.tmp);
			let _ = std::fs::remove_file(&run.tmp_path);
		}
		self.next_compact_at = Some(Instant::now() + Duration::from_secs(COMPACT_RETRY_SECS));
		log::warn!("journal runtime compaction aborted: {e} — retrying in {COMPACT_RETRY_SECS}s");
	}

	#[cfg(any(test, feature = "test-hooks"))]
	pub fn compaction_active(&self) -> bool {
		self.run.is_some()
	}

	#[cfg(any(test, feature = "test-hooks"))]
	pub fn reset_compact_cooldown(&mut self) {
		self.next_compact_at = None;
	}

	/// Flush + fsync every shard with something new (the once-a-second durability beat, and
	/// graceful shutdown). Skipping already-synced writers is what keeps this O(shards written)
	/// rather than O(shards open): the trigger in `maintain` is one global `wrote_since_sync`
	/// flag, so a single append to one region used to fsync every open descriptor — and an fsync
	/// costs milliseconds on a container volume, straight out of the sim tick.
	pub fn sync(&mut self) {
		let mut errs = Vec::new();
		for w in self.writers.values_mut() {
			if !w.dirty && !w.unsynced {
				continue;
			}
			if let Err(e) = w.file.flush() {
				errs.push(("flush", e));
				continue;
			}
			w.dirty = false;
			if let Err(e) = w.file.get_ref().sync_data() {
				errs.push(("fsync", e));
				continue;
			}
			w.unsynced = false;
		}
		for (what, e) in errs {
			self.note_write_failure(what, &e);
		}
		self.wrote_since_sync = false;
		self.last_sync = Instant::now();
	}
}

/// One journal record. Authorship of compacted records is not tracked
/// by the index, so both compaction paths pass SERVER_ID.
fn encode_record(x: i32, y: i32, z: i32, block: u16, by: PlayerId) -> [u8; RECORD_LEN as usize] {
	let mut rec = [0u8; RECORD_LEN as usize];
	rec[0..4].copy_from_slice(&x.to_le_bytes());
	rec[4..8].copy_from_slice(&y.to_le_bytes());
	rec[8..12].copy_from_slice(&z.to_le_bytes());
	rec[12..14].copy_from_slice(&block.to_le_bytes());
	rec[14] = by;
	// The trailing byte is reserved, and stays zero.
	rec
}

/// Copy whole columns of the run's region into the tmp file until this
/// tick's record budget is spent; returns true when the snapshot is
/// exhausted. Free function so the caller can borrow `run` mutably
/// alongside the shared index.
#[cfg(not(target_arch = "wasm32"))]
fn copy_step(run: &mut CompactionRun, index: &JournalIndex) -> std::io::Result<bool> {
	let mut wrote_this_tick = 0u64;
	while run.cursor < run.cols.len() && wrote_this_tick < COMPACT_RECORDS_PER_TICK {
		let col = run.cols[run.cursor];
		run.cursor += 1;
		if let Some(cells) = index.columns.get(&col) {
			for (&(x, y, z), &block) in cells {
				run.tmp
					.write_all(&encode_record(x, y, z, block, SERVER_ID))?;
			}
			let n = cells.len() as u64;
			wrote_this_tick += n;
			run.written += n;
			run.since_sync += n;
		}
		// Only now do mid-run appends to this column divert to the delta.
		run.pending.remove(&col);
		if run.since_sync >= COMPACT_RECORDS_PER_SYNC {
			run.tmp.flush()?;
			run.tmp.get_ref().sync_data()?;
			run.since_sync = 0;
		}
	}
	Ok(run.cursor >= run.cols.len())
}

/// Rewrite one region's shard as one record per live cell: tmp, fsync,
/// atomic rename. Returns the record count now in the file. Shared by
/// migration and boot compaction; it holds the whole region in memory and
/// blocks, which is why the runtime equivalent is the incremental
/// `CompactionRun` instead.
#[cfg(not(target_arch = "wasm32"))]
fn write_shard(
	shards: &Path,
	seed: Seed,
	index: &JournalIndex,
	region: (i32, i32),
) -> std::io::Result<u64> {
	let path = shard_path(shards, region);
	let tmp = tmp_path(&path);
	let bytes = region_snapshot_bytes(seed, index, region);
	{
		let mut out = File::create(&tmp)?;
		out.write_all(&bytes)?;
		out.sync_data()?;
	}
	std::fs::rename(&tmp, &path)?;
	Ok((bytes.len() as u64 - HEADER_LEN) / RECORD_LEN)
}

/// Replay one shard into `index`, truncating a torn tail; returns its
/// whole-record count.
#[cfg(not(target_arch = "wasm32"))]
fn load_shard(path: &Path, seed: Seed, index: &mut JournalIndex) -> std::io::Result<u64> {
	let mut file = OpenOptions::new().read(true).write(true).open(path)?;
	let len = file.metadata()?.len();
	if len < HEADER_LEN {
		return Err(bad_data(format!(
			"{}: shorter than the journal header",
			path.display()
		)));
	}
	let mut header = [0u8; HEADER_LEN as usize];
	file.read_exact(&mut header)?;
	check_header(&header, seed).map_err(|e| bad_data(format!("{}: {e}", path.display())))?;
	// Torn tail: keep only whole records.
	let records = (len - HEADER_LEN) / RECORD_LEN;
	let good_len = HEADER_LEN + records * RECORD_LEN;
	if good_len != len {
		log::warn!(
			"journal torn tail in {}: truncating {len} -> {good_len}",
			path.display()
		);
		file.set_len(good_len)?;
	}
	let mut body = vec![0u8; (good_len - HEADER_LEN) as usize];
	file.seek(SeekFrom::Start(HEADER_LEN))?;
	file.read_exact(&mut body)?;
	replay_records(&body, index);
	Ok(records)
}

/// Split a pre-shard whole-world journal into region shards. Crash safety rests entirely on the
/// ordering: every shard is written to a tmp, fsynced and atomically renamed, the shard directory
/// is fsynced, and only THEN is the legacy file removed — a crash before that removal leaves the
/// legacy file intact and the next boot redoes the split, a crash after it leaves a complete
/// shard set, and there is no window in which both are incomplete. The shards come out compacted.
#[cfg(not(target_arch = "wasm32"))]
fn migrate_legacy(legacy: &Path, shards: &Path, seed: Seed) -> std::io::Result<()> {
	let bytes = std::fs::read(legacy)?;
	let index =
		parse_snapshot(&bytes, seed).map_err(|e| bad_data(format!("{}: {e}", legacy.display())))?;
	std::fs::create_dir_all(shards)?;
	let regions: Vec<(i32, i32)> = index.regions().collect();
	for &region in &regions {
		write_shard(shards, seed, &index, region)?;
	}
	sync_dir(shards);
	std::fs::remove_file(legacy)?;
	if let Some(parent) = legacy.parent() {
		sync_dir(parent);
	}
	log::info!(
		"journal migrated to region shards: {} records -> {} live cells across {} regions",
		(bytes.len() as u64).saturating_sub(HEADER_LEN) / RECORD_LEN,
		index.live_cells(),
		regions.len()
	);
	Ok(())
}

/// Best-effort directory fsync — what actually makes a rename durable on
/// Unix. Windows can't open a directory as a file and orders the rename
/// itself, so a failure here is ignored rather than propagated.
#[cfg(not(target_arch = "wasm32"))]
fn sync_dir(dir: &Path) {
	if let Ok(f) = File::open(dir) {
		let _ = f.sync_all();
	}
}

/// The pre-shard single-file journal. Differs from [`shard_dir`] only by
/// the `.journal` suffix, so both can sit side by side mid-migration.
#[cfg(not(target_arch = "wasm32"))]
fn legacy_path(dir: &Path, seed: f64) -> PathBuf {
	dir.join(format!("world-{:016x}.journal", seed.to_bits()))
}

/// The world's shard directory under the caller's data root.
#[cfg(not(target_arch = "wasm32"))]
fn shard_dir(dir: &Path, seed: f64) -> PathBuf {
	dir.join(format!("world-{:016x}", seed.to_bits()))
}

#[cfg(not(target_arch = "wasm32"))]
fn shard_path(shards: &Path, region: (i32, i32)) -> PathBuf {
	shards.join(format!("r.{}.{}.journal", region.0, region.1))
}

#[cfg(not(target_arch = "wasm32"))]
fn tmp_path(shard: &Path) -> PathBuf {
	shard.with_extension("journal.tmp")
}

/// `r.{rx}.{rz}.journal` → its region. Anything else in the directory (a
/// tmp, an archive, an operator's notes) is ignored.
#[cfg(not(target_arch = "wasm32"))]
fn parse_shard_name(name: &str) -> Option<(i32, i32)> {
	let (rx, rz) = name
		.strip_prefix("r.")?
		.strip_suffix(".journal")?
		.split_once('.')?;
	Some((rx.parse().ok()?, rz.parse().ok()?))
}

/// Regions with a shard on disk; an absent directory is an empty world,
/// not an error (a fresh save, or one created but never entered).
#[cfg(not(target_arch = "wasm32"))]
fn list_shards(shards: &Path) -> std::io::Result<Vec<(i32, i32)>> {
	let entries = match std::fs::read_dir(shards) {
		Ok(e) => e,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(e) => return Err(e),
	};
	let mut out = Vec::new();
	for entry in entries.flatten() {
		if let Some(region) = entry.file_name().to_str().and_then(parse_shard_name) {
			out.push(region);
		}
	}
	Ok(out)
}

/// A crashed compaction or migration leaves a `.journal.tmp` behind; it
/// is always garbage — the shard it was replacing is still the truth.
#[cfg(not(target_arch = "wasm32"))]
fn remove_stale_tmps(shards: &Path) {
	let Ok(entries) = std::fs::read_dir(shards) else {
		return;
	};
	for entry in entries.flatten() {
		if entry
			.file_name()
			.to_str()
			.is_some_and(|n| n.ends_with(".journal.tmp"))
		{
			let _ = std::fs::remove_file(entry.path());
		}
	}
}

/// Bytes 6..8 hold the generator. They were written as zero and read by nobody, which is exactly
/// why the field costs no `VERSION` bump: a file from before this slot existed reads zero, and
/// zero IS the first generator (`TerrainVersion::from_wire`). Every world alive today opens
/// untouched.
fn header_bytes(seed: Seed) -> [u8; HEADER_LEN as usize] {
	let mut h = [0u8; HEADER_LEN as usize];
	h[0..4].copy_from_slice(MAGIC);
	h[4..6].copy_from_slice(&VERSION.to_le_bytes());
	h[6..8].copy_from_slice(&seed.version.to_wire().to_le_bytes());
	h[8..16].copy_from_slice(&seed.value.to_le_bytes());
	h
}

/// The generator a header names, or `None` for one this build does not carry — the world is
/// intact and unreadable only by THIS build, which is what a client too old to join now means.
pub fn header_terrain(header: &[u8]) -> Option<TerrainVersion> {
	TerrainVersion::from_wire(u16::from_le_bytes([header[6], header[7]]))
}

/// The generator a STORED world was made with. `Ok(None)` = nothing stored here yet, so the world
/// starts now under whatever the caller chooses. `Err` names a generator this build does not
/// carry, and the boot must fail on it rather than fall back: regenerating an existing world with
/// any other generator quietly replaces the terrain its edits are anchored in.
#[cfg(not(target_arch = "wasm32"))]
pub fn world_terrain(dir: &Path, seed: f64) -> std::io::Result<Option<TerrainVersion>> {
	let legacy = legacy_path(dir, seed);
	let path = if legacy.exists() {
		legacy
	} else {
		let shards = shard_dir(dir, seed);
		match list_shards(&shards)?.into_iter().next() {
			Some(region) => shard_path(&shards, region),
			None => return Ok(None),
		}
	};
	let mut header = [0u8; HEADER_LEN as usize];
	let mut file = match File::open(&path) {
		Ok(f) => f,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(e),
	};
	if file.read_exact(&mut header).is_err() || &header[0..4] != MAGIC {
		return Ok(None);
	}
	match header_terrain(&header) {
		Some(v) => Ok(Some(v)),
		None => Err(std::io::Error::other(format!(
			"{} was generated by terrain {}, which this build does not carry",
			path.display(),
			u16::from_le_bytes([header[6], header[7]])
		))),
	}
}

/// The version of the journal for `seed` under `dir` when it is NOT the
/// current one; None when there is none, it is current, or it is
/// malformed. Pub so a save store can flag an incompatible world in its
/// list (one header read per shard) with a friendlier message than
/// `open`'s refusal.
#[cfg(not(target_arch = "wasm32"))]
pub fn world_version(dir: &Path, seed: f64) -> std::io::Result<Option<u16>> {
	let legacy = legacy_path(dir, seed);
	if legacy.exists() {
		return file_version(&legacy);
	}
	shard_version(&shard_dir(dir, seed))
}

/// The first shard reporting a foreign version. One build writes every
/// shard, so the first answer is the world's answer; the scan runs to the
/// end only while everything is current.
#[cfg(not(target_arch = "wasm32"))]
fn shard_version(shards: &Path) -> std::io::Result<Option<u16>> {
	for region in list_shards(shards)? {
		if let Some(v) = file_version(&shard_path(shards, region))? {
			return Ok(Some(v));
		}
	}
	Ok(None)
}

/// The header's version when `path` is a well-formed journal blob from
/// another format version; None when the file is absent, current, or
/// malformed (short file / bad magic is corruption — `open` reports it as
/// an error rather than archiving data it might still want intact for
/// forensics). [`world_version`] is the layout-aware entry point.
#[cfg(not(target_arch = "wasm32"))]
pub fn file_version(path: &Path) -> std::io::Result<Option<u16>> {
	let mut file = match File::open(path) {
		Ok(f) => f,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(e),
	};
	let mut header = [0u8; HEADER_LEN as usize];
	if file.read_exact(&mut header).is_err() || &header[0..4] != MAGIC {
		return Ok(None);
	}
	let version = u16::from_le_bytes([header[4], header[5]]);
	Ok((version != VERSION).then_some(version))
}

/// The open-time refusal for a stale-format world, which is left standing
/// byte-intact: losing a world is [`archive_world`]'s caller's explicit
/// decision, never a boot side effect.
#[cfg(not(target_arch = "wasm32"))]
fn refuse(path: &Path, version: u16) -> std::io::Error {
	bad_data(format!(
		"journal version {version} != {VERSION}: {} is from another record format and was left \
		 untouched. Run the build that wrote it, or archive it and start fresh (the server's \
		 --archive-world)",
		path.display()
	))
}

/// Archive the stored world aside (`.vN.bak`, byte-intact) so the next open starts fresh — the
/// ONE lever behind every way a world is replaced: `open` refuses a stale format rather than
/// doing this itself, and a worldgen release keeps a world on the generator that made it, so
/// taking the new terrain is also this call. Inert (`Ok(false)`) on a current or absent world,
/// so a flag an operator leaves set archives once per deliberate upgrade, never once per boot.
#[cfg(not(target_arch = "wasm32"))]
pub fn archive_world(dir: &Path, seed: f64) -> std::io::Result<bool> {
	let mut archived = false;
	for path in [legacy_path(dir, seed), shard_dir(dir, seed)] {
		let Some((format, terrain)) = stored_header(&path)? else {
			continue;
		};
		if format == VERSION && TerrainVersion::from_wire(terrain) == Some(TerrainVersion::CURRENT)
		{
			continue;
		}
		archive(&path, format)?;
		archived = true;
	}
	Ok(archived)
}

/// The (format version, terrain wire value) pair a stored world's header names — the first
/// shard's for a shard directory, since one build writes every shard. None when nothing readable
/// stands at `path`.
#[cfg(not(target_arch = "wasm32"))]
fn stored_header(path: &Path) -> std::io::Result<Option<(u16, u16)>> {
	let file = if path.is_dir() {
		match list_shards(path)?.into_iter().next() {
			Some(region) => shard_path(path, region),
			None => return Ok(None),
		}
	} else {
		path.to_path_buf()
	};
	let mut file = match File::open(&file) {
		Ok(f) => f,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(e),
	};
	let mut header = [0u8; HEADER_LEN as usize];
	if file.read_exact(&mut header).is_err() || &header[0..4] != MAGIC {
		return Ok(None);
	}
	Ok(Some((
		u16::from_le_bytes([header[4], header[5]]),
		u16::from_le_bytes([header[6], header[7]]),
	)))
}

/// Move a stored world out of the way, keeping it byte-intact under the
/// record version it was written at. `path` is the legacy file or the
/// shard directory — either way the whole name is kept and suffixed.
#[cfg(not(target_arch = "wasm32"))]
fn archive(path: &Path, version: u16) -> std::io::Result<()> {
	let bak = archive_path(path, version);
	log::warn!("archiving {} to {}", path.display(), bak.display());
	std::fs::rename(path, &bak)
}

/// First free `<name>.v{version}.bak[N]` slot next to `path` — repeated
/// format breaks never overwrite an earlier archive.
#[cfg(not(target_arch = "wasm32"))]
fn archive_path(path: &Path, version: u16) -> PathBuf {
	let name = path
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_default();
	let mut n = 0u32;
	loop {
		let suffix = if n == 0 {
			format!("v{version}.bak")
		} else {
			format!("v{version}.bak{n}")
		};
		let candidate = path.with_file_name(format!("{name}.{suffix}"));
		if !candidate.exists() {
			return candidate;
		}
		n += 1;
	}
}

#[cfg(not(target_arch = "wasm32"))]
fn bad_data(msg: impl Into<String>) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use super::*;
	use std::assert_matches;

	fn edit(x: i32, y: i32, z: i32, block: u16) -> BlockEdit {
		BlockEdit { x, y, z, block }
	}

	/// A world's shard directory, and one shard inside it. Tests that
	/// assert on-disk sizes go through these so the layout lives in one
	/// place.
	fn shards_of(dir: &std::path::Path, seed: Seed) -> PathBuf {
		shard_dir(dir, seed.value)
	}

	fn shard_of(dir: &std::path::Path, seed: Seed, region: (i32, i32)) -> PathBuf {
		shard_path(&shard_dir(dir, seed.value), region)
	}

	fn shard_len(dir: &std::path::Path, seed: Seed, region: (i32, i32)) -> u64 {
		std::fs::metadata(shard_of(dir, seed, region))
			.unwrap_or_else(|e| panic!("shard r.{}.{}: {e}", region.0, region.1))
			.len()
	}

	/// Shard file names in a world directory, sorted.
	fn shard_names(dir: &std::path::Path, seed: Seed) -> Vec<String> {
		let mut names: Vec<String> = std::fs::read_dir(shards_of(dir, seed))
			.unwrap()
			.flatten()
			.map(|e| e.file_name().to_string_lossy().into_owned())
			.collect();
		names.sort();
		names
	}

	/// `columns_near` is an optimization, so its only contract is that it never MISSES a column a
	/// brute-force filter would find. It is allowed to return extras (whole regions, not discs) —
	/// callers re-test. Checked against `edited_columns` over a spread wide enough to cross
	/// several regions in both directions, including negative coordinates, where `div_euclid` and
	/// a plain `/` disagree.
	#[test]
	fn columns_near_never_misses_what_a_full_scan_finds() {
		let mut index = JournalIndex::default();
		// One edit per column over a square span straddling zero, wide
		// enough to cover many whole regions each way.
		for cx in -48..48 {
			for cz in -48..48 {
				index.record(edit(cx * 16, 130, cz * 16, 1));
			}
		}
		for center in [(0, 0), (-33, 17), (40, -40), (-1, -1), (7, 100)] {
			for radius in [0, 1, 4, 20, 47] {
				let want: FxHashSet<(i32, i32)> = index
					.edited_columns()
					.filter(|&c| {
						let (dx, dz) = (c.0 - center.0, c.1 - center.1);
						dx * dx + dz * dz <= radius * radius
					})
					.collect();
				let got: FxHashSet<(i32, i32)> = index.columns_near(center, radius).collect();
				assert!(
					want.is_subset(&got),
					"center {center:?} r{radius}: missed {:?}",
					want.difference(&got).collect::<Vec<_>>()
				);
				// And it is a real narrowing, not just `edited_columns` in
				// disguise — otherwise the whole point is lost.
				if radius <= 4 {
					assert!(
						got.len() < index.edited_columns().count(),
						"center {center:?} r{radius}: walked the whole world"
					);
				}
			}
		}
	}

	#[test]
	fn snapshot_roundtrip_and_refusals() {
		let mut index = JournalIndex::default();
		index.record(edit(5, 130, -2, 0));
		index.record(edit(20, 131, 5, 3));
		index.record(edit(5, 130, -2, 4)); // LWW

		let bytes = snapshot_bytes(Seed::current(42.0), &index);
		assert_eq!(
			bytes.len() as u64,
			HEADER_LEN + 2 * RECORD_LEN,
			"one record per live cell"
		);
		let parsed = parse_snapshot(&bytes, Seed::current(42.0)).unwrap();
		assert_eq!(parsed.live_cells(), 2);
		assert_eq!(parsed.column(0, -1).unwrap().get(&(5, 130, -2)), Some(&4));
		assert_eq!(parsed.column(1, 0).unwrap().get(&(20, 131, 5)), Some(&3));

		// Torn tail: trailing partial record ignored, like the file path.
		let mut torn = bytes.clone();
		torn.extend_from_slice(&[0xAB; 7]);
		assert_eq!(
			parse_snapshot(&torn, Seed::current(42.0))
				.unwrap()
				.live_cells(),
			2
		);

		// Wrong version is a distinct, non-destructive refusal.
		let mut old = bytes.clone();
		old[4..6].copy_from_slice(&1u16.to_le_bytes());
		assert_eq!(
			parse_snapshot(&old, Seed::current(42.0)).unwrap_err(),
			JournalError::WrongVersion(1)
		);
		// Seed mismatch and corruption are refused too.
		assert_matches!(
			parse_snapshot(&bytes, Seed::current(43.0)).unwrap_err(),
			JournalError::SeedMismatch { .. }
		);
		assert_matches!(
			parse_snapshot(b"nope", Seed::current(42.0)).unwrap_err(),
			JournalError::Corrupt(_)
		);
	}

	#[test]
	fn append_reopen_last_write_wins() {
		let dir = std::env::temp_dir().join(format!("dbj-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		{
			let mut j = Journal::open(&dir, Seed::current(42.0)).unwrap();
			assert_eq!(j.append(edit(5, 130, -2, 0), 1), (0, true));
			assert_eq!(j.append(edit(20, 131, 5, 3), 1), (1, true));
			// Same cell edited twice: last write wins in the index, and
			// the column is no longer new.
			assert_eq!(j.append(edit(5, 130, -2, 4), 2), (2, false));
			j.sync();
		}
		let j = Journal::open(&dir, Seed::current(42.0)).unwrap();
		assert_eq!(j.index.next_seq, 3);
		let col = j.index.column(0, -1).expect("column (0,-1)");
		assert_eq!(col.get(&(5, 130, -2)), Some(&4));
		let col = j.index.column(1, 0).expect("column (1,0)");
		assert_eq!(col.get(&(20, 131, 5)), Some(&3));
		// The two cells sit in different columns, and those columns fall in
		// different regions — so they landed in different shards, each
		// named for the region it holds.
		assert_eq!(
			shard_names(&dir, Seed::current(42.0)),
			vec!["r.0.-1.journal", "r.0.0.journal"]
		);
		assert_eq!(
			shard_len(&dir, Seed::current(42.0), (0, -1)),
			HEADER_LEN + 2 * RECORD_LEN
		);
		assert_eq!(
			shard_len(&dir, Seed::current(42.0), (0, 0)),
			HEADER_LEN + RECORD_LEN
		);
		// Another seed is another directory — a fresh, empty world.
		let other = Journal::open(&dir, Seed::current(43.0)).unwrap();
		assert_eq!(other.index.next_seq, 0);
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// A pre-shard single-file world MIGRATES rather than staling: the
	/// container changed, the record format did not.
	#[test]
	fn legacy_single_file_migrates_to_region_shards() {
		let dir = std::env::temp_dir().join(format!("dbj-migrate-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let seed = Seed::current(77.0);
		// Edits spread over four regions, with an LWW overwrite and a
		// second cell sharing the first's column.
		let cells: [(i32, i32, i32, u16); 6] = [
			(5, 130, -2, 1),      // column (0,-1) -> region (0,-1)
			(300, 131, 20, 2),    // column (18,1) -> region (1,0)
			(-40, 120, -300, 3),  // column (-3,-19) -> region (-1,-2)
			(5000, 100, 5000, 4), // column (312,312) -> region (19,19)
			(5, 130, -2, 9),      // LWW over the first record
			(6, 130, -2, 5),      // same column as the first
		];
		let mut bytes = header_bytes(seed).to_vec();
		for &(x, y, z, block) in &cells {
			bytes.extend_from_slice(&encode_record(x, y, z, block, 1));
		}
		let legacy = dir.join(format!("world-{:016x}.journal", seed.value.to_bits()));
		std::fs::write(&legacy, &bytes).unwrap();
		// A half-written shard set from an interrupted migration must be
		// overwritten wholesale, not merged into.
		std::fs::create_dir_all(shards_of(&dir, seed)).unwrap();
		let mut stale = header_bytes(seed).to_vec();
		stale.extend_from_slice(&encode_record(5, 130, -2, 200, 1));
		std::fs::write(shard_of(&dir, seed, (0, -1)), &stale).unwrap();

		let j = Journal::open(&dir, seed).unwrap();
		assert!(
			!legacy.exists(),
			"legacy file dropped once shards are durable"
		);
		assert_eq!(j.index.live_cells(), 5, "every cell preserved (LWW)");
		assert_eq!(j.index.column(0, -1).unwrap().get(&(5, 130, -2)), Some(&9));
		assert_eq!(j.index.column(0, -1).unwrap().get(&(6, 130, -2)), Some(&5));
		assert_eq!(
			j.index.column(18, 1).unwrap().get(&(300, 131, 20)),
			Some(&2)
		);
		assert_eq!(
			j.index.column(-3, -19).unwrap().get(&(-40, 120, -300)),
			Some(&3)
		);
		assert_eq!(
			j.index.column(312, 312).unwrap().get(&(5000, 100, 5000)),
			Some(&4)
		);
		assert_eq!(
			shard_names(&dir, seed),
			vec![
				"r.-1.-2.journal",
				"r.0.-1.journal",
				"r.1.0.journal",
				"r.19.19.journal",
			],
			"one shard per edited region, negative coordinates included"
		);
		// Shards are compacted on the way out, and hold ONLY their region.
		assert_eq!(shard_len(&dir, seed, (0, -1)), HEADER_LEN + 2 * RECORD_LEN);
		assert_eq!(shard_len(&dir, seed, (19, 19)), HEADER_LEN + RECORD_LEN);
		drop(j);

		// Reopening is a plain load: nothing to migrate, nothing to
		// rewrite, and seq restarts at the record count on disk.
		let before = shard_names(&dir, seed);
		let j = Journal::open(&dir, seed).unwrap();
		assert_eq!(shard_names(&dir, seed), before, "reopen is a no-op");
		assert_eq!(j.index.live_cells(), 5);
		assert_eq!(j.index.next_seq, 5);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn torn_tail_recovers() {
		let dir = std::env::temp_dir().join(format!("dbj-torn-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		{
			let mut j = Journal::open(&dir, Seed::current(7.0)).unwrap();
			j.append(edit(1, 100, 1, 2), 1);
			j.append(edit(2, 100, 2, 3), 1);
			j.sync();
		}
		// Simulate a crash mid-record: append a partial record's worth of
		// stray bytes to the shard.
		let path = shard_of(&dir, Seed::current(7.0), (0, 0));
		let mut f = OpenOptions::new().append(true).open(&path).unwrap();
		f.write_all(&[0xAB; 7]).unwrap();
		drop(f);

		let j = Journal::open(&dir, Seed::current(7.0)).unwrap();
		assert_eq!(j.index.next_seq, 2, "partial record dropped");
		assert!(j.index.column(0, 0).unwrap().contains_key(&(2, 100, 2)));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn boot_compaction_rewrites_bloated_shards() {
		let dir = std::env::temp_dir().join(format!("dbj-compact-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		// Many records over a handful of distinct cells (fluid-style
		// churn), all in one region, plus one lone cell far away in
		// another.
		{
			let mut j = Journal::open(&dir, Seed::current(9.0)).unwrap();
			for i in 0..30_000u32 {
				let cell = (i % 100) as i32;
				j.append(edit(cell, 120, 0, (i % 7) as u16), 1);
			}
			j.append(edit(3000, 120, 3000, 2), 1);
			j.sync();
		}
		assert_eq!(
			shard_len(&dir, Seed::current(9.0), (0, 0)),
			HEADER_LEN + 30_000 * RECORD_LEN
		);
		assert_eq!(
			shard_len(&dir, Seed::current(9.0), (11, 11)),
			HEADER_LEN + RECORD_LEN
		);

		// Reopen: the bloated shard compacts down to just its live cells,
		// index identical; the quiet shard is not rewritten at all.
		let j = Journal::open(&dir, Seed::current(9.0)).unwrap();
		assert_eq!(
			shard_len(&dir, Seed::current(9.0), (0, 0)),
			HEADER_LEN + 100 * RECORD_LEN
		);
		assert_eq!(
			shard_len(&dir, Seed::current(9.0), (11, 11)),
			HEADER_LEN + RECORD_LEN
		);
		assert_eq!(j.index.next_seq, 101, "seq restarts at the on-disk count");
		assert_eq!(j.index.live_cells(), 101);
		// A surviving cell holds whatever its LAST write wrote.
		let col = j.index.column(0, 0).unwrap();
		assert_eq!(col.get(&(5, 120, 0)), Some(&((29_905u32 % 7) as u16)));
		drop(j);

		// Reopen again: already compact, a no-op.
		let j = Journal::open(&dir, Seed::current(9.0)).unwrap();
		assert_eq!(
			shard_len(&dir, Seed::current(9.0), (0, 0)),
			HEADER_LEN + 100 * RECORD_LEN
		);
		assert_eq!(j.index.live_cells(), 101);

		// And the compacted shard still survives a torn tail.
		drop(j);
		let mut f = OpenOptions::new()
			.append(true)
			.open(shard_of(&dir, Seed::current(9.0), (0, 0)))
			.unwrap();
		f.write_all(&[0xCD; 5]).unwrap();
		drop(f);
		let j = Journal::open(&dir, Seed::current(9.0)).unwrap();
		assert_eq!(j.index.live_cells(), 101, "torn tail after compaction ok");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn repeated_write_failures_flip_health() {
		let dir = std::env::temp_dir().join(format!("dbj-health-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(11.0)).unwrap();
		assert!(j.healthy());
		j.force_write_failure = true;
		for i in 0..3 {
			j.append(edit(i, 100, 0, 1), 1);
		}
		assert!(!j.healthy(), "three consecutive failures flip health");
		// Recovery: a successful write resets the counter.
		j.force_write_failure = false;
		j.append(edit(9, 100, 0, 1), 1);
		assert!(j.healthy(), "successful write restores health");
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// Drive `maintain()` until the active run completes (bounded).
	fn drive(j: &mut Journal) {
		for _ in 0..1000 {
			j.maintain();
			if !j.compaction_active() {
				return;
			}
		}
		panic!("compaction did not complete in 1000 steps");
	}

	/// Bloat a fresh journal: `n` appends over `cells` distinct cells.
	fn bloat(j: &mut Journal, n: u32, cells: u32) {
		for i in 0..n {
			j.append(edit((i % cells) as i32, 120, 0, (i % 7) as u16), 1);
		}
	}

	#[test]
	fn runtime_compaction_triggers_and_shrinks_shard() {
		let dir = std::env::temp_dir().join(format!("dbj-rt-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(21.0)).unwrap();
		bloat(&mut j, 30_000, 100);
		j.maintain(); // starts the run
		assert!(j.compaction_active());
		drive(&mut j);
		assert_eq!(
			shard_len(&dir, Seed::current(21.0), (0, 0)),
			HEADER_LEN + 100 * RECORD_LEN
		);
		// The runtime contrast to boot compaction: seq is NOT reset.
		assert_eq!(j.index.next_seq, 30_000, "next_seq untouched");
		assert_eq!(j.index.live_cells(), 100);
		drop(j);
		// Reopen replays the compacted shard (no further compaction).
		let j = Journal::open(&dir, Seed::current(21.0)).unwrap();
		assert_eq!(j.index.live_cells(), 100);
		let col = j.index.column(0, 0).unwrap();
		assert_eq!(col.get(&(5, 120, 0)), Some(&((29_905u32 % 7) as u16)));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn appends_during_runtime_compaction_survive() {
		let dir = std::env::temp_dir().join(format!("dbj-rtmid-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(22.0)).unwrap();
		// A few columns of several thousand distinct cells each, sized so
		// one copy step (COMPACT_RECORDS_PER_TICK) covers all but one of
		// them, guaranteeing a mid-run window with a column still pending
		// — whichever the hash order picks. Writing every cell several
		// times over puts the record count safely past the ratio that
		// triggers compaction.
		for round in 1..=3u16 {
			for col in 0..3i32 {
				for i in 0..4000i32 {
					j.append(edit(col * 16, i % 256, i / 256, round), 1);
				}
			}
		}
		j.maintain(); // start
		j.maintain(); // copy step: two of the three columns
		assert!(j.compaction_active(), "one column must still be pending");
		// Re-edit one cell in EVERY column (copied ones divert to the
		// delta, the pending one lands in its base copy), plus a cell in
		// a brand-new column of ANOTHER region — which must NOT enter the
		// delta, since this run doesn't replace that region's shard.
		for col in 0..3i32 {
			j.append(edit(col * 16, 0, 0, 9), 2);
		}
		j.append(edit(300, 50, 7, 5), 2);
		drive(&mut j);
		// Base: the already-copied columns, plus the pending one with its
		// re-edit already applied. Delta: the re-edits that diverted.
		let live = j.index.live_cells();
		assert_eq!(live, 12_001);
		assert_eq!(
			shard_len(&dir, Seed::current(22.0), (0, 0)),
			HEADER_LEN + (12_000 + 2) * RECORD_LEN,
			"base 12000 + 2 delta re-edits"
		);
		assert_eq!(
			shard_len(&dir, Seed::current(22.0), (1, 0)),
			HEADER_LEN + RECORD_LEN,
			"the far column went to its own shard, untouched by the run"
		);
		drop(j);
		// LWW replay: every re-edit and the new column survive.
		let j = Journal::open(&dir, Seed::current(22.0)).unwrap();
		assert_eq!(j.index.live_cells(), 12_001);
		for col in 0..3i32 {
			let c = j.index.column(col, 0).unwrap();
			assert_eq!(c.get(&(col * 16, 0, 0)), Some(&9), "column {col} re-edit");
		}
		assert_eq!(j.index.column(18, 0).unwrap().get(&(300, 50, 7)), Some(&5));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn runtime_compaction_crash_midway_is_safe() {
		let dir = std::env::temp_dir().join(format!("dbj-rtcrash-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		{
			let mut j = Journal::open(&dir, Seed::current(23.0)).unwrap();
			bloat(&mut j, 30_000, 100);
			j.maintain(); // start: tmp created with header only
			assert!(j.compaction_active());
			j.sync();
			// Simulated crash: Journal dropped mid-run, tmp left behind.
		}
		let tmp = tmp_path(&shard_of(&dir, Seed::current(23.0), (0, 0)));
		assert!(tmp.exists(), "stale tmp left by the crash");
		// Reopen: the old shard is the complete truth (boot compaction
		// then shrinks it) and the stale tmp is removed.
		let j = Journal::open(&dir, Seed::current(23.0)).unwrap();
		assert!(!tmp.exists(), "stale tmp cleaned up on open");
		assert_eq!(j.index.live_cells(), 100);
		let col = j.index.column(0, 0).unwrap();
		assert_eq!(col.get(&(5, 120, 0)), Some(&((29_905u32 % 7) as u16)));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn runtime_compaction_seq_continuity() {
		let dir = std::env::temp_dir().join(format!("dbj-rtseq-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(24.0)).unwrap();
		bloat(&mut j, 30_000, 100);
		j.maintain(); // start
		let (seq_mid, _) = j.append(edit(500, 60, 0, 3), 1);
		assert_eq!(seq_mid, 30_000, "seq keeps counting during the run");
		drive(&mut j);
		let (seq_after, _) = j.append(edit(501, 60, 0, 3), 1);
		assert_eq!(seq_after, 30_001, "seq keeps counting across the swap");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn runtime_compaction_failure_degrades_gracefully() {
		let dir = std::env::temp_dir().join(format!("dbj-rtfail-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(25.0)).unwrap();
		bloat(&mut j, 30_000, 100);
		j.maintain(); // start
		j.force_compact_failure = true;
		j.maintain(); // step: injected failure aborts the run
		assert!(!j.compaction_active());
		assert!(j.healthy(), "compaction failure must not gate edits");
		let tmp = tmp_path(&shard_of(&dir, Seed::current(25.0), (0, 0)));
		assert!(!tmp.exists(), "aborted run removes its tmp");
		// A cell in a second region: the eligibility sweep must still find
		// the bloated one, and the quiet shard must be left alone.
		j.append(edit(600, 70, 0, 2), 1);
		// Backoff holds until reset; then a clean run completes.
		j.maintain();
		assert!(!j.compaction_active(), "retry backoff respected");
		j.force_compact_failure = false;
		j.reset_compact_cooldown();
		j.maintain();
		assert!(j.compaction_active());
		drive(&mut j);
		assert_eq!(
			shard_len(&dir, Seed::current(25.0), (0, 0)),
			HEADER_LEN + 100 * RECORD_LEN
		);
		assert_eq!(
			shard_len(&dir, Seed::current(25.0), (2, 0)),
			HEADER_LEN + RECORD_LEN
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn runtime_compaction_respects_cooldown() {
		let dir = std::env::temp_dir().join(format!("dbj-rtcool-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(26.0)).unwrap();
		bloat(&mut j, 30_000, 100);
		j.maintain();
		drive(&mut j);
		// Re-bloat immediately: the cooldown must hold the next run.
		bloat(&mut j, 30_000, 100);
		j.maintain();
		assert!(!j.compaction_active(), "cooldown holds");
		j.reset_compact_cooldown();
		j.maintain();
		assert!(j.compaction_active(), "eligible again after cooldown");
		drive(&mut j);
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// A version-mismatched file (e.g. v1, whose block field was a u8)
	/// can't be read — misparsing it as v2 would corrupt every cell — and
	/// open REFUSES, leaving it byte-intact where it stands. Starting fresh
	/// is `archive_world`, the caller's explicit decision, and repeated
	/// breaks never overwrite an earlier archive.
	#[test]
	fn an_incompatible_journal_refuses_until_archived() {
		let dir = std::env::temp_dir().join(format!("dbj-v1-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join(format!("world-{:016x}.journal", 3.0f64.to_bits()));
		let mut old = Vec::new();
		old.extend_from_slice(MAGIC);
		old.extend_from_slice(&1u16.to_le_bytes()); // VERSION 1
		old.extend_from_slice(&0u16.to_le_bytes());
		old.extend_from_slice(&3.0f64.to_le_bytes());
		old.extend_from_slice(&[0xAB; 13]); // v1 record bytes, any shape
		std::fs::write(&path, &old).unwrap();

		let err = Journal::open(&dir, Seed::current(3.0))
			.err()
			.expect("refuses");
		assert!(err.to_string().contains("version 1"), "{err}");
		assert_eq!(std::fs::read(&path).unwrap(), old, "left byte-intact");

		assert!(archive_world(&dir, 3.0).unwrap(), "stale world archives");
		let j = Journal::open(&dir, Seed::current(3.0)).unwrap();
		assert!(j.records.is_empty(), "fresh world, no records");
		drop(j);
		let bak = path.with_extension("journal.v1.bak");
		assert_eq!(std::fs::read(&bak).unwrap(), old, "archived byte-intact");

		// A second break with a .bak already present takes the next slot.
		std::fs::write(&path, &old).unwrap();
		assert!(archive_world(&dir, 3.0).unwrap());
		assert!(path.with_extension("journal.v1.bak1").exists());

		// The fresh world reopens as a normal current-version journal.
		Journal::open(&dir, Seed::current(3.0)).unwrap();
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// Same policy one layout up: a whole shard DIRECTORY from another
	/// version refuses intact until archived, and the fresh world is
	/// current.
	#[test]
	fn an_incompatible_shard_dir_refuses_until_archived() {
		let dir = std::env::temp_dir().join(format!("dbj-vdir-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let seed = Seed::current(4.0);
		{
			let mut j = Journal::open(&dir, seed).unwrap();
			j.append(edit(1, 100, 1, 2), 1);
			j.sync();
		}
		// Rewrite the shard's header version, as a format break would.
		let shard = shard_of(&dir, seed, (0, 0));
		let mut bytes = std::fs::read(&shard).unwrap();
		bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
		std::fs::write(&shard, &bytes).unwrap();
		assert_eq!(world_version(&dir, seed.value).unwrap(), Some(1));

		let err = Journal::open(&dir, seed).err().expect("refuses");
		assert!(err.to_string().contains("version 1"), "{err}");
		assert_eq!(std::fs::read(&shard).unwrap(), bytes, "left byte-intact");

		assert!(archive_world(&dir, seed.value).unwrap());
		let j = Journal::open(&dir, seed).unwrap();
		assert!(j.records.is_empty(), "fresh world, no records");
		assert_eq!(j.index.live_cells(), 0);
		drop(j);
		let bak = shards_of(&dir, seed)
			.with_file_name(format!("world-{:016x}.v1.bak", seed.value.to_bits()));
		assert_eq!(
			std::fs::read(bak.join("r.0.0.journal")).unwrap(),
			bytes,
			"archived byte-intact"
		);
		assert_eq!(
			world_version(&dir, seed.value).unwrap(),
			None,
			"fresh is current"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// `archive_world` is inert on a world already on the current format
	/// and generator: an operator flag left set archives once per
	/// deliberate upgrade, never once per boot.
	#[test]
	fn archive_world_never_touches_a_current_world() {
		let dir = std::env::temp_dir().join(format!("dbj-inert-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let seed = Seed::current(6.0);
		{
			let mut j = Journal::open(&dir, seed).unwrap();
			j.append(edit(1, 100, 1, 2), 1);
			j.sync();
		}
		assert!(
			!archive_world(&dir, seed.value).unwrap(),
			"current world is not archived"
		);
		let j = Journal::open(&dir, seed).unwrap();
		assert_eq!(j.index.live_cells(), 1, "edits survive");
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// A world whose header names a generator other than the current one is
	/// intact and playable — continuing is `open`'s job — and trading it
	/// for the current generator is `archive_world`'s, at the current
	/// record version's suffix since the FORMAT is not what differs.
	#[test]
	fn archive_world_rotates_a_foreign_generator_world() {
		let dir = std::env::temp_dir().join(format!("dbj-gen-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let seed = Seed::current(8.0);
		{
			let mut j = Journal::open(&dir, seed).unwrap();
			j.append(edit(1, 100, 1, 2), 1);
			j.sync();
		}
		// Rewrite the header's generator slot to one this build lacks — the
		// shape a world from a later build (or a later generator row) has.
		let shard = shard_of(&dir, seed, (0, 0));
		let mut bytes = std::fs::read(&shard).unwrap();
		bytes[6..8].copy_from_slice(&999u16.to_le_bytes());
		std::fs::write(&shard, &bytes).unwrap();

		assert!(archive_world(&dir, seed.value).unwrap());
		let bak = shards_of(&dir, seed).with_file_name(format!(
			"world-{:016x}.v{VERSION}.bak",
			seed.value.to_bits()
		));
		assert_eq!(
			std::fs::read(bak.join("r.0.0.journal")).unwrap(),
			bytes,
			"archived byte-intact"
		);
		let j = Journal::open(&dir, seed).unwrap();
		assert_eq!(j.index.live_cells(), 0, "fresh world");
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// The writer LRU keeps the descriptor count bounded no matter how far
	/// a player wanders, and every closed shard is still complete.
	#[test]
	fn open_writers_are_capped() {
		let dir = std::env::temp_dir().join(format!("dbj-lru-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, Seed::current(31.0)).unwrap();
		let regions = MAX_OPEN_REGIONS + 8;
		for r in 0..regions as i32 {
			// One cell per region: each append lands a whole region
			// further along x.
			j.append(edit(r * 256, 100, 0, (r % 7) as u16), 1);
			assert!(j.writers.len() <= MAX_OPEN_REGIONS, "descriptor cap held");
		}
		j.sync();
		assert_eq!(j.writers.len(), MAX_OPEN_REGIONS);
		drop(j);
		// Every region — including the ones evicted mid-session — reloads.
		let j = Journal::open(&dir, Seed::current(31.0)).unwrap();
		assert_eq!(j.index.live_cells(), regions as u64);
		for r in 0..regions as i32 {
			let col = j.index.column(r * 16, 0).expect("column");
			assert_eq!(col.get(&(r * 256, 100, 0)), Some(&((r % 7) as u16)));
		}
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// The generator round-trips through the header, and a header from before the slot existed —
	/// two zero bytes where it now lives — reads as the FIRST generator rather than as absent.
	/// That is the whole claim behind the field costing no `VERSION` bump: every world alive when
	/// it landed opens untouched, with the generator that actually made it.
	#[test]
	fn a_header_carries_its_generator_and_an_old_one_reads_as_the_first() {
		for version in TerrainVersion::ALL {
			let h = header_bytes(Seed::new(1234.5, version));
			assert_eq!(header_terrain(&h), Some(version));
			assert_eq!(
				u16::from_le_bytes([h[4], h[5]]),
				VERSION,
				"{version:?} must not move the record format"
			);
		}
		let mut old = header_bytes(Seed::current(1234.5));
		old[6] = 0;
		old[7] = 0;
		assert_eq!(
			header_terrain(&old),
			Some(TerrainVersion::V1),
			"a pre-slot header is the first generator, not a migration"
		);
	}

	/// A stored world reports the generator that made it, so a server restarted onto a build with
	/// a newer one still regenerates the terrain its edits are anchored in. No world yet is
	/// `None` — the caller chooses, and that is the only place CURRENT is assumed.
	#[test]
	fn a_stored_world_reports_the_generator_that_made_it() {
		let dir = std::env::temp_dir().join(format!("dbj-gen-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let seed = Seed::current(4242.0);
		assert_eq!(
			world_terrain(&dir, seed.value).unwrap(),
			None,
			"nothing stored yet, so the caller chooses"
		);
		let mut j = Journal::open(&dir, seed).unwrap();
		j.append(
			BlockEdit {
				x: 1,
				y: 2,
				z: 3,
				block: 4,
			},
			1,
		);
		drop(j);
		assert_eq!(
			world_terrain(&dir, seed.value).unwrap(),
			Some(seed.version),
			"a restart regenerates the terrain the edits are anchored in"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}
}
