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
use crate::mesher::{mesh_chunk, MeshData, MeshInput, Src};
use crate::metrics::JobTiming;
use crate::worldgen::build_column;

pub struct MeshOutput {
	pub pos: ChunkPos,
	pub gen: u64,
	pub mesh: MeshData,
}

pub enum Job {
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

pub enum JobResult {
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

/// Enough of a [`Job`] to put it back on the heap it came off.
///
/// A pool that loses a job — a worker that trapped, was killed, or
/// answered with a refusal — hands these back through
/// [`JobDispatch::lost`], and `Pipeline::reclaim` requeues them. Without
/// that a lost reply strands its chunk for the whole session, silently: a
/// popped job is remembered NOWHERE else, so the column sits `Generating`
/// forever (a hole in the world) or the chunk stays at
/// `submitted_gen == mesh_gen` and never meshes again (invisible terrain).
///
/// The payload is deliberately not retained. A `Mesh` job's `MeshInput`
/// is ~12 KB and a pool may hold dozens outstanding — real memory on the
/// platform this exists for — and the pipeline rebuilds it from the store
/// at redispatch anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKey {
	Generate { x: i32, z: i32 },
	Mesh { pos: ChunkPos, gen: u64 },
}

impl Job {
	pub fn key(&self) -> JobKey {
		match self {
			Job::Generate { x, z, .. } => JobKey::Generate { x: *x, z: *z },
			Job::Mesh { pos, gen, .. } => JobKey::Mesh {
				pos: *pos,
				gen: *gen,
			},
		}
	}
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
	/// Generation of the last accepted mesh, if that mesh was empty AND
	/// was built with every face neighbor present. A chunk whose current
	/// mesh is empty — every buried chunk — can then skip re-dirties
	/// entirely: neighbors' shell cells only shade existing faces, they
	/// never create them, so nothing stays nothing.
	///
	/// The face-neighbor condition is load-bearing, not caution.
	/// `Src::Missing` doesn't merely read as air — it SUPPRESSES that
	/// whole border (`MeshInput::missing`), so a chunk meshed beside an
	/// ungenerated neighbor is empty for a reason that expires. Trusting
	/// that emptiness left the border unemitted for the rest of the
	/// session: a permanent hole wherever a solid chunk met the first
	/// neighbor to arrive, which is every cave mouth and every surface
	/// seam on the column that generated first.
	empty_mesh_gen: Option<u64>,
	/// The dispatched mesh job had at least one missing face neighbor
	/// (see `empty_mesh_gen`). Tracked here rather than round-tripped
	/// through the job, because the pipeline is what decided it.
	mesh_missing: bool,
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
	/// d² of the nearest job dispatched since the executor was last idle
	/// — the in-flight half of `stream_urgency`.
	in_flight_min_d2: Option<i64>,
	/// Inline job-cost predictor (unused with a worker pool).
	job_cost: JobCost,
	/// Chunks whose GI-window region is stale (RTGI; GI-DESIGN.md):
	/// arrivals and edits land here, the engine drains via
	/// `take_gi_dirty` under its upload budget. Tracked unconditionally —
	/// a set insert is noise, and it stays bounded by the resident set
	/// (drained or not) because it IS a set.
	gi_dirty: FxHashSet<ChunkPos>,
	/// Edited world cells awaiting probe invalidation (RTGI).
	gi_edits: Vec<(i32, i32, i32)>,
	/// The camera chunk the GI window is currently centered on — the
	/// scroll anchor `gi_recenter` diffs against.
	gi_center: Option<(i32, i32)>,
}

/// Is a chunk column inside the GI window's half-extent of `center`?
/// The window is `w` chunks per side and toroidal, so exactly the
/// columns in `[center - w/2, center + w/2)` map unambiguously; anything
/// outside would alias onto an in-range column and corrupt it.
fn gi_in_window(pos: ChunkPos, center: (i32, i32), w: i32) -> bool {
	let lo = (center.0 - w / 2, center.1 - w / 2);
	pos.x >= lo.0 && pos.x < lo.0 + w && pos.z >= lo.1 && pos.z < lo.1 + w
}

/// A job pool this crate cannot build for itself — the browser's Web
/// Workers, injected by the launcher exactly as `NetTransport` is, so
/// `digblock-core` stays free of platform deps and the whole path is
/// testable in-process (see `ImmediateDispatch` in this file's tests, the
/// twin of `digblock-net`'s loopback transport).
///
/// Everything here is non-blocking: it is called from inside the frame.
pub trait JobDispatch {
	/// Is the pool usable at all? False while workers are still starting,
	/// and permanently false if the platform refused to spawn any — the
	/// pipeline then runs jobs inline, which is exactly the behaviour
	/// this trait exists to replace. That fallback is why a browser that
	/// blocks workers degrades instead of stalling.
	fn ready(&self) -> bool;
	/// Non-blocking submit. Hands the job back when the pool is full;
	/// the caller requeues it (priority is re-evaluated next frame
	/// anyway). Never run a refused job inline — it is refused because
	/// the pool is busy, not because it is unavailable.
	fn submit(&mut self, job: Job) -> Result<(), Job>;
	/// Drain one finished job, if any. Results arrive asynchronously
	/// (a worker message is its own event-loop task, not part of this
	/// frame), so the pipeline pulls them into its own queue each
	/// `maintain` and drains that under the frame budget.
	fn poll(&mut self) -> Option<JobResult>;
	/// Drain one job the pool accepted and can no longer answer for: a
	/// worker that trapped, was killed, or refused the work. The pipeline
	/// requeues each one (see [`JobKey`]).
	///
	/// This is not an optional nicety — it is the other half of `submit`.
	/// A job handed to a pool is remembered nowhere else, so a pool that
	/// silently drops one leaves a permanent hole in the world. A pool
	/// that structurally cannot lose a job returns `None` forever.
	fn lost(&mut self) -> Option<JobKey>;
	/// Workers attached and answering right now — what the debug overlay
	/// and the bench summary report.
	///
	/// This must be the TRUTH rather than what was asked for. Every path
	/// that yields no usable pool (the platform refused to spawn workers,
	/// the module failed to load, every worker died) falls back to running
	/// jobs on this thread, and a run that streamed inline must not be
	/// recorded in BENCH.md as a pooled arm.
	fn worker_count(&self) -> usize;
	/// Abandon everything in flight: after this returns, a result for any
	/// job submitted before it must NEVER be delivered.
	///
	/// Load-bearing across a world change. A pool outlives the pipeline
	/// that fed it (see `take_dispatch`), and a `Generated` result carries
	/// only a column coordinate — so one still in flight from the old
	/// world would land as authoritative terrain in the new one, at the
	/// same (x, z), from a different seed. Implementations that can't
	/// recall a dispatched job must instead discard its reply.
	///
	/// Abandoned jobs are NOT reported through `lost` either: the new
	/// pipeline's store has never heard of those chunks. And whatever
	/// per-worker accounting bounds `submit` has to be released here — a
	/// pool that still counted the old world's jobs against its depth
	/// would refuse the new world's while `ready` stayed true, which is
	/// the one state where nothing streams and nothing runs inline.
	fn reset(&mut self);
}

/// How jobs run: a worker-thread pool (native), an injected out-of-process
/// pool (the web), or synchronously inside `maintain` (`workers == 0` —
/// the wasm main thread before its workers are up, and a native debug mode
/// via `--workers 0`). Selected at runtime so the inline path is testable
/// and benchable everywhere.
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
	External {
		dispatch: Box<dyn JobDispatch>,
		/// Both the pool's drained results and any run inline while the
		/// pool was unavailable. One queue, so the drain, the upload
		/// budget and `has_pending` need no idea which is which.
		results: VecDeque<JobResult>,
	},
}

impl Executor {
	fn try_recv(&mut self) -> Option<JobResult> {
		match self {
			Executor::Inline { results } | Executor::External { results, .. } => {
				results.pop_front()
			}
			Executor::Workers { result_rx, .. } => result_rx.try_recv().ok(),
		}
	}

	fn has_pending(&self) -> bool {
		match self {
			Executor::Inline { results } | Executor::External { results, .. } => {
				!results.is_empty()
			}
			Executor::Workers { result_rx, .. } => !result_rx.is_empty(),
		}
	}

	/// Where an inline-executed job's result lands. Only the two
	/// main-thread executors ever run one.
	fn push_result(&mut self, r: JobResult) {
		match self {
			Executor::Inline { results } | Executor::External { results, .. } => {
				results.push_back(r);
			}
			Executor::Workers { .. } => unreachable!("worker pools don't run jobs inline"),
		}
	}

	/// True when jobs must run on this thread: the inline executor, or an
	/// external pool that isn't up yet.
	fn runs_inline(&self) -> bool {
		match self {
			Executor::Inline { .. } => true,
			Executor::External { dispatch, .. } => !dispatch.ready(),
			Executor::Workers { .. } => false,
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
			in_flight_min_d2: None,
			job_cost: JobCost::new(),
			gi_dirty: FxHashSet::default(),
			gi_edits: Vec::new(),
			gi_center: None,
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

	/// Attach a platform job pool, upgrading the inline executor in place.
	/// The pool need not be ready yet — jobs keep running inline until it
	/// is, so nothing has to wait on a worker handshake.
	///
	/// Ignored when a worker-thread pool is already running: native has
	/// one, and two pools would race for the same jobs.
	pub fn set_dispatch(&mut self, dispatch: Box<dyn JobDispatch>) {
		let results = match &mut self.exec {
			Executor::Inline { results } | Executor::External { results, .. } => {
				std::mem::take(results)
			}
			Executor::Workers { .. } => return,
		};
		self.exec = Executor::External { dispatch, results };
	}

	/// Detach the pool so it can outlive this pipeline — a world switch or
	/// server join rebuilds the pipeline, and workers are expensive to
	/// start and hold no world state of their own.
	///
	/// Resets the pool on the way out: in-flight jobs belong to the world
	/// being torn down (see [`JobDispatch::reset`]).
	pub fn take_dispatch(&mut self) -> Option<Box<dyn JobDispatch>> {
		if !matches!(self.exec, Executor::External { .. }) {
			return None;
		}
		let old = std::mem::replace(
			&mut self.exec,
			Executor::Inline {
				results: VecDeque::new(),
			},
		);
		let Executor::External { mut dispatch, .. } = old else {
			unreachable!("checked above")
		};
		dispatch.reset();
		self.in_flight = 0;
		self.in_flight_min_d2 = None;
		// The abandoned jobs were the only thing that would ever complete
		// those slots — a chunk popped off the heap and handed to a pool
		// is remembered nowhere else, so it would sit `Generating` for the
		// rest of the session. Drop everything unfinished and force a
		// rescan; `add_column` re-queues vacant slots from scratch. (The
		// usual caller discards this pipeline anyway, but a half-built
		// world that never finishes is not a state to leave reachable.)
		self.store
			.retain(|_, s| matches!(s.state, ChunkState::Ready(_)));
		// A SURVIVING chunk strands the same way and more quietly:
		// `next_mesh_job` refuses while `mesh_gen <= submitted_gen`, and
		// the abandoned mesh job was the only thing that would ever roll
		// that marker back (`requeue` does it; a dropped pool cannot). We
		// can't tell which markers were in flight — a completed job leaves
		// the same `submitted_gen == mesh_gen` — so roll every one back and
		// re-queue, which is what `add_column`'s vacant-only rule
		// structurally cannot do for a slot that is not vacant. Costs a
		// re-mesh of the resident set, which is the cheap half of the
		// regeneration the retain above already implies.
		let center = self.center;
		self.gen_heap.clear();
		self.mesh_heap.clear();
		for (pos, slot) in self.store.iter_mut() {
			slot.submitted_gen = 0;
			if matches!(&slot.state, ChunkState::Ready(d) if !is_air_chunk(d)) {
				let d = dist2(ChunkPos::new(pos.x, 0, pos.z), center);
				self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
			}
		}
		self.scanned = None;
		Some(dispatch)
	}

	/// Call every frame with the player's chunk position. Performs the
	/// visibility scan on chunk-boundary crossings, tops up workers, and
	/// drains results (mesh outputs capped at `upload_budget`; the whole
	/// drain stops at `deadline` — undrained results wait in the channel,
	/// so worker throughput is unaffected, only main-thread pacing).
	/// `inline_allowance` lets the inline executor run extra jobs beyond
	/// its fixed floor while their predicted cost fits (the adaptive web
	/// budget); `None` keeps the floor-only batch. Ignored while jobs run
	/// off-thread — the allowance prices JOBS, and a pool doesn't spend
	/// this thread's time on them. `deadline` still governs everything
	/// else, pool or not: pulling the pool's replies, the drain, and
	/// feeding the pool are all main-thread work either way.
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
		};
		let center = ChunkPos::new(center.x, 0, center.z);
		if self.scanned != Some((center, self.draw_dist)) {
			self.center = center;
			self.visibility_scan(&mut out.evicted);
		}
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
		//
		// An external pool's results arrived as their own event-loop tasks
		// while this thread was elsewhere; pull them into the queue the
		// drain reads first, so budgeting, staleness and `has_pending` stay
		// identical for every executor.
		//
		// Under the deadline, because the pull is not free: every `poll`
		// decodes, and a `Generated` column is 16 chunks of bounds-checked
		// u16 reads plus 16 fresh allocations on the one thread this whole
		// change exists to protect. Always at least one, the same
		// minimum-progress floor every other stage keeps — a frame already
		// over budget must still absorb something, or the pool's inbox only
		// grows.
		let mut lost: Vec<JobKey> = Vec::new();
		// The pull can stop with replies still in the pool, which
		// `has_pending` — which sees only the decoded queue — cannot know.
		let mut pull_cut = false;
		if let Executor::External { dispatch, results } = &mut self.exec {
			let mut pulled = 0usize;
			loop {
				if pulled > 0 && deadline.is_some_and(|d| web_time::Instant::now() >= d) {
					pull_cut = true;
					break;
				}
				let Some(r) = dispatch.poll() else {
					break;
				};
				results.push_back(r);
				pulled += 1;
			}
			// Jobs the pool accepted and cannot answer for. Unbudgeted on
			// purpose: this is a handful of 16-byte keys in a failure that
			// is rare by construction, and deferring it is deferring the
			// only record that those chunks were ever dispatched.
			while let Some(key) = dispatch.lost() {
				lost.push(key);
			}
		}
		if !lost.is_empty() {
			log::warn!("{} chunk job(s) lost by the pool; requeued", lost.len());
			for key in lost {
				self.in_flight = self.in_flight.saturating_sub(1);
				self.reclaim(key);
			}
		}
		if self.exec.runs_inline() {
			self.run_inline_jobs(inline_allowance);
		}
		self.drain_results(&mut out, upload_budget, deadline);
		out.saturated = self.exec.has_pending() || pull_cut;
		out.requeued = self.top_up_workers(deadline);
		out.gen_queue = self.gen_heap.len() as u32;
		out.mesh_queue = self.mesh_heap.len() as u32;
		out.in_flight = self.in_flight;
		// Everything dispatched has been absorbed, so the in-flight half of
		// `stream_urgency` has nothing left to speak for.
		if self.in_flight == 0 {
			self.in_flight_min_d2 = None;
		}
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
					mesh_missing: false,
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
					// Only a column still WAITING for terrain may take it.
					// A `Generated` result carries no generation counter,
					// so without this the SECOND one — a column evicted
					// with its job still in a pool, re-entered, and
					// dispatched again — overwrites live cells with
					// pristine worldgen. Single-player survives that (the
					// overlay is re-applied just below); a server's edits
					// do not, because `apply_authoritative` drops a cell
					// from `deferred_edits` the moment it lands. The
					// column's chunks enter and leave together, so y=0 is
					// the probe `next_gen_job` uses too.
					if !matches!(
						self.store.get(&ChunkPos::new(x, 0, z)).map(|s| &s.state),
						Some(ChunkState::Generating)
					) {
						out.stale_dropped += 1;
						continue;
					}
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
						//
						// "Meshed to nothing" is narrower than it looks, and
						// `empty_mesh_gen` is where that narrowness lives: a
						// missing FACE neighbour also suppresses its whole
						// border, so a chunk empty for THAT reason is not
						// empty once the neighbour lands, and the slot
						// declines to record it. Read its doc before widening
						// this skip.
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
							slot.empty_mesh_gen =
								(m.mesh.is_empty() && !slot.mesh_missing).then_some(m.gen);
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
			let d2 = self.job_dist2(&job);
			let result = run_job(job);
			let dur_ms = match &result {
				JobResult::Generated { dur_ms, .. } | JobResult::Meshed { dur_ms, .. } => *dur_ms,
			};
			self.job_cost.observe(is_gen, dur_ms);
			self.in_flight += 1;
			self.note_dispatch(d2);
			self.exec.push_result(result);
			if is_gen {
				floor_done = true;
			} else {
				meshes += 1;
				floor_done |= meshes >= INLINE_MESH_CAP;
			}
		}
	}

	/// Returns the number of jobs bounced back because workers were
	/// saturated (0 or 1 — a bounce ends the loop). No-op whenever jobs
	/// run on this thread (they already ran in `run_inline_jobs`).
	///
	/// Shares the caller's frame deadline, with the usual one-job floor.
	/// Handing a job to a pool is not free on every platform: the web's
	/// `submit` encodes it and copies it into a JS buffer — two passes
	/// over an ~12 KB `MeshInput` that native's channel send does not
	/// make — and a pass can offer `DEPTH × workers` of them, so an
	/// unbudgeted top-up is a millisecond-scale stage hiding inside a
	/// budget that thinks it covers everything.
	fn top_up_workers(&mut self, deadline: Option<web_time::Instant>) -> u32 {
		if self.exec.runs_inline() {
			return 0;
		}
		let mut submitted = 0usize;
		loop {
			if submitted > 0 && deadline.is_some_and(|d| web_time::Instant::now() >= d) {
				break;
			}
			let Some(job) = self.pop_next_job() else {
				break;
			};
			let d2 = self.job_dist2(&job);
			let sent = match &mut self.exec {
				Executor::Workers { job_tx, .. } => match job_tx.try_send(job) {
					Ok(()) => Ok(()),
					Err(TrySendError::Full(job)) => Err(Some(job)),
					// The pool is gone; stop feeding it. The job stays
					// popped — a disconnected pool is a dead client.
					Err(TrySendError::Disconnected(_)) => Err(None),
				},
				Executor::External { dispatch, .. } => dispatch.submit(job).map_err(Some),
				Executor::Inline { .. } => unreachable!("checked above"),
			};
			match sent {
				Ok(()) => {
					self.in_flight += 1;
					self.note_dispatch(d2);
					submitted += 1;
				}
				// Put it back; the pool is saturated.
				Err(Some(job)) => {
					self.requeue(job);
					return 1;
				}
				Err(None) => break,
			}
		}
		0
	}

	/// Flat d² from the streaming centre of whatever column a job covers —
	/// the same key both heaps are ordered by.
	fn job_dist2(&self, job: &Job) -> i64 {
		let (x, z) = match job {
			Job::Generate { x, z, .. } => (*x, *z),
			Job::Mesh { pos, .. } => (pos.x, pos.z),
		};
		dist2(ChunkPos::new(x, 0, z), self.center)
	}

	/// Remember the nearest job dispatched since the executor was last
	/// idle. Jobs go out nearest-first, so the minimum over everything
	/// dispatched can only be nearer than the minimum over what is still
	/// outstanding — it overestimates urgency and never underestimates it,
	/// exactly like `stream_urgency`'s stale heap entries.
	fn note_dispatch(&mut self, d2: i64) {
		self.in_flight_min_d2 = Some(self.in_flight_min_d2.map_or(d2, |m| m.min(d2)));
	}

	/// Put back a job the pool lost. The twin of `requeue` for a job whose
	/// payload is gone (see [`JobKey`]): a mesh job's `MeshInput` is
	/// rebuilt at redispatch, so only the marker rollback carries over.
	fn reclaim(&mut self, key: JobKey) {
		match key {
			JobKey::Generate { x, z } => {
				// Still `Generating` means nothing else has taken the
				// column on; an evicted or already-landed one has no work
				// to redo. Same y=0 probe `next_gen_job` uses.
				if !matches!(
					self.store.get(&ChunkPos::new(x, 0, z)).map(|s| &s.state),
					Some(ChunkState::Generating)
				) {
					return;
				}
				let d = dist2(ChunkPos::new(x, 0, z), self.center);
				self.gen_heap.push((Reverse(d), (x, z)));
			}
			JobKey::Mesh { pos, gen } => {
				let Some(slot) = self.store.get_mut(&pos) else {
					return; // evicted
				};
				if slot.submitted_gen == gen {
					slot.submitted_gen = gen.saturating_sub(1);
				}
				let d = dist2(ChunkPos::new(pos.x, 0, pos.z), self.center);
				self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
			}
		}
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
		let missing = input.missing.iter().any(|&m| m);
		let slot = self.store.get_mut(&pos).expect("slot exists");
		slot.submitted_gen = gen;
		slot.mesh_missing = missing;
		Some(Job::Mesh { pos, gen, input })
	}

	fn build_mesh_input(&self, pos: ChunkPos, center: &ChunkData) -> MeshInput {
		let input = MeshInput::build(center, |dx, dy, dz| {
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
	///
	/// **In-flight work counts.** With the inline executor an empty pair
	/// of heaps genuinely meant nothing was pending — a job left the heap
	/// and finished inside the same `run_inline_jobs` call. A pool breaks
	/// that: `top_up_workers` drains BOTH heaps into it, so the frame the
	/// backlog is handed over reads as idle and collapses the shared
	/// deadline to its floor — precisely as those results start landing on
	/// this thread to be absorbed. `in_flight_min_d2` is what keeps the
	/// question "how near is the work" independent of WHERE it is running.
	pub fn stream_urgency(&self) -> f32 {
		let gen = self.gen_heap.peek().map(|(Reverse(d), _)| *d);
		let mesh = self.mesh_heap.peek().map(|(Reverse(d), _)| *d);
		let Some(d2) = gen
			.into_iter()
			.chain(mesh)
			.chain(self.in_flight_min_d2)
			.min()
		else {
			return 0.0;
		};
		crate::stream_budget::urgency((d2 as f32).sqrt(), self.draw_dist as f32)
	}

	/// Workers actually running this pipeline's jobs — 0 on the inline
	/// path, including a web build whose pool never started or has fallen
	/// back to this thread. The overlay and the bench summary report this
	/// rather than the requested count, so a run that streamed inline
	/// cannot be recorded as a pooled one.
	pub fn worker_count(&self) -> usize {
		match &self.exec {
			Executor::Inline { .. } => 0,
			Executor::Workers { _handles, .. } => _handles.len(),
			Executor::External { dispatch, .. } => dispatch.worker_count(),
		}
	}

	/// Drain the chunks whose GI-window region is stale (RTGI). The
	/// caller owns budgeting: it takes everything and queues the excess,
	/// so a burst (arrival column, cold fill) never re-dirties here.
	pub fn take_gi_dirty(&mut self) -> Vec<ChunkPos> {
		self.gi_dirty.drain().collect()
	}

	/// Is this chunk inside the GI window centered on the last
	/// `gi_recenter` call? The upload drain MUST check this: a chunk
	/// outside the half-extent would alias onto an in-window column in
	/// the toroidal texture and stamp far terrain over near.
	pub fn gi_uploadable(&self, pos: ChunkPos, window_chunks: i32) -> bool {
		self.gi_center
			.is_some_and(|c| gi_in_window(pos, c, window_chunks))
	}

	/// Scroll the GI window: when the camera crosses a chunk boundary,
	/// re-dirty every resident chunk that just ENTERED the half-extent —
	/// its window texels still hold whatever far column aliased there
	/// while it was out of range. The strip that left needs nothing:
	/// reads are lateral-capped to the half-extent, so its stale texels
	/// are unreachable until it re-enters (and re-dirties here).
	pub fn gi_recenter(&mut self, center: ChunkPos, window_chunks: i32) {
		let c = (center.x, center.z);
		let old = self.gi_center;
		if old == Some(c) {
			return;
		}
		self.gi_center = Some(c);
		for &pos in self.store.keys() {
			if gi_in_window(pos, c, window_chunks)
				&& !old.is_some_and(|o| gi_in_window(pos, o, window_chunks))
			{
				self.gi_dirty.insert(pos);
			}
		}
	}

	/// Drain edited cells for probe invalidation (RTGI).
	pub fn take_gi_edits(&mut self) -> Vec<(i32, i32, i32)> {
		std::mem::take(&mut self.gi_edits)
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
		// The GI voxel window re-uploads this chunk (RTGI; GI-DESIGN.md).
		// Chunk-granular: the window has no padded shell, so an edit
		// dirties exactly its own 16³ region. Edits (unlike arrivals)
		// also invalidate nearby probes so relight starts immediately.
		self.gi_dirty.insert(pos);
		self.gi_edits.push((x, y, z));
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
/// The entire worker surface: two pure calls over owned inputs. A
/// platform job pool needs nothing else from this crate.
pub fn run_job(job: Job) -> JobResult {
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
			let mesh = mesh_chunk(&input);
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

	/// Run maintain until all queues and workers are idle, keeping every
	/// mesh emitted on the way (see `mesh_census`).
	fn quiesce_meshed(p: &mut Pipeline, center: ChunkPos) -> Vec<MeshOutput> {
		let mut meshed = Vec::new();
		for _ in 0..2000 {
			let mut m = p.maintain(center, usize::MAX, None, None);
			meshed.append(&mut m.meshed);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				return meshed;
			}
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
		panic!("pipeline never quiesced");
	}

	/// Run maintain until all queues and workers are idle.
	fn quiesce(p: &mut Pipeline, center: ChunkPos) {
		quiesce_meshed(p, center);
	}

	fn mesh_gen(p: &Pipeline, pos: ChunkPos) -> u64 {
		p.store[&pos].mesh_gen
	}

	/// The in-process twin of a Web Worker pool, and the reason the
	/// External executor is testable by plain `cargo test` at all —
	/// `digblock-net`'s loopback transport plays the same role for the
	/// wire. Jobs go through the BYTES here, so a codec mistake fails the
	/// pipeline's own tests instead of only `job_wire`'s.
	#[derive(Default)]
	struct DispatchStats {
		submitted: usize,
		resets: usize,
	}

	type Stats = std::rc::Rc<std::cell::RefCell<DispatchStats>>;

	struct ImmediateDispatch {
		ready: bool,
		/// Results allowed to sit undelivered before `submit` refuses —
		/// the saturation path a real pool hits constantly.
		cap: usize,
		done: VecDeque<JobResult>,
		/// Shared with the test, which is how "the pool actually ran
		/// these" is distinguishable from "they quietly ran inline".
		stats: Stats,
	}

	impl ImmediateDispatch {
		fn new(cap: usize) -> (Self, Stats) {
			let stats = Stats::default();
			(
				Self {
					ready: true,
					cap,
					done: VecDeque::new(),
					stats: stats.clone(),
				},
				stats,
			)
		}
	}

	impl JobDispatch for ImmediateDispatch {
		fn ready(&self) -> bool {
			self.ready
		}

		fn submit(&mut self, job: Job) -> Result<(), Job> {
			if self.done.len() >= self.cap {
				return Err(job);
			}
			self.stats.borrow_mut().submitted += 1;
			let job = Job::decode(&job.encode()).expect("job round-trip");
			let result = run_job(job);
			self.done
				.push_back(JobResult::decode(&result.encode()).expect("result round-trip"));
			Ok(())
		}

		fn poll(&mut self) -> Option<JobResult> {
			self.done.pop_front()
		}

		fn lost(&mut self) -> Option<JobKey> {
			None // runs every job it accepts, inside `submit`
		}

		fn worker_count(&self) -> usize {
			usize::from(self.ready)
		}

		fn reset(&mut self) {
			self.stats.borrow_mut().resets += 1;
			self.done.clear();
		}
	}

	/// A pool that behaves like a real one: a job is ACCEPTED now and
	/// answered whenever the test says so, or never.
	///
	/// `ImmediateDispatch` cannot model the contracts that actually bite,
	/// because it finishes every job inside `submit` — there is no such
	/// thing as a dispatched-but-unanswered job in it, so `reset`'s
	/// load-bearing rule (a reply for a pre-reset job must NEVER be
	/// delivered) and the lost-job path are both invisible to it. The
	/// state is shared with the test, which drives the two events a real
	/// pool suffers and a synchronous double cannot: a late reply and a
	/// dead worker.
	#[derive(Default)]
	struct HoldBackState {
		/// Accepted, unanswered: the job, whether its worker is still
		/// alive, and the world it was dispatched in.
		held: VecDeque<(Job, bool, u64)>,
		done: VecDeque<JobResult>,
		lost: VecDeque<JobKey>,
		/// Bumped by `reset`. Everything held under an older epoch belongs
		/// to a world that no longer exists — a real pool drops those on
		/// their dispatch id; this drops them on the epoch.
		epoch: u64,
	}

	impl HoldBackState {
		/// Run the oldest held job and deliver it. False when nothing was
		/// held.
		fn release(&mut self) -> bool {
			let Some((job, alive, epoch)) = self.held.pop_front() else {
				return false;
			};
			let key = job.key();
			let result = run_job(job);
			if epoch != self.epoch {
				// Its world is gone. Not delivered, and NOT reported lost
				// either: the current pipeline's store has never heard of
				// that chunk.
				return true;
			}
			if alive {
				self.done.push_back(result);
			} else {
				self.lost.push_back(key);
			}
			true
		}

		/// The worker holding the oldest job dies: the job was accepted,
		/// and no result will ever come back for it.
		fn kill_oldest(&mut self) {
			if let Some((_, alive, _)) = self.held.front_mut() {
				*alive = false;
			}
		}
	}

	type Held = std::rc::Rc<std::cell::RefCell<HoldBackState>>;

	struct HoldBackDispatch {
		state: Held,
		depth: usize,
	}

	impl HoldBackDispatch {
		fn new(depth: usize) -> (Self, Held) {
			let state = Held::default();
			(
				Self {
					state: state.clone(),
					depth,
				},
				state,
			)
		}
	}

	impl JobDispatch for HoldBackDispatch {
		fn ready(&self) -> bool {
			true
		}

		fn submit(&mut self, job: Job) -> Result<(), Job> {
			let mut s = self.state.borrow_mut();
			if s.held.len() >= self.depth {
				return Err(job);
			}
			let epoch = s.epoch;
			s.held.push_back((job, true, epoch));
			Ok(())
		}

		fn poll(&mut self) -> Option<JobResult> {
			self.state.borrow_mut().done.pop_front()
		}

		fn lost(&mut self) -> Option<JobKey> {
			self.state.borrow_mut().lost.pop_front()
		}

		fn worker_count(&self) -> usize {
			1
		}

		fn reset(&mut self) {
			let mut s = self.state.borrow_mut();
			s.epoch += 1;
			s.done.clear();
			s.lost.clear();
		}
	}

	/// Per-chunk air flag plus a content hash — enough to catch a dropped
	/// cell, a transposed copy, or an air chunk that decoded into a
	/// look-alike instead of the shared sentinel.
	///
	/// The hash is POSITION-WEIGHTED on purpose. A plain sum over the
	/// cells is permutation-invariant, so a codec that shuffled a chunk's
	/// contents — the exact class of mistake a bytes-crossing job pool
	/// introduces — would pass it unchanged.
	fn world_census(p: &Pipeline) -> Vec<(ChunkPos, bool, u64)> {
		let mut v: Vec<_> = p
			.store
			.iter()
			.map(|(pos, s)| match &s.state {
				ChunkState::Ready(d) => (*pos, is_air_chunk(d), cell_hash(&d.blocks)),
				ChunkState::Generating => (*pos, false, u64::MAX),
			})
			.collect();
		v.sort_by_key(|(p, _, _)| (p.x, p.y, p.z));
		v
	}

	fn cell_hash(cells: &[u16]) -> u64 {
		cells.iter().fold(0xcbf29ce484222325u64, |h, &c| {
			(h ^ u64::from(c)).wrapping_mul(0x100000001b3)
		})
	}

	/// The census `world_census` structurally cannot take: a mesh never
	/// enters the store, so a job pool that mangled vertex words on the
	/// way back would leave every stored cell intact. Keyed by chunk, the
	/// newest emission winning, hashed in emission order per chunk.
	fn mesh_census(meshed: &[MeshOutput]) -> Vec<(ChunkPos, u64)> {
		let mut by_pos: std::collections::BTreeMap<(i32, i32, i32), u64> = Default::default();
		for m in meshed {
			let h = m
				.mesh
				.solid
				.iter()
				.chain(&m.mesh.water)
				.fold(0xcbf29ce484222325u64, |h, &w| {
					(h ^ u64::from(w)).wrapping_mul(0x100000001b3)
				});
			by_pos.insert((m.pos.x, m.pos.y, m.pos.z), h);
		}
		by_pos
			.into_iter()
			.map(|((x, y, z), h)| (ChunkPos::new(x, y, z), h))
			.collect()
	}

	/// The executor is an implementation detail of WHERE jobs run, never
	/// of what they produce. Same seed through a pool and through the
	/// inline path must land the same world, sky chunks included.
	#[test]
	fn an_external_pool_builds_the_same_world_as_inline() {
		let center = ChunkPos::new(0, 0, 0);
		let mut inline = Pipeline::new(42.0, 3, 0);
		let inline_meshes = quiesce_meshed(&mut inline, center);

		let mut pooled = Pipeline::new(42.0, 3, 0);
		let (d, stats) = ImmediateDispatch::new(64);
		pooled.set_dispatch(Box::new(d));
		let pooled_meshes = quiesce_meshed(&mut pooled, center);

		assert_eq!(world_census(&pooled), world_census(&inline));
		assert_eq!(
			mesh_census(&pooled_meshes),
			mesh_census(&inline_meshes),
			"the vertices are the half of a job's output the store never sees"
		);
		assert!(
			stats.borrow().submitted > 0,
			"the pool must have run the jobs — a silent inline fallback \
			 would pass the census and prove nothing"
		);
	}

	/// A browser that refuses workers, and the seconds before a pool is
	/// up, are the same case: keep running jobs on this thread.
	#[test]
	fn an_unready_pool_runs_jobs_inline() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(42.0, 3, 0);
		let (mut d, stats) = ImmediateDispatch::new(64);
		d.ready = false;
		p.set_dispatch(Box::new(d));
		quiesce(&mut p, center);

		let mut control = Pipeline::new(42.0, 3, 0);
		quiesce(&mut control, center);
		assert_eq!(
			world_census(&p),
			world_census(&control),
			"an unavailable pool must degrade to the inline path, not stall"
		);
		assert_eq!(
			stats.borrow().submitted,
			0,
			"nothing may be handed to a pool that isn't up"
		);
	}

	/// Saturation is the steady state of a real pool: the world still has
	/// to finish through the requeue path.
	#[test]
	fn a_saturated_pool_still_finishes_the_world() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(42.0, 3, 0);
		// One outstanding result at a time — every submit past the first
		// bounces until the drain catches up.
		let (d, stats) = ImmediateDispatch::new(1);
		p.set_dispatch(Box::new(d));
		quiesce(&mut p, center);

		let mut control = Pipeline::new(42.0, 3, 0);
		quiesce(&mut control, center);
		assert_eq!(world_census(&p), world_census(&control));
		assert!(
			stats.borrow().submitted > 0,
			"a pool that refused everything would finish the world inline \
			 and prove nothing about the requeue path"
		);
	}

	/// A pool outlives the pipeline that fed it, so detaching one must
	/// abandon its in-flight work — a `Generated` result carries only a
	/// column coordinate, and the next world reuses those coordinates.
	#[test]
	fn taking_the_pool_back_resets_it() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(42.0, 3, 0);
		let (d, stats) = ImmediateDispatch::new(64);
		p.set_dispatch(Box::new(d));
		// One maintain is enough to have work outstanding.
		p.maintain(center, usize::MAX, None, None);

		let taken = p.take_dispatch().expect("a pool was attached");
		assert_eq!(
			stats.borrow().resets,
			1,
			"a detached pool must abandon the old world's jobs"
		);
		assert_eq!(p.in_flight, 0, "in-flight work went with the pool");
		assert!(
			p.take_dispatch().is_none(),
			"the executor fell back to inline"
		);
		drop(taken);

		// And the fallback still builds the world.
		quiesce(&mut p, center);
		let mut control = Pipeline::new(42.0, 3, 0);
		quiesce(&mut control, center);
		assert_eq!(world_census(&p), world_census(&control));
	}

	/// `reset`'s actual contract, which `ImmediateDispatch` cannot state:
	/// a job dispatched before a world change must never land after it.
	/// A `Generated` result carries only (x, z) — no seed — so one that
	/// leaked would stamp the OLD world's terrain into the new one at the
	/// same coordinates, and the census would show it as a world neither
	/// seed generates.
	#[test]
	fn a_pre_reset_reply_never_reaches_the_next_world() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(42.0, 3, 0);
		let (d, held) = HoldBackDispatch::new(64);
		p.set_dispatch(Box::new(d));
		// Hand the pool a batch of the FIRST world's columns, unanswered.
		p.maintain(center, usize::MAX, None, None);
		assert!(
			p.in_flight > 0 && !held.borrow().held.is_empty(),
			"the pool must be holding work"
		);

		// World change: the pool crosses over, its backlog does not.
		let taken = p.take_dispatch().expect("a pool was attached");
		let mut next = Pipeline::new(7.0, 3, 0);
		next.set_dispatch(taken);
		// Every one of those held jobs now finishes — real, decodable
		// results for real columns, from the wrong seed — and the new
		// world builds around them.
		for _ in 0..8000 {
			while held.borrow_mut().release() {}
			let m = next.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}

		let mut control = Pipeline::new(7.0, 3, 0);
		quiesce(&mut control, center);
		assert_eq!(
			world_census(&next),
			world_census(&control),
			"a previous world's column landed in this one"
		);
	}

	/// A worker that dies with jobs in hand is the failure the whole
	/// `lost` path exists for: nothing else remembers those chunks were
	/// dispatched, so without a requeue the column sits `Generating`
	/// forever (a hole in the world) or the chunk never meshes again
	/// (invisible terrain).
	#[test]
	fn a_pool_that_loses_jobs_still_finishes_the_world() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(42.0, 3, 0);
		let (d, held) = HoldBackDispatch::new(8);
		p.set_dispatch(Box::new(d));

		let mut killed = 0usize;
		for _ in 0..8000 {
			{
				// The worker holding the oldest job dies, over and over,
				// until the pipeline has had to reclaim its way through a
				// long run of them. Bounded so the run still terminates:
				// the heaps are nearest-first, so an unbounded kill would
				// keep killing the same nearest column forever.
				let mut h = held.borrow_mut();
				if killed < 50 && !h.held.is_empty() {
					h.kill_oldest();
					killed += 1;
				}
				while h.release() {}
			}
			let m = p.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}
		assert!(killed > 0, "no job was ever lost — the test proved nothing");

		let mut control = Pipeline::new(42.0, 3, 0);
		quiesce(&mut control, center);
		assert_eq!(
			world_census(&p),
			world_census(&control),
			"a lost job left a permanent hole in the world"
		);
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

	/// Work in a pool is still work. `top_up_workers` drains BOTH heaps
	/// into the pool, so a heaps-only urgency reads the busiest frame of
	/// the fill as idle and collapses the shared streaming deadline to its
	/// floor — exactly as those results start landing on this thread to be
	/// absorbed. With the inline executor this was unreachable: a job left
	/// the heap and finished inside the same call.
	#[test]
	fn urgency_counts_work_that_is_in_the_pool() {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(7.5, 8, 0);
		// Deep enough to swallow the whole first scan, and holding: after
		// this call the heaps are empty and every job is outstanding.
		let (d, held) = HoldBackDispatch::new(1 << 16);
		p.set_dispatch(Box::new(d));
		p.maintain(center, usize::MAX, None, None);
		assert!(
			p.gen_heap.is_empty() && p.mesh_heap.is_empty(),
			"the pool must have taken the whole backlog for this to mean anything"
		);
		assert!(p.in_flight > 0, "and be holding it");
		assert_eq!(
			p.stream_urgency(),
			1.0,
			"near-field work in a pool is near-field work"
		);

		for _ in 0..8000 {
			while held.borrow_mut().release() {}
			let m = p.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}
		assert_eq!(p.stream_urgency(), 0.0, "quiescent again once absorbed");
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

#[cfg(test)]
mod gi_window_tests {
	use super::*;

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

	/// THE invariant the toroidal window rests on: the admitted columns are
	/// exactly one period wide, so no two of them share a texel. `w` columns
	/// starting at `center - w/2` is a complete residue system mod `w` —
	/// admit one more and it aliases onto a column already in the window,
	/// stamping far terrain over near with no error anywhere.
	#[test]
	fn the_gi_window_admits_exactly_one_period_per_axis() {
		for w in [4, 8, 20, 21] {
			for center in [(0, 0), (3, -7), (-40, 96)] {
				let mut seen = std::collections::HashSet::new();
				let mut count = 0;
				for x in (center.0 - 2 * w)..(center.0 + 2 * w) {
					for z in (center.1 - 2 * w)..(center.1 + 2 * w) {
						if !gi_in_window(ChunkPos::new(x, 0, z), center, w) {
							continue;
						}
						count += 1;
						// The toroidal slot this column would occupy.
						let slot = (x.rem_euclid(w), z.rem_euclid(w));
						assert!(
							seen.insert(slot),
							"w {w} center {center:?}: column ({x},{z}) aliases onto \
							 slot {slot:?}, which another admitted column already owns"
						);
					}
				}
				assert_eq!(count, w * w, "w {w} center {center:?}: admitted count");
			}
		}
	}

	/// `gi_uploadable` is the gate on the upload drain, and it must refuse
	/// everything until a centre exists — an upload before the first
	/// `gi_recenter` has no window to be inside of.
	#[test]
	fn nothing_is_uploadable_before_the_window_is_centered() {
		let p = Pipeline::new(0.0, 4, 0);
		assert!(!p.gi_uploadable(ChunkPos::new(0, 0, 0), 8));
	}

	/// Scrolling must re-dirty every resident column that just ENTERED the
	/// half-extent: while it was outside, its texels held whatever far
	/// column aliased into them. The strip that LEFT needs nothing, because
	/// reads are capped to the half-extent and its stale texels are
	/// unreachable until it re-enters (and re-dirties here).
	#[test]
	fn recentering_redirties_exactly_the_columns_that_entered() {
		let w = 4;
		let mut p = Pipeline::new(0.0, 6, 0);
		quiesce(&mut p, ChunkPos::new(0, 0, 0));

		p.gi_recenter(ChunkPos::new(0, 0, 0), w);
		let _ = p.take_gi_dirty();

		// One chunk east. Columns at the new leading edge enter; the
		// trailing edge leaves.
		p.gi_recenter(ChunkPos::new(1, 0, 0), w);
		let dirty: std::collections::HashSet<ChunkPos> = p.take_gi_dirty().into_iter().collect();

		let resident: Vec<ChunkPos> = p.store.keys().copied().collect();
		assert!(!resident.is_empty(), "test needs resident chunks");
		for pos in resident {
			let was_in = gi_in_window(pos, (0, 0), w);
			let is_in = gi_in_window(pos, (1, 0), w);
			let entered = is_in && !was_in;
			assert_eq!(
				dirty.contains(&pos),
				entered,
				"{pos:?}: entered={entered} but dirty={}",
				dirty.contains(&pos)
			);
		}
	}

	/// A recentre onto the SAME chunk is a no-op — the camera moving within
	/// one chunk must not re-upload the window every frame.
	#[test]
	fn recentering_onto_the_same_chunk_dirties_nothing() {
		let mut p = Pipeline::new(0.0, 4, 0);
		quiesce(&mut p, ChunkPos::new(0, 0, 0));
		p.gi_recenter(ChunkPos::new(0, 0, 0), 8);
		let _ = p.take_gi_dirty();
		p.gi_recenter(ChunkPos::new(0, 0, 0), 8);
		assert!(p.take_gi_dirty().is_empty());
	}

	/// An edit dirties exactly its own column for the window (which has no
	/// padded shell, unlike a mesh job) and queues the cell for probe
	/// invalidation so relight starts without waiting for the sweep.
	#[test]
	fn an_edit_dirties_its_own_column_and_queues_a_probe_invalidation() {
		let mut p = Pipeline::new(0.0, 4, 0);
		let center = ChunkPos::new(0, 0, 0);
		quiesce(&mut p, center);
		p.gi_recenter(center, 8);
		let _ = p.take_gi_dirty();
		let _ = p.take_gi_edits();

		// An edit well inside a chunk, so no neighbour shell is involved.
		let (x, y, z) = (8, 40, 8);
		let cur = p.block_at(x, y, z);
		let next = if cur == Some(crate::block::BlockType::Stone) {
			crate::block::BlockType::Dirt
		} else {
			crate::block::BlockType::Stone
		};
		assert!(p.set_block(x, y, z, next).is_some(), "edit landed");

		let dirty = p.take_gi_dirty();
		assert_eq!(
			dirty,
			vec![ChunkPos::new(0, y.div_euclid(16), 0)],
			"exactly the edited column, no shell"
		);
		assert_eq!(p.take_gi_edits(), vec![(x, y, z)]);
	}
}
