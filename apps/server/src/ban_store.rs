//! Persistent ban list (`bans.json` in the data dir).
//!
//! JSON on purpose — unlike the inventory store this file is tiny and
//! operator-facing (hand-editable, greppable). Same durability policy as
//! the binary stores: whole-file snapshot via tmp + fsync + rename, and
//! an unparseable file is archived and replaced rather than crashing the
//! server. Mutations write through immediately — a ban must survive a
//! crash, so there is no dirty/flush cadence.
//!
//! A ban records BOTH the account and the IP, and either one enforces.
//! They do different jobs. The account is the durable half: bans used to
//! be IP-only, so evasion was a new address while the evader's inventory
//! and position survived intact. The IP is still
//! worth keeping because it also catches an unauthenticated peer and
//! because kicking an address is what an operator reaches for when someone
//! is actively griefing — it just is no longer the only key.

use digblock_net::protocol::AccountId;
use serde::{Deserialize, Serialize};
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BanEntry {
	/// Display name at ban time — the operator-facing label for
	/// `/ban-list` and `/unban`. Not enforced against: a name can be
	/// changed, and the account behind it cannot.
	pub name: String,
	pub ip: IpAddr,
	/// The banned account, as a hyphenated UUID. Optional so an existing
	/// hand-written `bans.json` still loads, and absent for a ban placed
	/// before this existed.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub account: Option<String>,
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

	/// Is this account banned? `AccountId::NONE` never matches — an
	/// unverified connection has no account, and treating "no account" as a
	/// value that could equal a stored one would ban everybody the moment a
	/// malformed entry crept in.
	pub fn is_account_banned(&self, account: AccountId) -> bool {
		if account.is_none() {
			return false;
		}
		self.entries.iter().any(|e| {
			e.account
				.as_deref()
				.and_then(AccountId::parse)
				.is_some_and(|a| a == account)
		})
	}

	/// Add (or re-label) a ban and write through. Matching on the account
	/// when there is one, so re-banning someone who has changed address
	/// updates their entry rather than accumulating a row per IP.
	///
	/// The account-less fallback is what ADOPTS a pre-account row rather than
	/// shadowing it. Without it, banning an account never matched a legacy
	/// IP-only entry for the same player, so a second row was pushed and the
	/// first became unreachable: `/unban <account-id>` matches neither its
	/// name nor its (unparseable-as-IP) argument, so the orphan kept
	/// enforcing by address after an unban that reported success.
	pub fn ban(&mut self, name: &str, ip: IpAddr, account: AccountId) {
		let key = (!account.is_none()).then(|| account.to_string());
		let existing = self.entries.iter_mut().find(|e| match &key {
			Some(_) => e.account == key || (e.account.is_none() && e.ip == ip),
			None => e.ip == ip,
		});
		if let Some(e) = existing {
			e.name = name.into();
			e.ip = ip;
			if key.is_some() {
				e.account = key;
			}
		} else {
			self.entries.push(BanEntry {
				name: name.into(),
				ip,
				account: key,
			});
		}
		self.write();
	}

	/// Remove every entry matching `arg` — by name (case-insensitive), by
	/// IP, or by account id, whichever it parses as. Returns the removed
	/// count.
	pub fn unban_matching(&mut self, arg: &str) -> usize {
		let ip: Option<IpAddr> = arg.parse().ok();
		let account = AccountId::parse(arg).filter(|a| !a.is_none());
		let before = self.entries.len();
		self.entries.retain(|e| {
			let by_account =
				account.is_some() && e.account.as_deref().and_then(AccountId::parse) == account;
			!(e.name.eq_ignore_ascii_case(arg) || Some(e.ip) == ip || by_account)
		});
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

	fn account(n: u8) -> AccountId {
		AccountId([n; 16])
	}

	#[test]
	fn round_trips_across_reopen() {
		let dir = temp_dir("roundtrip");
		let ip: IpAddr = "10.1.2.3".parse().unwrap();
		{
			let mut s = BanStore::open(&dir).unwrap();
			s.ban("griefer", ip, account(1));
			assert!(s.is_banned(ip));
			assert!(s.is_account_banned(account(1)));
		}
		let mut s = BanStore::open(&dir).unwrap();
		assert!(s.is_banned(ip));
		assert!(s.is_account_banned(account(1)), "the account survives too");
		assert_eq!(s.entries().len(), 1);
		assert_eq!(s.unban_matching("GRIEFER"), 1);
		assert!(!s.is_banned(ip));
		assert!(!s.is_account_banned(account(1)));
		let s = BanStore::open(&dir).unwrap();
		assert!(!s.is_banned(ip));
	}

	/// The durable half of a ban: a new address does not get you back in.
	/// This is what IP-only bans could not do.
	#[test]
	fn an_account_ban_follows_the_player_to_a_new_address() {
		let dir = temp_dir("acctban");
		let mut s = BanStore::open(&dir).unwrap();
		s.ban("griefer", "10.1.2.3".parse().unwrap(), account(2));
		assert!(!s.is_banned("10.4.5.6".parse().unwrap()), "different IP");
		assert!(s.is_account_banned(account(2)), "same account, still out");
		assert!(!s.is_account_banned(account(3)), "somebody else is fine");
		// An unverified connection carries no account and must not match.
		assert!(!s.is_account_banned(AccountId::NONE));
		assert_eq!(s.unban_matching(&account(2).to_string()), 1, "unban by id");
	}

	#[test]
	fn unban_by_ip_and_reban_relabels() {
		let dir = temp_dir("byip");
		let ip: IpAddr = "10.9.9.9".parse().unwrap();
		let mut s = BanStore::open(&dir).unwrap();
		s.ban("alice", ip, account(4));
		s.ban("bob", ip, account(4)); // same account re-banned under a new name
		assert_eq!(s.entries().len(), 1);
		assert_eq!(s.entries()[0].name, "bob");
		assert_eq!(s.unban_matching("10.9.9.9"), 1);
		assert!(!s.is_banned(ip));
	}

	/// A `bans.json` written before bans carried accounts has rows with no
	/// account at all. Re-banning that player must ADOPT the legacy row, not
	/// shadow it: two rows for one player leaves the account-less one
	/// unreachable from `/unban`, since it answers to neither the account id
	/// nor (any longer) the name the operator types.
	#[test]
	fn re_banning_upgrades_a_pre_account_entry_instead_of_duplicating_it() {
		let dir = temp_dir("legacy");
		let ip: IpAddr = "10.7.7.7".parse().unwrap();
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(
			dir.join("bans.json"),
			br#"[{"name":"alice","ip":"10.7.7.7"}]"#,
		)
		.unwrap();

		let mut s = BanStore::open(&dir).unwrap();
		assert_eq!(s.entries().len(), 1);
		assert!(s.entries()[0].account.is_none(), "legacy row, IP only");

		s.ban("alice", ip, account(5));
		assert_eq!(s.entries().len(), 1, "adopted, not duplicated");
		assert!(s.is_account_banned(account(5)));

		// The whole point: one unban by id clears the address too.
		assert_eq!(s.unban_matching(&account(5).to_string()), 1);
		assert!(!s.is_banned(ip), "no orphan left enforcing by address");
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
