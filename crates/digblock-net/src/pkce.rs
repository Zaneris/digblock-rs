//! The RFC 7636 challenge and the two encodings around it. Both
//! launchers must compute a challenge the master will accept, and
//! identically to each other — the same reason `legacy_identity_hash`
//! lives here rather than once per launcher.

/// base64url, unpadded — the encoding RFC 7636 specifies for a PKCE
/// challenge. Hand-rolled because it is twenty lines and this is the only
/// place the client needs base64 at all.
pub fn b64url(bytes: &[u8]) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
	let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
	for chunk in bytes.chunks(3) {
		let b = [
			chunk[0],
			*chunk.get(1).unwrap_or(&0),
			*chunk.get(2).unwrap_or(&0),
		];
		let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
		// 3 bytes → 4 chars; a short final chunk emits only the characters
		// its bits actually reach, which is what "unpadded" means.
		for i in 0..chunk.len() + 1 {
			out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3F) as usize] as char);
		}
	}
	out
}

/// The `S256` code challenge for a verifier.
pub fn s256(verifier: &str) -> String {
	use sha2::{Digest, Sha256};
	b64url(&Sha256::digest(verifier.as_bytes()))
}

/// Percent-encode for a query value. RFC 3986 unreserved set only —
/// encoding a little more than necessary is always safe here.
pub fn urlencode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
				out.push(b as char);
			}
			_ => out.push_str(&format!("%{b:02X}")),
		}
	}
	out
}

#[cfg(test)]
mod tests;
