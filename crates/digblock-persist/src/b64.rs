//! Minimal base64 (standard alphabet, padded) for byte-blob stores
//! (localStorage holds strings). Hand-rolled: a handful of lines beats a
//! new dependency, and living here (not the wasm-only web crate) keeps it
//! under native unit tests.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
	let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
	for chunk in bytes.chunks(3) {
		let b = [
			chunk[0],
			*chunk.get(1).unwrap_or(&0),
			*chunk.get(2).unwrap_or(&0),
		];
		let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
		let chars = [
			B64[(n >> 18) as usize & 63],
			B64[(n >> 12) as usize & 63],
			B64[(n >> 6) as usize & 63],
			B64[n as usize & 63],
		];
		let keep = chunk.len() + 1;
		for (i, c) in chars.iter().enumerate() {
			out.push(if i < keep { *c as char } else { '=' });
		}
	}
	out
}

/// None on any non-base64 byte or impossible length.
pub fn decode(text: &str) -> Option<Vec<u8>> {
	let val = |c: u8| -> Option<u32> {
		match c {
			b'A'..=b'Z' => Some(u32::from(c - b'A')),
			b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
			b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
			b'+' => Some(62),
			b'/' => Some(63),
			_ => None,
		}
	};
	// Padding is legal only at the tail.
	let trimmed = text.trim_end_matches('=');
	if trimmed.contains('=') {
		return None;
	}
	let raw: Vec<u8> = trimmed.bytes().collect();
	let mut out = Vec::with_capacity(raw.len() * 3 / 4);
	for chunk in raw.chunks(4) {
		if chunk.len() < 2 {
			return None;
		}
		let mut n = 0u32;
		for (i, &c) in chunk.iter().enumerate() {
			n |= val(c)? << (18 - 6 * i);
		}
		out.push((n >> 16) as u8);
		if chunk.len() > 2 {
			out.push((n >> 8) as u8);
		}
		if chunk.len() > 3 {
			out.push(n as u8);
		}
	}
	Some(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn roundtrip_all_lengths_and_bytes() {
		// Every length remainder mod 3, over the full byte range.
		for len in 0..=300usize {
			let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8).collect();
			let enc = encode(&bytes);
			assert_eq!(enc.len() % 4, 0, "padded to quads");
			assert_eq!(decode(&enc).as_deref(), Some(bytes.as_slice()), "len {len}");
		}
		// Known vector.
		assert_eq!(encode(b"Man"), "TWFu");
		assert_eq!(encode(b"Ma"), "TWE=");
		assert_eq!(encode(b"M"), "TQ==");
		// Garbage refuses.
		assert!(decode("not base64!").is_none());
		assert!(decode("TQ=A").is_none());
	}
}
