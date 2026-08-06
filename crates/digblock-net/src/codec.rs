//! Postcard encoding plus stream framing.
//!
//! Datagrams: one message per QUIC datagram, raw postcard — the datagram
//! boundary is the frame. Reliable streams: u32-LE length prefix + body,
//! reassembled incrementally by [`FrameReader`].

use crate::messages::Datagram;
use crate::protocol::{MAX_DATAGRAM, MAX_FRAME};
use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug)]
pub enum CodecError {
	Encode(postcard::Error),
	Decode(postcard::Error),
	Oversize(usize),
}

impl std::fmt::Display for CodecError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			CodecError::Encode(e) => write!(f, "encode: {e}"),
			CodecError::Decode(e) => write!(f, "decode: {e}"),
			CodecError::Oversize(n) => write!(f, "frame too large: {n} bytes"),
		}
	}
}

impl std::error::Error for CodecError {}

pub fn encode_datagram(d: &Datagram) -> Result<Vec<u8>, CodecError> {
	let mut out = Vec::new();
	encode_datagram_into(d, &mut out)?;
	Ok(out)
}

/// Encode into a caller-owned buffer (cleared first) and return the
/// encoded slice. Senders that keep the buffer between calls — the
/// per-frame input datagram, the 20 Hz snapshot fan-out — reuse one
/// allocation for the life of the connection.
pub fn encode_datagram_into<'a>(
	d: &Datagram,
	out: &'a mut Vec<u8>,
) -> Result<&'a [u8], CodecError> {
	out.clear();
	postcard::to_io(d, &mut *out).map_err(CodecError::Encode)?;
	if out.len() > MAX_DATAGRAM {
		return Err(CodecError::Oversize(out.len()));
	}
	Ok(&out[..])
}

pub fn decode_datagram(bytes: &[u8]) -> Result<Datagram, CodecError> {
	postcard::from_bytes(bytes).map_err(CodecError::Decode)
}

/// Append one length-prefixed frame to `out`. postcard writes straight
/// into `out` (the header is backfilled once the body length is known),
/// so a message costs no staging Vec of its own — join sync frames these
/// by the thousand. `out` is restored to its entry length on error.
pub fn write_frame<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
	let at = out.len();
	out.extend_from_slice(&[0u8; 4]);
	if let Err(e) = postcard::to_io(msg, &mut *out) {
		out.truncate(at);
		return Err(CodecError::Encode(e));
	}
	let len = out.len() - at - 4;
	if len > MAX_FRAME {
		out.truncate(at);
		return Err(CodecError::Oversize(len));
	}
	out[at..at + 4].copy_from_slice(&(len as u32).to_le_bytes());
	Ok(())
}

pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, CodecError> {
	postcard::from_bytes(frame).map_err(CodecError::Decode)
}

/// Incremental reassembly of length-prefixed frames from a byte stream.
#[derive(Default)]
pub struct FrameReader {
	buf: Vec<u8>,
	cursor: usize,
}

impl FrameReader {
	pub fn extend(&mut self, bytes: &[u8]) {
		// Drop consumed prefix before growing, so the buffer stays small.
		if self.cursor > 0 {
			self.buf.drain(..self.cursor);
			self.cursor = 0;
		}
		self.buf.extend_from_slice(bytes);
	}

	/// The next complete frame, borrowed straight out of the reassembly
	/// buffer — `decode` takes `&[u8]`, so nothing needs to own it. The
	/// borrow ends at the next call. `Ok(None)` = need more bytes.
	pub fn next_frame_ref(&mut self) -> Result<Option<&[u8]>, CodecError> {
		let avail = &self.buf[self.cursor..];
		if avail.len() < 4 {
			return Ok(None);
		}
		let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]) as usize;
		if len > MAX_FRAME {
			return Err(CodecError::Oversize(len));
		}
		if avail.len() < 4 + len {
			return Ok(None);
		}
		let start = self.cursor + 4;
		self.cursor += 4 + len;
		Ok(Some(&self.buf[start..start + len]))
	}

	/// Owning wrapper for callers that hold a frame across further reads
	/// (or across a `&mut self` call on their own state). `Ok(None)` =
	/// need more bytes.
	pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
		Ok(self.next_frame_ref()?.map(<[u8]>::to_vec))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::messages::{InputBatch, InputTick};

	#[test]
	fn frame_reader_reassembles_byte_at_a_time() {
		let mut wire = Vec::new();
		write_frame(&"hello".to_string(), &mut wire).unwrap();
		write_frame(&"world".to_string(), &mut wire).unwrap();

		let mut r = FrameReader::default();
		let mut got = Vec::new();
		for b in wire {
			r.extend(&[b]);
			while let Some(frame) = r.next_frame().unwrap() {
				got.push(decode::<String>(&frame).unwrap());
			}
		}
		assert_eq!(got, vec!["hello".to_string(), "world".to_string()]);
	}

	/// The borrowed and owning readers must split a stream identically —
	/// `next_frame` is the same cursor walk plus a copy.
	#[test]
	fn borrowed_and_owned_readers_agree() {
		let mut wire = Vec::new();
		for s in ["a", "bb", "ccc"] {
			write_frame(&s.to_string(), &mut wire).unwrap();
		}
		let mut owned = FrameReader::default();
		owned.extend(&wire);
		let mut borrowed = FrameReader::default();
		borrowed.extend(&wire);
		while let Some(want) = owned.next_frame().unwrap() {
			let got = borrowed
				.next_frame_ref()
				.unwrap()
				.expect("same frame count");
			assert_eq!(&want[..], got);
		}
		assert!(borrowed.next_frame_ref().unwrap().is_none());
	}

	/// Frames are written in place now, so a failed encode must not leave
	/// a half-serialized body (or a stale length header) behind for the
	/// caller to transmit — send_hello prefixes the preamble into the
	/// same buffer.
	#[test]
	fn failed_write_frame_restores_the_buffer() {
		let mut buf = vec![0xaa; 5];
		// A frame body over MAX_FRAME is the only reachable failure.
		let huge = vec![0u8; MAX_FRAME + 16];
		assert!(write_frame(&huge, &mut buf).is_err());
		assert_eq!(buf, vec![0xaa; 5], "buffer untouched by the failure");
	}

	#[test]
	fn frame_reader_rejects_oversize_header() {
		let mut r = FrameReader::default();
		r.extend(&(u32::MAX).to_le_bytes());
		assert!(r.next_frame().is_err());
	}

	#[test]
	fn datagram_oversize_rejected() {
		let batch = InputBatch {
			inputs: vec![
				InputTick {
					tick: 0,
					move_axis: [1.0, 1.0],
					jump: true,
					look: [1.0; 3],
					claimed_pos: [1.0; 3],
					claimed_vel: [1.0; 3],
					active_slot: 0,
				};
				64
			],
		};
		assert!(encode_datagram(&Datagram::Input(batch)).is_err());
	}
}
