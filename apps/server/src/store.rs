//! Headless server-side chunk store. Deliberately NOT the client
//! `Pipeline`: no meshing, and residency is the union of N player discs
//! instead of one center. Reuses the same primitives — `build_column`
//! workers, `AIR_CHUNK` sharing, `Arc::make_mut` copy-on-write — so a
//! given seed produces bit-identical terrain on client and server.
//!
//! Eviction is safe at any time: edits live in the `JournalIndex` and are
//! re-overlaid whenever a column regenerates.

use crate::journal::JournalIndex;
use crossbeam_channel::{unbounded, Receiver, Sender};
use digblock_core::block::BlockType;
use digblock_core::chunk::{ChunkData, CHUNK_SIZE, WORLD_VBLOCK, WORLD_VCHUNK};
use digblock_core::fluid::FluidWorld;
use digblock_core::worldgen::build_column;
use digblock_net::messages::BlockEdit;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// Hysteresis: evict only beyond `radius + EVICT_SLACK` of every center.
const EVICT_SLACK: i32 = 2;

type ColumnResult = (i32, i32, Vec<Arc<ChunkData>>);

pub struct ChunkStore {
	seed: f64,
	columns: FxHashMap<(i32, i32), Vec<Arc<ChunkData>>>,
	pending: FxHashSet<(i32, i32)>,
	job_tx: Sender<(i32, i32)>,
	result_rx: Receiver<ColumnResult>,
	_workers: Vec<std::thread::JoinHandle<()>>,
}

impl ChunkStore {
	pub fn new(seed: f64, workers: usize) -> Self {
		let (job_tx, job_rx) = unbounded::<(i32, i32)>();
		let (result_tx, result_rx) = unbounded::<ColumnResult>();
		let workers = (0..workers.max(1))
			.map(|i| {
				let rx = job_rx.clone();
				let tx = result_tx.clone();
				std::thread::Builder::new()
					.name(format!("store-worker-{i}"))
					.spawn(move || {
						while let Ok((cx, cz)) = rx.recv() {
							let stack = build_column(cx, cz, seed);
							if tx.send((cx, cz, stack)).is_err() {
								return;
							}
						}
					})
					.expect("spawn store worker")
			})
			.collect();
		Self {
			seed,
			columns: FxHashMap::default(),
			pending: FxHashSet::default(),
			job_tx,
			result_rx,
			_workers: workers,
		}
	}

	pub fn seed(&self) -> f64 {
		self.seed
	}

	/// Request residency for all columns within `radius` (in columns) of
	/// any center; evict columns beyond `radius + EVICT_SLACK` of every
	/// center.
	pub fn set_residency(&mut self, centers: &[(i32, i32)], radius: i32) {
		let want = |cx: i32, cz: i32, r: i32| {
			centers.iter().any(|&(px, pz)| {
				let (dx, dz) = (cx - px, cz - pz);
				dx * dx + dz * dz <= r * r
			})
		};
		for &(px, pz) in centers {
			for cz in (pz - radius)..=(pz + radius) {
				for cx in (px - radius)..=(px + radius) {
					if want(cx, cz, radius)
						&& !self.columns.contains_key(&(cx, cz))
						&& self.pending.insert((cx, cz))
					{
						let _ = self.job_tx.send((cx, cz));
					}
				}
			}
		}
		let evict_r = radius + EVICT_SLACK;
		self.columns.retain(|&(cx, cz), _| want(cx, cz, evict_r));
		self.pending.retain(|&(cx, cz)| want(cx, cz, evict_r));
	}

	/// Drain generated columns, overlaying journal edits as they arrive.
	/// Returns how many columns became ready.
	pub fn drain(&mut self, journal: &JournalIndex) -> u32 {
		let mut ready = 0;
		while let Ok((cx, cz, mut stack)) = self.result_rx.try_recv() {
			// Evicted (or never re-requested) while in flight: drop.
			if !self.pending.remove(&(cx, cz)) {
				continue;
			}
			if let Some(cells) = journal.column(cx, cz) {
				for (&(x, y, z), &block) in cells {
					overlay(&mut stack, x, y, z, block);
				}
			}
			self.columns.insert((cx, cz), stack);
			ready += 1;
		}
		ready
	}

	pub fn column_ready(&self, cx: i32, cz: i32) -> bool {
		self.columns.contains_key(&(cx, cz))
	}

	pub fn pending_columns(&self) -> usize {
		self.pending.len()
	}

	pub fn resident(&self) -> usize {
		self.columns.len()
	}

	/// Same contract as `Pipeline::block_at`: None = unloaded or outside
	/// the vertical world.
	pub fn block_at(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		if !(0..=WORLD_VBLOCK).contains(&y) {
			return None;
		}
		let stack = self.columns.get(&(x.div_euclid(16), z.div_euclid(16)))?;
		let chunk = &stack[y.div_euclid(16) as usize];
		Some(chunk.get(
			x.rem_euclid(16) as usize,
			y.rem_euclid(16) as usize,
			z.rem_euclid(16) as usize,
		))
	}

	/// Same contract as `Pipeline::set_block`: `Some(prev)` if the edit
	/// landed, `None` if out of bounds, unloaded, or a no-op.
	pub fn set_block(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> Option<BlockType> {
		if !(0..=WORLD_VBLOCK).contains(&y) {
			return None;
		}
		let stack = self
			.columns
			.get_mut(&(x.div_euclid(16), z.div_euclid(16)))?;
		let chunk = &mut stack[y.div_euclid(16) as usize];
		let (lx, ly, lz) = (
			x.rem_euclid(16) as usize,
			y.rem_euclid(16) as usize,
			z.rem_euclid(16) as usize,
		);
		let prev = chunk.get(lx, ly, lz);
		if prev == b {
			return None;
		}
		Arc::make_mut(chunk).set(lx, ly, lz, b);
		Some(prev)
	}
}

fn overlay(stack: &mut [Arc<ChunkData>], x: i32, y: i32, z: i32, block: u8) {
	if !(0..=WORLD_VBLOCK).contains(&y) {
		return;
	}
	let cy = y.div_euclid(16) as usize;
	debug_assert!(cy < WORLD_VCHUNK as usize && stack.len() == WORLD_VCHUNK as usize);
	Arc::make_mut(&mut stack[cy]).set(
		x.rem_euclid(16) as usize,
		y.rem_euclid(16) as usize,
		z.rem_euclid(16) as usize,
		BlockType::from_u8(block),
	);
}

/// `FluidWorld` over the store that records every landed write, so fluid
/// steps become journal appends + broadcasts.
pub struct StoreWorld<'a> {
	pub store: &'a mut ChunkStore,
	pub edits: &'a mut Vec<BlockEdit>,
}

impl FluidWorld for StoreWorld<'_> {
	fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		self.store.block_at(x, y, z)
	}

	fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool {
		if self.store.set_block(x, y, z, b).is_some() {
			self.edits.push(BlockEdit {
				x,
				y,
				z,
				block: b as u8,
			});
			true
		} else {
			false
		}
	}
}

// CHUNK_SIZE is 16 everywhere above; keep the constant tied.
const _: () = assert!(CHUNK_SIZE == 16);

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_net::messages::BlockEdit;
	use std::time::{Duration, Instant};

	fn drain_until(store: &mut ChunkStore, journal: &JournalIndex, want: usize) {
		let deadline = Instant::now() + Duration::from_secs(30);
		while store.resident() < want {
			store.drain(journal);
			assert!(Instant::now() < deadline, "timed out generating columns");
			std::thread::yield_now();
		}
	}

	#[test]
	fn two_center_residency_and_eviction() {
		let journal = JournalIndex::default();
		let mut store = ChunkStore::new(42.0, 2);
		store.set_residency(&[(0, 0), (40, 0)], 2);
		let per_disc = 13; // r=2 disc: 1 + 4 + 8
		drain_until(&mut store, &journal, per_disc * 2);
		assert!(store.column_ready(0, 0));
		assert!(store.column_ready(40, 0));
		assert!(!store.column_ready(20, 0), "between discs stays unloaded");

		// Move both players to one place: far disc evicts.
		store.set_residency(&[(0, 0)], 2);
		assert!(!store.column_ready(40, 0));
		assert!(store.column_ready(0, 0));
	}

	#[test]
	fn journal_overlay_survives_evict_and_regen() {
		let mut journal = JournalIndex::default();
		let mut store = ChunkStore::new(42.0, 2);
		store.set_residency(&[(0, 0)], 1);
		drain_until(&mut store, &journal, 5);

		// Find some solid block near the origin surface and dig it.
		let (x, z) = (4, 4);
		let y = (0..=WORLD_VBLOCK)
			.rev()
			.find(|&y| matches!(store.block_at(x, y, z), Some(b) if b.props().collides))
			.expect("a solid block in the column");
		let prev = store
			.set_block(x, y, z, BlockType::Air)
			.expect("edit lands");
		assert!(prev.props().collides);
		journal.record(BlockEdit {
			x,
			y,
			z,
			block: BlockType::Air as u8,
		});

		// Walk away (evict), come back (regen + overlay). Eviction zeroes
		// the resident count, so we wait for the origin disc's 5 again.
		store.set_residency(&[(100, 100)], 1);
		assert!(!store.column_ready(0, 0));
		store.set_residency(&[(0, 0)], 1);
		drain_until(&mut store, &journal, 5);
		assert!(store.column_ready(0, 0));
		assert_eq!(
			store.block_at(x, y, z),
			Some(BlockType::Air),
			"edit restored"
		);
	}

	#[test]
	fn store_world_records_landed_writes_only() {
		let journal = JournalIndex::default();
		let mut store = ChunkStore::new(42.0, 2);
		store.set_residency(&[(0, 0)], 1);
		drain_until(&mut store, &journal, 5);

		let y = (0..=WORLD_VBLOCK)
			.rev()
			.find(|&y| matches!(store.block_at(2, y, 2), Some(b) if b.props().collides))
			.unwrap();
		let mut edits = Vec::new();
		let mut world = StoreWorld {
			store: &mut store,
			edits: &mut edits,
		};
		assert!(world.set(2, y, 2, BlockType::Water));
		// No-op write (same value) does not land or record.
		assert!(!world.set(2, y, 2, BlockType::Water));
		// Unloaded column: does not land.
		assert!(!world.set(5000, 100, 5000, BlockType::Water));
		assert_eq!(edits.len(), 1);
		assert_eq!(edits[0].block, BlockType::Water as u8);
	}
}
