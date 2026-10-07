//! The master-era saves a master-free server can still hand back. While the master ran, a save was
//! filed under the player's master account; with the master gone nothing vouches for that account
//! except the session token the player's client still holds. `master-claims.txt` in the data
//! directory is an export of the master's live sessions (`tools/fleet/master_claims.py`): one
//! line per session, the first 16 bytes of the token hash the master stored in hex, then the
//! account. A client presenting `protocol::master_session_claim` of its token proves the account
//! with no master to ask. Each line is a key to a save, so the file is kept owner-only.

use digblock_net::protocol::AccountId;
use rustc_hash::FxHashMap;
use std::io;
use std::path::Path;

pub const FILE: &str = "master-claims.txt";

/// The export in `dir`, claim to account. No file is no claims; a malformed line is an error,
/// since quietly skipping it would strand that player's save.
pub fn load(dir: &Path) -> io::Result<FxHashMap<[u8; 16], AccountId>> {
	let text = match std::fs::read_to_string(dir.join(FILE)) {
		Ok(t) => t,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(FxHashMap::default()),
		Err(e) => return Err(e),
	};
	parse(&text)
}

fn parse(text: &str) -> io::Result<FxHashMap<[u8; 16], AccountId>> {
	let mut claims = FxHashMap::default();
	for (n, line) in text.lines().enumerate() {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("{FILE} line {}", n + 1));
		let (claim, account) = line.split_once(char::is_whitespace).ok_or_else(bad)?;
		// The claim's 32 hex digits read as an account id would: same width, same alphabet.
		let claim = AccountId::parse(claim)
			.filter(|_| claim.len() == 32)
			.ok_or_else(bad)?;
		let account = AccountId::parse(account.trim()).ok_or_else(bad)?;
		claims.insert(claim.0, account);
	}
	Ok(claims)
}

#[cfg(test)]
mod tests;
