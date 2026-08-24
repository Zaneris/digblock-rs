//! Construction and the overlay wiring. `with_overlay` is what keeps
//! an `EditOverlay` in multiplayer too -- there it MIRRORS the server's
//! journal rather than recording the client's guesses.

use super::*;
use crate::worldgen::Seed;

impl Pipeline {
	pub fn new(seed: Seed, draw_dist: i32, workers: usize) -> Self {
		Self::build(seed, draw_dist, workers, None)
	}

	/// A pipeline with a persistent edit overlay (single-player worlds):
	/// pass the saved world's edits (empty for a fresh world) so even the
	/// spawn columns generate already-overlaid. Multiplayer keeps `new`.
	pub fn with_overlay(
		seed: Seed,
		draw_dist: i32,
		workers: usize,
		overlay: crate::overlay::EditOverlay,
	) -> Self {
		Self::build(seed, draw_dist, workers, Some(overlay))
	}

	pub(super) fn build(
		seed: Seed,
		draw_dist: i32,
		workers: usize,
		overlay: Option<crate::overlay::EditOverlay>,
	) -> Self {
		// No std threads on the web — force the inline executor there no
		// matter what the caller asked for.
		let workers = if cfg!(target_arch = "wasm32") {
			0
		} else {
			workers
		};
		let exec = if workers == 0 {
			Executor::Inline {
				results: VecDeque::new(),
			}
		} else {
			// Deep enough that workers never starve between once-per-frame
			// top-ups (a shallow queue capped throughput at ~queue+workers
			// jobs per frame); shallow enough that priority ordering stays
			// relevant — queued jobs can't be re-prioritized, only
			// staleness-dropped.
			let (job_tx, job_rx) = bounded::<Job>(workers * 16);
			let (result_tx, result_rx) = unbounded::<JobResult>();
			let handles = (0..workers)
				.map(|i| {
					let rx = job_rx.clone();
					let tx = result_tx.clone();
					std::thread::Builder::new()
						.name(format!("chunk-worker-{i}"))
						.spawn(move || worker_loop(rx, tx))
						.expect("spawn worker")
				})
				.collect();
			Executor::Workers {
				job_tx,
				result_rx,
				_handles: handles,
			}
		};
		Self {
			seed,
			draw_dist,
			store: FxHashMap::default(),
			exec,
			gen_heap: BinaryHeap::new(),
			mesh_heap: BinaryHeap::new(),
			center: ChunkPos::new(i32::MAX, 0, i32::MAX),
			deferred_edits: FxHashMap::default(),
			deferred_count: 0,
			record_local: overlay.is_some(),
			edits: overlay.unwrap_or_default(),
			levels: crate::worldgen::nodes::Levels::default(),
			recorded: Vec::new(),
			scanned: None,
			in_flight: 0,
			in_flight_min_d2: None,
			job_cost: JobCost::new(),
			gi_dirty: FxHashSet::default(),
			gi_edits: Vec::new(),
			gi_center: None,
		}
	}
}
