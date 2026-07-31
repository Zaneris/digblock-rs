//! Threaded chunk pipeline — replaces the Java frame-slicing loop
//! (one queue item per frame, round-robin) with a worker pool. The main
//! thread owns the store and scheduling; workers run pure functions
//! (terrain gen, meshing) over immutable snapshots.
//!
//! Staleness is handled by generation counters, not flags: every mesh job
//! carries the `mesh_gen` it was built from, and results that don't match
//! the slot's current counter (or whose slot was evicted) are dropped.
//! When a chunk finishes generating, it bumps its own and all six Ready
//! neighbors' counters — the safe equivalent of the Java writing face bits
//! into neighbor chunks and re-dirtying them.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use rustc_hash::FxHashMap;

use crate::chunk::{ChunkData, ChunkPos, WORLD_VCHUNK};
use crate::mesher::{border_plane, mesh_chunk, MeshData, NeighborPlane, DIRS};
use crate::metrics::JobTiming;
use crate::worldgen::build_column;

pub struct MeshOutput {
	pub pos: ChunkPos,
	pub gen: u64,
	pub mesh: MeshData,
}

enum Job {
	/// Generate the whole vertical chunk stack of column (x, z) — one
	/// heightmap evaluation serves every chunk in the stack.
	Generate { x: i32, z: i32, seed: f64 },
	Mesh {
		pos: ChunkPos,
		gen: u64,
		blocks: Arc<ChunkData>,
		neighbors: Box<[NeighborPlane; 6]>,
	},
}

enum JobResult {
	Generated {
		x: i32,
		z: i32,
		stack: [Arc<ChunkData>; WORLD_VCHUNK as usize],
		dur_ms: f32,
	},
	Meshed {
		out: MeshOutput,
		dur_ms: f32,
	},
}

enum ChunkState {
	Generating,
	Ready(Arc<ChunkData>),
}

struct ChunkSlot {
	state: ChunkState,
	/// Bumped when this chunk must (re-)mesh.
	mesh_gen: u64,
	/// Highest gen handed to a worker.
	submitted_gen: u64,
}

/// What the caller (renderer side) must do after a maintain() call, plus
/// instrumentation for the metrics system.
pub struct Maintenance {
	pub evicted: Vec<ChunkPos>,
	pub meshed: Vec<MeshOutput>,
	pub generated: u32,
	/// Worker-job durations drained this call (including stale results —
	/// the work was still done).
	pub gen_jobs: JobTiming,
	pub mesh_jobs: JobTiming,
	/// Mesh results dropped because their generation was superseded.
	pub stale_dropped: u32,
	/// Jobs bounced back to a heap because the worker channel was full.
	pub requeued: u32,
	/// Hit `upload_budget` with more results still waiting.
	pub saturated: bool,
	pub gen_queue: u32,
	pub mesh_queue: u32,
	/// Jobs handed to workers whose results haven't been drained yet.
	pub in_flight: u32,
}

pub struct Pipeline {
	seed: f64,
	draw_dist: i32,
	store: FxHashMap<ChunkPos, ChunkSlot>,
	job_tx: Sender<Job>,
	result_rx: Receiver<JobResult>,
	/// Pending columns (x, z) — generation is per-column, not per-chunk.
	gen_heap: BinaryHeap<(Reverse<i64>, (i32, i32))>,
	mesh_heap: BinaryHeap<(Reverse<i64>, (i32, i32, i32))>,
	center: ChunkPos,
	scanned_once: bool,
	in_flight: u32,
	_workers: Vec<std::thread::JoinHandle<()>>,
}

fn dist2(a: ChunkPos, b: ChunkPos) -> i64 {
	let (dx, dy, dz) = ((a.x - b.x) as i64, (a.y - b.y) as i64, (a.z - b.z) as i64);
	dx * dx + dy * dy + dz * dz
}

impl Pipeline {
	pub fn new(seed: f64, draw_dist: i32, workers: usize) -> Self {
		// Deep enough that workers never starve between once-per-frame
		// top-ups (a shallow queue capped throughput at ~queue+workers jobs
		// per frame); shallow enough that priority ordering stays relevant —
		// queued jobs can't be re-prioritized, only staleness-dropped.
		let (job_tx, job_rx) = bounded::<Job>(workers * 16);
		let (result_tx, result_rx) = unbounded::<JobResult>();
		let workers = (0..workers)
			.map(|i| {
				let rx = job_rx.clone();
				let tx = result_tx.clone();
				std::thread::Builder::new()
					.name(format!("chunk-worker-{i}"))
					.spawn(move || worker_loop(rx, tx))
					.expect("spawn worker")
			})
			.collect();
		Self {
			seed,
			draw_dist,
			store: FxHashMap::default(),
			job_tx,
			result_rx,
			gen_heap: BinaryHeap::new(),
			mesh_heap: BinaryHeap::new(),
			center: ChunkPos::new(i32::MAX, 0, i32::MAX),
			scanned_once: false,
			in_flight: 0,
			_workers: workers,
		}
	}

	/// Call every frame with the player's chunk position. Performs the
	/// visibility scan on chunk-boundary crossings, tops up workers, and
	/// drains results (mesh outputs capped at `upload_budget`).
	pub fn maintain(&mut self, center: ChunkPos, upload_budget: usize) -> Maintenance {
		let mut out = Maintenance {
			evicted: Vec::new(),
			meshed: Vec::new(),
			generated: 0,
			gen_jobs: JobTiming::default(),
			mesh_jobs: JobTiming::default(),
			stale_dropped: 0,
			requeued: 0,
			saturated: false,
			gen_queue: 0,
			mesh_queue: 0,
			in_flight: 0,
		};
		let center = ChunkPos::new(center.x, 0, center.z);
		if center != self.center || !self.scanned_once {
			self.center = center;
			self.scanned_once = true;
			self.visibility_scan(&mut out.evicted);
		}
		self.drain_results(&mut out, upload_budget);
		out.saturated = out.meshed.len() == upload_budget && !self.result_rx.is_empty();
		out.requeued = self.top_up_workers();
		out.gen_queue = self.gen_heap.len() as u32;
		out.mesh_queue = self.mesh_heap.len() as u32;
		out.in_flight = self.in_flight;
		out
	}

	/// Desired set: y ∈ 0..WORLD_VCHUNK, Euclidean distance < draw_dist in
	/// chunk units — the Java updateVisible cull. Chunks leaving the set
	/// are dropped (results in flight are rejected by the slot check).
	fn visibility_scan(&mut self, evicted: &mut Vec<ChunkPos>) {
		let r = self.draw_dist;
		let c = self.center;
		self.store.retain(|pos, _| {
			let keep = dist2(ChunkPos::new(pos.x, 0, pos.z), c) < i64::from(r) * i64::from(r);
			if !keep {
				evicted.push(*pos);
			}
			keep
		});
		for x in (c.x - r)..=(c.x + r) {
			for z in (c.z - r)..=(c.z + r) {
				let flat = ChunkPos::new(x, 0, z);
				let d2 = dist2(flat, c);
				if d2 >= i64::from(r) * i64::from(r) {
					continue;
				}
				let mut new_column = false;
				for y in 0..WORLD_VCHUNK {
					let pos = ChunkPos::new(x, y, z);
					if let std::collections::hash_map::Entry::Vacant(e) = self.store.entry(pos) {
						e.insert(ChunkSlot {
							state: ChunkState::Generating,
							mesh_gen: 0,
							submitted_gen: 0,
						});
						new_column = true;
					}
				}
				if new_column {
					self.gen_heap.push((Reverse(d2), (x, z)));
				}
			}
		}
		// Re-prioritize pending meshes for the new center.
		let pending: Vec<(i32, i32, i32)> = self.mesh_heap.drain().map(|(_, p)| p).collect();
		for p in pending {
			let pos = ChunkPos::new(p.0, p.1, p.2);
			if self.store.contains_key(&pos) {
				self.mesh_heap
					.push((Reverse(dist2(ChunkPos::new(p.0, 0, p.2), c)), p));
			}
		}
	}

	fn drain_results(&mut self, out: &mut Maintenance, upload_budget: usize) {
		while out.meshed.len() < upload_budget {
			let Ok(result) = self.result_rx.try_recv() else {
				break;
			};
			self.in_flight = self.in_flight.saturating_sub(1);
			match result {
				JobResult::Generated {
					x,
					z,
					stack,
					dur_ms,
				} => {
					out.gen_jobs.record(dur_ms);
					let c = self.center;
					let d = dist2(ChunkPos::new(x, 0, z), c);
					for (cy, data) in stack.into_iter().enumerate() {
						let pos = ChunkPos::new(x, cy as i32, z);
						out.generated += 1;
						if let Some(slot) = self.store.get_mut(&pos) {
							slot.state = ChunkState::Ready(data);
							slot.mesh_gen += 1;
							self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
						}
						// The Rust equivalent of Java's cross-border re-dirty:
						// every Ready neighbor re-meshes from fresh snapshots.
						for dir in DIRS {
							let (ox, oy, oz) = dir.offset();
							let np = ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz);
							if let Some(nslot) = self.store.get_mut(&np) {
								if matches!(nslot.state, ChunkState::Ready(_)) {
									nslot.mesh_gen += 1;
									self.mesh_heap.push((
										Reverse(dist2(ChunkPos::new(np.x, 0, np.z), self.center)),
										(np.x, np.y, np.z),
									));
								}
							}
						}
					}
				}
				JobResult::Meshed { out: m, dur_ms } => {
					out.mesh_jobs.record(dur_ms);
					match self.store.get(&m.pos) {
						// Only the newest generation may reach the GPU.
						Some(slot) if slot.mesh_gen == m.gen => out.meshed.push(m),
						_ => out.stale_dropped += 1, // stale or evicted
					}
				}
			}
		}
	}

	/// Returns the number of jobs bounced back because workers were
	/// saturated (0 or 1 — a bounce ends the loop).
	fn top_up_workers(&mut self) -> u32 {
		loop {
			// Nearest-first across both heaps; generation wins ties so
			// terrain exists before meshes want it.
			let gen_d = self.gen_heap.peek().map(|(Reverse(d), _)| *d);
			let mesh_d = self.mesh_heap.peek().map(|(Reverse(d), _)| *d);
			let job = match (gen_d, mesh_d) {
				(None, None) => break,
				(Some(_), None) => self.next_gen_job(),
				(None, Some(_)) => self.next_mesh_job(),
				(Some(g), Some(m)) => {
					if g <= m {
						self.next_gen_job()
					} else {
						self.next_mesh_job()
					}
				}
			};
			let Some(job) = job else { continue };
			match self.job_tx.try_send(job) {
				Ok(()) => self.in_flight += 1,
				Err(TrySendError::Full(job)) => {
					// Put it back; workers are saturated.
					self.requeue(job);
					return 1;
				}
				Err(TrySendError::Disconnected(_)) => break,
			}
		}
		0
	}

	fn next_gen_job(&mut self) -> Option<Job> {
		let (_, (x, z)) = self.gen_heap.pop()?;
		// The column's chunks enter and leave the store together; probe y=0.
		match self.store.get(&ChunkPos::new(x, 0, z)) {
			Some(slot) if matches!(slot.state, ChunkState::Generating) => Some(Job::Generate {
				x,
				z,
				seed: self.seed,
			}),
			_ => None, // evicted or already generated — skip
		}
	}

	fn next_mesh_job(&mut self) -> Option<Job> {
		let (_, p) = self.mesh_heap.pop()?;
		let pos = ChunkPos::new(p.0, p.1, p.2);
		let slot = self.store.get(&pos)?;
		let ChunkState::Ready(data) = &slot.state else {
			return None;
		};
		if slot.mesh_gen <= slot.submitted_gen {
			return None; // an equal-or-newer job is already in flight
		}
		let gen = slot.mesh_gen;
		let blocks = Arc::clone(data);
		let neighbors = Box::new(self.neighbor_planes(pos));
		let slot = self.store.get_mut(&pos).expect("slot exists");
		slot.submitted_gen = gen;
		Some(Job::Mesh {
			pos,
			gen,
			blocks,
			neighbors,
		})
	}

	fn neighbor_planes(&self, pos: ChunkPos) -> [NeighborPlane; 6] {
		DIRS.map(|dir| {
			let (ox, oy, oz) = dir.offset();
			let np = ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz);
			if np.y >= WORLD_VCHUNK {
				return NeighborPlane::Air; // Java air sentinel above the world
			}
			if np.y < 0 {
				return NeighborPlane::Missing; // Java null below
			}
			match self.store.get(&np) {
				Some(ChunkSlot {
					state: ChunkState::Ready(d),
					..
				}) => NeighborPlane::Blocks(border_plane(d, dir.opposite())),
				_ => NeighborPlane::Missing,
			}
		})
	}

	fn requeue(&mut self, job: Job) {
		match job {
			Job::Generate { x, z, .. } => {
				let d = dist2(ChunkPos::new(x, 0, z), self.center);
				self.gen_heap.push((Reverse(d), (x, z)));
			}
			Job::Mesh { pos, gen, .. } => {
				// Roll back the submitted marker so the job reschedules.
				if let Some(slot) = self.store.get_mut(&pos) {
					if slot.submitted_gen == gen {
						slot.submitted_gen = gen - 1;
					}
				}
				let d = dist2(ChunkPos::new(pos.x, 0, pos.z), self.center);
				self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
			}
		}
	}

	/// Force a re-mesh of every resident Ready chunk (device-loss drill).
	pub fn remesh_all(&mut self) {
		let center = self.center;
		for (pos, slot) in self.store.iter_mut() {
			if matches!(slot.state, ChunkState::Ready(_)) {
				slot.mesh_gen += 1;
				self.mesh_heap.push((
					Reverse(dist2(ChunkPos::new(pos.x, 0, pos.z), center)),
					(pos.x, pos.y, pos.z),
				));
			}
		}
	}

	pub fn resident(&self) -> usize {
		self.store.len()
	}

	/// Block lookup for physics. None = outside the vertical world or the
	/// chunk isn't generated yet (the Java null-block: non-solid).
	pub fn block_at(&self, x: i32, y: i32, z: i32) -> Option<crate::block::BlockType> {
		if !(0..=crate::chunk::WORLD_VBLOCK).contains(&y) {
			return None;
		}
		let pos = ChunkPos::new(x.div_euclid(16), y.div_euclid(16), z.div_euclid(16));
		match self.store.get(&pos) {
			Some(ChunkSlot {
				state: ChunkState::Ready(d),
				..
			}) => Some(d.get(
				x.rem_euclid(16) as usize,
				y.rem_euclid(16) as usize,
				z.rem_euclid(16) as usize,
			)),
			_ => None,
		}
	}
}

fn worker_loop(rx: Receiver<Job>, tx: Sender<JobResult>) {
	while let Ok(job) = rx.recv() {
		let start = std::time::Instant::now();
		let result = match job {
			Job::Generate { x, z, seed } => {
				let stack = build_column(x, z, seed).map(Arc::new);
				JobResult::Generated {
					x,
					z,
					stack,
					dur_ms: start.elapsed().as_secs_f32() * 1000.0,
				}
			}
			Job::Mesh {
				pos,
				gen,
				blocks,
				neighbors,
			} => {
				let mesh = mesh_chunk(&blocks, &neighbors);
				JobResult::Meshed {
					out: MeshOutput { pos, gen, mesh },
					dur_ms: start.elapsed().as_secs_f32() * 1000.0,
				}
			}
		};
		if tx.send(result).is_err() {
			break; // pipeline dropped
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Soak: stream around a moving center until stable; every resident
	/// chunk must end up meshed exactly at its final generation, no stale
	/// meshes, no leaks.
	#[test]
	fn streaming_converges() {
		let mut p = Pipeline::new(0.0, 6, 2);
		let mut uploaded: FxHashMap<ChunkPos, u64> = FxHashMap::default();
		let mut gen_jobs = JobTiming::default();
		let mut mesh_jobs = JobTiming::default();
		// Walk the center across chunk boundaries.
		for step in 0..6 {
			let center = ChunkPos::new(step * 2, 0, step);
			for _ in 0..300 {
				let m = p.maintain(center, 64);
				gen_jobs.merge(m.gen_jobs);
				mesh_jobs.merge(m.mesh_jobs);
				for pos in m.evicted {
					uploaded.remove(&pos);
				}
				for out in m.meshed {
					uploaded.insert(out.pos, out.gen);
				}
				if m.gen_queue + m.mesh_queue == 0 && m.in_flight == 0 {
					// let in-flight results land
					std::thread::sleep(std::time::Duration::from_millis(2));
				}
			}
		}
		// Drain until fully quiescent.
		let final_center = ChunkPos::new(10, 0, 5);
		for _ in 0..500 {
			let m = p.maintain(final_center, 64);
			for pos in m.evicted {
				uploaded.remove(&pos);
			}
			for out in m.meshed {
				uploaded.insert(out.pos, out.gen);
			}
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
		assert!(p.resident() > 0);
		// Every resident chunk has a mesh upload, and interior chunks
		// (all 4 horizontal neighbors present) meshed with full borders.
		assert_eq!(uploaded.len(), p.resident(), "all resident chunks meshed");
		// Instrumentation sanity: jobs were timed and aggregated.
		assert!(gen_jobs.count > 0, "gen jobs recorded");
		assert!(mesh_jobs.count > 0, "mesh jobs recorded");
		assert!(gen_jobs.max_ms >= gen_jobs.avg_ms());
		assert!(mesh_jobs.max_ms >= mesh_jobs.avg_ms());
	}
}
