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

mod build;
mod edits;
mod gi;
mod jobs;
mod mesh_input;
mod residency;

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
