//! Append-only world persistence: every accepted edit is one fixed-size
//! record. Boot loads the whole file into a last-write-wins in-memory
//! index; application to chunks is lazy (the store overlays a column's
//! cells when that column generates), so replay never waits on chunk
//! loading. Fixed-size records give trivial torn-tail recovery: truncate
//! to the last whole record.

use digblock_net::messages::BlockEdit;
use digblock_net::protocol::{PlayerId, SERVER_ID};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"DBJ1";
/// v2: the block field widened to a u16 CELL (id + reserved state bits)
/// at record bytes 12..14; `by` moved to byte 14. Old-version files are
/// archived to `.vN.bak` at open and the world starts fresh.
/// v3: worldgen retune (SIM v5) — the record format is unchanged, but the
/// same seed now yields different terrain, so stored edits are stale;
/// archive and start fresh.
const VERSION: u16 = 3;
const HEADER_LEN: u64 = 16;
const RECORD_LEN: u64 = 16;
const SYNC_INTERVAL_SECS: f32 = 1.0;

/// Final block CELL value per edited cell within one chunk column.
pub type ColumnEdits = FxHashMap<(i32, i32, i32), u16>;

/// Last-write-wins cell state, grouped by chunk column for join sync.
#[derive(Default)]
pub struct JournalIndex {
	columns: FxHashMap<(i32, i32), ColumnEdits>,
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

	/// Columns that have at least one edit.
	pub fn edited_columns(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
		self.columns.keys().copied()
	}
}

/// Consecutive write failures before the journal reports unhealthy.
const MAX_WRITE_FAILURES: u32 = 3;
/// Compact (boot or runtime) when the file holds this many times more
/// records than live cells (and enough of them to matter).
const COMPACT_RATIO: u64 = 2;
const COMPACT_MIN_RECORDS: u64 = 10_000;
/// Runtime compaction: copy at least this many records per tick. A whole
/// column always copies atomically (map iteration order is invalidated
/// by mutation), so a single tick can overshoot by one column.
const COMPACT_RECORDS_PER_TICK: u64 = 4096;
/// Intermediate tmp fsync cadence (~2 MB) so the finishing tick never
/// pays one huge fsync inside the 60Hz budget.
const COMPACT_RECORDS_PER_SYNC: u64 = 131_072;
/// Cooldown after a completed run (fluid churn re-bloats slowly).
const COMPACT_COOLDOWN_SECS: u64 = 60;
/// Backoff after a failed run.
const COMPACT_RETRY_SECS: u64 = 300;
/// Abort if mid-run appends outpace the copy this badly — impossible
/// within the fluid/edit rate budgets, but fail safe.
const COMPACT_DELTA_MAX: usize = 65_536;

/// In-flight runtime compaction, stepped once per `maintain()` call.
///
/// Invariants the correctness rests on (see `append` for the capture
/// rule): the in-memory `index` and `next_seq` are NEVER touched — only
/// the file is rewritten, so join sync and seq monotonicity are safe by
/// construction. All live appends keep going to the OLD file until the
/// atomic rename, so a crash at any point leaves a complete old file
/// plus a garbage tmp that the next boot truncates.
struct CompactionRun {
	tmp: BufWriter<File>,
	tmp_path: PathBuf,
	/// Snapshot of column keys at start; copied whole, one cursor step at
	/// a time. Columns are never removed from the index, so every key
	/// stays valid for the whole run.
	cols: Vec<(i32, i32)>,
	cursor: usize,
	/// Columns not yet copied. A mid-run append goes to `delta` iff its
	/// column is NOT here: a pending column's copy will read the current
	/// (newer or equal) index value when its turn comes, and a delta
	/// record — written after the base — would resurrect the OLDER value
	/// on last-write-wins replay.
	pending: FxHashSet<(i32, i32)>,
	/// Raw records appended mid-run to already-copied or brand-new
	/// columns; written after the base copy, so they win LWW replay.
	delta: Vec<[u8; RECORD_LEN as usize]>,
	written: u64,
	since_sync: u64,
}

pub struct Journal {
	file: BufWriter<File>,
	path: PathBuf,
	seed: f64,
	pub index: JournalIndex,
	dirty: bool,
	wrote_since_sync: bool,
	last_sync: Instant,
	/// Records currently in the file (successful appends since open, on
	/// top of the boot state) — the runtime-compaction trigger input.
	records: u64,
	run: Option<CompactionRun>,
	/// Runtime compaction not eligible before this instant (cooldown
	/// after success, backoff after failure). None = eligible now.
	next_compact_at: Option<Instant>,
	/// Consecutive append/flush/fsync failures; reset on success. Past
	/// MAX_WRITE_FAILURES the sim stops accepting edits (rejecting with
	/// ServerError) rather than diverging memory from disk silently.
	write_failures: u32,
	last_health_log: Option<Instant>,
	#[cfg(test)]
	pub force_write_failure: bool,
	#[cfg(test)]
	pub force_compact_failure: bool,
}

impl Journal {
	/// Open (or create) the journal for `seed` under `dir`. A seed
	/// mismatch with an existing file is a hard error — the terrain the
	/// edits apply to would not exist.
	pub fn open(dir: &Path, seed: f64) -> std::io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join(format!("world-{:016x}.journal", seed.to_bits()));
		// A crashed runtime compaction may leave a stale tmp behind.
		let _ = std::fs::remove_file(path.with_extension("journal.tmp"));
		// Format break: a file from another version can't be read, but
		// refusing to boot bricks persistent volumes (the compose rig's
		// ./data) until someone intervenes. Archive it and start fresh —
		// the old world stays next door as `.vN.bak`.
		if let Some(version) = stale_version(&path)? {
			let bak = archive_path(&path, version);
			log::warn!(
				"journal version {version} != {VERSION}: archiving to {}, starting fresh",
				bak.display()
			);
			std::fs::rename(&path, &bak)?;
		}
		let mut file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(&path)?;

		let len = file.metadata()?.len();
		let mut index = JournalIndex::default();
		let mut record_count = 0u64;
		if len == 0 {
			file.write_all(&header_bytes(seed))?;
			file.sync_data()?;
		} else {
			let mut header = [0u8; HEADER_LEN as usize];
			file.read_exact(&mut header)?;
			if &header[0..4] != MAGIC {
				return Err(bad_data("journal magic mismatch"));
			}
			let version = u16::from_le_bytes([header[4], header[5]]);
			if version != VERSION {
				return Err(bad_data(format!("journal version {version} != {VERSION}")));
			}
			let file_seed = f64::from_le_bytes(header[8..16].try_into().unwrap());
			if file_seed.to_bits() != seed.to_bits() {
				return Err(bad_data(format!(
					"journal seed {file_seed} != requested {seed}"
				)));
			}
			// Torn tail: keep only whole records.
			let records = (len - HEADER_LEN) / RECORD_LEN;
			let good_len = HEADER_LEN + records * RECORD_LEN;
			if good_len != len {
				log::warn!("journal torn tail: truncating {len} -> {good_len}");
				file.set_len(good_len)?;
			}
			let mut body = Vec::with_capacity((good_len - HEADER_LEN) as usize);
			file.seek(SeekFrom::Start(HEADER_LEN))?;
			(&mut file)
				.take(good_len - HEADER_LEN)
				.read_to_end(&mut body)?;
			for rec in body.chunks_exact(RECORD_LEN as usize) {
				let edit = BlockEdit {
					x: i32::from_le_bytes(rec[0..4].try_into().unwrap()),
					y: i32::from_le_bytes(rec[4..8].try_into().unwrap()),
					z: i32::from_le_bytes(rec[8..12].try_into().unwrap()),
					block: u16::from_le_bytes([rec[12], rec[13]]),
				};
				index.record(edit);
			}

			// Boot compaction: fluid appends every cell change, so the
			// file grows without bound while the index stays last-write-
			// wins small. Rewrite (tmp + atomic rename) when bloated.
			// Seq numbers restart at the live-cell count — they are
			// per-boot monotonic only, clients never persist them.
			let records = (good_len - HEADER_LEN) / RECORD_LEN;
			let live = index.live_cells();
			record_count = records;
			if records > COMPACT_RATIO * live && records > COMPACT_MIN_RECORDS {
				log::info!("journal compaction: {records} records -> {live} live cells");
				file = compact(&path, seed, &index)?;
				index.next_seq = live;
				record_count = live;
			}
		}
		file.seek(SeekFrom::End(0))?;
		Ok(Self {
			file: BufWriter::new(file),
			path,
			seed,
			index,
			dirty: false,
			wrote_since_sync: false,
			last_sync: Instant::now(),
			records: record_count,
			run: None,
			next_compact_at: None,
			write_failures: 0,
			last_health_log: None,
			#[cfg(test)]
			force_write_failure: false,
			#[cfg(test)]
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
		match self.write_record(&rec) {
			Ok(()) => {
				self.write_failures = 0;
				self.records += 1;
			}
			Err(e) => self.note_write_failure("write", &e),
		}
		// Mid-compaction capture (see CompactionRun::pending for the
		// rule). Tracked off the index like the base copy, so even an
		// append the old file lost lands in the compacted file.
		let mut delta_overflow = false;
		if let Some(run) = &mut self.run {
			let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
			if !run.pending.contains(&col) {
				run.delta.push(rec);
				delta_overflow = run.delta.len() > COMPACT_DELTA_MAX;
			}
		}
		if delta_overflow {
			self.abort_compaction(&std::io::Error::other(
				"delta overflow (churn outpaced the copy)",
			));
		}
		self.dirty = true;
		self.wrote_since_sync = true;
		self.index.record(edit)
	}

	fn write_record(&mut self, rec: &[u8]) -> std::io::Result<()> {
		#[cfg(test)]
		if self.force_write_failure {
			return Err(std::io::Error::other("injected write failure"));
		}
		self.file.write_all(rec)
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
		if self.dirty {
			match self.file.flush() {
				Ok(()) => {}
				Err(e) => self.note_write_failure("flush", &e),
			}
			self.dirty = false;
		}
		if self.wrote_since_sync && self.last_sync.elapsed().as_secs_f32() >= SYNC_INTERVAL_SECS {
			self.sync();
		}
		if self.run.is_some() {
			self.step_compaction();
		} else {
			self.maybe_start_compaction();
		}
	}

	fn maybe_start_compaction(&mut self) {
		if !self.healthy() {
			return;
		}
		if self.next_compact_at.is_some_and(|at| Instant::now() < at) {
			return;
		}
		let live = self.index.live_cells();
		if self.records <= COMPACT_RATIO * live || self.records <= COMPACT_MIN_RECORDS {
			return;
		}
		match self.start_compaction() {
			Ok(()) => log::info!(
				"journal runtime compaction started: {} records, {live} live cells",
				self.records
			),
			Err(e) => self.abort_compaction(&e),
		}
	}

	fn start_compaction(&mut self) -> std::io::Result<()> {
		let tmp_path = self.path.with_extension("journal.tmp");
		let mut tmp = BufWriter::new(File::create(&tmp_path)?);
		tmp.write_all(&header_bytes(self.seed))?;
		let cols: Vec<(i32, i32)> = self.index.columns.keys().copied().collect();
		let pending = cols.iter().copied().collect();
		self.run = Some(CompactionRun {
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
	/// exhausted. Any I/O error aborts the run (the old file is still the
	/// complete truth).
	fn step_compaction(&mut self) {
		#[cfg(test)]
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

	/// Append the delta, fsync, atomically swap the file, and continue
	/// appending to the compacted journal. `index`/`next_seq` are never
	/// touched — runtime compaction is invisible to join sync and seq
	/// monotonicity.
	fn finish_compaction(&mut self) -> std::io::Result<()> {
		let mut run = self.run.take().expect("active run");
		for rec in &run.delta {
			run.tmp.write_all(rec)?;
		}
		run.tmp.flush()?;
		run.tmp.get_ref().sync_data()?;
		drop(run.tmp);
		std::fs::rename(&run.tmp_path, &self.path)?;
		let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
		file.seek(SeekFrom::End(0))?;
		let old = self.records;
		self.records = run.written + run.delta.len() as u64;
		// The old writer was flushed at the top of this maintain() and
		// every append since start is in the delta — nothing is lost.
		self.file = BufWriter::new(file);
		self.dirty = false;
		self.wrote_since_sync = false;
		self.last_sync = Instant::now();
		self.next_compact_at = Some(Instant::now() + Duration::from_secs(COMPACT_COOLDOWN_SECS));
		log::info!(
			"journal runtime compaction: {old} -> {} records",
			self.records
		);
		Ok(())
	}

	/// Best-effort cleanup + backoff. Deliberately does NOT touch
	/// `write_failures`: that counter gates edit acceptance, and a failed
	/// compaction leaves a correct (merely bloated) file — rejecting
	/// edits over it would be worse than the disease.
	fn abort_compaction(&mut self, e: &std::io::Error) {
		if let Some(run) = self.run.take() {
			drop(run.tmp);
			let _ = std::fs::remove_file(&run.tmp_path);
		}
		self.next_compact_at = Some(Instant::now() + Duration::from_secs(COMPACT_RETRY_SECS));
		log::warn!("journal runtime compaction aborted: {e} — retrying in {COMPACT_RETRY_SECS}s");
	}

	#[cfg(test)]
	pub fn compaction_active(&self) -> bool {
		self.run.is_some()
	}

	#[cfg(test)]
	pub fn reset_compact_cooldown(&mut self) {
		self.next_compact_at = None;
	}

	/// Flush + fsync now (graceful shutdown).
	pub fn sync(&mut self) {
		if let Err(e) = self.file.flush() {
			self.note_write_failure("flush", &e);
		}
		if let Err(e) = self.file.get_ref().sync_data() {
			self.note_write_failure("fsync", &e);
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
	// rec[15] reserved, zero.
	rec
}

/// Copy whole columns into the tmp file until this tick's record budget
/// is spent; returns true when the snapshot is exhausted. Free function
/// so the caller can borrow `run` mutably alongside the shared index.
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

/// Rewrite the journal as one record per live cell: tmp file, fsync,
/// atomic rename, reopen. Boot-time (blocking) variant; the runtime
/// equivalent is the incremental `CompactionRun`.
fn compact(path: &Path, seed: f64, index: &JournalIndex) -> std::io::Result<File> {
	let tmp = path.with_extension("journal.tmp");
	{
		let mut out = BufWriter::new(File::create(&tmp)?);
		out.write_all(&header_bytes(seed))?;
		for cells in index.columns.values() {
			for (&(x, y, z), &block) in cells {
				out.write_all(&encode_record(x, y, z, block, SERVER_ID))?;
			}
		}
		out.flush()?;
		out.get_ref().sync_data()?;
	}
	std::fs::rename(&tmp, path)?;
	OpenOptions::new().read(true).write(true).open(path)
}

fn header_bytes(seed: f64) -> [u8; HEADER_LEN as usize] {
	let mut h = [0u8; HEADER_LEN as usize];
	h[0..4].copy_from_slice(MAGIC);
	h[4..6].copy_from_slice(&VERSION.to_le_bytes());
	h[8..16].copy_from_slice(&seed.to_le_bytes());
	h
}

/// The header's version when `path` is a well-formed journal from another
/// format version; None when the file is absent, current, or malformed
/// (short file / bad magic is corruption — `open` reports it as an error
/// rather than archiving data it might still want intact for forensics).
fn stale_version(path: &Path) -> std::io::Result<Option<u16>> {
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

/// First free `<world>.journal.v{version}.bak[N]` slot next to `path` —
/// repeated format breaks never overwrite an earlier archive.
fn archive_path(path: &Path, version: u16) -> PathBuf {
	let mut n = 0u32;
	loop {
		let ext = if n == 0 {
			format!("journal.v{version}.bak")
		} else {
			format!("journal.v{version}.bak{n}")
		};
		let candidate = path.with_extension(ext);
		if !candidate.exists() {
			return candidate;
		}
		n += 1;
	}
}

fn bad_data(msg: impl Into<String>) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn edit(x: i32, y: i32, z: i32, block: u16) -> BlockEdit {
		BlockEdit { x, y, z, block }
	}

	#[test]
	fn append_reopen_last_write_wins() {
		let dir = std::env::temp_dir().join(format!("dbj-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		{
			let mut j = Journal::open(&dir, 42.0).unwrap();
			assert_eq!(j.append(edit(5, 130, -2, 0), 1), (0, true));
			assert_eq!(j.append(edit(20, 131, 5, 3), 1), (1, true));
			// Same cell edited twice: last write wins in the index, and
			// the column is no longer new.
			assert_eq!(j.append(edit(5, 130, -2, 4), 2), (2, false));
			j.sync();
		}
		let j = Journal::open(&dir, 42.0).unwrap();
		assert_eq!(j.index.next_seq, 3);
		let col = j.index.column(0, -1).expect("column (0,-1)");
		assert_eq!(col.get(&(5, 130, -2)), Some(&4));
		let col = j.index.column(1, 0).expect("column (1,0)");
		assert_eq!(col.get(&(20, 131, 5)), Some(&3));
		// Another seed is another file — a fresh, empty world.
		let other = Journal::open(&dir, 43.0).unwrap();
		assert_eq!(other.index.next_seq, 0);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn torn_tail_recovers() {
		let dir = std::env::temp_dir().join(format!("dbj-torn-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		{
			let mut j = Journal::open(&dir, 7.0).unwrap();
			j.append(edit(1, 100, 1, 2), 1);
			j.append(edit(2, 100, 2, 3), 1);
			j.sync();
		}
		// Simulate a crash mid-record: append 7 stray bytes.
		let path = dir.join(format!("world-{:016x}.journal", 7.0f64.to_bits()));
		let mut f = OpenOptions::new().append(true).open(&path).unwrap();
		f.write_all(&[0xAB; 7]).unwrap();
		drop(f);

		let j = Journal::open(&dir, 7.0).unwrap();
		assert_eq!(j.index.next_seq, 2, "partial record dropped");
		assert!(j.index.column(0, 0).unwrap().contains_key(&(2, 100, 2)));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn boot_compaction_rewrites_bloated_files() {
		let dir = std::env::temp_dir().join(format!("dbj-compact-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		// 30k records over 100 distinct cells (fluid-style churn).
		{
			let mut j = Journal::open(&dir, 9.0).unwrap();
			for i in 0..30_000u32 {
				let cell = (i % 100) as i32;
				j.append(edit(cell, 120, 0, (i % 7) as u16), 1);
			}
			j.sync();
		}
		let path = dir.join(format!("world-{:016x}.journal", 9.0f64.to_bits()));
		let bloated = std::fs::metadata(&path).unwrap().len();
		assert_eq!(bloated, HEADER_LEN + 30_000 * RECORD_LEN);

		// Reopen: compacts to 100 live cells, index identical.
		let j = Journal::open(&dir, 9.0).unwrap();
		assert_eq!(
			std::fs::metadata(&path).unwrap().len(),
			HEADER_LEN + 100 * RECORD_LEN
		);
		assert_eq!(j.index.next_seq, 100, "seq restarts at live count");
		assert_eq!(j.index.live_cells(), 100);
		// Last write for cell 5: i = 29905 -> block 29905 % 7.
		let col = j.index.column(0, 0).unwrap();
		assert_eq!(col.get(&(5, 120, 0)), Some(&((29_905u32 % 7) as u16)));
		drop(j);

		// Reopen again: already compact, a no-op.
		let j = Journal::open(&dir, 9.0).unwrap();
		assert_eq!(
			std::fs::metadata(&path).unwrap().len(),
			HEADER_LEN + 100 * RECORD_LEN
		);
		assert_eq!(j.index.live_cells(), 100);

		// And the compacted file still survives a torn tail.
		drop(j);
		let mut f = OpenOptions::new().append(true).open(&path).unwrap();
		f.write_all(&[0xCD; 5]).unwrap();
		drop(f);
		let j = Journal::open(&dir, 9.0).unwrap();
		assert_eq!(j.index.live_cells(), 100, "torn tail after compaction ok");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn repeated_write_failures_flip_health() {
		let dir = std::env::temp_dir().join(format!("dbj-health-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, 11.0).unwrap();
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
	fn runtime_compaction_triggers_and_shrinks_file() {
		let dir = std::env::temp_dir().join(format!("dbj-rt-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, 21.0).unwrap();
		bloat(&mut j, 30_000, 100);
		j.maintain(); // starts the run
		assert!(j.compaction_active());
		drive(&mut j);
		let path = dir.join(format!("world-{:016x}.journal", 21.0f64.to_bits()));
		assert_eq!(
			std::fs::metadata(&path).unwrap().len(),
			HEADER_LEN + 100 * RECORD_LEN
		);
		// The runtime contrast to boot compaction: seq is NOT reset.
		assert_eq!(j.index.next_seq, 30_000, "next_seq untouched");
		assert_eq!(j.index.live_cells(), 100);
		drop(j);
		// Reopen replays the compacted file (no further compaction).
		let j = Journal::open(&dir, 21.0).unwrap();
		assert_eq!(j.index.live_cells(), 100);
		let col = j.index.column(0, 0).unwrap();
		assert_eq!(col.get(&(5, 120, 0)), Some(&((29_905u32 % 7) as u16)));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn appends_during_runtime_compaction_survive() {
		let dir = std::env::temp_dir().join(format!("dbj-rtmid-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, 22.0).unwrap();
		// Three columns × 4000 distinct cells: one copy step covers
		// exactly two columns (budget 4096), guaranteeing a mid-run
		// window with one column still pending — whichever the hash
		// order picks.
		// Three writes per cell: 36k records over 12k live cells, safely
		// past the >2× ratio trigger.
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
		// a brand-new column.
		for col in 0..3i32 {
			j.append(edit(col * 16, 0, 0, 9), 2);
		}
		j.append(edit(300, 50, 7, 5), 2);
		drive(&mut j);
		// Base: two untouched columns (4000 each) + pending column with
		// the re-edit already applied (4000). Delta: two re-edits + the
		// new column's cell.
		let live = j.index.live_cells();
		assert_eq!(live, 12_001);
		let path = dir.join(format!("world-{:016x}.journal", 22.0f64.to_bits()));
		assert_eq!(
			std::fs::metadata(&path).unwrap().len(),
			HEADER_LEN + (12_000 + 3) * RECORD_LEN,
			"base 12000 + 2 delta re-edits + 1 delta new-column cell"
		);
		drop(j);
		// LWW replay: every re-edit and the new column survive.
		let j = Journal::open(&dir, 22.0).unwrap();
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
			let mut j = Journal::open(&dir, 23.0).unwrap();
			bloat(&mut j, 30_000, 100);
			j.maintain(); // start: tmp created with header only
			assert!(j.compaction_active());
			j.sync();
			// Simulated crash: Journal dropped mid-run, tmp left behind.
		}
		let tmp = dir.join(format!("world-{:016x}.journal.tmp", 23.0f64.to_bits()));
		assert!(tmp.exists(), "stale tmp left by the crash");
		// Reopen: the old file is the complete truth (boot compaction
		// then shrinks it) and the stale tmp is removed.
		let j = Journal::open(&dir, 23.0).unwrap();
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
		let mut j = Journal::open(&dir, 24.0).unwrap();
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
		let mut j = Journal::open(&dir, 25.0).unwrap();
		bloat(&mut j, 30_000, 100);
		j.maintain(); // start
		j.force_compact_failure = true;
		j.maintain(); // step: injected failure aborts the run
		assert!(!j.compaction_active());
		assert!(j.healthy(), "compaction failure must not gate edits");
		let tmp = dir.join(format!("world-{:016x}.journal.tmp", 25.0f64.to_bits()));
		assert!(!tmp.exists(), "aborted run removes its tmp");
		j.append(edit(600, 70, 0, 2), 1);
		// Backoff holds until reset; then a clean run completes.
		j.maintain();
		assert!(!j.compaction_active(), "retry backoff respected");
		j.force_compact_failure = false;
		j.reset_compact_cooldown();
		j.maintain();
		assert!(j.compaction_active());
		drive(&mut j);
		let path = dir.join(format!("world-{:016x}.journal", 25.0f64.to_bits()));
		assert_eq!(
			std::fs::metadata(&path).unwrap().len(),
			HEADER_LEN + 101 * RECORD_LEN
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn runtime_compaction_respects_cooldown() {
		let dir = std::env::temp_dir().join(format!("dbj-rtcool-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut j = Journal::open(&dir, 26.0).unwrap();
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

	/// A version-mismatched file (e.g. v1: u8 block at byte 12) can't be
	/// read — misparsing it as v2 would corrupt every cell — but refusing
	/// to boot bricks persistent volumes across format breaks. It is
	/// archived byte-intact to `.v1.bak` and a fresh world starts.
	#[test]
	fn incompatible_journal_is_archived() {
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

		let j = Journal::open(&dir, 3.0).unwrap();
		assert_eq!(j.records, 0, "fresh world, no records");
		drop(j);
		let bak = path.with_extension("journal.v1.bak");
		assert_eq!(std::fs::read(&bak).unwrap(), old, "archived byte-intact");

		// A second break with a .bak already present takes the next slot.
		std::fs::write(&path, &old).unwrap();
		Journal::open(&dir, 3.0).unwrap();
		assert!(path.with_extension("journal.v1.bak1").exists());

		// The fresh file reopens as a normal current-version journal.
		Journal::open(&dir, 3.0).unwrap();
		let _ = std::fs::remove_dir_all(&dir);
	}
}
