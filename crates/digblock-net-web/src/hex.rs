/// Parse the hex SHA-256 hash the master serves as `certHash`
/// (separators like `:` or spaces are tolerated). Mirrors
/// digblock-net-native's `CertPolicy::pinned_from_hex` — change both
/// together.
pub fn parse_sha256_hex(hex: &str) -> Result<[u8; 32], String> {
	let clean: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
	if clean.len() != 64 {
		return Err(format!("want 64 hex chars, got {}", clean.len()));
	}
	let mut out = [0u8; 32];
	for (i, byte) in out.iter_mut().enumerate() {
		*byte = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).expect("hex checked");
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trips_and_tolerates_separators() {
		let hex = "0a".repeat(32);
		assert_eq!(parse_sha256_hex(&hex).unwrap(), [0x0a; 32]);
		let colons: String = (0..32).map(|_| "0a:").collect();
		assert_eq!(parse_sha256_hex(&colons).unwrap(), [0x0a; 32]);
		assert!(parse_sha256_hex("beef").is_err());
	}
}
