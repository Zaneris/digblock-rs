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
use digblock_core::chunk::{split_world, BlockWorld, ChunkData, CHUNK_SIZE, WORLD_VCHUNK};
use digblock_core::fluid::FluidWorld;
use digblock_core::protect::Region;
use digblock_core::worldgen::build_column;
use digblock_net::messages::BlockEdit;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// Hysteresis: evict only beyond `radius + EVICT_SLACK` of every center.
const EVICT_SLACK: i32 = 2;

/// Pluggable column generator. Production uses [`build_column`]; tests
/// inject synthetic worlds so store/sim tests never pay real worldgen.
pub type ColumnSource = Arc<dyn Fn(i32, i32) -> Vec<Arc<ChunkData>> + Send + Sync>;

type ColumnResult = (i32, i32, Vec<Arc<ChunkData>>);

/// One column that just finished generating (with journal overlay).
pub struct ReadyColumn {
	pub cx: i32,
	pub cz: i32,
	/// Journal-overlaid cells that came back as water.
	pub overlaid_water: Vec<(i32, i32, i32)>,
}

pub struct ChunkStore {
	seed: f64,
	columns: FxHashMap<(i32, i32), Vec<Arc<ChunkData>>>,
	pending: FxHashSet<(i32, i32)>,
	/// Scratch for [`ChunkStore::set_residency`]'s keep set, kept between
	/// calls so a residency pass allocates nothing steady-state.
	residency_keep: FxHashSet<(i32, i32)>,
	job_tx: Sender<(i32, i32)>,
	result_rx: Receiver<ColumnResult>,
	_workers: Vec<std::thread::JoinHandle<()>>,
}

impl ChunkStore {
	pub fn new(seed: f64, workers: usize) -> Self {
		Self::new_with_source(
			seed,
			workers,
			Arc::new(move |cx, cz| build_column(cx, cz, seed)),
		)
	}

	pub fn new_with_source(seed: f64, workers: usize, source: ColumnSource) -> Self {
		let (job_tx, job_rx) = unbounded::<(i32, i32)>();
		let (result_tx, result_rx) = unbounded::<ColumnResult>();
		let workers = (0..workers.max(1))
			.map(|i| {
				let rx = job_rx.clone();
				let tx = result_tx.clone();
				let source = source.clone();
				std::thread::Builder::new()
					.name(format!("store-worker-{i}"))
					.spawn(move || {
						while let Ok((cx, cz)) = rx.recv() {
							let stack = source(cx, cz);
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
			residency_keep: FxHashSet::default(),
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
	///
	/// Both halves are O(centers × r²) — the keep set is materialized ONCE
	/// and then tested by lookup. The old shape asked a closure that
	/// re-scanned every center, once per cell of every center's box and
	/// again per resident column, making the whole call O(n²·r²): at a
	/// co-located crowd on a wide sim radius that was hundreds of thousands
	/// of comparisons, fired whenever any player crossed a column boundary —
	/// i.e. on exactly the ticks everyone is moving. It also computed
	/// `cx - px` in i32 against an unbounded separation; deriving the cells
	/// from each center instead keeps every delta inside the radius by
	/// construction.
	pub fn set_residency(&mut self, centers: &[(i32, i32)], radius: i32) {
		let evict_r = radius + EVICT_SLACK;
		// Union of the evict-radius discs: what survives this pass.
		let mut keep = std::mem::take(&mut self.residency_keep);
		keep.clear();
		for &(px, pz) in centers {
			for dz in -evict_r..=evict_r {
				for dx in -evict_r..=evict_r {
					if dx * dx + dz * dz <= evict_r * evict_r {
						keep.insert((px + dx, pz + dz));
					}
				}
			}
		}
		// Request the inner disc. Testing against THIS center is enough:
		// a cell inside another center's radius is reached when that
		// center's own box is walked.
		for &(px, pz) in centers {
			for dz in -radius..=radius {
				for dx in -radius..=radius {
					if dx * dx + dz * dz > radius * radius {
						continue;
					}
					let col = (px + dx, pz + dz);
					if !self.columns.contains_key(&col) && self.pending.insert(col) {
						let _ = self.job_tx.send(col);
					}
				}
			}
		}
		self.columns.retain(|col, _| keep.contains(col));
		self.pending.retain(|col| keep.contains(col));
		self.residency_keep = keep;
	}

	/// Drain generated columns, overlaying journal edits as they arrive.
	/// Returns the columns that became ready, with the water cells their
	/// overlay restored — the sim re-schedules those so a region evicted
	/// mid-flow resumes instead of staying frozen forever.
	pub fn drain(&mut self, journal: &JournalIndex) -> Vec<ReadyColumn> {
		let mut ready = Vec::new();
		while let Ok((cx, cz, mut stack)) = self.result_rx.try_recv() {
			// Evicted (or never re-requested) while in flight: drop.
			if !self.pending.remove(&(cx, cz)) {
				continue;
			}
			let mut overlaid_water = Vec::new();
			if let Some(cells) = journal.column(cx, cz) {
				for (&(x, y, z), &block) in cells {
					overlay(&mut stack, x, y, z, block);
					if BlockType::from_id(block).is_water() {
						overlaid_water.push((x, y, z));
					}
				}
			}
			self.columns.insert((cx, cz), stack);
			ready.push(ReadyColumn {
				cx,
				cz,
				overlaid_water,
			});
		}
		ready
	}

	pub fn column_ready(&self, cx: i32, cz: i32) -> bool {
		self.columns.contains_key(&(cx, cz))
	}

	/// Test seam: worker-drain loops spin on the resident count.
	#[cfg(test)]
	pub fn resident(&self) -> usize {
		self.columns.len()
	}

	/// The `BlockWorld` contract: None = unloaded or outside the vertical
	/// world (shared with `Pipeline::block_at` via `split_world`).
	pub fn block_at(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		self.cell_at(x, y, z)
			.map(|cell| BlockType::from_id(digblock_core::chunk::cell_id(cell)))
	}

	/// Full-cell lookup (id + state nibble); same loading semantics.
	pub fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		let ((cx, cy, cz), (lx, ly, lz)) = split_world(x, y, z)?;
		let stack = self.columns.get(&(cx, cz))?;
		Some(stack[cy as usize].get_cell(lx, ly, lz))
	}

	/// Same contract as `Pipeline::set_block`: `Some(prev)` if the edit
	/// landed, `None` if out of bounds, unloaded, or a no-op.
	pub fn set_block(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> Option<BlockType> {
		self.set_cell(x, y, z, b as u16)
			.map(|prev| BlockType::from_id(digblock_core::chunk::cell_id(prev)))
	}

	/// Cell-level twin of [`Self::set_block`]: full u16 in, previous full
	/// u16 out.
	pub fn set_cell(&mut self, x: i32, y: i32, z: i32, cell: u16) -> Option<u16> {
		let ((cx, cy, cz), (lx, ly, lz)) = split_world(x, y, z)?;
		let stack = self.columns.get_mut(&(cx, cz))?;
		let chunk = &mut stack[cy as usize];
		let prev = chunk.get_cell(lx, ly, lz);
		if prev == cell {
			return None;
		}
		Arc::make_mut(chunk).set_cell(lx, ly, lz, cell);
		Some(prev)
	}
}

impl BlockWorld for ChunkStore {
	fn block_at(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		ChunkStore::block_at(self, x, y, z)
	}

	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		ChunkStore::cell_at(self, x, y, z)
	}
}

fn overlay(stack: &mut [Arc<ChunkData>], x: i32, y: i32, z: i32, block: u16) {
	let Some(((_, cy, _), (lx, ly, lz))) = split_world(x, y, z) else {
		return;
	};
	debug_assert!((cy as usize) < WORLD_VCHUNK as usize && stack.len() == WORLD_VCHUNK as usize);
	// Full u16 cell, not id-only: the journal stores whole cells exactly
	// so the id/state split stays revisable without a format break.
	Arc::make_mut(&mut stack[cy as usize]).set_cell(lx, ly, lz, block);
}

/// `FluidWorld` over the store that records every landed write (with the
/// block it replaced), so fluid steps become journal appends + broadcasts
/// carrying an honest `prev`.
pub struct StoreWorld<'a> {
	pub store: &'a mut ChunkStore,
	pub edits: &'a mut Vec<(BlockEdit, u16)>,
	/// Protected region, if any. Refusing the write here — rather than
	/// filtering the schedule — is what stops water *sourced outside* a
	/// locked region flowing into it: the fluid sim treats a false return
	/// as "didn't land" and declines to propagate onward, so one check
	/// covers every direction of approach.
	pub lock: Option<Region>,
}

impl FluidWorld for StoreWorld<'_> {
	fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		self.store.block_at(x, y, z)
	}

	fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool {
		if digblock_core::protect::immutable(x, y, z, self.lock.as_ref()) {
			return false;
		}
		if let Some(prev) = self.store.set_block(x, y, z, b) {
			self.edits.push((
				BlockEdit {
					x,
					y,
					z,
					block: b as u16,
				},
				prev as u16,
			));
			true
		} else {
			false
		}
	}
}

// The column arithmetic above assumes today's CHUNK_SIZE; keep it tied.
const _: () = assert!(CHUNK_SIZE == 16);

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::chunk::WORLD_VBLOCK;
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
			block: BlockType::Air as u16,
		});

		// Walk away (evict), come back (regen + overlay). Eviction zeroes
		// the resident count, so we wait for the origin disc all over again.
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
		let prev_block = store.block_at(2, y, 2).unwrap();
		let mut edits = Vec::new();
		let mut world = StoreWorld {
			store: &mut store,
			edits: &mut edits,
			lock: None,
		};
		assert!(world.set(2, y, 2, BlockType::Water));
		// No-op write (same value) does not land or record.
		assert!(!world.set(2, y, 2, BlockType::Water));
		// Unloaded column: does not land.
		assert!(!world.set(5000, 100, 5000, BlockType::Water));
		assert_eq!(edits.len(), 1);
		assert_eq!(edits[0].0.block, BlockType::Water as u16);
		assert_eq!(edits[0].1, prev_block as u16, "real prev recorded");
	}

	#[test]
	fn drain_reports_overlaid_water_cells() {
		let mut journal = JournalIndex::default();
		// One water cell and one solid cell journaled into column (0,0).
		journal.record(BlockEdit {
			x: 3,
			y: 140,
			z: 3,
			block: BlockType::Water as u16,
		});
		journal.record(BlockEdit {
			x: 4,
			y: 140,
			z: 4,
			block: BlockType::Stone as u16,
		});
		let mut store = ChunkStore::new(42.0, 2);
		store.set_residency(&[(0, 0)], 1);
		let deadline = Instant::now() + Duration::from_secs(30);
		let mut water = Vec::new();
		while store.resident() < 5 {
			for rc in store.drain(&journal) {
				if (rc.cx, rc.cz) == (0, 0) {
					water = rc.overlaid_water.clone();
				}
			}
			assert!(Instant::now() < deadline, "timed out generating");
			std::thread::yield_now();
		}
		assert_eq!(water, vec![(3, 140, 3)], "water reported, solid not");
	}
}
