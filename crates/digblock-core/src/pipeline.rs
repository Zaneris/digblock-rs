//! Threaded chunk pipeline: the main thread owns the store and scheduling,
//! workers run pure functions (terrain gen, meshing) over immutable snapshots.
//!
//! Staleness rides generation counters, not flags: every mesh job carries the
//! `mesh_gen` it was built from, and results that don't match the slot's
//! current counter — or whose slot was evicted — are dropped. A chunk that
//! finishes generating bumps its own and its Ready neighbours' counters: face
//! neighbours unconditionally, edge and corner diagonals only where the shared
//! padded shell could shade an existing face.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::chunk::{ChunkData, ChunkPos, WORLD_VCHUNK, is_air_chunk};
use crate::mesher::{MeshData, MeshInput, Src, mesh_chunk};
use crate::metrics::JobTiming;
use crate::worldgen::build_column;

pub struct MeshOutput {
	pub pos: ChunkPos,
	pub generation: u64,
	pub mesh: MeshData,
}

pub enum Job {
	/// Generate the whole vertical chunk stack of column (x, z) — one
	/// heightmap evaluation serves every chunk in the stack.
	///
	/// Deliberately NOT sliced into resumable continuations. Measured in
	/// Chrome on Apple Silicon at draw 20, a gen job fits inside a frame
	/// with room to spare, so slicing would refactor worldgen's column path
	/// for a possibly unmeasurable win. Revisit only if gen shows up as the
	/// tall pole on weaker hardware — `?spikelog` attributes it and
	/// `maintain_ms` bounds it. Do not do this refactor without that
	/// evidence first.
	Generate { x: i32, z: i32, seed: f64 },
	Mesh {
		pos: ChunkPos,
		generation: u64,
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
/// A pool that loses a job — a worker that trapped, was killed, or answered
/// with a refusal — hands these back through [`JobDispatch::lost`]. A popped
/// job is remembered NOWHERE else, so without the requeue its column sits
/// `Generating` forever (a hole in the world) or never meshes again (invisible
/// terrain). The payload is deliberately not retained: a `Mesh` job's
/// `MeshInput` is a whole padded snapshot, dozens may be outstanding, and the
/// pipeline rebuilds it from the store at redispatch anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKey {
	Generate { x: i32, z: i32 },
	Mesh { pos: ChunkPos, generation: u64 },
}

impl Job {
	pub fn key(&self) -> JobKey {
		match self {
			Job::Generate { x, z, .. } => JobKey::Generate { x: *x, z: *z },
			Job::Mesh {
				pos, generation, ..
			} => JobKey::Mesh {
				pos: *pos,
				generation: *generation,
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
	/// Generation of the last accepted mesh, if that mesh was empty AND was
	/// built with every face neighbor present — a buried chunk can then skip
	/// re-dirties, since neighbours' shell cells only shade existing faces
	/// and never create them.
	///
	/// The face-neighbor condition is load-bearing. `Src::Missing`
	/// SUPPRESSES a whole border (`MeshInput::missing`) rather than reading
	/// as air, so a chunk meshed beside an ungenerated neighbour is empty
	/// for a reason that expires; trusting that emptiness leaves a permanent
	/// hole at every cave mouth and every seam on the column that generated
	/// first.
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
	/// Every cell this world has ever written, local or authoritative.
	///
	/// Three jobs: **re-applying** edits onto a column that regenerated,
	/// which is what makes hard eviction safe; **answering "has this cell
	/// been touched?"** for the node reveal; and, locally, **reporting**
	/// what to persist. The first two run in multiplayer too — what differs
	/// is where the map comes from, since `set_cell` stops recording there
	/// (`record_local`) and `apply_authoritative` takes over, so the map
	/// holds the JOURNAL rather than this client's guesses. Re-application
	/// has no other source: the server prunes a client's synced set on a
	/// radius WIDER than the client's residency, so a hard-evicted column is
	/// never re-sent.
	edits: crate::overlay::EditOverlay,
	/// Record local writes into `edits` and report them for persistence:
	/// the single-player save path. False in multiplayer, where a local
	/// write is a PREDICTION and `apply_authoritative` records the
	/// server's answer instead — recording a guess is unfixable, since a
	/// rejection reverts through `set_cell` and `record` is insert-only.
	record_local: bool,
	/// The looker's skill levels, which decide which node kinds the mesh
	/// stamp reveals (SKILLS-DESIGN §5.3 — Mining for the ore tiers,
	/// Alchemy for the surface nodes). Crossing a threshold re-meshes
	/// everything resident — see [`Pipeline::set_levels`].
	levels: crate::worldgen::nodes::Levels,
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
	/// Abandon everything in flight: after this returns, a result for any job
	/// submitted before it must NEVER be delivered.
	///
	/// A pool outlives the pipeline that fed it (see `take_dispatch`), and a
	/// `Generated` result carries only a column coordinate — so one in flight
	/// from the old world would land as authoritative terrain in the new one,
	/// same (x, z), different seed. A pool that cannot recall a dispatched job
	/// must discard its reply instead. Abandoned jobs are NOT reported through
	/// `lost` (the new store has never heard of those chunks), and any
	/// per-worker depth accounting must be released here, or the pool refuses
	/// the new world's jobs while `ready` stays true — the one state where
	/// nothing streams and nothing runs inline.
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
/// batch immediately (a column is a whole stack of chunks of
/// uninterruptible work); otherwise up to this many mesh jobs run. A
/// count, not a wall-clock deadline, because job granularity is what
/// actually bounds the stall. With an `inline_allowance`, more jobs may
/// follow (see `run_inline_jobs`); this floor always runs regardless.
const INLINE_MESH_CAP: usize = 4;

/// Worst-of-recent window for inline job-cost prediction.
const COST_N: usize = 8;

/// Fraction of the lifetime worst that survives as a floor under the
/// recent window. Admission control needs a PESSIMISTIC bound, not a
/// recent average: gen cost is bimodal (a flat ocean column is far cheaper
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
	generation: CostRing,
	mesh: CostRing,
}

impl JobCost {
	fn new() -> Self {
		Self {
			generation: CostRing::new(12.0),
			mesh: CostRing::new(3.0),
		}
	}

	fn observe(&mut self, is_gen: bool, ms: f32) {
		let ring = if is_gen {
			&mut self.generation
		} else {
			&mut self.mesh
		};
		ring.observe(ms);
	}

	fn predicted(&self, is_gen: bool) -> std::time::Duration {
		let ring = if is_gen { &self.generation } else { &self.mesh };
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
/// removes that churn for the price of the rim band's memory (a few
/// percent more resident columns). Adds still use the exact draw distance —
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
/// Indexed [((oy+1)*3 + (oz+1))*3 + (ox+1)]; the whole scan is a couple of
/// border planes' worth of byte reads.
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

	/// Edits recorded since the last drain (single-player persistence);
	/// empty when no overlay is installed.
	pub fn take_recorded_edits(&mut self) -> Vec<(i32, i32, i32, u16)> {
		std::mem::take(&mut self.recorded)
	}

	/// The persistent edit overlay, when this world OWNS one — the
	/// single-player save path. Multiplayer keeps and re-applies the same
	/// map (the ore reveal reads it, and eviction would otherwise lose
	/// the server's edits), but it is a mirror of somebody else's
	/// journal, so there is nothing here to hand a store.
	pub fn overlay(&self) -> Option<&crate::overlay::EditOverlay> {
		self.record_local.then_some(&self.edits)
	}

	/// Set the skill levels the node reveal stamps at, returning true when
	/// that crosses some node's reveal threshold — on either axis — and
	/// every resident chunk therefore has to re-mesh. The caller decides
	/// what to do about it (the engine sweeps and says so in the feed) —
	/// this only reports the crossing, so a run of level-ups does not
	/// re-mesh once per level.
	pub fn set_levels(&mut self, levels: crate::worldgen::nodes::Levels) -> bool {
		let was = std::mem::replace(&mut self.levels, levels);
		crate::worldgen::nodes::NODES.iter().any(|&n| {
			crate::unlock::reveal(n)
				.is_some_and(|need| (was.mining < need) != (levels.mining < need))
				|| crate::unlock::alchemy_reveal(n)
					.is_some_and(|need| (was.alchemy < need) != (levels.alchemy < need))
		})
	}

	pub fn levels(&self) -> crate::worldgen::nodes::Levels {
		self.levels
	}

	/// Has this cell ever been written in this world? The ore reveal's
	/// anti-dupe gate, answered from the same map the mesh stamp reads —
	/// so the client's dig prediction and its own rendering agree, and
	/// both agree with the server's journal.
	pub fn cell_edited(&self, x: i32, y: i32, z: i32) -> bool {
		self.edits
			.column(x.div_euclid(16), z.div_euclid(16))
			.is_some_and(|c| c.contains_key(&(x, y, z)))
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
		// Inline jobs run before the drain so their results land this frame;
		// workers get topped up after.
		//
		// The deadline is deliberately NOT extended by what the jobs spent —
		// that makes the stages ADD UP, which is the miss the budget exists
		// to avoid. Slicing bounds the overrun at the source and every stage
		// keeps a minimum-progress floor, so a blown deadline delays work
		// without ever starving it.
		//
		// An external pool's results arrived as their own event-loop tasks,
		// so pull them into the queue the drain reads first — under the
		// deadline, because every `poll` decodes, but always at least one, or
		// the pool's inbox only grows.
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
			// purpose: this is a handful of small keys in a failure that
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
	/// cadence, once per chunk boundary crossed.
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
					// column's chunks enter and leave together, so its
					// bottom chunk is the probe `next_gen_job` uses too.
					if !matches!(
						self.store.get(&ChunkPos::new(x, 0, z)).map(|s| &s.state),
						Some(ChunkState::Generating)
					) {
						out.stale_dropped += 1;
						continue;
					}
					// Re-apply persistent edits BEFORE the per-chunk
					// air classification below: a block placed in a sky
					// chunk must defeat `is_air_chunk` or it would never
					// mesh. `Arc::make_mut` clones the shared AIR_CHUNK
					// sentinel rather than mutating it.
					//
					// Multiplayer included: the map there mirrors the
					// server's journal, and without this a hard-evicted
					// column comes back as pristine worldgen and STAYS
					// that way — `apply_authoritative` drops a cell the
					// moment it lands and the server never resends it.
					if let Some(cells) = self.edits.column(x, z) {
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
						// Cross-border re-dirty: every Ready neighbour
						// re-meshes. Diagonals only shade existing faces
						// via the padded shell, so they are skipped for
						// air chunks — without that the buried bulk of the
						// world remeshes 26-ways on every generation.
						//
						// The empty-mesh and queued-remesh skips below
						// apply to FACES too, sound only because this is
						// the ARRIVAL path: coverage only ever INCREASES,
						// and more coverage can only cull faces. Edits
						// remove coverage, so `set_cell` bumps
						// unconditionally. Read `empty_mesh_gen`'s doc
						// before widening this skip.
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
						Some(slot) if slot.mesh_gen == m.generation => {
							slot.empty_mesh_gen =
								(m.mesh.is_empty() && !slot.mesh_missing).then_some(m.generation);
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
	/// over a whole `MeshInput` that native's channel send does not
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
				// to redo. Same bottom-chunk probe `next_gen_job` uses.
				if !matches!(
					self.store.get(&ChunkPos::new(x, 0, z)).map(|s| &s.state),
					Some(ChunkState::Generating)
				) {
					return;
				}
				let d = dist2(ChunkPos::new(x, 0, z), self.center);
				self.gen_heap.push((Reverse(d), (x, z)));
			}
			JobKey::Mesh { pos, generation } => {
				let Some(slot) = self.store.get_mut(&pos) else {
					return; // evicted
				};
				if slot.submitted_gen == generation {
					slot.submitted_gen = generation.saturating_sub(1);
				}
				let d = dist2(ChunkPos::new(pos.x, 0, pos.z), self.center);
				self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
			}
		}
	}

	fn next_gen_job(&mut self) -> Option<Job> {
		let (_, (x, z)) = self.gen_heap.pop()?;
		// The column's chunks enter and leave the store together; probe the
		// bottom one.
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
		let generation = slot.mesh_gen;
		let input = Box::new(self.build_mesh_input(pos, data));
		let missing = input.missing.iter().any(|&m| m);
		let slot = self.store.get_mut(&pos).expect("slot exists");
		slot.submitted_gen = generation;
		slot.mesh_missing = missing;
		Some(Job::Mesh {
			pos,
			generation,
			input,
		})
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
		input.nodes = self.node_stamp(pos, &input.blocks);
		input
	}

	/// The node reveal's per-job parcel, or `None` when this chunk cannot show
	/// a node at all. The edited-cell list is gathered HERE because this is the
	/// only place holding the map — the worker has block ids and nothing else.
	///
	/// Runs on the MAIN thread once per mesh job, out of the streaming budget,
	/// which is why both guards exist. No node source reaching the box means no
	/// stamp: the deposit half asks worldgen over the same box `apply_nodes`
	/// walks, the surface halves scan the freshly built SNAPSHOT for hosts, and
	/// the verdict rides the stamp as [`crate::mesher::NodeHosts`]. Otherwise
	/// the edited-cell walk takes whichever side is smaller — `EditOverlay` has
	/// no eviction and multiplayer keeps one fed by every broadcast edit, so
	/// exactly where players build a column holds tens of thousands of entries.
	///
	/// The reveal LEVELS are NOT a guard, though they were written as one: the
	/// first surface node reveals at the level everyone starts at. The host
	/// scan is the gate.
	fn node_stamp(
		&self,
		pos: ChunkPos,
		blocks: &[u16; crate::mesher::PADDED_VOLUME],
	) -> Option<crate::mesher::NodeStamp> {
		use crate::worldgen::ore;
		let n = crate::chunk::CHUNK_SIZE as i32;
		let origin = (pos.x * n - 1, pos.y * n - 1, pos.z * n - 1);
		let span = crate::mesher::PADDED as i32 - 1;
		let far = (origin.0 + span, origin.1 + span, origin.2 + span);
		// Which hosts could matter HERE — the levels and the depth, asked
		// before the scan so the scan only has to recognize ids that are
		// still in play. Deposits are the one half a level test can still
		// skip outright, and the depth test beside it is the half that
		// really pays: most of a world is nowhere near the bands.
		let reveal_met = |b| crate::worldgen::nodes::reveal_met(b, self.levels);
		let want_deposits = origin.1 <= ore::MAX_DEPOSIT_Y + ore::MAX_RADIUS
			&& (self.levels.mining >= ore::lowest_reveal()
				|| self.levels.alchemy >= ore::lowest_alchemy_reveal());
		let want_heartwood = reveal_met(crate::block::BlockType::Heartwood);
		let want_springs = reveal_met(crate::block::BlockType::Springstone);
		if !want_deposits && !want_heartwood && !want_springs {
			return None;
		}
		let (mut stone, mut wood, mut sand) = (false, false, false);
		for &cell in blocks.iter() {
			// Only the hosts still in play are worth recognizing.
			match cell & crate::chunk::ID_MASK {
				id if want_deposits && id == crate::block::BlockType::Stone as u16 => stone = true,
				id if want_heartwood && id == crate::block::BlockType::Wood as u16 => wood = true,
				id if want_springs && id == crate::block::BlockType::Sand as u16 => sand = true,
				_ => {}
			}
		}
		// `deposits_near` is the expensive half and asks worldgen, so it
		// waits until a host has actually been seen.
		let hosts = crate::mesher::NodeHosts {
			deposits: stone && !ore::deposits_near(origin, far, self.seed).is_empty(),
			heartwood: wood,
			springs: sand,
		};
		if hosts.is_empty() {
			return None;
		}
		let mut cols = Vec::new();
		let mut entries = 0usize;
		for cz in origin.2.div_euclid(n)..=(origin.2 + span).div_euclid(n) {
			for cx in origin.0.div_euclid(n)..=(origin.0 + span).div_euclid(n) {
				if let Some(cells) = self.edits.column(cx, cz) {
					entries += cells.len();
					cols.push(((cx, cz), cells));
				}
			}
		}
		let volume = ((span + 1) as usize).pow(3);
		let mut edited = Vec::new();
		if entries <= volume {
			for (_, cells) in &cols {
				for &(x, y, z) in cells.keys() {
					let (lx, ly, lz) = (x - origin.0, y - origin.1, z - origin.2);
					if (0..=span).contains(&lx)
						&& (0..=span).contains(&ly)
						&& (0..=span).contains(&lz)
					{
						edited.push(crate::mesher::pidx(lx - 1, ly - 1, lz - 1) as u16);
					}
				}
			}
		} else {
			// One lookup per cell of the box, and each cell belongs to
			// exactly one column — so this is `volume` probes in total, not
			// per column.
			for ((cx, cz), cells) in &cols {
				let x0 = (cx * n).max(origin.0);
				let x1 = (cx * n + n - 1).min(origin.0 + span);
				let z0 = (cz * n).max(origin.2);
				let z1 = (cz * n + n - 1).min(origin.2 + span);
				for z in z0..=z1 {
					for x in x0..=x1 {
						for y in origin.1..=origin.1 + span {
							if cells.contains_key(&(x, y, z)) {
								edited.push(crate::mesher::pidx(
									x - origin.0 - 1,
									y - origin.1 - 1,
									z - origin.2 - 1,
								) as u16);
							}
						}
					}
				}
			}
		}
		edited.sort_unstable();
		Some(crate::mesher::NodeStamp {
			seed: self.seed,
			origin,
			levels: self.levels,
			hosts,
			edited,
		})
	}

	fn requeue(&mut self, job: Job) {
		match job {
			Job::Generate { x, z, .. } => {
				let d = dist2(ChunkPos::new(x, 0, z), self.center);
				self.gen_heap.push((Reverse(d), (x, z)));
			}
			Job::Mesh {
				pos, generation, ..
			} => {
				// Roll back the submitted marker so the job reschedules.
				if let Some(slot) = self.store.get_mut(&pos)
					&& slot.submitted_gen == generation
				{
					slot.submitted_gen = generation - 1;
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
	/// (`stream_budget::urgency` of the nearest pending job): 1 = work at the
	/// player, 0 = rim churn or nothing pending. Both heaps are keyed by flat
	/// d² from center so this is a peek; a stale entry can only overestimate.
	///
	/// **In-flight work counts.** `top_up_workers` drains both heaps into the
	/// pool, so the frame the backlog is handed over would read as idle and
	/// collapse the shared deadline to its floor — precisely as those results
	/// start landing here to be absorbed. `in_flight_min_d2` keeps "how near
	/// is the work" independent of WHERE it is running.
	pub fn stream_urgency(&self) -> f32 {
		let generation = self.gen_heap.peek().map(|(Reverse(d), _)| *d);
		let mesh = self.mesh_heap.peek().map(|(Reverse(d), _)| *d);
		let Some(d2) = generation
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
		// COW: clones the cell array only if a worker still holds a snapshot.
		Arc::make_mut(data).set_cell(lx, ly, lz, cell);
		// The GI voxel window re-uploads this chunk (RTGI; GI-DESIGN.md).
		// Chunk-granular: the window has no padded shell, so an edit
		// dirties exactly its own chunk. Edits (unlike arrivals)
		// also invalidate nearby probes so relight starts immediately.
		self.gi_dirty.insert(pos);
		self.gi_edits.push((x, y, z));
		// Recorded here ONLY where this write is the authority. In
		// multiplayer it is a prediction and `apply_authoritative` records
		// the server's answer instead, so the map fills with what the
		// JOURNAL holds.
		//
		// The difference is rejections. A refusal reverts through this same
		// function and `EditOverlay::record` is insert-only, so recording a
		// prediction leaves a thrown-out cell marked edited forever — the
		// two ends then answer `nodes::dig_mode` differently about it and
		// the cell is permanently un-minable. Every rejection reason reaches
		// here.
		if self.record_local {
			self.edits.record(x, y, z, cell);
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
					if let Some(nslot) = self.store.get_mut(&np)
						&& matches!(nslot.state, ChunkState::Ready(_))
					{
						nslot.mesh_gen += 1;
						self.mesh_heap.push((
							Reverse(dist2(ChunkPos::new(np.x, 0, np.z), center)),
							(np.x, np.y, np.z),
						));
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
		// Recorded BEFORE the applied/deferred split: a cell the server has
		// written is a cell the ore reveal must treat as spent whether or
		// not its chunk happens to be resident yet.
		let newly_edited = self.edits.record(x, y, z, cell);
		let col = (x.div_euclid(16), z.div_euclid(16));
		// A record can change what a chunk MESHES without changing a single
		// cell. `set_cell` early-returns when the value already matches and
		// never bumps `mesh_gen`, but the reveal keys on "edited", not on
		// the value — so an LWW re-application or a re-streamed column
		// writing Stone over Stone left a seam still glittering in a chunk
		// whose dig path had just started calling it spent. Holding on it
		// dug plain stone and destroyed the deposit for good.
		if newly_edited && self.cell_at(x, y, z) == Some(cell) {
			self.remesh_at(x, y, z);
		}
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

	/// Re-mesh the chunk owning this cell, without writing anything. For
	/// the one case where a chunk's MESH depends on something other than
	/// its cells: the ore reveal reads the edit overlay, so recording a
	/// cell can change what its column draws while every value stays put.
	/// Neighbours are deliberately not bumped — the reveal is a per-cell
	/// stamp inside the owning chunk and no padded shell reads it.
	fn remesh_at(&mut self, x: i32, y: i32, z: i32) {
		let Some(((cx, cy, cz), _)) = crate::chunk::split_world(x, y, z) else {
			return;
		};
		let pos = ChunkPos::new(cx, cy, cz);
		let Some(slot) = self.store.get_mut(&pos) else {
			return;
		};
		if !matches!(slot.state, ChunkState::Ready(_)) {
			return;
		}
		slot.mesh_gen += 1;
		let center = self.center;
		self.mesh_heap.push((
			Reverse(dist2(ChunkPos::new(pos.x, 0, pos.z), center)),
			(pos.x, pos.y, pos.z),
		));
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
		Job::Mesh {
			pos,
			generation,
			mut input,
		} => {
			// The node reveal runs HERE, in the worker, before anything
			// reads the array: it rewrites qualifying pristine host cells
			// to their node's block id, so the mesher, the texture path
			// and the emissive bit all just see a real node block (§5.3).
			input.apply_nodes();
			let mesh = mesh_chunk(&input);
			JobResult::Meshed {
				out: MeshOutput {
					pos,
					generation,
					mesh,
				},
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
mod tests;

#[cfg(test)]
mod gi_window_tests;
