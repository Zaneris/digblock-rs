//! Persistent per-player inventories, keyed by a hash of the client's
//! identity secret. NOT part of the block journal — the journal is
//! cell-keyed and its compaction discards authorship, so player-keyed
//! state lives in this separate, much smaller file.
//!
//! Inventories are tiny (≤ one count per block type per player), so the
//! format is a whole-file snapshot rewritten atomically (tmp + fsync +
//! rename) at most once per [`FLUSH_INTERVAL`], plus a synchronous
//! [`InventoryStore::sync`] on disconnect and shutdown. A hard crash
//! loses at most the last interval of count changes — deliberate: blocks
//! themselves are journaled, counts are low-stakes.
//!
//! Unlike the journal, a store that cannot write does NOT refuse edits:
//! the world stays consistent either way, only inventory persistence
//! degrades. It logs loudly and keeps serving from memory.

use digblock_core::inventory::Inventory;
use rustc_hash::FxHashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = *b"DBI1";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 4 + 2 + 2 + 4; // magic | version | pad | count

/// Dirty-state flush cadence.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Identity cap: beyond this, the oldest-inserted identity is evicted on
/// insert. Bounds the file; an evicted (absent) player's inventory is
/// lost — acceptable pre-deployment, revisit with real populations.
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

pub struct InventoryStore {
	path: PathBuf,
	/// Insertion-ordered for the eviction policy.
	map: FxHashMap<IdentityHash, Inventory>,
	order: Vec<IdentityHash>,
	dirty: bool,
	last_flush: Instant,
	write_failed_logged: bool,
}

impl InventoryStore {
	/// Missing file → empty store. An unreadable or wrong-version file is
	/// archived to `.vN.bak` and the store starts fresh (mirrors the
	/// journal's policy — never crash the server over persisted state).
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
				for (hash, inv) in entries {
					store.order.push(hash);
					store.map.insert(hash, inv);
				}
			}
			Err(e) => {
				let bak = bak_path(&path);
				log::warn!(
					"inventory store unreadable ({e}) — archiving to {} and starting fresh",
					bak.display()
				);
				let _ = std::fs::rename(&path, &bak);
			}
		}
		Ok(store)
	}

	/// A player's persisted inventory; default (empty) when unknown.
	pub fn get(&self, id: &IdentityHash) -> Inventory {
		self.map.get(id).cloned().unwrap_or_default()
	}

	/// Write-through to memory; durable at the next flush.
	pub fn put(&mut self, id: IdentityHash, inv: &Inventory) {
		if self.map.insert(id, inv.clone()).is_none() {
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
				// keep serving inventories from memory and retry next
				// interval. Log once per failure streak.
				if !self.write_failed_logged {
					log::error!("inventory store write failed (serving from memory): {e}");
					self.write_failed_logged = true;
				}
			}
		}
	}

	fn write_snapshot(&self) -> io::Result<()> {
		let mut buf = Vec::with_capacity(HEADER_LEN + self.map.len() * 32);
		buf.extend_from_slice(&MAGIC);
		buf.extend_from_slice(&VERSION.to_le_bytes());
		buf.extend_from_slice(&[0, 0]);
		buf.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
		for hash in &self.order {
			let Some(inv) = self.map.get(hash) else {
				continue;
			};
			let pairs = inv.to_wire();
			buf.extend_from_slice(hash);
			buf.push(pairs.len().min(u8::MAX as usize) as u8);
			for (block, count) in pairs.iter().take(u8::MAX as usize) {
				buf.extend_from_slice(&block.to_le_bytes());
				buf.extend_from_slice(&count.to_le_bytes());
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

fn parse(bytes: &[u8]) -> io::Result<Vec<(IdentityHash, Inventory)>> {
	let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
	let mut r = bytes;
	let mut header = [0u8; HEADER_LEN];
	r.read_exact(&mut header).map_err(|_| bad("short header"))?;
	if header[..4] != MAGIC {
		return Err(bad("bad magic"));
	}
	if u16::from_le_bytes([header[4], header[5]]) != VERSION {
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
		out.push((hash, Inventory::from_wire(&pairs)));
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

	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("dbinv-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	#[test]
	fn round_trips_across_reopen() {
		let dir = temp_dir("roundtrip");
		let id_a = hash_identity(&[1; 16]);
		let id_b = hash_identity(&[2; 16]);
		{
			let mut s = InventoryStore::open(&dir).unwrap();
			let mut inv = Inventory::default();
			inv.add(BlockType::Dirt, 7);
			inv.add(BlockType::Water, 2);
			s.put(id_a, &inv);
			s.put(id_b, &Inventory::default());
			s.sync();
		}
		let s = InventoryStore::open(&dir).unwrap();
		assert_eq!(s.get(&id_a).count(BlockType::Dirt), 7);
		assert_eq!(s.get(&id_a).count(BlockType::Water), 2);
		assert_eq!(s.get(&id_b), Inventory::default());
		assert_eq!(s.get(&hash_identity(&[3; 16])), Inventory::default());
	}

	#[test]
	fn corrupt_file_is_archived_and_store_starts_fresh() {
		let dir = temp_dir("corrupt");
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("inventories.bin");
		std::fs::write(&path, b"garbage not a store").unwrap();
		let s = InventoryStore::open(&dir).unwrap();
		assert_eq!(s.get(&[0; 16]), Inventory::default());
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
}
