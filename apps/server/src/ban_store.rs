//! Persistent IP ban list (`bans.json` in the data dir).
//!
//! JSON on purpose — unlike the inventory store this file is tiny and
//! operator-facing (hand-editable, greppable). Same durability policy as
//! the binary stores: whole-file snapshot via tmp + fsync + rename, and
//! an unparseable file is archived and replaced rather than crashing the
//! server. Mutations write through immediately — a ban must survive a
//! crash, so there is no dirty/flush cadence.

use serde::{Deserialize, Serialize};
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BanEntry {
	/// Display name at ban time — the operator-facing label for
	/// `/ban-list` and `/unban`; the IP is what is enforced.
	pub name: String,
	pub ip: IpAddr,
}

pub struct BanStore {
	path: PathBuf,
	entries: Vec<BanEntry>,
}

impl BanStore {
	pub fn open(dir: &Path) -> io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let path = dir.join("bans.json");
		let entries = match std::fs::read(&path) {
			Ok(bytes) => match serde_json::from_slice::<Vec<BanEntry>>(&bytes) {
				Ok(entries) => entries,
				Err(e) => {
					log::warn!("bans.json unreadable ({e}) — archiving and starting fresh");
					let _ = std::fs::rename(&path, bak_path(&path));
					Vec::new()
				}
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
			Err(e) => return Err(e),
		};
		Ok(Self { path, entries })
	}

	pub fn is_banned(&self, ip: IpAddr) -> bool {
		self.entries.iter().any(|e| e.ip == ip)
	}

	/// Add (or re-label) a ban and write through.
	pub fn ban(&mut self, name: &str, ip: IpAddr) {
		if let Some(e) = self.entries.iter_mut().find(|e| e.ip == ip) {
			e.name = name.into();
		} else {
			self.entries.push(BanEntry {
				name: name.into(),
				ip,
			});
		}
		self.write();
	}

	/// Remove every entry whose name matches case-insensitively, or whose
	/// IP equals `arg` when it parses as one. Returns the removed count.
	pub fn unban_matching(&mut self, arg: &str) -> usize {
		let ip: Option<IpAddr> = arg.parse().ok();
		let before = self.entries.len();
		self.entries
			.retain(|e| !(e.name.eq_ignore_ascii_case(arg) || Some(e.ip) == ip));
		let removed = before - self.entries.len();
		if removed > 0 {
			self.write();
		}
		removed
	}

	pub fn entries(&self) -> &[BanEntry] {
		&self.entries
	}

	/// Atomic write-through. Failure degrades (the in-memory list still
	/// enforces this run) and logs — never refuses service.
	fn write(&self) {
		let result = (|| -> io::Result<()> {
			let bytes = serde_json::to_vec_pretty(&self.entries)?;
			let tmp = self.path.with_extension("json.tmp");
			let mut f = std::fs::File::create(&tmp)?;
			io::Write::write_all(&mut f, &bytes)?;
			f.sync_all()?;
			std::fs::rename(&tmp, &self.path)
		})();
		if let Err(e) = result {
			log::error!("bans.json write failed: {e} (bans still enforced in memory)");
		}
	}
}

/// First free `bans.json.bak[N]` slot — never overwrite an earlier
/// archive (mirrors the inventory store's policy).
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

	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("dbban-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	#[test]
	fn round_trips_across_reopen() {
		let dir = temp_dir("roundtrip");
		let ip: IpAddr = "10.1.2.3".parse().unwrap();
		{
			let mut s = BanStore::open(&dir).unwrap();
			s.ban("griefer", ip);
			assert!(s.is_banned(ip));
		}
		let mut s = BanStore::open(&dir).unwrap();
		assert!(s.is_banned(ip));
		assert_eq!(s.entries().len(), 1);
		assert_eq!(s.unban_matching("GRIEFER"), 1);
		assert!(!s.is_banned(ip));
		let s = BanStore::open(&dir).unwrap();
		assert!(!s.is_banned(ip));
	}

	#[test]
	fn unban_by_ip_and_reban_relabels() {
		let dir = temp_dir("byip");
		let ip: IpAddr = "10.9.9.9".parse().unwrap();
		let mut s = BanStore::open(&dir).unwrap();
		s.ban("alice", ip);
		s.ban("bob", ip); // same IP re-banned under a new name
		assert_eq!(s.entries().len(), 1);
		assert_eq!(s.entries()[0].name, "bob");
		assert_eq!(s.unban_matching("10.9.9.9"), 1);
		assert!(!s.is_banned(ip));
	}

	#[test]
	fn corrupt_file_is_archived_and_store_starts_fresh() {
		let dir = temp_dir("corrupt");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("bans.json"), b"not json{{").unwrap();
		let s = BanStore::open(&dir).unwrap();
		assert!(s.entries().is_empty());
		assert!(dir.join("bans.json.bak").exists());
	}
}
