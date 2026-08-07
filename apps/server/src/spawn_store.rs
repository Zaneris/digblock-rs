//! Persistent spawn lock (`spawn.json` in the data dir).
//!
//! JSON for the same reason as the ban list: tiny, operator-facing, and
//! worth being able to read or clear by hand. Same durability policy —
//! whole-file snapshot via tmp + fsync + rename, an unparseable file is
//! archived rather than crashing the server, and mutations write through
//! immediately (a lock is an operator decision that must survive a crash;
//! there is no dirty/flush cadence).
//!
//! The locked eye position is stored, not just the flag. Spawn is
//! otherwise a pure function of `(seed, edits)` and recomputed on demand,
//! but a *lock* must pin one specific region for enforcement: the whole
//! point is that the region cannot change, so re-deriving it every boot
//! would defeat it if worldgen or the search rule ever shifted.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SpawnLock {
	/// Eye position the region was built around.
	pub eye: [f32; 3],
}

pub struct SpawnStore {
	path: PathBuf,
	lock: Option<SpawnLock>,
}

impl SpawnStore {
	pub fn open(dir: &Path) -> io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join("spawn.json");
		let lock = match std::fs::read(&path) {
			Ok(bytes) => match serde_json::from_slice::<Option<SpawnLock>>(&bytes) {
				Ok(lock) => lock,
				Err(e) => {
					log::warn!("spawn.json unreadable ({e}) — archiving and starting unlocked");
					let _ = std::fs::rename(&path, bak_path(&path));
					None
				}
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => None,
			Err(e) => return Err(e),
		};
		Ok(Self { path, lock })
	}

	pub fn lock(&self) -> Option<SpawnLock> {
		self.lock
	}

	pub fn set(&mut self, lock: Option<SpawnLock>) {
		if self.lock != lock {
			self.lock = lock;
			self.write();
		}
	}

	/// Atomic write-through. Failure degrades (the in-memory lock still
	/// enforces this run) and logs — never refuses service.
	fn write(&self) {
		let result = (|| -> io::Result<()> {
			let bytes = serde_json::to_vec_pretty(&self.lock)?;
			let tmp = self.path.with_extension("json.tmp");
			let mut f = std::fs::File::create(&tmp)?;
			io::Write::write_all(&mut f, &bytes)?;
			f.sync_all()?;
			std::fs::rename(&tmp, &self.path)
		})();
		if let Err(e) = result {
			log::error!("spawn.json write failed: {e} (lock still enforced in memory)");
		}
	}
}

/// First free `spawn.json.bak[N]` slot — never overwrite an earlier
/// archive (mirrors the ban and inventory stores).
fn bak_path(path: &Path) -> PathBuf {
	for n in 0..100u32 {
		let p = if n == 0 {
			path.with_extension("json.bak")
		} else {
			path.with_extension(format!("json.bak{n}"))
		};
		if !p.exists() {
			return p;
		}
	}
	path.with_extension("json.bak")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tmpdir(tag: &str) -> PathBuf {
		let d =
			std::env::temp_dir().join(format!("digblock-spawnstore-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&d);
		d
	}

	#[test]
	fn absent_file_starts_unlocked() {
		let d = tmpdir("absent");
		let s = SpawnStore::open(&d).unwrap();
		assert_eq!(s.lock(), None);
		let _ = std::fs::remove_dir_all(&d);
	}

	#[test]
	fn lock_survives_reopen() {
		let d = tmpdir("persist");
		let want = SpawnLock {
			eye: [8.5, 130.5, -3.5],
		};
		let mut s = SpawnStore::open(&d).unwrap();
		s.set(Some(want));
		drop(s);
		assert_eq!(SpawnStore::open(&d).unwrap().lock(), Some(want));
		let _ = std::fs::remove_dir_all(&d);
	}

	#[test]
	fn unlocking_survives_reopen() {
		let d = tmpdir("unlock");
		let mut s = SpawnStore::open(&d).unwrap();
		s.set(Some(SpawnLock {
			eye: [0.5, 130.5, 0.5],
		}));
		s.set(None);
		drop(s);
		assert_eq!(SpawnStore::open(&d).unwrap().lock(), None);
		let _ = std::fs::remove_dir_all(&d);
	}

	#[test]
	fn unparseable_file_is_archived_not_fatal() {
		let d = tmpdir("corrupt");
		std::fs::create_dir_all(&d).unwrap();
		std::fs::write(d.join("spawn.json"), b"{not json").unwrap();
		assert_eq!(SpawnStore::open(&d).unwrap().lock(), None);
		assert!(
			d.join("spawn.json.bak").exists(),
			"corrupt file not archived"
		);
		let _ = std::fs::remove_dir_all(&d);
	}
}
