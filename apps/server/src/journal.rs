//! Append-only world persistence: every accepted edit is one fixed-size
//! record. Boot loads the whole file into a last-write-wins in-memory
//! index; application to chunks is lazy (the store overlays a column's
//! cells when that column generates), so replay never waits on chunk
//! loading. Fixed-size records give trivial torn-tail recovery: truncate
//! to the last whole record.

use digblock_net::messages::BlockEdit;
use digblock_net::protocol::PlayerId;
use rustc_hash::FxHashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

const MAGIC: &[u8; 4] = b"DBJ1";
/// v2: the block field widened to a u16 CELL (id + reserved state bits)
/// at record bytes 12..14; `by` moved to byte 14. v1 files are refused —
/// pre-deployment data only, delete ./data.
const VERSION: u16 = 2;
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
/// Compact on boot when the file holds this many times more records than
/// live cells (and enough of them to matter).
const COMPACT_RATIO: u64 = 2;
const COMPACT_MIN_RECORDS: u64 = 10_000;

pub struct Journal {
	file: BufWriter<File>,
	pub index: JournalIndex,
	dirty: bool,
	wrote_since_sync: bool,
	last_sync: Instant,
	/// Consecutive append/flush/fsync failures; reset on success. Past
	/// MAX_WRITE_FAILURES the sim stops accepting edits (rejecting with
	/// ServerError) rather than diverging memory from disk silently.
	write_failures: u32,
	last_health_log: Option<Instant>,
	#[cfg(test)]
	pub force_write_failure: bool,
}

impl Journal {
	/// Open (or create) the journal for `seed` under `dir`. A seed
	/// mismatch with an existing file is a hard error — the terrain the
	/// edits apply to would not exist.
	pub fn open(dir: &Path, seed: f64) -> std::io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join(format!("world-{:016x}.journal", seed.to_bits()));
		let mut file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(&path)?;

		let len = file.metadata()?.len();
		let mut index = JournalIndex::default();
		if len == 0 {
			let mut header = Vec::with_capacity(HEADER_LEN as usize);
			header.extend_from_slice(MAGIC);
			header.extend_from_slice(&VERSION.to_le_bytes());
			header.extend_from_slice(&0u16.to_le_bytes());
			header.extend_from_slice(&seed.to_le_bytes());
			file.write_all(&header)?;
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
			if records > COMPACT_RATIO * live && records > COMPACT_MIN_RECORDS {
				log::info!("journal compaction: {records} records -> {live} live cells");
				file = compact(&path, seed, &index)?;
				index.next_seq = live;
			}
		}
		file.seek(SeekFrom::End(0))?;
		Ok(Self {
			file: BufWriter::new(file),
			index,
			dirty: false,
			wrote_since_sync: false,
			last_sync: Instant::now(),
			write_failures: 0,
			last_health_log: None,
			#[cfg(test)]
			force_write_failure: false,
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
		let mut rec = [0u8; RECORD_LEN as usize];
		rec[0..4].copy_from_slice(&edit.x.to_le_bytes());
		rec[4..8].copy_from_slice(&edit.y.to_le_bytes());
		rec[8..12].copy_from_slice(&edit.z.to_le_bytes());
		rec[12..14].copy_from_slice(&edit.block.to_le_bytes());
		rec[14] = by;
		// rec[15] reserved, zero.
		match self.write_record(&rec) {
			Ok(()) => self.write_failures = 0,
			Err(e) => self.note_write_failure("write", &e),
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

	/// Flush buffered writes; fsync at most once a second.
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

/// Rewrite the journal as one record per live cell: tmp file, fsync,
/// atomic rename, reopen. Authorship is not tracked by the index, so
/// compacted records carry SERVER_ID.
fn compact(path: &Path, seed: f64, index: &JournalIndex) -> std::io::Result<File> {
	let tmp = path.with_extension("journal.tmp");
	{
		let mut out = BufWriter::new(File::create(&tmp)?);
		let mut header = Vec::with_capacity(HEADER_LEN as usize);
		header.extend_from_slice(MAGIC);
		header.extend_from_slice(&VERSION.to_le_bytes());
		header.extend_from_slice(&0u16.to_le_bytes());
		header.extend_from_slice(&seed.to_le_bytes());
		out.write_all(&header)?;
		for cells in index.columns.values() {
			for (&(x, y, z), &block) in cells {
				let mut rec = [0u8; RECORD_LEN as usize];
				rec[0..4].copy_from_slice(&x.to_le_bytes());
				rec[4..8].copy_from_slice(&y.to_le_bytes());
				rec[8..12].copy_from_slice(&z.to_le_bytes());
				rec[12..14].copy_from_slice(&block.to_le_bytes());
				rec[14] = digblock_net::protocol::SERVER_ID;
				out.write_all(&rec)?;
			}
		}
		out.flush()?;
		out.get_ref().sync_data()?;
	}
	std::fs::rename(&tmp, path)?;
	OpenOptions::new().read(true).write(true).open(path)
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

	/// v1 files (u8 block at byte 12) are refused by the version check —
	/// silently misreading them as v2 would corrupt every cell.
	#[test]
	fn v1_journal_is_refused() {
		let dir = std::env::temp_dir().join(format!("dbj-v1-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join(format!("world-{:016x}.journal", 3.0f64.to_bits()));
		let mut header = Vec::new();
		header.extend_from_slice(MAGIC);
		header.extend_from_slice(&1u16.to_le_bytes()); // VERSION 1
		header.extend_from_slice(&0u16.to_le_bytes());
		header.extend_from_slice(&3.0f64.to_le_bytes());
		std::fs::write(&path, header).unwrap();
		let err = match Journal::open(&dir, 3.0) {
			Err(e) => e,
			Ok(_) => panic!("v1 journal must be refused"),
		};
		assert!(err.to_string().contains("version"), "{err}");
		let _ = std::fs::remove_dir_all(&dir);
	}
}
