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
const VERSION: u16 = 1;
const HEADER_LEN: u64 = 16;
const RECORD_LEN: u64 = 16;
const SYNC_INTERVAL_SECS: f32 = 1.0;

/// Final block value per edited cell within one chunk column.
pub type ColumnEdits = FxHashMap<(i32, i32, i32), u8>;

/// Last-write-wins cell state, grouped by chunk column for join sync.
#[derive(Default)]
pub struct JournalIndex {
	columns: FxHashMap<(i32, i32), ColumnEdits>,
	pub next_seq: u64,
}

impl JournalIndex {
	pub fn record(&mut self, edit: BlockEdit) -> u64 {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		self.columns
			.entry(col)
			.or_default()
			.insert((edit.x, edit.y, edit.z), edit.block);
		let seq = self.next_seq;
		self.next_seq += 1;
		seq
	}

	pub fn column(&self, cx: i32, cz: i32) -> Option<&ColumnEdits> {
		self.columns.get(&(cx, cz))
	}

	/// Columns that have at least one edit.
	pub fn edited_columns(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
		self.columns.keys().copied()
	}
}

pub struct Journal {
	file: BufWriter<File>,
	pub index: JournalIndex,
	dirty: bool,
	wrote_since_sync: bool,
	last_sync: Instant,
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
					block: rec[12],
				};
				index.record(edit);
			}
		}
		file.seek(SeekFrom::End(0))?;
		Ok(Self {
			file: BufWriter::new(file),
			index,
			dirty: false,
			wrote_since_sync: false,
			last_sync: Instant::now(),
		})
	}

	/// Append one accepted edit; returns its global sequence number.
	pub fn append(&mut self, edit: BlockEdit, by: PlayerId) -> u64 {
		let mut rec = [0u8; RECORD_LEN as usize];
		rec[0..4].copy_from_slice(&edit.x.to_le_bytes());
		rec[4..8].copy_from_slice(&edit.y.to_le_bytes());
		rec[8..12].copy_from_slice(&edit.z.to_le_bytes());
		rec[12] = edit.block;
		rec[13] = by;
		// rec[14..16] reserved, zero.
		if let Err(e) = self.file.write_all(&rec) {
			log::error!("journal write failed: {e}");
		}
		self.dirty = true;
		self.wrote_since_sync = true;
		self.index.record(edit)
	}

	/// Flush buffered writes; fsync at most once a second.
	pub fn maintain(&mut self) {
		if self.dirty {
			if let Err(e) = self.file.flush() {
				log::error!("journal flush failed: {e}");
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
			log::error!("journal flush failed: {e}");
		}
		if let Err(e) = self.file.get_ref().sync_data() {
			log::error!("journal fsync failed: {e}");
		}
		self.wrote_since_sync = false;
		self.last_sync = Instant::now();
	}
}

fn bad_data(msg: impl Into<String>) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn edit(x: i32, y: i32, z: i32, block: u8) -> BlockEdit {
		BlockEdit { x, y, z, block }
	}

	#[test]
	fn append_reopen_last_write_wins() {
		let dir = std::env::temp_dir().join(format!("dbj-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		{
			let mut j = Journal::open(&dir, 42.0).unwrap();
			assert_eq!(j.append(edit(5, 130, -2, 0), 1), 0);
			assert_eq!(j.append(edit(20, 131, 5, 3), 1), 1);
			// Same cell edited twice: last write wins in the index.
			assert_eq!(j.append(edit(5, 130, -2, 4), 2), 2);
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
}
