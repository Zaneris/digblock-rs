//! Persistent spawn overrides (`spawn.json` in the data dir): the hand-picked
//! spawn point, the facing that comes with it, and the spawn lock.
//!
//! JSON for the same reason as the ban list: tiny, operator-facing, worth
//! reading or clearing by hand. Same durability policy — whole-file snapshot via
//! tmp + fsync + rename, an unparseable file is archived rather than crashing,
//! and mutations write through immediately.
//!
//! The point and the lock both store an eye POSITION, not a flag. Spawn is
//! otherwise a pure function of `(seed, edits)` recomputed on demand, but each
//! override must pin one specific spot: a lock's whole purpose is that its region
//! cannot change, and a hand-picked point is by definition not something the
//! search could rediscover.
//!
//! Point and lock are separate fields because they unset separately, and when
//! both are present they hold the SAME eye — `/set-spawn` under an active lock
//! moves the lock with it. The facing is orthogonal to both.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SpawnLock {
	/// Eye position the region was built around.
	pub eye: [f32; 3],
}

/// The persisted file body.
///
/// Every field defaults, so a build that predates one can still read the
/// file; `deny_unknown_fields` is what keeps that from swallowing a
/// legacy body (or an operator's typo) as an empty config.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SpawnConfig {
	/// Hand-picked spawn eye (`/set-spawn`), overriding the search.
	pub point: Option<[f32; 3]>,
	/// The lock's pinned eye, when spawn protection is on.
	pub lock: Option<SpawnLock>,
	/// Facing players adopt when they spawn (`/set-spawn-dir`), a
	/// horizontal unit vector. Independent of the point: a world can pin
	/// one, the other, or both.
	pub dir: Option<[f32; 3]>,
}

/// Read shim for files written before `/set-spawn` existed, when the body
/// was the lock alone. Legacy first: it requires an `eye` key that the
/// current shape never has, while [`SpawnConfig`]'s fields are both
/// optional and would happily match a legacy body and drop the lock.
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored {
	Legacy(SpawnLock),
	Current(SpawnConfig),
}

pub struct SpawnStore {
	path: PathBuf,
	cfg: SpawnConfig,
}

impl SpawnStore {
	pub fn open(dir: &Path) -> io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join("spawn.json");
		let cfg = match std::fs::read(&path) {
			Ok(bytes) => match serde_json::from_slice::<Option<Stored>>(&bytes) {
				Ok(None) => SpawnConfig::default(),
				Ok(Some(Stored::Legacy(lock))) => SpawnConfig {
					lock: Some(lock),
					..SpawnConfig::default()
				},
				Ok(Some(Stored::Current(cfg))) => cfg,
				Err(e) => {
					log::warn!("spawn.json unreadable ({e}) — archiving and starting unlocked");
					let _ = std::fs::rename(&path, bak_path(&path));
					SpawnConfig::default()
				}
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => SpawnConfig::default(),
			Err(e) => return Err(e),
		};
		Ok(Self { path, cfg })
	}

	pub fn lock(&self) -> Option<SpawnLock> {
		self.cfg.lock
	}

	/// The hand-picked spawn eye, if an admin has set one.
	pub fn point(&self) -> Option<[f32; 3]> {
		self.cfg.point
	}

	/// The spawn eye the overrides pin, if either does. A hand-picked
	/// point wins: when both are set they agree, and after `/lock-spawn
	/// off` only the point remains.
	pub fn pinned(&self) -> Option<[f32; 3]> {
		self.cfg.point.or(self.cfg.lock.map(|l| l.eye))
	}

	/// The facing to hand a spawning player, if an admin has set one.
	pub fn dir(&self) -> Option<[f32; 3]> {
		self.cfg.dir
	}

	pub fn set_dir(&mut self, dir: [f32; 3]) {
		self.write_if_changed(SpawnConfig {
			dir: Some(dir),
			..self.cfg
		});
	}

	pub fn set_lock(&mut self, lock: Option<SpawnLock>) {
		self.write_if_changed(SpawnConfig { lock, ..self.cfg });
	}

	/// Hand-pick the spawn. The lock follows the point unconditionally —
	/// protection is always on, so the old cells become editable again and
	/// the new ones are protected — which is what keeps the lock region
	/// and the spawn it guards from ever describing two different places.
	pub fn set_point(&mut self, eye: [f32; 3]) {
		self.write_if_changed(SpawnConfig {
			point: Some(eye),
			lock: Some(SpawnLock { eye }),
			..self.cfg
		});
	}

	fn write_if_changed(&mut self, cfg: SpawnConfig) {
		if self.cfg != cfg {
			self.cfg = cfg;
			self.write();
		}
	}

	/// Atomic write-through. Failure degrades (the in-memory state still
	/// applies this run) and logs — never refuses service.
	fn write(&self) {
		let result = (|| -> io::Result<()> {
			let bytes = serde_json::to_vec_pretty(&self.cfg)?;
			let tmp = self.path.with_extension("json.tmp");
			let mut f = std::fs::File::create(&tmp)?;
			io::Write::write_all(&mut f, &bytes)?;
			f.sync_all()?;
			std::fs::rename(&tmp, &self.path)
		})();
		if let Err(e) = result {
			log::error!("spawn.json write failed: {e} (still applied in memory)");
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
	fn lock_survives_reopen() {
		let d = tmpdir("persist");
		let want = SpawnLock {
			eye: [8.5, 130.5, -3.5],
		};
		let mut s = SpawnStore::open(&d).unwrap();
		s.set_lock(Some(want));
		drop(s);
		assert_eq!(SpawnStore::open(&d).unwrap().lock(), Some(want));
		let _ = std::fs::remove_dir_all(&d);
	}

	/// A hand-picked point outlives the lock: `/lock-spawn off` releases
	/// the cells, it does not un-set the spawn.
	#[test]
	fn point_survives_reopen_and_unlocking() {
		let d = tmpdir("point");
		let eye = [40.5, 200.5, -8.5];
		let mut s = SpawnStore::open(&d).unwrap();
		s.set_point(eye);
		s.set_lock(Some(SpawnLock { eye }));
		s.set_lock(None);
		drop(s);
		let s = SpawnStore::open(&d).unwrap();
		assert_eq!(s.point(), Some(eye));
		assert_eq!(s.lock(), None);
		assert_eq!(s.pinned(), Some(eye));
		let _ = std::fs::remove_dir_all(&d);
	}

	/// Setting the spawn carries the lock with it unconditionally —
	/// protection is always on, so the old cells are released, the new
	/// ones protected, and a store that somehow had no lock gains one.
	#[test]
	fn setting_the_point_always_carries_the_lock() {
		let d = tmpdir("movelock");
		let old = [0.5, 130.5, 0.5];
		let new = [64.5, 140.5, 64.5];
		let mut s = SpawnStore::open(&d).unwrap();
		s.set_lock(Some(SpawnLock { eye: old }));
		s.set_point(new);
		assert_eq!(s.lock(), Some(SpawnLock { eye: new }));
		// ...and even with no standing lock, a set re-establishes one.
		s.set_lock(None);
		s.set_point(old);
		assert_eq!(s.lock(), Some(SpawnLock { eye: old }));
		let _ = std::fs::remove_dir_all(&d);
	}

	/// Files written before `/set-spawn` existed held the lock alone.
	#[test]
	fn a_legacy_lock_only_file_still_reads() {
		let d = tmpdir("legacy");
		std::fs::create_dir_all(&d).unwrap();
		std::fs::write(d.join("spawn.json"), br#"{"eye":[1.5,130.5,2.5]}"#).unwrap();
		let s = SpawnStore::open(&d).unwrap();
		assert_eq!(
			s.lock(),
			Some(SpawnLock {
				eye: [1.5, 130.5, 2.5]
			}),
			"legacy lock dropped"
		);
		assert_eq!(s.point(), None);
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
