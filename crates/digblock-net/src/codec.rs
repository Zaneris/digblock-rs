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
	let bytes = postcard::to_stdvec(d).map_err(CodecError::Encode)?;
	if bytes.len() > MAX_DATAGRAM {
		return Err(CodecError::Oversize(bytes.len()));
	}
	Ok(bytes)
}

pub fn decode_datagram(bytes: &[u8]) -> Result<Datagram, CodecError> {
	postcard::from_bytes(bytes).map_err(CodecError::Decode)
}

/// Append one length-prefixed frame to `out`.
pub fn write_frame<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
	let body = postcard::to_stdvec(msg).map_err(CodecError::Encode)?;
	if body.len() > MAX_FRAME {
		return Err(CodecError::Oversize(body.len()));
	}
	out.extend_from_slice(&(body.len() as u32).to_le_bytes());
	out.extend_from_slice(&body);
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

	/// `Ok(None)` = need more bytes.
	pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
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
		let frame = avail[4..4 + len].to_vec();
		self.cursor += 4 + len;
		Ok(Some(frame))
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
