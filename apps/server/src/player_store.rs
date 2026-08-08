//! Persistent per-player state (inventory counts + item layout), keyed
//! by a hash of the client's identity secret. NOT part of the block
//! journal — the journal is cell-keyed and its compaction discards
//! authorship, so player-keyed state lives in this separate, much
//! smaller file.
//!
//! Records are tiny, so the format is a whole-file snapshot rewritten
//! atomically (tmp + fsync + rename) at most once per [`FLUSH_INTERVAL`],
//! plus a synchronous [`PlayerStore::sync`] on disconnect and shutdown. A
//! hard crash loses at most the last interval of changes — deliberate:
//! blocks themselves are journaled, counts are low-stakes.
//!
//! Unlike the journal, a store that cannot write does NOT refuse edits:
//! the world stays consistent either way, only persistence degrades. It
//! logs loudly and keeps serving from memory.
//!
//! Format v2 appends an optional item layout (hotbar + grid wire
//! arrangements) per record; v3 appends the player's last position and
//! return-to-spawn cooldown. v1 and v2 files are still read — the
//! archive-and-start-fresh path is reserved for actual corruption, never
//! a routine upgrade (it would wipe every player's inventory). An older
//! file simply has no stored position, so those players resume at spawn
//! once and are stored in v3 from their next disconnect on.

use digblock_core::hotbar::HOTBAR_SLOTS;
use digblock_core::inv_grid::GRID_SLOTS;
use digblock_core::inventory::Inventory;
use rustc_hash::FxHashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = *b"DBI1";
const VERSION: u16 = 3;
const HEADER_LEN: usize = 4 + 2 + 2 + 4; // magic | version | pad | count

/// Dirty-state flush cadence.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Identity cap: beyond this, the oldest-inserted identity is evicted on
/// insert. Bounds the file; an evicted (absent) player's state is lost —
/// acceptable pre-deployment, revisit with real populations.
const MAX_IDENTITIES: usize = 10_000;

/// The stored key: SHA-256 of the client's 16-byte identity secret,
/// truncated. The raw secret is a bearer credential and never touches
/// disk server-side.
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
}

pub struct PlayerStore {
	path: PathBuf,
	/// Insertion-ordered for the eviction policy.
	map: FxHashMap<IdentityHash, PlayerRecord>,
	order: Vec<IdentityHash>,
	dirty: bool,
	last_flush: Instant,
	write_failed_logged: bool,
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
			path: path.clone(),
			map: FxHashMap::default(),
			order: Vec::new(),
			dirty: false,
			last_flush: Instant::now(),
			write_failed_logged: false,
		};
		let bytes = match std::fs::read(&path) {
			Ok(b) => b,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(store),
			Err(e) => return Err(e),
		};
		match parse(&bytes) {
			Ok(entries) => {
				for (hash, rec) in entries {
					store.order.push(hash);
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

	/// Write-through to memory; durable at the next flush.
	pub fn put(&mut self, id: IdentityHash, rec: PlayerRecord) {
		if self.map.insert(id, rec).is_none() {
			self.order.push(id);
			if self.order.len() > MAX_IDENTITIES {
				let evicted = self.order.remove(0);
				self.map.remove(&evicted);
			}
		}
		self.dirty = true;
	}

	/// Called once per sim tick: flush if dirty and the interval passed.
	pub fn maintain(&mut self) {
		if self.dirty && self.last_flush.elapsed() >= FLUSH_INTERVAL {
			self.flush();
		}
	}

	/// Synchronous flush (disconnect of an identified player, shutdown).
	pub fn sync(&mut self) {
		if self.dirty {
			self.flush();
		}
	}

	fn flush(&mut self) {
		self.last_flush = Instant::now();
		match self.write_snapshot() {
			Ok(()) => {
				self.dirty = false;
				self.write_failed_logged = false;
			}
			Err(e) => {
				// Degrade, don't refuse: the world is journaled elsewhere;
				// keep serving from memory and retry next interval. Log
				// once per failure streak.
				if !self.write_failed_logged {
					log::error!("player store write failed (serving from memory): {e}");
					self.write_failed_logged = true;
				}
			}
		}
	}

	fn write_snapshot(&self) -> io::Result<()> {
		let mut buf = Vec::with_capacity(HEADER_LEN + self.map.len() * 128);
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&VERSION.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
		for hash in &self.order {
			let Some(rec) = self.map.get(hash) else {
				continue;
			};
			let pairs = rec.inventory.to_wire();
			buf.extend_from_slice(hash);
			buf.push(pairs.len().min(u8::MAX as usize) as u8);
			for (block, count) in pairs.iter().take(u8::MAX as usize) {
				buf.extend_from_slice(&block.to_le_bytes());
				buf.extend_from_slice(&count.to_le_bytes());
			}
			// v2: the layout blob, presence-flagged.
			match &rec.layout {
				None => buf.push(0),
				Some(layout) => {
					buf.push(1);
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
		}
		let tmp = self.path.with_extension("bin.tmp");
		{
			let mut f = std::fs::File::create(&tmp)?;
			f.write_all(&buf)?;
			f.sync_all()?;
		}
		std::fs::rename(&tmp, &self.path)
	}
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
			let mut flag = [0u8; 1];
			r.read_exact(&mut flag).map_err(|_| bad("short record"))?;
			if flag[0] == 1 {
				let mut words = [0u16; HOTBAR_SLOTS + GRID_SLOTS];
				for w in words.iter_mut() {
					let mut b = [0u8; 2];
					r.read_exact(&mut b).map_err(|_| bad("short record"))?;
					*w = u16::from_le_bytes(b);
				}
				Some(PlayerLayout {
					hotbar: words[..HOTBAR_SLOTS].try_into().unwrap(),
					grid: words[HOTBAR_SLOTS..].try_into().unwrap(),
				})
			} else {
				None
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
		out.push((
			hash,
			PlayerRecord {
				inventory: Inventory::from_wire(&pairs),
				layout,
				pos,
				last_return_tick,
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
	use digblock_core::hotbar::{WIRE_EMPTY, WIRE_TOOL};

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
		// Hand-built v1 snapshot: one record, one (Dirt, 7) pair.
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

	/// An existing v2 file (inventory + layout, no position) keeps every
	/// player's items and arrangement and simply reports no stored
	/// position — those players resume at spawn once, then store one.
	/// This must NOT trip the archive-and-start-fresh path.
	#[test]
	fn v2_file_reads_with_no_position() {
		let dir = temp_dir("v2compat");
		std::fs::create_dir_all(&dir).unwrap();
		let id = hash_identity(&[6; 16]);
		// Hand-built v2 snapshot: one record, one (Dirt, 3) pair, layout.
		let mut buf = Vec::new();
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&2u16.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&1u32.to_le_bytes());
		buf.extend_from_slice(&id);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&3u32.to_le_bytes());
		buf.push(1);
		let l = layout();
		for w in l.hotbar.iter().chain(l.grid.iter()) {
			buf.extend_from_slice(&w.to_le_bytes());
		}
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
				},
			);
			s.sync();
		}
		let s = PlayerStore::open(&dir).unwrap();
		assert_eq!(s.get(&id).pos, None);
		assert_eq!(s.get(&id).last_return_tick, None);
	}
}
