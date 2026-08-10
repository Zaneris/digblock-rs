//! Threaded chunk pipeline — replaces the Java frame-slicing loop
//! (one queue item per frame, round-robin) with a worker pool. The main
//! thread owns the store and scheduling; workers run pure functions
//! (terrain gen, meshing) over immutable snapshots.
//!
//! Staleness is handled by generation counters, not flags: every mesh job
//! carries the `mesh_gen` it was built from, and results that don't match
//! the slot's current counter (or whose slot was evicted) are dropped.
//! When a chunk finishes generating, it bumps its own and its Ready
//! neighbors' counters — face neighbors unconditionally (the safe
//! equivalent of the Java writing face bits into neighbor chunks and
//! re-dirtying them), edge/corner diagonals only where the shared padded
//! shell cells could shade an existing face.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::chunk::{is_air_chunk, ChunkData, ChunkPos, WORLD_VCHUNK};
use crate::light::{facts_of, CellFacts, LightField, LIGHT_BUDGET};
use crate::mesher::{mesh_chunk, MeshData, MeshInput, Src};
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
		/// Padded snapshot — the worker holds no Arc into the store, so
		/// `set_block`'s copy-on-write almost never actually clones.
		input: Box<MeshInput>,
	},
}

enum JobResult {
	Generated {
		x: i32,
		z: i32,
		/// One Arc per chunk, bottom-up; sky chunks share AIR_CHUNK.
		stack: Vec<Arc<ChunkData>>,
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
	/// Generation of the last accepted mesh, if that mesh was empty.
	/// Diagonal neighbors' shell cells only shade existing faces (they
	/// never create or cull them), so a chunk whose current mesh is empty
	/// — every buried chunk — can skip diagonal re-dirties entirely.
	empty_mesh_gen: Option<u64>,
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
	/// Light-BFS pops consumed this maintain (both `step_light` sites).
	pub light_pops: u32,
	/// Light queue depth left after this maintain — nonzero means the
	/// budget carried propagation over to the next frame.
	pub light_pending: u32,
	/// Water cells restored by the edit overlay in columns that became
	/// Ready this call — the caller re-schedules fluid there so a region
	/// evicted mid-flow resumes (the client twin of the server's
	/// `ReadyColumn::overlaid_water`).
	pub overlaid_water: Vec<(i32, i32, i32)>,
}

pub struct Pipeline {
	seed: f64,
	draw_dist: i32,
	store: FxHashMap<ChunkPos, ChunkSlot>,
	exec: Executor,
	/// Pending columns (x, z) — generation is per-column, not per-chunk.
	gen_heap: BinaryHeap<(Reverse<i64>, (i32, i32))>,
	mesh_heap: BinaryHeap<(Reverse<i64>, (i32, i32, i32))>,
	center: ChunkPos,
	/// Authoritative edits waiting for their chunk to generate — see
	/// `apply_authoritative`.
	///
	/// Grouped by chunk COLUMN, because that is the granularity they are
	/// released at. Joining a heavily-edited server defers most of the
	/// journal (everything outside the generated radius) and holds it for
	/// the whole session; a flat map meant every column that landed
	/// rescanned all of it, on every streaming frame.
	deferred_edits: FxHashMap<(i32, i32), DeferredCells>,
	/// Running total across the grouped map, so reporting it stays O(1).
	deferred_count: usize,
	/// Persistent local-edit overlay (single-player worlds): every
	/// `set_block` records here and every generated column re-applies its
	/// cells, so hard eviction can't lose edits. None in multiplayer —
	/// the server's journal owns that world.
	overlay: Option<crate::overlay::EditOverlay>,
	/// Edits recorded since the last `take_recorded_edits` drain (the
	/// engine forwards them to the world store).
	recorded: Vec<(i32, i32, i32, u16)>,
	/// Centre and radius the resident set was last built for; None forces a
	/// full rebuild. Both are compared, so a draw-distance change rescans
	/// even when the player hasn't moved.
	scanned: Option<(ChunkPos, i32)>,
	in_flight: u32,
	/// Inline job-cost predictor (unused with a worker pool).
	job_cost: JobCost,
	/// Stage-B voxel light (LIGHT-DESIGN.md) — a sibling of the store, not
	/// a `ChunkSlot` field, so the BFS can read blocks while writing light
	/// and so shared AIR_CHUNK sentinels still carry per-cell light.
	light: LightField,
	/// Chunks whose GI-window region is stale (RTGI; GI-DESIGN.md):
	/// arrivals and edits land here, the engine drains via
	/// `take_gi_dirty` under its upload budget. Tracked unconditionally —
	/// a set insert is noise, and it stays bounded by the resident set
	/// (drained or not) because it IS a set.
	gi_dirty: FxHashSet<ChunkPos>,
}

/// How jobs run: a worker-thread pool (native), or synchronously inside
/// `maintain` (`workers == 0` — the wasm main thread, and a native debug
/// mode via `--workers 0`). Selected at runtime so the inline path is
/// testable and benchable everywhere.
enum Executor {
	Inline {
		/// Results of inline-executed jobs waiting for `drain_results`
		/// (the drain's `upload_budget`/deadline can leave leftovers).
		results: VecDeque<JobResult>,
	},
	Workers {
		job_tx: Sender<Job>,
		result_rx: Receiver<JobResult>,
		_handles: Vec<std::thread::JoinHandle<()>>,
	},
}

impl Executor {
	fn try_recv(&mut self) -> Option<JobResult> {
		match self {
			Executor::Inline { results } => results.pop_front(),
			Executor::Workers { result_rx, .. } => result_rx.try_recv().ok(),
		}
	}

	fn has_pending(&self) -> bool {
		match self {
			Executor::Inline { results } => !results.is_empty(),
			Executor::Workers { result_rx, .. } => !result_rx.is_empty(),
		}
	}
}

/// Per-`maintain` floor for the inline executor: one `Generate` ends the
/// batch immediately (a column is ~16 chunks of uninterruptible work);
/// otherwise up to this many mesh jobs run. A count, not a wall-clock
/// deadline, because job granularity is what actually bounds the stall.
/// With an `inline_allowance`, more jobs may follow (see
/// `run_inline_jobs`); this floor always runs regardless.
const INLINE_MESH_CAP: usize = 4;

/// Worst-of-recent window for inline job-cost prediction.
const COST_N: usize = 8;

/// Fraction of the lifetime worst that survives as a floor under the
/// recent window. Admission control needs a PESSIMISTIC bound, not a
/// recent average: gen cost is bimodal (a flat ocean column is ~10× cheaper
/// than a mountain one), so `COST_N` cheap samples in a row would otherwise
/// collapse the prediction and admit a job an order of magnitude bigger
/// than the remaining budget. Half, not all, so a genuinely one-off
/// outlier (a scheduler preemption, a resize stall) decays out instead of
/// suppressing streaming for the rest of the run.
const LIFETIME_FLOOR_FRAC: f32 = 0.5;

/// Predicts an inline job's cost as the worst of its kind's last
/// `COST_N` measured durations, floored by a share of its lifetime worst.
/// The pessimistic seed participates until the ring fills, so extra jobs
/// never start on optimistic early samples.
struct CostRing {
	recent: [f32; COST_N],
	len: usize,
	head: usize,
	seed_ms: f32,
	/// Worst ever observed — the bimodality guard (see
	/// `LIFETIME_FLOOR_FRAC`).
	lifetime_max_ms: f32,
}

impl CostRing {
	fn new(seed_ms: f32) -> Self {
		Self {
			recent: [0.0; COST_N],
			len: 0,
			head: 0,
			seed_ms,
			lifetime_max_ms: 0.0,
		}
	}

	fn observe(&mut self, ms: f32) {
		self.recent[self.head] = ms;
		self.head = (self.head + 1) % COST_N;
		self.len = (self.len + 1).min(COST_N);
		self.lifetime_max_ms = self.lifetime_max_ms.max(ms);
	}

	fn predicted_ms(&self) -> f32 {
		let worst = self.recent[..self.len]
			.iter()
			.fold(0.0f32, |a, &b| a.max(b))
			.max(self.lifetime_max_ms * LIFETIME_FLOOR_FRAC);
		if self.len < COST_N {
			worst.max(self.seed_ms)
		} else {
			worst
		}
	}
}

struct JobCost {
	gen: CostRing,
	mesh: CostRing,
}

impl JobCost {
	fn new() -> Self {
		Self {
			gen: CostRing::new(12.0),
			mesh: CostRing::new(3.0),
		}
	}

	fn observe(&mut self, is_gen: bool, ms: f32) {
		let ring = if is_gen {
			&mut self.gen
		} else {
			&mut self.mesh
		};
		ring.observe(ms);
	}

	fn predicted(&self, is_gen: bool) -> std::time::Duration {
		let ring = if is_gen { &self.gen } else { &self.mesh };
		std::time::Duration::from_secs_f32(ring.predicted_ms() / 1000.0)
	}
}

fn dist2(a: ChunkPos, b: ChunkPos) -> i64 {
	let (dx, dy, dz) = ((a.x - b.x) as i64, (a.y - b.y) as i64, (a.z - b.z) as i64);
	dx * dx + dy * dy + dz * dz
}

/// Authoritative edits held for one chunk column until it generates.
type DeferredCells = FxHashMap<(i32, i32, i32), u16>;

/// Chunks stay resident this far past the draw distance before eviction.
///
/// Crossing one chunk boundary shifts the whole disc by one, so without a
/// margin a player pacing back and forth over a single line evicts a
/// crescent and immediately regenerates it, forever. One chunk of slack
/// removes that churn for the price of the rim band's memory (~10% more
/// resident columns at draw 20). Adds still use the exact draw distance —
/// only eviction is lazy.
const KEEP_MARGIN: i32 = 1;

/// Greatest |dx| with `dx² + dz² < r²`, or None if the row misses the disc.
fn row_half_width(dz: i32, r: i32) -> Option<i32> {
	let lim = i64::from(r) * i64::from(r) - i64::from(dz) * i64::from(dz);
	if lim <= 0 {
		return None;
	}
	// Largest s with s² < lim (the disc predicate is a strict `<`). The
	// float sqrt is only a seed — the two correction loops make the result
	// exact, so a rounding error near a perfect square can't widen or
	// narrow a row by one column.
	let mut s = ((lim - 1) as f64).sqrt() as i64;
	while s > 0 && s * s >= lim {
		s -= 1;
	}
	while (s + 1) * (s + 1) < lim {
		s += 1;
	}
	Some(s as i32)
}

fn row_span(cx: i32, half: Option<i32>) -> Option<(i32, i32)> {
	half.map(|w| (cx - w, cx + w))
}

/// The at-most-two stretches of `[a, b]` that `other` does not cover.
/// Empty stretches come back as `a > b`, so `for x in a..=b` skips them.
fn interval_diff(a: i32, b: i32, other: Option<(i32, i32)>) -> [(i32, i32); 2] {
	match other {
		None => [(a, b), (1, 0)],
		Some((oa, ob)) => [(a, b.min(oa - 1)), (a.max(ob + 1), b)],
	}
}

/// For each diagonal chunk offset, whether this chunk's cells visible in
/// that neighbor's padded shell (a shared edge line or corner cell)
/// contain anything non-air. All-air shared cells read identically to the
/// zero-filled shell of a missing chunk, so such neighbors' existing
/// meshes cannot be affected by this chunk's arrival. Face offsets are
/// left false — face re-dirties are unconditional and never consult this.
/// Indexed [((oy+1)*3 + (oz+1))*3 + (ox+1)]; ~200 byte reads total.
fn shell_affects(data: &ChunkData) -> [bool; 27] {
	// The local cell range this chunk contributes to a neighbor at axis
	// offset o (the neighbor sees us at -o): our high border row for +1,
	// our low row for -1.
	let span = |o: i32| match o {
		1 => 15..16usize,
		-1 => 0..1,
		_ => 0..16,
	};
	let mut out = [false; 27];
	for oy in -1..=1i32 {
		for oz in -1..=1i32 {
			for ox in -1..=1i32 {
				let diagonal = (ox != 0) as u8 + (oy != 0) as u8 + (oz != 0) as u8 >= 2;
				if !diagonal {
					continue;
				}
				let i = (((oy + 1) * 3 + (oz + 1)) * 3 + (ox + 1)) as usize;
				out[i] = 'scan: {
					for y in span(oy) {
						for z in span(oz) {
							for x in span(ox) {
								if data.blocks[(y * 16 + z) * 16 + x] != 0 {
									break 'scan true;
								}
							}
						}
					}
					false
				};
			}
		}
	}
	out
}

impl Pipeline {
	pub fn new(seed: f64, draw_dist: i32, workers: usize) -> Self {
		Self::build(seed, draw_dist, workers, None)
	}

	/// A pipeline with a persistent edit overlay (single-player worlds):
	/// pass the saved world's edits (empty for a fresh world) so even the
	/// spawn columns generate already-overlaid. Multiplayer keeps `new`.
	pub fn with_overlay(
		seed: f64,
		draw_dist: i32,
		workers: usize,
		overlay: crate::overlay::EditOverlay,
	) -> Self {
		Self::build(seed, draw_dist, workers, Some(overlay))
	}

	fn build(
		seed: f64,
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
			overlay,
			recorded: Vec::new(),
			scanned: None,
			in_flight: 0,
			job_cost: JobCost::new(),
			light: LightField::new(),
			gi_dirty: FxHashSet::default(),
		}
	}

	/// Edits recorded since the last drain (single-player persistence);
	/// empty when no overlay is installed.
	pub fn take_recorded_edits(&mut self) -> Vec<(i32, i32, i32, u16)> {
		std::mem::take(&mut self.recorded)
	}

	/// The persistent edit overlay, when one is installed.
	pub fn overlay(&self) -> Option<&crate::overlay::EditOverlay> {
		self.overlay.as_ref()
	}

	/// Call every frame with the player's chunk position. Performs the
	/// visibility scan on chunk-boundary crossings, tops up workers, and
	/// drains results (mesh outputs capped at `upload_budget`; the whole
	/// drain stops at `deadline` — undrained results wait in the channel,
	/// so worker throughput is unaffected, only main-thread pacing).
	/// `inline_allowance` lets the inline executor run extra jobs beyond
	/// its fixed floor while their predicted cost fits (the adaptive web
	/// budget); `None` keeps the floor-only batch. Ignored with workers.
	pub fn maintain(
		&mut self,
		center: ChunkPos,
		upload_budget: usize,
		deadline: Option<web_time::Instant>,
		inline_allowance: Option<std::time::Duration>,
	) -> Maintenance {
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
			overlaid_water: Vec::new(),
			light_pops: 0,
			light_pending: 0,
		};
		let center = ChunkPos::new(center.x, 0, center.z);
		if self.scanned != Some((center, self.draw_dist)) {
			self.center = center;
			self.visibility_scan(&mut out.evicted);
			// Evicted chunks drop their light — the field is derived and
			// re-entry recomputes it via arrival seeding.
			for &pos in &out.evicted {
				self.light.drop_chunk(pos);
			}
		}
		// Settle-before-dispatch, site 1: edits since the last maintain
		// propagate before the inline executor dispatches (and snapshots)
		// this frame's mesh jobs.
		out.light_pops += self.step_light();
		// Inline jobs run before the drain so their results land this
		// frame; workers instead get topped up after (they produce across
		// frames anyway).
		//
		// The deadline is deliberately NOT extended by what the jobs spent.
		// It used to be, because a column-gen job could outlast the whole
		// stream budget on its own and leave the drain nothing — but paying
		// for that by handing the drain a fresh full budget made the stages
		// ADD UP (jobs + drain + uploads ≈ 1.5× budget plus overshoot),
		// which is the miss the budget exists to avoid. Job slicing bounds
		// the overrun at the source now, and each stage keeps a
		// minimum-progress floor (the drain always pulls one result, the
		// upload loop always does `UPLOAD_MIN`), so a blown deadline delays
		// work without ever starving it.
		if matches!(self.exec, Executor::Inline { .. }) {
			self.run_inline_jobs(inline_allowance);
		}
		self.drain_results(&mut out, upload_budget, deadline);
		// Settle-before-dispatch, site 2: arrival seeds from the drain
		// propagate before the workers snapshot their next batch.
		out.light_pops += self.step_light();
		out.light_pending = self.light.pending() as u32;
		out.saturated = self.exec.has_pending();
		out.requeued = self.top_up_workers();
		out.gen_queue = self.gen_heap.len() as u32;
		out.mesh_queue = self.mesh_heap.len() as u32;
		out.in_flight = self.in_flight;
		out
	}

	/// Desired set: y ∈ 0..WORLD_VCHUNK, Euclidean distance < draw_dist in
	/// chunk units — the Java updateVisible cull. Chunks leaving the set
	/// are dropped (results in flight are rejected by the slot check).
	///
	/// Ordinary movement takes the incremental path: crossing one chunk
	/// boundary shifts the disc by one, which changes O(r) columns out of
	/// O(r²), and this runs OUTSIDE the streaming deadline — a full rescan
	/// here was an unbudgeted multi-millisecond burst arriving at walking
	/// cadence, every 16 blocks.
	fn visibility_scan(&mut self, evicted: &mut Vec<ChunkPos>) {
		let r = self.draw_dist;
		let c = self.center;
		match self.scanned {
			Some((pc, pr)) if pr == r => self.visibility_diff(pc, c, r, evicted),
			// First scan, or the draw distance itself changed. Rebuild
			// exactly — and with no hysteresis band, because draw distance
			// is a memory/perf setting the user just chose and must be
			// honoured to the chunk.
			_ => self.visibility_rebuild(c, r, evicted),
		}
		self.scanned = Some((c, r));
		// Pending meshes are deliberately NOT re-prioritized for the new
		// centre: heap keys are priorities, not correctness, they drift by
		// at most one chunk per crossing, and `next_mesh_job` already drops
		// entries whose chunk left the store. Draining and rebuilding the
		// whole heap to reorder it cost more than the misordering ever did.
	}

	/// Full rebuild: exact membership, no hysteresis.
	fn visibility_rebuild(&mut self, c: ChunkPos, r: i32, evicted: &mut Vec<ChunkPos>) {
		let r2 = i64::from(r) * i64::from(r);
		self.store.retain(|pos, _| {
			let keep = dist2(ChunkPos::new(pos.x, 0, pos.z), c) < r2;
			if !keep {
				evicted.push(*pos);
			}
			keep
		});
		for z in (c.z - r)..=(c.z + r) {
			let Some(w) = row_half_width(z - c.z, r) else {
				continue;
			};
			for x in (c.x - w)..=(c.x + w) {
				self.add_column(x, z);
			}
		}
	}

	/// Incremental update between two centres at the same radius: only the
	/// entering and leaving crescents are touched.
	fn visibility_diff(&mut self, pc: ChunkPos, c: ChunkPos, r: i32, evicted: &mut Vec<ChunkPos>) {
		if pc == c {
			return;
		}
		// Leaving: in the old KEEP disc, out of the new one.
		let keep = r + KEEP_MARGIN;
		for z in (pc.z - keep)..=(pc.z + keep) {
			let Some(w) = row_half_width(z - pc.z, keep) else {
				continue;
			};
			let now = row_span(c.x, row_half_width(z - c.z, keep));
			for (a, b) in interval_diff(pc.x - w, pc.x + w, now) {
				for x in a..=b {
					for y in 0..WORLD_VCHUNK {
						let pos = ChunkPos::new(x, y, z);
						if self.store.remove(&pos).is_some() {
							evicted.push(pos);
						}
					}
				}
			}
		}
		// Entering: in the new desired disc, out of the old one. Columns
		// still resident from the hysteresis band land here too and are
		// no-ops — which is the entire point of keeping them.
		for z in (c.z - r)..=(c.z + r) {
			let Some(w) = row_half_width(z - c.z, r) else {
				continue;
			};
			let before = row_span(pc.x, row_half_width(z - pc.z, r));
			for (a, b) in interval_diff(c.x - w, c.x + w, before) {
				for x in a..=b {
					self.add_column(x, z);
				}
			}
		}
	}

	/// Ensure a column's chunk slots exist; queues generation if any were
	/// missing. Idempotent, so re-entering a column that never left is free.
	fn add_column(&mut self, x: i32, z: i32) {
		let mut fresh = false;
		for y in 0..WORLD_VCHUNK {
			let pos = ChunkPos::new(x, y, z);
			if let std::collections::hash_map::Entry::Vacant(e) = self.store.entry(pos) {
				e.insert(ChunkSlot {
					state: ChunkState::Generating,
					mesh_gen: 0,
					submitted_gen: 0,
					empty_mesh_gen: None,
				});
				fresh = true;
			}
		}
		if fresh {
			let d2 = dist2(ChunkPos::new(x, 0, z), self.center);
			self.gen_heap.push((Reverse(d2), (x, z)));
		}
	}

	fn drain_results(
		&mut self,
		out: &mut Maintenance,
		upload_budget: usize,
		deadline: Option<web_time::Instant>,
	) {
		let mut landed: Vec<(i32, i32)> = Vec::new();
		let mut pulled = 0usize;
		while out.meshed.len() < upload_budget {
			// Generated results are the expensive drains (stack insert plus
			// neighbor re-mesh cascade), so the deadline gates every pull,
			// not just meshed ones — but never the first: at least one
			// result must land per frame or an over-budget frame starves
			// the drain while jobs keep producing (the player would fall
			// through a world that exists but never reaches the store).
			if pulled > 0 && deadline.is_some_and(|d| web_time::Instant::now() >= d) {
				break;
			}
			let Some(result) = self.exec.try_recv() else {
				break;
			};
			pulled += 1;
			self.in_flight = self.in_flight.saturating_sub(1);
			match result {
				JobResult::Generated {
					x,
					z,
					mut stack,
					dur_ms,
				} => {
					out.gen_jobs.record(dur_ms);
					// Re-apply persistent edits BEFORE the per-chunk air
					// classification below: a block placed in a sky chunk
					// must defeat `is_air_chunk` or it would never mesh.
					// `Arc::make_mut` clones the shared AIR_CHUNK sentinel
					// rather than mutating it (refcount > 1), mirroring the
					// server's `ChunkStore::drain` overlay.
					if let Some(overlay) = &self.overlay {
						if let Some(cells) = overlay.column(x, z) {
							for (&(wx, wy, wz), &cell) in cells {
								let Some(((_, cy, _), (lx, ly, lz))) =
									crate::chunk::split_world(wx, wy, wz)
								else {
									continue;
								};
								Arc::make_mut(&mut stack[cy as usize]).set_cell(lx, ly, lz, cell);
								if crate::block::BlockType::from_id(cell).is_water() {
									out.overlaid_water.push((wx, wy, wz));
								}
							}
						}
					}
					landed.push((x, z));
					// Arrival light seeding reads the stack AFTER the
					// overlay re-applied (a restored torch must be in the
					// scanned data); Arc clones are pointer-cheap. Air
					// chunks ride along for the skylight descend (they
					// become Uniform full-sky tags).
					let seed_stack: Vec<(i32, Arc<ChunkData>)> = stack
						.iter()
						.enumerate()
						.map(|(cy, d)| (cy as i32, d.clone()))
						.collect();
					self.light.seed_column(x, z, &seed_stack);
					// Every arriving chunk dirties its GI-window region —
					// air ones included, since the toroidal window may hold
					// a stale previous tenant there (GI-DESIGN.md).
					for cy in 0..crate::chunk::WORLD_VCHUNK {
						self.gi_dirty.insert(ChunkPos::new(x, cy, z));
					}
					let c = self.center;
					let d = dist2(ChunkPos::new(x, 0, z), c);
					for (cy, data) in stack.into_iter().enumerate() {
						let pos = ChunkPos::new(x, cy as i32, z);
						out.generated += 1;
						// Air chunks are stored (uniform lookups) but never
						// meshed — they can't emit faces.
						let air = is_air_chunk(&data);
						let affects = if air {
							[false; 27]
						} else {
							shell_affects(&data)
						};
						if let Some(slot) = self.store.get_mut(&pos) {
							slot.state = ChunkState::Ready(data);
							if !air {
								slot.mesh_gen += 1;
								self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
							}
						}
						// The Rust equivalent of Java's cross-border re-dirty:
						// every Ready neighbor re-meshes from fresh snapshots.
						// Diagonals only shade existing faces via the padded
						// shell, so they're skipped for air chunks (no shell
						// content) — without this the buried bulk of the
						// world remeshes 26-ways on every generation.
						//
						// The empty-mesh and queued-remesh skips below apply
						// to FACES too, not just diagonals. Both are sound
						// specifically because this is the ARRIVAL path: the
						// shell here goes from all-air (a missing neighbour
						// reads as zero) to this chunk's real cells, so
						// coverage only ever INCREASES. A face is emitted
						// where a solid cell meets a non-covering neighbour,
						// so more coverage can only cull faces, never create
						// them — a neighbour that already meshed to nothing
						// stays nothing. (This does NOT hold for edits, where
						// digging removes coverage; `set_cell` has its own
						// unconditional bump.)
						for oy in -1..=1 {
							for oz in -1..=1i32 {
								for ox in -1..=1i32 {
									if (ox, oy, oz) == (0, 0, 0) {
										continue;
									}
									let diagonal =
										(ox != 0) as u8 + (oy != 0) as u8 + (oz != 0) as u8 >= 2;
									// All-air shared cells (or an all-air
									// chunk) can't affect a diagonal
									// neighbor's mesh.
									if diagonal
										&& !affects
											[(((oy + 1) * 3 + (oz + 1)) * 3 + (ox + 1)) as usize]
									{
										continue;
									}
									let np = ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz);
									if let Some(nslot) = self.store.get_mut(&np) {
										if !matches!(&nslot.state, ChunkState::Ready(d) if !is_air_chunk(d))
										{
											continue;
										}
										if nslot.empty_mesh_gen == Some(nslot.mesh_gen) {
											continue;
										}
										// A queued-but-undispatched remesh
										// snapshots at dispatch time, after
										// this chunk landed in the store — it
										// already sees the fresh shell, so
										// bumping again would only duplicate
										// the job.
										if nslot.submitted_gen < nslot.mesh_gen {
											continue;
										}
										nslot.mesh_gen += 1;
										self.mesh_heap.push((
											Reverse(dist2(
												ChunkPos::new(np.x, 0, np.z),
												self.center,
											)),
											(np.x, np.y, np.z),
										));
									}
								}
							}
						}
					}
				}
				JobResult::Meshed { out: m, dur_ms } => {
					out.mesh_jobs.record(dur_ms);
					match self.store.get_mut(&m.pos) {
						// Only the newest generation may reach the GPU.
						Some(slot) if slot.mesh_gen == m.gen => {
							slot.empty_mesh_gen = m.mesh.is_empty().then_some(m.gen);
							out.meshed.push(m);
						}
						_ => out.stale_dropped += 1, // stale or evicted
					}
				}
			}
		}
		self.apply_deferred_for(&landed);
	}

	/// Nearest-first across both heaps; generation wins ties so terrain
	/// exists before meshes want it. Loops past stale heap entries; None
	/// means both heaps are exhausted.
	fn pop_next_job(&mut self) -> Option<Job> {
		loop {
			let gen_d = self.gen_heap.peek().map(|(Reverse(d), _)| *d);
			let mesh_d = self.mesh_heap.peek().map(|(Reverse(d), _)| *d);
			let job = match (gen_d, mesh_d) {
				(None, None) => return None,
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
			if let Some(job) = job {
				return Some(job);
			}
		}
	}

	/// Inline executor: run the unconditional floor batch (one `Generate`
	/// or up to `INLINE_MESH_CAP` meshes), then — with an allowance —
	/// keep starting jobs while their predicted cost still fits. Jobs are
	/// uninterruptible, so prediction is the only overshoot control; a
	/// popped job that doesn't fit is requeued (one wasted `MeshInput`
	/// build at most). Results queue for `drain_results`, which applies the
	/// caller's shared deadline.
	fn run_inline_jobs(&mut self, allowance: Option<std::time::Duration>) {
		let start = web_time::Instant::now();
		let mut meshes = 0usize;
		let mut floor_done = false;
		loop {
			if floor_done && allowance.is_none() {
				break;
			}
			let Some(job) = self.pop_next_job() else {
				break;
			};
			let is_gen = matches!(job, Job::Generate { .. });
			if floor_done {
				let fits = allowance
					.is_some_and(|a| start.elapsed() + self.job_cost.predicted(is_gen) <= a);
				if !fits {
					self.requeue(job);
					break;
				}
			}
			let result = run_job(job);
			let dur_ms = match &result {
				JobResult::Generated { dur_ms, .. } | JobResult::Meshed { dur_ms, .. } => *dur_ms,
			};
			self.job_cost.observe(is_gen, dur_ms);
			self.in_flight += 1;
			let Executor::Inline { results } = &mut self.exec else {
				unreachable!("run_inline_jobs is only called on Inline")
			};
			results.push_back(result);
			if is_gen {
				floor_done = true;
			} else {
				meshes += 1;
				floor_done |= meshes >= INLINE_MESH_CAP;
			}
		}
	}

	/// Returns the number of jobs bounced back because workers were
	/// saturated (0 or 1 — a bounce ends the loop). No-op on the inline
	/// executor (its jobs ran in `run_inline_jobs`).
	fn top_up_workers(&mut self) -> u32 {
		if matches!(self.exec, Executor::Inline { .. }) {
			return 0;
		}
		while let Some(job) = self.pop_next_job() {
			let Executor::Workers { job_tx, .. } = &self.exec else {
				unreachable!("checked above")
			};
			match job_tx.try_send(job) {
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
		if is_air_chunk(data) {
			return None; // sky chunk: nothing to mesh
		}
		if slot.mesh_gen <= slot.submitted_gen {
			return None; // an equal-or-newer job is already in flight
		}
		let gen = slot.mesh_gen;
		let input = Box::new(self.build_mesh_input(pos, data));
		let slot = self.store.get_mut(&pos).expect("slot exists");
		slot.submitted_gen = gen;
		Some(Job::Mesh { pos, gen, input })
	}

	fn build_mesh_input(&self, pos: ChunkPos, center: &ChunkData) -> MeshInput {
		let mut input = MeshInput::build(center, |dx, dy, dz| {
			let np = ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
			if np.y >= WORLD_VCHUNK {
				return Src::Air; // Java air sentinel above the world
			}
			if np.y < 0 {
				return Src::Missing; // Java null below
			}
			match self.store.get(&np) {
				Some(ChunkSlot {
					state: ChunkState::Ready(d),
					..
				}) => {
					if is_air_chunk(d) {
						Src::Air // skip the shell copy
					} else {
						Src::Chunk(d)
					}
				}
				_ => Src::Missing,
			}
		});
		// The light shell rides the same snapshot: already-settled field
		// values only (maintain steps the BFS before any dispatch).
		input.fill_light(
			|dx, dy, dz| {
				self.light
					.chunk(ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz))
			},
			|dx, dy, dz| {
				self.light
					.bounce_chunk(ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz))
			},
		);
		input
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

	/// Change the streaming radius at runtime (settings slider). The next
	/// `maintain` sees a radius that doesn't match `scanned` and rebuilds
	/// exactly: a shrink evicts out-of-range columns (with no hysteresis
	/// band — the user asked for this radius), a grow enqueues the new ring
	/// nearest-first under the usual stream budget.
	pub fn set_draw_dist(&mut self, draw_dist: i32) {
		self.draw_dist = draw_dist;
	}

	/// Ceiling scale for the adaptive stream budget
	/// (`stream_budget::urgency` of the nearest pending job): 1 = work
	/// at the player, 0 = draw-distance rim churn or nothing pending.
	/// Both heaps are keyed by flat d² from center so this is a peek; a
	/// stale entry (column evicted or completed since queueing) can only
	/// overestimate urgency, and only until a pop consumes it.
	pub fn stream_urgency(&self) -> f32 {
		let gen = self.gen_heap.peek().map(|(Reverse(d), _)| *d);
		let mesh = self.mesh_heap.peek().map(|(Reverse(d), _)| *d);
		let Some(d2) = gen.into_iter().chain(mesh).min() else {
			return 0.0;
		};
		crate::stream_budget::urgency((d2 as f32).sqrt(), self.draw_dist as f32)
	}

	/// Run the light BFS for one budget slice, then convert its dirty set
	/// into remesh bumps. Both settle-before-dispatch sites call this, so
	/// a mesh snapshot is never built from a half-propagated field.
	fn step_light(&mut self) -> u32 {
		let Pipeline { light, store, .. } = self;
		let pops = light.step(LIGHT_BUDGET, |x, y, z| -> Option<CellFacts> {
			let ((cx, cy, cz), (lx, ly, lz)) = crate::chunk::split_world(x, y, z)?;
			match store.get(&ChunkPos::new(cx, cy, cz)) {
				Some(ChunkSlot {
					state: ChunkState::Ready(d),
					..
				}) => Some(facts_of(d.get_cell(lx, ly, lz))),
				_ => None, // not resident: a wall until arrival seeding
			}
		});
		// Convert light writes into remesh bumps only once the field has
		// SETTLED: draining per budget slice re-bumped the same chunks
		// every maintain during mass streaming (+21% mesh jobs on the
		// bench). Dispatch order is unaffected — a queued mesh snapshots
		// at dispatch and the settle-before-dispatch sites run first.
		if self.light.pending() == 0 {
			self.apply_light_dirty();
		}
		pops
	}

	/// Bump `mesh_gen` for chunks whose padded shell saw a light write.
	/// Reuses the arrival path's two skips: an empty mesh stays empty
	/// (light shades faces, never creates them), and a queued job
	/// snapshots at dispatch, after the field settled.
	fn apply_light_dirty(&mut self) {
		let Pipeline {
			light,
			store,
			mesh_heap,
			center,
			..
		} = self;
		for pos in light.take_dirty() {
			let Some(slot) = store.get_mut(&pos) else {
				continue;
			};
			if !matches!(&slot.state, ChunkState::Ready(d) if !is_air_chunk(d)) {
				continue;
			}
			if slot.empty_mesh_gen == Some(slot.mesh_gen) {
				continue;
			}
			if slot.submitted_gen < slot.mesh_gen {
				continue;
			}
			slot.mesh_gen += 1;
			mesh_heap.push((
				Reverse(dist2(ChunkPos::new(pos.x, 0, pos.z), *center)),
				(pos.x, pos.y, pos.z),
			));
		}
	}

	/// Blocklight at a world cell — the mesher's shell builder and the
	/// tests read the field through this.
	pub fn light_at(&self, x: i32, y: i32, z: i32) -> u8 {
		self.light.block_light(x, y, z)
	}

	/// Skylight at a world cell.
	pub fn sky_at(&self, x: i32, y: i32, z: i32) -> u8 {
		self.light.sky_light(x, y, z)
	}

	/// Drain the chunks whose GI-window region is stale (RTGI). The
	/// caller owns budgeting: it takes everything and queues the excess,
	/// so a burst (arrival column, cold fill) never re-dirties here.
	pub fn take_gi_dirty(&mut self) -> Vec<ChunkPos> {
		self.gi_dirty.drain().collect()
	}

	/// Classify one resident chunk for the GI window. None = not (or no
	/// longer) resident — the caller drops the entry; if the region ever
	/// becomes visible again it arrives, and arriving re-dirties it.
	pub fn classify_gi(&self, pos: ChunkPos) -> Option<crate::gi::GiChunk> {
		match &self.store.get(&pos)?.state {
			ChunkState::Ready(data) => Some(crate::gi::classify_chunk(data)),
			_ => None,
		}
	}

	/// Re-dirty every resident chunk's GI-window region (device-loss
	/// rebuild: the fresh window is empty and all GI data is derived).
	pub fn gi_redirty_all(&mut self) {
		self.gi_dirty.extend(self.store.keys().copied());
	}

	/// Force a re-mesh of every resident Ready chunk (device-loss drill).
	pub fn remesh_all(&mut self) {
		let center = self.center;
		for (pos, slot) in self.store.iter_mut() {
			if matches!(&slot.state, ChunkState::Ready(d) if !is_air_chunk(d)) {
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

	/// Set one block, remeshing the chunk and every Ready neighbor whose
	/// padded shell can see the edited cell (faces, and the edge/corner
	/// diagonals at touched borders). Returns the previous block; None if
	/// outside the world, the chunk isn't Ready, or the edit is a no-op.
	/// In-flight meshes of the old data are dropped by the generation
	/// check in `drain_results`.
	pub fn set_block(
		&mut self,
		x: i32,
		y: i32,
		z: i32,
		b: crate::block::BlockType,
	) -> Option<crate::block::BlockType> {
		self.set_cell(x, y, z, b as u16)
			.map(|prev| crate::block::BlockType::from_id(crate::chunk::cell_id(prev)))
	}

	/// Cell-level twin of [`set_block`]: writes the full u16 (id + state
	/// nibble) and returns the previous cell. The single local-write
	/// funnel — dig, place, fluid, and shaped-block placement all land
	/// here, so the overlay (and its persistence drain) sees them all.
	pub fn set_cell(&mut self, x: i32, y: i32, z: i32, cell: u16) -> Option<u16> {
		let ((cx, cy, cz), (lx, ly, lz)) = crate::chunk::split_world(x, y, z)?;
		let pos = ChunkPos::new(cx, cy, cz);
		let slot = self.store.get_mut(&pos)?;
		let ChunkState::Ready(data) = &mut slot.state else {
			return None;
		};
		let prev = data.get_cell(lx, ly, lz);
		if prev == cell {
			return None;
		}
		// COW: clones the 4KB array only if a worker still holds a snapshot.
		Arc::make_mut(data).set_cell(lx, ly, lz, cell);
		// The light hook rides the single write funnel, so dig, place,
		// fluid, and authoritative edits all feed the field. Propagation
		// itself waits for the next maintain's `step_light`.
		self.light.on_cell_changed(x, y, z, prev, cell);
		// The GI voxel window re-uploads this chunk (RTGI; GI-DESIGN.md).
		// Chunk-granular: the window has no padded shell, so an edit
		// dirties exactly its own 16³ region.
		self.gi_dirty.insert(pos);
		if let Some(overlay) = &mut self.overlay {
			overlay.record(x, y, z, cell);
			self.recorded.push((x, y, z, cell));
		}
		slot.mesh_gen += 1;
		let center = self.center;
		self.mesh_heap.push((
			Reverse(dist2(ChunkPos::new(pos.x, 0, pos.z), center)),
			(pos.x, pos.y, pos.z),
		));
		// Every neighbor whose padded shell can see the edited cell: the
		// face across each touched border, plus the edge/corner diagonals
		// where two or three borders meet (their AO / corner data reads
		// this cell). Face edit → 1 neighbor, edge cell → 3, corner → 7.
		let offsets = |l: usize| -> &'static [i32] {
			match l {
				0 => &[0, -1],
				15 => &[0, 1],
				_ => &[0],
			}
		};
		for &oy in offsets(ly) {
			for &oz in offsets(lz) {
				for &ox in offsets(lx) {
					if (ox, oy, oz) == (0, 0, 0) {
						continue;
					}
					let np = ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz);
					if let Some(nslot) = self.store.get_mut(&np) {
						if matches!(nslot.state, ChunkState::Ready(_)) {
							nslot.mesh_gen += 1;
							self.mesh_heap.push((
								Reverse(dist2(ChunkPos::new(np.x, 0, np.z), center)),
								(np.x, np.y, np.z),
							));
						}
					}
				}
			}
		}
		Some(prev)
	}

	/// Block lookup for physics. None = outside the vertical world or the
	/// chunk isn't generated yet (the Java null-block: non-solid).
	pub fn block_at(&self, x: i32, y: i32, z: i32) -> Option<crate::block::BlockType> {
		self.cell_at(x, y, z)
			.map(|cell| crate::block::BlockType::from_id(crate::chunk::cell_id(cell)))
	}

	/// Full-cell lookup (id + state nibble); same loading semantics as
	/// [`Self::block_at`].
	pub fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		let ((cx, cy, cz), (lx, ly, lz)) = crate::chunk::split_world(x, y, z)?;
		match self.store.get(&ChunkPos::new(cx, cy, cz)) {
			Some(ChunkSlot {
				state: ChunkState::Ready(d),
				..
			}) => Some(d.get_cell(lx, ly, lz)),
			_ => None,
		}
	}

	/// Apply an AUTHORITATIVE block value (a server edit): unlike
	/// [`set_block`], an edit into a not-yet-generated chunk is not
	/// dropped — it is held and applied (pre-mesh) when the chunk lands,
	/// so join-sync edits racing generation can never be lost. Every
	/// world consumer gets this for free by owning a `Pipeline`, instead
	/// of re-inventing the retry map the engine used to keep.
	pub fn apply_authoritative(&mut self, x: i32, y: i32, z: i32, cell: u16) -> ApplyEdit {
		if crate::chunk::split_world(x, y, z).is_none() {
			return ApplyEdit::Rejected;
		}
		let col = (x.div_euclid(16), z.div_euclid(16));
		if self.try_apply((x, y, z), cell) {
			if let Some(cells) = self.deferred_edits.get_mut(&col) {
				if cells.remove(&(x, y, z)).is_some() {
					self.deferred_count -= 1;
				}
				if cells.is_empty() {
					self.deferred_edits.remove(&col);
				}
			}
			ApplyEdit::Applied
		} else {
			if self
				.deferred_edits
				.entry(col)
				.or_default()
				.insert((x, y, z), cell)
				.is_none()
			{
				self.deferred_count += 1;
			}
			ApplyEdit::Deferred
		}
	}

	/// Authoritative edits still waiting for their chunk to generate.
	pub fn deferred_edits(&self) -> usize {
		self.deferred_count
	}

	/// True if the cell now holds `v` (freshly written or already there).
	fn try_apply(&mut self, cell: (i32, i32, i32), v: u16) -> bool {
		self.set_cell(cell.0, cell.1, cell.2, v).is_some()
			|| self.cell_at(cell.0, cell.1, cell.2) == Some(v)
	}

	/// Deferred authoritative edits whose column just generated: apply
	/// through `set_block` (COW + remesh + border bumps) now that the
	/// chunks are Ready. Queued-but-undispatched meshes snapshot at
	/// dispatch time, so they see these writes.
	fn apply_deferred_for(&mut self, landed: &[(i32, i32)]) {
		if self.deferred_edits.is_empty() {
			return;
		}
		// O(edits in the landed columns), not O(everything still deferred).
		for col in landed {
			let Some(cells) = self.deferred_edits.remove(col) else {
				continue;
			};
			self.deferred_count -= cells.len();
			let mut unapplied = DeferredCells::default();
			for (cell, v) in cells {
				if !self.try_apply(cell, v) {
					unapplied.insert(cell, v);
				}
			}
			// A column can land with some chunks still absent (sky chunks
			// resolve to the shared air sentinel); those edits keep waiting.
			if !unapplied.is_empty() {
				self.deferred_count += unapplied.len();
				self.deferred_edits.insert(*col, unapplied);
			}
		}
	}
}

impl crate::chunk::BlockWorld for Pipeline {
	fn block_at(&self, x: i32, y: i32, z: i32) -> Option<crate::block::BlockType> {
		Pipeline::block_at(self, x, y, z)
	}

	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		Pipeline::cell_at(self, x, y, z)
	}
}

/// Outcome of [`Pipeline::apply_authoritative`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApplyEdit {
	/// The cell holds the value now.
	Applied,
	/// Chunk not generated yet: held, applied when it lands.
	Deferred,
	/// Outside the world; never applicable.
	Rejected,
}

/// The job body — two pure functions over owned snapshots. Shared by the
/// worker threads and the inline executor.
fn run_job(job: Job) -> JobResult {
	let start = web_time::Instant::now();
	match job {
		Job::Generate { x, z, seed } => {
			let stack = build_column(x, z, seed);
			JobResult::Generated {
				x,
				z,
				stack,
				dur_ms: start.elapsed().as_secs_f32() * 1000.0,
			}
		}
		Job::Mesh { pos, gen, input } => {
			let mesh = mesh_chunk(&input, (pos.x * 16, pos.y * 16, pos.z * 16));
			JobResult::Meshed {
				out: MeshOutput { pos, gen, mesh },
				dur_ms: start.elapsed().as_secs_f32() * 1000.0,
			}
		}
	}
}

fn worker_loop(rx: Receiver<Job>, tx: Sender<JobResult>) {
	while let Ok(job) = rx.recv() {
		if tx.send(run_job(job)).is_err() {
			break; // pipeline dropped
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType;

	/// Run maintain until all queues and workers are idle.
	fn quiesce(p: &mut Pipeline, center: ChunkPos) {
		for _ in 0..2000 {
			let m = p.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				return;
			}
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
		panic!("pipeline never quiesced");
	}

	fn mesh_gen(p: &Pipeline, pos: ChunkPos) -> u64 {
		p.store[&pos].mesh_gen
	}

	/// Run maintain until the light field is settled AND its dirty set
	/// has drained. `quiesce` only waits for chunk work — the budgeted
	/// BFS can still be paying down the initial skylight backlog when it
	/// returns, and light assertions made before settle are read races.
	fn settle_light(p: &mut Pipeline, center: ChunkPos) {
		for _ in 0..2000 {
			let m = p.maintain(center, usize::MAX, None, None);
			if m.light_pending == 0 && m.light_pops == 0 {
				return;
			}
		}
		panic!("light never settled");
	}

	#[test]
	fn set_block_edits_and_bumps_border_neighbors() {
		let mut p = Pipeline::new(0.0, 4, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);

		let snapshot = |p: &Pipeline| -> Vec<(ChunkPos, u64)> {
			p.store.iter().map(|(pos, s)| (*pos, s.mesh_gen)).collect()
		};

		// Pick a value different from whatever generated there.
		let other = |cur: Option<BlockType>| {
			if cur == Some(BlockType::Stone) {
				BlockType::Dirt
			} else {
				BlockType::Stone
			}
		};

		// Interior edit: only the chunk itself re-meshes.
		let before = snapshot(&p);
		let new = other(p.block_at(8, 8, 8));
		let prev = p.set_block(8, 8, 8, new).expect("edit lands");
		assert_ne!(prev, new);
		assert_eq!(p.block_at(8, 8, 8), Some(new));
		for (pos, gen) in &before {
			let expect = gen + u64::from(*pos == ChunkPos::new(0, 0, 0));
			assert_eq!(mesh_gen(&p, *pos), expect, "{pos:?}");
		}

		// No-op returns None and bumps nothing.
		let before = snapshot(&p);
		assert!(p.set_block(8, 8, 8, new).is_none());
		for (pos, gen) in &before {
			assert_eq!(mesh_gen(&p, *pos), *gen);
		}

		// Face-cell edit at world (0,8,8): local (0,8,8) touches only the
		// −x border — self + exactly 1 neighbor.
		let before = snapshot(&p);
		let new = other(p.block_at(0, 8, 8));
		p.set_block(0, 8, 8, new).expect("face edit");
		for (pos, gen) in &before {
			let bumped = *pos == ChunkPos::new(0, 0, 0) || *pos == ChunkPos::new(-1, 0, 0);
			assert_eq!(mesh_gen(&p, *pos), gen + u64::from(bumped), "{pos:?}");
		}

		// Corner-of-world edit at (0,0,0): local (0,0,0) touches −x, −z,
		// and −y borders; the y−1 chunks are outside the world, so the
		// bumped set is self + the two faces + their shared diagonal.
		let before = snapshot(&p);
		let new = other(p.block_at(0, 0, 0));
		p.set_block(0, 0, 0, new).expect("corner edit");
		for (pos, gen) in &before {
			let bumped = *pos == ChunkPos::new(0, 0, 0)
				|| *pos == ChunkPos::new(-1, 0, 0)
				|| *pos == ChunkPos::new(0, 0, -1)
				|| *pos == ChunkPos::new(-1, 0, -1);
			assert_eq!(mesh_gen(&p, *pos), gen + u64::from(bumped), "{pos:?}");
		}

		// Out-of-world y is rejected.
		assert!(p.set_block(0, -1, 0, BlockType::Stone).is_none());
		assert!(p.set_block(0, 1000, 0, BlockType::Stone).is_none());
	}

	/// The client twin of the server's
	/// `journal_overlay_survives_evict_and_regen`: with an overlay
	/// installed, a local edit survives hard eviction because the column
	/// re-applies it when it regenerates.
	#[test]
	fn overlay_survives_evict_and_regen() {
		let mut p = Pipeline::with_overlay(0.0, 4, 2, crate::overlay::EditOverlay::default());
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);

		let old = p.block_at(8, 8, 8).expect("chunk ready");
		let new = if old == BlockType::Stone {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		p.set_block(8, 8, 8, new).expect("edit lands");
		let recorded = p.take_recorded_edits();
		assert_eq!(recorded, vec![(8, 8, 8, new as u16)], "edit recorded");

		// Walk far away: the column at (0,0) hard-evicts...
		let far = ChunkPos::new(40, 0, 0);
		quiesce(&mut p, far);
		assert!(p.block_at(8, 8, 8).is_none(), "column evicted");
		// ...and coming back regenerates it WITH the edit.
		quiesce(&mut p, center);
		assert_eq!(p.block_at(8, 8, 8), Some(new), "edit survived eviction");
		// Regeneration re-applies the overlay but records nothing new.
		assert!(p.take_recorded_edits().is_empty());
	}

	/// An overlay passed at construction lands in the very first
	/// generation of its columns (a loaded save's spawn area).
	#[test]
	fn overlay_at_construction_applies_to_first_generation() {
		let mut probe = Pipeline::new(0.0, 4, 2);
		quiesce(&mut probe, ChunkPos::new(0, 0, 0));
		let old = probe.block_at(8, 8, 8).expect("ready");
		let new = if old == BlockType::Stone {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};

		let mut overlay = crate::overlay::EditOverlay::default();
		overlay.record(8, 8, 8, new as u16);
		let mut p = Pipeline::with_overlay(0.0, 4, 2, overlay);
		quiesce(&mut p, ChunkPos::new(0, 0, 0));
		assert_eq!(p.block_at(8, 8, 8), Some(new));
	}

	/// A block placed in an all-air sky chunk must defeat the air
	/// classification on regen — the overlay applies BEFORE
	/// `is_air_chunk`, so the chunk stores, meshes, and renders it.
	#[test]
	fn overlay_into_sky_chunk_defeats_air_classification() {
		let mut p = Pipeline::with_overlay(0.0, 4, 2, crate::overlay::EditOverlay::default());
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);

		// y=250 is far above any terrain: chunk (0, 15, 0) is a shared
		// AIR_CHUNK sentinel.
		let sky = ChunkPos::new(0, 15, 0);
		assert!(
			matches!(&p.store[&sky].state, ChunkState::Ready(d) if is_air_chunk(d)),
			"precondition: sky chunk is air"
		);
		p.set_block(8, 250, 8, BlockType::Stone)
			.expect("place in sky");
		assert!(
			!crate::chunk::is_air_chunk(match &p.store[&sky].state {
				ChunkState::Ready(d) => d,
				_ => panic!("ready"),
			}),
			"sentinel was COWed, not mutated"
		);

		quiesce(&mut p, ChunkPos::new(40, 0, 0));
		quiesce(&mut p, center);
		assert_eq!(p.block_at(8, 250, 8), Some(BlockType::Stone));
		assert!(
			p.store[&sky].mesh_gen > 0,
			"regenerated sky chunk with content must mesh"
		);
	}

	/// A torch in a dug tunnel crossing a chunk border: light follows the
	/// open cells into the neighbor chunk (occlusion is structural — the
	/// surrounding rock stays dark), and the remesh set is EXACTLY the
	/// chunks whose padded shell saw a light write.
	#[test]
	fn light_crosses_chunk_borders_and_bumps_only_shell_touched_chunks() {
		let mut p = Pipeline::new(0.0, 4, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);

		// A sealed stone box in the sky (worldgen caves near the terrain
		// tunnel leaked light in an earlier version of this test), with a
		// 1-cell tunnel at y=200 from x=6 through the border to x=19.
		for x in 5..=20 {
			for y in 199..=201 {
				for z in 7..=9 {
					p.set_block(x, y, z, BlockType::Stone);
				}
			}
		}
		for x in 6..=19 {
			p.set_block(x, 200, 8, BlockType::Air);
		}
		quiesce(&mut p, center);
		settle_light(&mut p, center);

		let before: Vec<(ChunkPos, u64)> =
			p.store.iter().map(|(pos, s)| (*pos, s.mesh_gen)).collect();
		p.set_block(10, 200, 8, BlockType::Torch)
			.expect("torch lands");
		settle_light(&mut p, center);

		// The field follows the tunnel and nothing else.
		assert_eq!(p.light_at(10, 200, 8), 14, "the source");
		assert_eq!(p.light_at(6, 200, 8), 10, "west end");
		assert_eq!(p.light_at(15, 200, 8), 9, "last cell before the border");
		assert_eq!(p.light_at(16, 200, 8), 8, "first cell across");
		assert_eq!(p.light_at(19, 200, 8), 5, "east end");
		assert_eq!(p.light_at(10, 201, 8), 0, "stone above stays dark");
		assert_eq!(p.light_at(20, 200, 8), 0, "stone past the end stays dark");

		// Exactly the torch's chunk (edit + interior writes) and the east
		// neighbor (tunnel cells 16..=19, plus cell 15's shell) re-mesh.
		let (west, east) = (ChunkPos::new(0, 12, 0), ChunkPos::new(1, 12, 0));
		for (pos, gen) in &before {
			let bumped = *pos == west || *pos == east;
			assert_eq!(
				mesh_gen(&p, *pos),
				gen + u64::from(bumped),
				"unexpected remesh at {pos:?}"
			);
		}
	}

	/// A column that generates AFTER a nearby torch settled must be seeded
	/// from the resident field on arrival — the light mirror of the
	/// cross-border re-dirty rule.
	#[test]
	fn a_late_neighbor_column_is_seeded_on_arrival() {
		let mut p = Pipeline::new(0.0, 4, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);

		// Torch in open sky near the +x residency rim: column 4 (x 64..79)
		// is not resident at draw distance 4.
		p.set_block(60, 200, 8, BlockType::Torch).expect("torch");
		settle_light(&mut p, center);
		assert_eq!(p.light_at(63, 200, 8), 11, "lit to the resident edge");
		assert_eq!(p.light_at(64, 200, 8), 0, "nothing beyond residency");

		// Step east: column 4 enters and generates; its cells must pick
		// the field up from column 3's lit border.
		quiesce(&mut p, ChunkPos::new(1, 0, 0));
		settle_light(&mut p, ChunkPos::new(1, 0, 0));
		assert_eq!(p.light_at(64, 200, 8), 10, "seeded across the border");
		assert_eq!(p.light_at(70, 200, 8), 4, "spread deep into the newcomer");
	}

	/// The arrival descend: open columns carry full skylight from the
	/// world top to the terrain surface (sky chunks as unallocated
	/// Uniform tags), and buried cells hold none.
	#[test]
	fn open_column_is_full_skylight_to_the_ground() {
		let mut p = Pipeline::new(0.0, 4, 2);
		quiesce(&mut p, ChunkPos::new(0, 0, 0));
		settle_light(&mut p, ChunkPos::new(0, 0, 0));
		let surface = (0..=255)
			.rev()
			.find(|&y| {
				p.block_at(8, y, 8)
					.is_some_and(|b| b.props().class == crate::block::RenderClass::Opaque)
			})
			.expect("a surface exists");
		assert_eq!(p.sky_at(8, 250, 8), 15, "high sky chunk");
		assert_eq!(p.sky_at(8, surface + 1, 8), 15, "full sky at the surface");
		assert_eq!(
			p.sky_at(8, surface, 8),
			0,
			"the surface cell itself is opaque"
		);
		assert_eq!(p.sky_at(8, 2, 8), 0, "buried bedrock stays dark");
	}

	/// Eviction drops a chunk's light; walking back relights it from the
	/// overlay-restored torch (the field is derived, never persisted).
	#[test]
	fn evicted_light_is_dropped_and_reentry_relights() {
		let mut p = Pipeline::with_overlay(0.0, 4, 2, crate::overlay::EditOverlay::default());
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);

		p.set_block(8, 200, 8, BlockType::Torch).expect("torch");
		settle_light(&mut p, center);
		assert_eq!(p.light_at(12, 200, 8), 10);

		let far = ChunkPos::new(40, 0, 0);
		quiesce(&mut p, far);
		assert_eq!(p.light_at(12, 200, 8), 0, "evicted light dropped");

		quiesce(&mut p, center);
		settle_light(&mut p, center);
		assert_eq!(
			p.block_at(8, 200, 8),
			Some(BlockType::Torch),
			"overlay restored"
		);
		assert_eq!(p.light_at(8, 200, 8), 14, "reseeded on arrival");
		assert_eq!(p.light_at(12, 200, 8), 10, "relit to the same field");
	}

	/// Overlaid water cells are reported for fluid rescheduling; solid
	/// cells are not.
	#[test]
	fn overlay_reports_restored_water_only() {
		let mut overlay = crate::overlay::EditOverlay::default();
		overlay.record(8, 200, 8, BlockType::Water as u16);
		overlay.record(9, 200, 8, BlockType::Stone as u16);
		let mut p = Pipeline::with_overlay(0.0, 4, 2, overlay);
		let mut water = Vec::new();
		for _ in 0..2000 {
			let m = p.maintain(ChunkPos::new(0, 0, 0), usize::MAX, None, None);
			water.extend(m.overlaid_water);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
		assert_eq!(water, vec![(8, 200, 8)]);
	}

	/// Without an overlay (multiplayer), nothing records.
	#[test]
	fn no_overlay_records_nothing() {
		let mut p = Pipeline::new(0.0, 4, 2);
		quiesce(&mut p, ChunkPos::new(0, 0, 0));
		let old = p.block_at(8, 8, 8).unwrap();
		let new = if old == BlockType::Stone {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		p.set_block(8, 8, 8, new).unwrap();
		assert!(p.take_recorded_edits().is_empty());
		assert!(p.overlay().is_none());
	}

	/// The incremental crescent diff must maintain exactly the invariant a
	/// full rebuild would: everything inside the draw distance is resident,
	/// and nothing beyond the hysteresis band is.
	#[test]
	fn incremental_scan_holds_the_residency_invariant() {
		const R: i32 = 5;
		let mut p = Pipeline::new(0.0, R, 2);
		// A path that moves on both axes, diagonally, and doubles back.
		let path = [
			(0, 0),
			(1, 0),
			(2, 0),
			(2, 1),
			(1, 1),
			(1, 2),
			(0, 2),
			(-1, 1),
			(-1, 0),
			(0, 0),
			(0, -1),
			(-2, -2),
			(3, 3),
		];
		for (cx, cz) in path {
			let c = ChunkPos::new(cx, 0, cz);
			quiesce(&mut p, c);
			for z in (cz - R - 2)..=(cz + R + 2) {
				for x in (cx - R - 2)..=(cx + R + 2) {
					let d2 = dist2(ChunkPos::new(x, 0, z), c);
					let here = p.store.contains_key(&ChunkPos::new(x, 0, z));
					if d2 < i64::from(R) * i64::from(R) {
						assert!(here, "inside draw dist but missing: ({x},{z}) at {c:?}");
					}
					let keep = i64::from(R + KEEP_MARGIN) * i64::from(R + KEEP_MARGIN);
					if d2 >= keep {
						assert!(!here, "past the keep band but resident: ({x},{z}) at {c:?}");
					}
				}
			}
		}
	}

	/// The churn this exists to kill: pacing back and forth over one chunk
	/// boundary used to evict a crescent and immediately regenerate it.
	#[test]
	fn boundary_pacing_does_not_regenerate() {
		let mut p = Pipeline::new(0.0, 5, 2);
		let a = ChunkPos::new(0, 0, 0);
		let b = ChunkPos::new(1, 0, 0);
		quiesce(&mut p, a);
		quiesce(&mut p, b);
		// Both discs are now covered by the resident set plus its margin,
		// so neither step may queue any generation at all.
		for i in 0..8 {
			for c in [a, b] {
				let m = p.maintain(c, usize::MAX, None, None);
				assert_eq!(
					m.gen_queue, 0,
					"pacing one boundary re-queued generation (step {i}, centre {c:?})"
				);
				assert!(
					m.evicted.is_empty(),
					"step {i} evicted a column it needs back"
				);
			}
		}
	}

	#[test]
	fn interval_diff_cases() {
		let all = |r: [(i32, i32); 2]| -> Vec<i32> { r.iter().flat_map(|&(a, b)| a..=b).collect() };
		assert_eq!(all(interval_diff(0, 4, None)), vec![0, 1, 2, 3, 4]);
		// Overlap on the left, on the right, contained, and disjoint.
		assert_eq!(all(interval_diff(0, 4, Some((-2, 1)))), vec![2, 3, 4]);
		assert_eq!(all(interval_diff(0, 4, Some((3, 9)))), vec![0, 1, 2]);
		assert_eq!(all(interval_diff(0, 4, Some((1, 3)))), vec![0, 4]);
		assert_eq!(all(interval_diff(0, 4, Some((7, 9)))), vec![0, 1, 2, 3, 4]);
		assert!(all(interval_diff(0, 4, Some((-1, 5)))).is_empty());
	}

	#[test]
	fn row_half_width_matches_the_disc_predicate() {
		for r in 1..12i32 {
			for dz in -13..=13i32 {
				let w = row_half_width(dz, r);
				for dx in -14..=14i32 {
					let inside = i64::from(dx) * i64::from(dx) + i64::from(dz) * i64::from(dz)
						< i64::from(r) * i64::from(r);
					assert_eq!(
						inside,
						w.is_some_and(|w| dx.abs() <= w),
						"r={r} dz={dz} dx={dx}"
					);
				}
			}
		}
	}

	#[test]
	fn set_draw_dist_rescans_both_ways() {
		let mut p = Pipeline::new(0.0, 6, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);
		let full = p.resident();

		// Shrink: same center, but the cleared scan flag forces a rescan
		// that evicts everything beyond the new radius.
		p.set_draw_dist(4);
		let m = p.maintain(center, usize::MAX, None, None);
		assert!(!m.evicted.is_empty(), "shrink evicts out-of-range chunks");
		let small = p.resident();
		assert!(small < full);
		for pos in p.store.keys() {
			assert!(
				dist2(ChunkPos::new(pos.x, 0, pos.z), center) < 16,
				"{pos:?}"
			);
		}

		// No-op set doesn't trigger a rescan.
		p.set_draw_dist(4);
		let m = p.maintain(center, usize::MAX, None, None);
		assert!(m.evicted.is_empty());

		// Grow back: the ring regenerates to the original census.
		p.set_draw_dist(6);
		quiesce(&mut p, center);
		assert_eq!(p.resident(), full, "grow restores the full radius");
	}

	#[test]
	fn set_block_cow_preserves_snapshots() {
		let mut p = Pipeline::new(0.0, 4, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);
		let pos = ChunkPos::new(0, 0, 0);
		let ChunkState::Ready(snapshot) = &p.store[&pos].state else {
			panic!("chunk not ready");
		};
		let snapshot = Arc::clone(snapshot); // simulate an in-flight mesh job
		let old = snapshot.get(3, 3, 3);
		let new = if old == BlockType::Stone {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		p.set_block(3, 3, 3, new).expect("edit lands");
		assert_eq!(snapshot.get(3, 3, 3), old, "job snapshot untouched");
		assert_eq!(p.block_at(3, 3, 3), Some(new));
	}

	#[test]
	fn set_block_into_sky_chunk_cows_the_sentinel() {
		let mut p = Pipeline::new(0.0, 4, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);
		// Top of the world is far above any terrain: a sentinel sky chunk.
		let y = crate::chunk::WORLD_VBLOCK - 1;
		let pos = ChunkPos::new(0, y.div_euclid(16), 0);
		{
			let ChunkState::Ready(d) = &p.store[&pos].state else {
				panic!("sky chunk not ready");
			};
			assert!(is_air_chunk(d), "sky chunk is the sentinel");
		}
		let before = p.store[&pos].mesh_gen;
		assert_eq!(p.set_block(0, y, 0, BlockType::Stone), Some(BlockType::Air));
		let ChunkState::Ready(d) = &p.store[&pos].state else {
			panic!("sky chunk not ready");
		};
		assert!(!is_air_chunk(d), "edit copy-on-wrote the sentinel");
		assert_eq!(p.block_at(0, y, 0), Some(BlockType::Stone));
		assert_eq!(
			crate::chunk::AIR_CHUNK.get(0, y.rem_euclid(16) as usize, 0),
			BlockType::Air,
			"shared sentinel untouched"
		);
		assert_eq!(p.store[&pos].mesh_gen, before + 1, "edited chunk remeshes");
	}

	#[test]
	fn edited_chunk_remeshes_at_new_gen() {
		let mut p = Pipeline::new(0.0, 4, 2);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);
		let new = if p.block_at(8, 8, 8) == Some(BlockType::Stone) {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		p.set_block(8, 8, 8, new).expect("edit lands");
		let want_gen = mesh_gen(&p, ChunkPos::new(0, 0, 0));
		// The next maintain cycle must deliver a mesh at the bumped gen.
		let mut got = None;
		for _ in 0..2000 {
			let m = p.maintain(center, usize::MAX, None, None);
			if let Some(out) = m
				.meshed
				.into_iter()
				.find(|o| o.pos == ChunkPos::new(0, 0, 0))
			{
				got = Some(out.gen);
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
		assert_eq!(got, Some(want_gen), "remesh delivered at the edit's gen");
	}

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
				let m = p.maintain(center, 64, None, None);
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
		// Drain until fully quiescent (bounded, but exit on quiescence so
		// the test tolerates a loaded machine).
		let final_center = ChunkPos::new(10, 0, 5);
		let mut quiet = 0;
		for _ in 0..5000 {
			let m = p.maintain(final_center, 64, None, None);
			gen_jobs.merge(m.gen_jobs);
			mesh_jobs.merge(m.mesh_jobs);
			for pos in m.evicted {
				uploaded.remove(&pos);
			}
			for out in m.meshed {
				uploaded.insert(out.pos, out.gen);
			}
			// Two consecutive idle observations: results can land between
			// the channel check and the gauges snapshot.
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				quiet += 1;
				if quiet >= 2 {
					break;
				}
			} else {
				quiet = 0;
			}
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
		assert!(p.resident() > 0);
		// Every meshable resident chunk has a mesh upload; sky chunks
		// resolve to the AIR_CHUNK sentinel and are never meshed.
		let meshable = p
			.store
			.values()
			.filter(|s| matches!(&s.state, ChunkState::Ready(d) if !is_air_chunk(d)))
			.count();
		let air = p.resident() - meshable;
		assert!(air > 0, "tall world has sentinel sky chunks");
		assert_eq!(uploaded.len(), meshable, "all meshable chunks meshed");
		// Instrumentation sanity: jobs were timed and aggregated.
		assert!(gen_jobs.count > 0, "gen jobs recorded");
		assert!(mesh_jobs.count > 0, "mesh jobs recorded");
		assert!(gen_jobs.max_ms >= gen_jobs.avg_ms());
		assert!(mesh_jobs.max_ms >= mesh_jobs.avg_ms());
	}

	/// The join-sync race: an authoritative edit arrives before its chunk
	/// has generated. It must be held and applied when the chunk lands —
	/// the old engine-side retry map is gone, this is the Pipeline's job.
	#[test]
	fn authoritative_edit_races_generation() {
		let mut p = Pipeline::new(0.0, 4, 2);
		// Before any maintain(): every chunk is ungenerated.
		assert_eq!(
			p.apply_authoritative(8, 8, 8, BlockType::Sand as u16),
			ApplyEdit::Deferred
		);
		assert_eq!(p.deferred_edits(), 1);
		// Out-of-world: rejected, never held.
		assert_eq!(
			p.apply_authoritative(8, -1, 8, BlockType::Sand as u16),
			ApplyEdit::Rejected
		);
		assert_eq!(p.deferred_edits(), 1);

		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);
		assert_eq!(p.deferred_edits(), 0, "applied when the chunk landed");
		assert_eq!(p.block_at(8, 8, 8), Some(BlockType::Sand));

		// Once the chunk is Ready, application is immediate.
		let other = if p.block_at(9, 8, 8) == Some(BlockType::Stone) {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		assert_eq!(
			p.apply_authoritative(9, 8, 8, other as u16),
			ApplyEdit::Applied
		);
		assert_eq!(p.block_at(9, 8, 8), Some(other));
	}

	/// The inline executor (workers == 0, the wasm path) must land the
	/// exact same world as the worker pool.
	#[test]
	fn inline_executor_matches_worker_pool() {
		let center = ChunkPos::new(0, 0, 0);
		let mut inline = Pipeline::new(7.5, 4, 0);
		let mut pool = Pipeline::new(7.5, 4, 2);
		quiesce(&mut inline, center);
		quiesce(&mut pool, center);

		assert_eq!(inline.resident(), pool.resident());
		for (pos, slot) in &inline.store {
			let other = &pool.store[pos];
			match (&slot.state, &other.state) {
				(ChunkState::Ready(a), ChunkState::Ready(b)) => {
					assert!(a.blocks == b.blocks, "chunk data differs at {pos:?}");
				}
				_ => panic!("chunk not Ready at {pos:?}"),
			}
		}

		// Edits work identically through the inline path.
		let cur = inline.block_at(8, 8, 8);
		let b = if cur == Some(BlockType::Stone) {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		assert_eq!(inline.set_block(8, 8, 8, b), cur);
		assert_eq!(inline.block_at(8, 8, 8), Some(b));
	}

	/// Inline drains make progress even when every frame is over budget:
	/// with an already-expired deadline each maintain, the center column
	/// (nearest-first, the first job popped) still reaches the store
	/// within the first frames. Regression for the web spawn fall-through:
	/// the inline gen job overran the drain deadline every frame, so
	/// generated columns piled up undrained while the player fell through
	/// a world that existed but never landed.
	#[test]
	fn inline_drain_progresses_past_expired_deadline() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(7.5, 12, 0);
		for frame in 0..3 {
			let expired = web_time::Instant::now() - std::time::Duration::from_millis(1);
			p.maintain(center, usize::MAX, Some(expired), None);
			let _ = frame;
		}
		assert!(
			p.cell_at(8, 100, 8).is_some(),
			"center column must land within the first frames"
		);
	}

	/// Without an allowance the inline executor keeps today's exact batch
	/// shape: at most one `Generate` (which ends the batch) and at most
	/// `INLINE_MESH_CAP` meshes per maintain.
	#[test]
	fn inline_allowance_none_matches_legacy() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(7.5, 4, 0);
		let mut meshed_batches = 0;
		for _ in 0..2000 {
			let m = p.maintain(center, usize::MAX, None, None);
			assert!(m.gen_jobs.count <= 1, "one Generate ends the batch");
			assert!(
				m.mesh_jobs.count as usize <= INLINE_MESH_CAP,
				"mesh floor respected"
			);
			meshed_batches += u32::from(m.mesh_jobs.count > 0);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}
		assert!(meshed_batches > 0, "the world actually meshed");
	}

	/// A large allowance lands strictly more jobs per maintain than the
	/// floor-only batch (contrast against `None`, not tight timing — the
	/// first maintain's gen heap holds every visible column, so the
	/// extension phase runs several where the floor runs one).
	#[test]
	fn inline_allowance_extends_batch() {
		let center = ChunkPos::new(0, 0, 0);
		let mut floor = Pipeline::new(7.5, 4, 0);
		let m = floor.maintain(center, usize::MAX, None, None);
		assert_eq!(m.gen_jobs.count, 1, "floor batch: exactly one Generate");

		let mut wide = Pipeline::new(7.5, 4, 0);
		let allowance = std::time::Duration::from_secs(60);
		let m = wide.maintain(center, usize::MAX, None, Some(allowance));
		assert!(
			m.gen_jobs.count > 1,
			"allowance extends past the floor (ran {})",
			m.gen_jobs.count
		);
	}

	/// A zero allowance is the floor, not a stall: the unconditional
	/// first batch still runs and the center column still lands (the
	/// spawn fall-through guarantee, now under the adaptive path).
	#[test]
	fn inline_allowance_zero_keeps_floor_guarantee() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(7.5, 12, 0);
		for _ in 0..3 {
			let expired = web_time::Instant::now() - std::time::Duration::from_millis(1);
			p.maintain(
				center,
				usize::MAX,
				Some(expired),
				Some(std::time::Duration::ZERO),
			);
		}
		assert!(
			p.cell_at(8, 100, 8).is_some(),
			"center column must land within the first frames"
		);
	}

	/// The urgency signal the adaptive budget's ceiling scales by: full
	/// while near-field work is pending, zero once only the rim (or
	/// nothing) remains.
	#[test]
	fn stream_urgency_tracks_nearest_pending() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(7.5, 8, 0);
		assert_eq!(p.stream_urgency(), 0.0, "no pending work before the scan");
		p.maintain(center, usize::MAX, None, None);
		assert_eq!(
			p.stream_urgency(),
			1.0,
			"near-field columns pending after the first scan"
		);
		quiesce(&mut p, center);
		assert_eq!(p.stream_urgency(), 0.0, "quiescent again once filled");
	}

	#[test]
	fn job_cost_prediction() {
		let mut c = CostRing::new(12.0);
		assert_eq!(c.predicted_ms(), 12.0, "seed stands in before samples");
		c.observe(2.0);
		assert_eq!(c.predicted_ms(), 12.0, "seed holds until the ring fills");
		for _ in 0..COST_N {
			c.observe(2.0);
		}
		assert_eq!(c.predicted_ms(), 2.0, "full ring drops the seed");
		c.observe(5.0);
		assert_eq!(c.predicted_ms(), 5.0, "tracks the recent worst");
		for _ in 0..COST_N {
			c.observe(1.0);
		}
		// The recent window has fully turned over, but admission control
		// keeps a share of the lifetime worst underneath it: gen cost is
		// bimodal, and a run of cheap ocean columns must not license
		// starting a mountain column that costs 10× the remaining budget.
		assert_eq!(
			c.predicted_ms(),
			5.0 * LIFETIME_FLOOR_FRAC,
			"lifetime floor survives the window turning over"
		);
		// It is a floor, not a memory: a recent sample above it still wins.
		c.observe(4.0);
		assert_eq!(c.predicted_ms(), 4.0, "recent worst beats the floor");
	}

	/// The bimodality this guards against, at the granularity it happens:
	/// `COST_N` cheap columns in a row used to collapse the prediction far
	/// enough to admit a job an order of magnitude larger.
	#[test]
	fn cheap_run_cannot_collapse_the_prediction() {
		let mut c = CostRing::new(12.0);
		for _ in 0..COST_N {
			c.observe(12.0); // a mountain column
		}
		for _ in 0..COST_N * 4 {
			c.observe(0.9); // a long run of ocean columns
		}
		assert!(
			c.predicted_ms() >= 6.0,
			"prediction {} collapsed below the lifetime floor",
			c.predicted_ms()
		);
	}
}
