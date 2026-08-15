//! Byte codec for [`Job`] / [`JobResult`] — the wire between the main
//! thread and a job pool that cannot share memory with it.
//!
//! Native worker threads pass these by move over a channel and never come
//! here. The browser's Web Workers can't: each worker is its own wasm
//! instance with its own linear memory, so a job crosses as bytes in a
//! transferable `ArrayBuffer`. That the pipeline's jobs are POD — the
//! worker deliberately holds no `Arc` into the store — is what makes this
//! a codec rather than a redesign.
//!
//! Everything is little-endian and explicit. Both endpoints are wasm
//! (always LE) and so is every host this builds for, but the format is
//! written out byte by byte rather than reinterpreted, so it can never
//! silently depend on the layout `rustc` happened to choose.
//!
//! **The air sentinel is part of the format.** A generated column shares
//! one [`AIR_CHUNK`] `Arc` for every sky chunk, and `is_air_chunk` tests
//! that by POINTER — the pipeline skips meshing on it, and `drain_results`
//! skips the neighbour re-dirty cascade. Encoding those chunks as a full
//! block of zeroes each would decode into one distinct all-air chunk per
//! level of the column — chunks that compare equal to air but aren't *the*
//! air, and the mesher would start meshing the sky. So an air chunk
//! travels as one flag byte and decodes back to a clone of the shared
//! sentinel, which is what `generated_result_keeps_the_air_sentinel` pins.

use std::sync::Arc;

use crate::chunk::{AIR_CHUNK, ChunkData, ChunkPos};
use crate::mesher::{MeshData, MeshInput, OreStamp, PADDED_VOLUME};
use crate::pipeline::{Job, JobResult, MeshOutput};

const TAG_GENERATE: u8 = 0;
const TAG_MESH: u8 = 1;

/// Sequential little-endian reader. Every read is bounds-checked and
/// answers `None` past the end: a truncated or corrupt message from a
/// worker must fail the decode, never panic the client.
struct Reader<'a> {
	b: &'a [u8],
	at: usize,
}

impl<'a> Reader<'a> {
	fn new(b: &'a [u8]) -> Self {
		Self { b, at: 0 }
	}

	fn take(&mut self, n: usize) -> Option<&'a [u8]> {
		let end = self.at.checked_add(n)?;
		let s = self.b.get(self.at..end)?;
		self.at = end;
		Some(s)
	}

	fn u8(&mut self) -> Option<u8> {
		Some(self.take(1)?[0])
	}

	fn u16(&mut self) -> Option<u16> {
		Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
	}

	fn u32(&mut self) -> Option<u32> {
		Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
	}

	fn i32(&mut self) -> Option<i32> {
		Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
	}

	fn u64(&mut self) -> Option<u64> {
		Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
	}

	fn f32(&mut self) -> Option<f32> {
		Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
	}

	fn f64(&mut self) -> Option<f64> {
		Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
	}

	fn pos(&mut self) -> Option<ChunkPos> {
		Some(ChunkPos::new(self.i32()?, self.i32()?, self.i32()?))
	}

	/// A length-prefixed run, rejected before allocating if the buffer
	/// couldn't possibly hold it — a bad length must not become a
	/// multi-gigabyte reservation.
	fn run<T, F: FnMut(&mut Self) -> Option<T>>(
		&mut self,
		elem: usize,
		mut f: F,
	) -> Option<Vec<T>> {
		let n = self.u32()? as usize;
		if n.checked_mul(elem)? > self.b.len() - self.at {
			return None;
		}
		let mut v = Vec::with_capacity(n);
		for _ in 0..n {
			v.push(f(self)?);
		}
		Some(v)
	}
}

fn put_u16s(out: &mut Vec<u8>, v: &[u16]) {
	for &x in v {
		out.extend_from_slice(&x.to_le_bytes());
	}
}

fn put_u32s(out: &mut Vec<u8>, v: &[u32]) {
	for &x in v {
		out.extend_from_slice(&x.to_le_bytes());
	}
}

fn put_pos(out: &mut Vec<u8>, p: ChunkPos) {
	out.extend_from_slice(&p.x.to_le_bytes());
	out.extend_from_slice(&p.y.to_le_bytes());
	out.extend_from_slice(&p.z.to_le_bytes());
}

fn put_run_len(out: &mut Vec<u8>, n: usize) {
	out.extend_from_slice(&(n as u32).to_le_bytes());
}

impl Job {
	/// Encoded size of a mesh job — the padded snapshot dominates, and its
	/// size is fixed, so the buffer is sized exactly once.
	fn encoded_hint(&self) -> usize {
		match self {
			Job::Generate { .. } => 17,
			// The ore stamp is a flag byte when absent, and otherwise
			// seed + origin + level + count + two bytes an edited cell.
			Job::Mesh { input, .. } => {
				let ore = input.ore.as_ref().map_or(1, |o| 29 + o.edited.len() * 2);
				26 + ore + PADDED_VOLUME * 2
			}
		}
	}

	pub fn encode(&self) -> Vec<u8> {
		let mut out = Vec::with_capacity(self.encoded_hint());
		match self {
			Job::Generate { x, z, seed } => {
				out.push(TAG_GENERATE);
				out.extend_from_slice(&x.to_le_bytes());
				out.extend_from_slice(&z.to_le_bytes());
				out.extend_from_slice(&seed.to_le_bytes());
			}
			Job::Mesh {
				pos,
				generation,
				input,
			} => {
				out.push(TAG_MESH);
				put_pos(&mut out, *pos);
				out.extend_from_slice(&generation.to_le_bytes());
				// Six face flags in one byte; `missing` suppresses a
				// border's faces, so losing one would punch holes in the
				// world at chunk seams.
				let mut mask = 0u8;
				for (i, &m) in input.missing.iter().enumerate() {
					mask |= u8::from(m) << i;
				}
				out.push(mask);
				// The ore reveal, when this job carries one. Present-flag
				// first: it is `None` for every chunk meshed for a reason
				// other than a reveal, which is most of them, and a level
				// that reveals nothing still has to cross as itself rather
				// than as absence — `apply_ore` is what decides.
				match &input.ore {
					None => out.push(0),
					Some(o) => {
						out.push(1);
						out.extend_from_slice(&o.seed.to_le_bytes());
						out.extend_from_slice(&o.origin.0.to_le_bytes());
						out.extend_from_slice(&o.origin.1.to_le_bytes());
						out.extend_from_slice(&o.origin.2.to_le_bytes());
						out.extend_from_slice(&o.mining.to_le_bytes());
						// The edit gate. Bounded by the edits inside one
						// padded snapshot, so a u32 count is never the limit, but
						// dropping it would let the anti-dupe re-reveal a
						// cell somebody has already mined.
						out.extend_from_slice(&(o.edited.len() as u32).to_le_bytes());
						put_u16s(&mut out, &o.edited);
					}
				}
				put_u16s(&mut out, input.blocks.as_slice());
			}
		}
		out
	}

	pub fn decode(b: &[u8]) -> Option<Job> {
		let mut r = Reader::new(b);
		match r.u8()? {
			TAG_GENERATE => Some(Job::Generate {
				x: r.i32()?,
				z: r.i32()?,
				seed: r.f64()?,
			}),
			TAG_MESH => {
				let pos = r.pos()?;
				let generation = r.u64()?;
				let mask = r.u8()?;
				let mut missing = [false; 6];
				for (i, m) in missing.iter_mut().enumerate() {
					*m = mask & (1 << i) != 0;
				}
				let ore = match r.u8()? {
					0 => None,
					1 => {
						let seed = r.f64()?;
						let origin = (r.i32()?, r.i32()?, r.i32()?);
						let mining = r.u32()?;
						let n = r.u32()? as usize;
						let mut edited = Vec::with_capacity(n.min(PADDED_VOLUME));
						for _ in 0..n {
							edited.push(r.u16()?);
						}
						Some(OreStamp {
							seed,
							origin,
							mining,
							edited,
						})
					}
					// Not a flag this format defines: fail the decode
					// rather than guess, exactly as a bad tag does.
					_ => return None,
				};
				let mut blocks = Box::new([0u16; PADDED_VOLUME]);
				for cell in blocks.iter_mut() {
					*cell = r.u16()?;
				}
				Some(Job::Mesh {
					pos,
					generation,
					input: Box::new(MeshInput {
						blocks,
						missing,
						ore,
					}),
				})
			}
			_ => None,
		}
	}
}

impl JobResult {
	pub fn encode(&self) -> Vec<u8> {
		let mut out = Vec::new();
		match self {
			JobResult::Generated {
				x,
				z,
				stack,
				dur_ms,
			} => {
				out.push(TAG_GENERATE);
				out.extend_from_slice(&x.to_le_bytes());
				out.extend_from_slice(&z.to_le_bytes());
				out.extend_from_slice(&dur_ms.to_le_bytes());
				put_run_len(&mut out, stack.len());
				for chunk in stack {
					// One byte for a sky chunk instead of a whole block of zeroes —
					// and the decode side is what makes it *the* sentinel
					// again rather than a look-alike (see the module doc).
					if is_air(chunk) {
						out.push(1);
					} else {
						out.push(0);
						put_u16s(&mut out, &chunk.blocks);
					}
				}
			}
			JobResult::Meshed { out: m, dur_ms } => {
				out.push(TAG_MESH);
				put_pos(&mut out, m.pos);
				out.extend_from_slice(&m.generation.to_le_bytes());
				out.extend_from_slice(&dur_ms.to_le_bytes());
				put_run_len(&mut out, m.mesh.solid.len());
				put_u32s(&mut out, &m.mesh.solid);
				put_run_len(&mut out, m.mesh.water.len());
				put_u32s(&mut out, &m.mesh.water);
				put_run_len(&mut out, m.mesh.impacts.len());
				put_u16s(&mut out, &m.mesh.impacts);
			}
		}
		out
	}

	pub fn decode(b: &[u8]) -> Option<JobResult> {
		let mut r = Reader::new(b);
		match r.u8()? {
			TAG_GENERATE => {
				let x = r.i32()?;
				let z = r.i32()?;
				let dur_ms = r.f32()?;
				let n = r.u32()? as usize;
				// A column is a fixed stack; anything else is corruption.
				if n != crate::chunk::WORLD_VCHUNK as usize {
					return None;
				}
				let mut stack = Vec::with_capacity(n);
				for _ in 0..n {
					if r.u8()? == 1 {
						stack.push(AIR_CHUNK.clone());
					} else {
						let mut d = ChunkData::default();
						for cell in d.blocks.iter_mut() {
							*cell = r.u16()?;
						}
						stack.push(Arc::new(d));
					}
				}
				Some(JobResult::Generated {
					x,
					z,
					stack,
					dur_ms,
				})
			}
			TAG_MESH => {
				let pos = r.pos()?;
				let generation = r.u64()?;
				let dur_ms = r.f32()?;
				let solid = r.run(4, |r| r.u32())?;
				let water = r.run(4, |r| r.u32())?;
				let impacts = r.run(2, |r| r.u16())?;
				Some(JobResult::Meshed {
					out: MeshOutput {
						pos,
						generation,
						mesh: MeshData {
							solid,
							water,
							impacts,
						},
					},
					dur_ms,
				})
			}
			_ => None,
		}
	}
}

fn is_air(d: &Arc<ChunkData>) -> bool {
	crate::chunk::is_air_chunk(d)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::pipeline::run_job;

	fn mesh_job() -> Job {
		let mut blocks = Box::new([0u16; PADDED_VOLUME]);
		// Something asymmetric, so a transposed or truncated copy fails.
		for (i, cell) in blocks.iter_mut().enumerate() {
			*cell = (i % 61) as u16;
		}
		Job::Mesh {
			pos: ChunkPos::new(-3, 5, 12),
			generation: 9_000_000_001,
			input: Box::new(MeshInput {
				blocks,
				missing: [true, false, false, true, false, true],
				ore: None,
			}),
		}
	}

	/// The same job carrying a reveal, which is the case a worker has to
	/// be told about: the stamp is gathered on the main thread and
	/// consumed inside `run_job`, so if it does not cross, a browser
	/// client meshes every chunk as though the looker had no Mining at
	/// all — ore simply never appears, and nothing errors.
	fn mesh_job_with_ore() -> Job {
		let Job::Mesh {
			pos,
			generation,
			input,
		} = mesh_job()
		else {
			unreachable!()
		};
		Job::Mesh {
			pos,
			generation,
			input: Box::new(MeshInput {
				ore: Some(OreStamp {
					seed: -8123.5,
					origin: (-32, 64, 4096),
					mining: 47,
					edited: vec![3, 17, 2044, 5831],
				}),
				..*input
			}),
		}
	}

	#[test]
	fn a_mesh_job_carries_its_ore_stamp() {
		let Some(Job::Mesh { input, .. }) = Job::decode(&mesh_job_with_ore().encode()) else {
			panic!("decode");
		};
		let ore = input.ore.expect("the stamp survived the crossing");
		assert_eq!(ore.seed, -8123.5);
		assert_eq!(ore.origin, (-32, 64, 4096));
		assert_eq!(
			ore.mining, 47,
			"two lookers mesh the same chunk differently"
		);
		assert_eq!(
			ore.edited,
			vec![3, 17, 2044, 5831],
			"the anti-dupe's edit gate is not decorative: without it a \
			 mined cell reveals again"
		);
		// And the blocks still land after it — a length the reader gets
		// wrong would desync the array that follows, not just the stamp.
		let Job::Mesh { input: orig, .. } = mesh_job() else {
			unreachable!()
		};
		assert_eq!(input.blocks.as_slice(), orig.blocks.as_slice());
	}

	/// Absence has to survive too, and it is the common case.
	#[test]
	fn a_mesh_job_without_ore_stays_without() {
		let Some(Job::Mesh { input, .. }) = Job::decode(&mesh_job().encode()) else {
			panic!("decode");
		};
		assert!(input.ore.is_none());
	}

	#[test]
	fn generate_job_round_trips() {
		let j = Job::Generate {
			x: -17,
			z: 4021,
			seed: 1234.5678,
		};
		let Some(Job::Generate { x, z, seed }) = Job::decode(&j.encode()) else {
			panic!("decode");
		};
		assert_eq!((x, z, seed), (-17, 4021, 1234.5678));
	}

	#[test]
	fn mesh_job_round_trips() {
		let Some(Job::Mesh {
			pos,
			generation,
			input,
		}) = Job::decode(&mesh_job().encode())
		else {
			panic!("decode");
		};
		let Job::Mesh {
			input: orig,
			pos: opos,
			generation: ogen,
		} = mesh_job()
		else {
			unreachable!()
		};
		assert_eq!((pos, generation), (opos, ogen));
		assert_eq!(
			input.missing, orig.missing,
			"the face mask is not decorative"
		);
		assert_eq!(input.blocks.as_slice(), orig.blocks.as_slice());
	}

	/// The whole point of the format: a column that came back from a
	/// worker must share the sentinel, or the mesher meshes the sky.
	#[test]
	fn generated_result_keeps_the_air_sentinel() {
		let result = run_job(Job::Generate {
			x: 0,
			z: 0,
			seed: 42.0,
		});
		let JobResult::Generated { stack: before, .. } = &result else {
			panic!("generate")
		};
		let air_before = before.iter().filter(|c| is_air(c)).count();
		assert!(air_before > 0, "spawn column should have sky chunks");

		let Some(JobResult::Generated { stack: after, .. }) = JobResult::decode(&result.encode())
		else {
			panic!("decode");
		};
		assert_eq!(after.len(), before.len());
		assert_eq!(
			after.iter().filter(|c| is_air(c)).count(),
			air_before,
			"decoded air chunks must be the shared AIR_CHUNK, not look-alikes"
		);
		for (a, b) in after.iter().zip(before.iter()) {
			assert_eq!(is_air(a), is_air(b));
			assert_eq!(a.blocks, b.blocks);
		}
	}

	#[test]
	fn meshed_result_round_trips() {
		// Mesh a real column's chunk, so the vecs carry real payloads
		// rather than hand-made ones.
		let JobResult::Generated { stack, .. } = run_job(Job::Generate {
			x: 0,
			z: 0,
			seed: 42.0,
		}) else {
			panic!("generate")
		};
		let solid_y = stack
			.iter()
			.position(|c| !is_air(c))
			.expect("a non-air chunk");
		let mut blocks = Box::new([0u16; PADDED_VOLUME]);
		for (i, cell) in blocks.iter_mut().enumerate() {
			*cell = stack[solid_y].blocks[i % crate::chunk::CHUNK_VOLUME];
		}
		let job = Job::Mesh {
			pos: ChunkPos::new(0, solid_y as i32, 0),
			generation: 7,
			input: Box::new(MeshInput {
				blocks,
				missing: [false; 6],
				ore: None,
			}),
		};
		let result = run_job(job);
		let JobResult::Meshed { out: before, .. } = &result else {
			panic!("mesh")
		};
		assert!(!before.mesh.solid.is_empty(), "test needs a real mesh");
		let Some(JobResult::Meshed { out: after, .. }) = JobResult::decode(&result.encode()) else {
			panic!("decode");
		};
		assert_eq!(
			(after.pos, after.generation),
			(before.pos, before.generation)
		);
		assert_eq!(after.mesh.solid, before.mesh.solid);
		assert_eq!(after.mesh.water, before.mesh.water);
		assert_eq!(after.mesh.impacts, before.mesh.impacts);
	}

	/// A worker can die mid-post, and a browser message is attacker-shaped
	/// input as far as this decoder is concerned: every truncation must be
	/// a None, never a panic or a huge allocation.
	#[test]
	fn truncated_or_garbage_input_is_none() {
		let full = mesh_job().encode();
		for cut in [0, 1, 5, 13, 26, full.len() - 1] {
			assert!(Job::decode(&full[..cut]).is_none(), "cut at {cut}");
		}
		assert!(Job::decode(&[9]).is_none(), "unknown tag");
		assert!(JobResult::decode(&[9]).is_none(), "unknown tag");
		// A length prefix that promises more than the buffer holds.
		let mut lying = vec![TAG_MESH];
		lying.extend_from_slice(&[0u8; 12]); // pos
		lying.extend_from_slice(&[0u8; 8]); // gen
		lying.extend_from_slice(&[0u8; 4]); // dur
		lying.extend_from_slice(&u32::MAX.to_le_bytes()); // solid len
		assert!(JobResult::decode(&lying).is_none());
	}
}
