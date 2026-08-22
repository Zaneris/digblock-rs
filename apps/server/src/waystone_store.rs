//! Persistent waystone sites (`waystones.json` in the data dir): the
//! RESOLVED eye per biome kind, in `SurfaceKind` declaration-order index.
//!
//! Stored rather than re-derived, for the lock's reason (`spawn.json`'s
//! doc): a site's whole purpose is that its region cannot change, so the
//! answer the first boot resolved against the journal is the answer
//! forever. Same durability policy as its siblings — tmp + fsync +
//! rename, an unparseable file archives rather than crashing, mutations
//! write through.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// The persisted body: `(kind index, resolved eye)` pairs. Every field
/// defaults so an older build reads a newer file's absence as empty.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct WaystoneConfig {
	sites: Vec<(u8, [f32; 3])>,
}

pub struct WaystoneStore {
	path: PathBuf,
	cfg: WaystoneConfig,
}

impl WaystoneStore {
	pub fn open(dir: &Path) -> io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join("waystones.json");
		let cfg = match std::fs::read(&path) {
			Ok(bytes) => match serde_json::from_slice::<Option<WaystoneConfig>>(&bytes) {
				Ok(cfg) => cfg.unwrap_or_default(),
				Err(e) => {
					log::warn!("waystones.json unreadable ({e}) — archiving and starting empty");
					let _ = std::fs::rename(&path, path.with_extension("json.bak"));
					WaystoneConfig::default()
				}
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => WaystoneConfig::default(),
			Err(e) => return Err(e),
		};
		Ok(Self { path, cfg })
	}

	/// Every resolved site, as stored.
	pub fn sites(&self) -> &[(u8, [f32; 3])] {
		&self.cfg.sites
	}

	pub fn get(&self, kind: u8) -> Option<[f32; 3]> {
		self.cfg
			.sites
			.iter()
			.find(|&&(k, _)| k == kind)
			.map(|&(_, eye)| eye)
	}

	/// Record a kind's resolved eye. Insert-only in practice — a stored
	/// site is never re-derived — but a re-set of the same kind replaces,
	/// which is what a future operator command would want.
	pub fn set(&mut self, kind: u8, eye: [f32; 3]) {
		match self.cfg.sites.iter_mut().find(|(k, _)| *k == kind) {
			Some(entry) => entry.1 = eye,
			None => self.cfg.sites.push((kind, eye)),
		}
		self.write();
	}

	/// Atomic write-through; failure degrades to in-memory and logs.
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
			log::error!("waystones.json write failed: {e} (still applied in memory)");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tmpdir(tag: &str) -> PathBuf {
		let d = std::env::temp_dir().join(format!(
			"digblock-waystonestore-{tag}-{}",
			std::process::id()
		));
		let _ = std::fs::remove_dir_all(&d);
		d
	}

	#[test]
	fn sites_persist_across_reopen() {
		let d = tmpdir("persist");
		let mut s = WaystoneStore::open(&d).unwrap();
		assert!(s.sites().is_empty());
		s.set(2, [10.5, 130.5, -6.5]);
		s.set(6, [900.5, 140.5, 44.5]);
		drop(s);
		let s = WaystoneStore::open(&d).unwrap();
		assert_eq!(s.get(2), Some([10.5, 130.5, -6.5]));
		assert_eq!(s.get(6), Some([900.5, 140.5, 44.5]));
		assert_eq!(s.sites().len(), 2);
		let _ = std::fs::remove_dir_all(&d);
	}

	#[test]
	fn unparseable_file_is_archived_not_fatal() {
		let d = tmpdir("corrupt");
		std::fs::create_dir_all(&d).unwrap();
		std::fs::write(d.join("waystones.json"), b"{not json").unwrap();
		assert!(WaystoneStore::open(&d).unwrap().sites().is_empty());
		assert!(d.join("waystones.json.bak").exists(), "original archived");
		let _ = std::fs::remove_dir_all(&d);
	}
}
