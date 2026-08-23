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

/// A pool that behaves like a real one: a job is ACCEPTED now and answered whenever the test says
/// so, or never. `ImmediateDispatch` finishes every job inside `submit`, so a
/// dispatched-but-unanswered job does not exist in it, and `reset`'s load-bearing rule — a reply
/// for a pre-reset job must NEVER be delivered — and the lost-job path are both invisible to it.
/// The state is shared with the test, which drives a late reply and a dead worker.
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

/// Per-chunk air flag plus a content hash — enough to catch a dropped cell, a transposed copy, or
/// an air chunk that decoded into a look-alike instead of the shared sentinel. The hash is
/// POSITION-WEIGHTED on purpose: a plain sum over the cells is permutation-invariant, so a codec
/// that shuffled a chunk's contents — the exact class of mistake a bytes-crossing job pool
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

/// `reset`'s actual contract, which `ImmediateDispatch` cannot state: a job dispatched before a
/// world change must never land after it. A `Generated` result carries only (x, z) — no seed — so
/// one that leaked would stamp the OLD world's terrain into the new one at the same coordinates,
/// and the census would show a world neither seed generates.
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

/// Every slot that owes a mesh has a heap entry to carry it.
///
/// `queue_mesh` skips the push whenever work is already owed, so this is the invariant the whole
/// dedupe rests on: a slot that lost its only entry never meshes again, which reads as a permanent
/// hole rather than as a dropped frame.
fn assert_owed_slots_are_queued(p: &Pipeline) -> usize {
	let queued: FxHashSet<ChunkPos> = p
		.mesh_heap
		.iter()
		.map(|&(_, (x, y, z))| ChunkPos::new(x, y, z))
		.collect();
	let mut owed = 0;
	for (pos, slot) in &p.store {
		if slot.mesh_gen > slot.submitted_gen {
			assert!(
				queued.contains(pos),
				"{pos:?} owes a mesh with no heap entry"
			);
			owed += 1;
		}
	}
	owed
}

/// A burst of edits leaves ONE heap entry per slot, not one per edit — and every slot it dirtied
/// still meshes. The two halves are the same property from both ends: the dedupe is only safe
/// because nothing it skips was needed, so a test that checked the count alone could pass while
/// stranding a chunk, and one that checked only the meshes could pass having deduped nothing.
#[test]
fn an_edit_burst_queues_each_slot_once_and_strands_none() {
	let mut p = Pipeline::new(0.0, 4, 2);
	let center = ChunkPos::new(0, 0, 0);
	quiesce(&mut p, center);
	assert!(p.mesh_heap.is_empty(), "a quiesced pipeline owes nothing");

	// A run down one column plus both border walls, so the burst hits the self, face, edge and
	// corner pushes — the sites that queue up to eight slots for a single cell.
	let other = |cur: Option<BlockType>| match cur {
		Some(BlockType::Stone) => BlockType::Dirt,
		_ => BlockType::Stone,
	};
	let mut edits = 0u32;
	let mut touched = FxHashSet::default();
	for y in 0..16 {
		for &(x, z) in &[(8, 8), (0, 8), (8, 0), (0, 0), (15, 15)] {
			let new = other(p.block_at(x, y, z));
			if p.set_block(x, y, z, new).is_some() {
				edits += 1;
				let (cx, cy, cz) = (x.div_euclid(16), y.div_euclid(16), z.div_euclid(16));
				touched.insert(ChunkPos::new(cx, cy, cz));
			}
		}
	}
	assert!(edits > 16, "the burst has to be bigger than the slot count");

	let owed = assert_owed_slots_are_queued(&p);
	// The tight form, not a bound: one entry per owed slot is exactly what the dedupe promises,
	// and `edits` is far larger — every extra push it skipped would show here.
	assert_eq!(p.mesh_heap.len(), owed, "one entry per owed slot");
	assert!(owed < edits as usize, "{owed} slots for {edits} edits");

	// And nothing was stranded: every slot the burst dirtied comes back with a mesh.
	let meshed = quiesce_meshed(&mut p, center);
	let built: FxHashSet<ChunkPos> = meshed.iter().map(|m| m.pos).collect();
	for pos in &touched {
		assert!(built.contains(pos), "{pos:?} was dirtied and never meshed");
	}
	assert_eq!(
		assert_owed_slots_are_queued(&p),
		0,
		"quiesced with work owed"
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
	for (pos, generation) in &before {
		let expect = generation + u64::from(*pos == ChunkPos::new(0, 0, 0));
		assert_eq!(mesh_gen(&p, *pos), expect, "{pos:?}");
	}

	// No-op returns None and bumps nothing.
	let before = snapshot(&p);
	assert!(p.set_block(8, 8, 8, new).is_none());
	for (pos, generation) in &before {
		assert_eq!(mesh_gen(&p, *pos), *generation);
	}

	// An edit on a chunk face: it touches only the −x border, so the
	// bumped set is self + exactly 1 neighbor.
	let before = snapshot(&p);
	let new = other(p.block_at(0, 8, 8));
	p.set_block(0, 8, 8, new).expect("face edit");
	for (pos, generation) in &before {
		let bumped = *pos == ChunkPos::new(0, 0, 0) || *pos == ChunkPos::new(-1, 0, 0);
		assert_eq!(
			mesh_gen(&p, *pos),
			generation + u64::from(bumped),
			"{pos:?}"
		);
	}

	// An edit in the world's bottom corner cell: it touches the −x, −z
	// and −y borders, and the chunks below are outside the world, so
	// the bumped set is self + the two faces + their shared diagonal.
	let before = snapshot(&p);
	let new = other(p.block_at(0, 0, 0));
	p.set_block(0, 0, 0, new).expect("corner edit");
	for (pos, generation) in &before {
		let bumped = *pos == ChunkPos::new(0, 0, 0)
			|| *pos == ChunkPos::new(-1, 0, 0)
			|| *pos == ChunkPos::new(0, 0, -1)
			|| *pos == ChunkPos::new(-1, 0, -1);
		assert_eq!(
			mesh_gen(&p, *pos),
			generation + u64::from(bumped),
			"{pos:?}"
		);
	}

	// Out-of-world y is rejected.
	assert!(p.set_block(0, -1, 0, BlockType::Stone).is_none());
	assert!(p.set_block(0, 1000, 0, BlockType::Stone).is_none());
}

/// The client twin of the server's
/// A reveal sweep costs a re-mesh of everything resident, so it has to
/// fire exactly on the reveal thresholds and at no other level — which
/// also means one award that crosses every one of them sweeps ONCE,
/// not once per tier.
#[test]
fn crossing_a_reveal_threshold_asks_for_a_sweep_and_nothing_else_does() {
	use crate::worldgen::nodes::{Levels, NODES};
	let mining_thresholds: Vec<u32> = NODES
		.iter()
		.filter_map(|&o| crate::unlock::reveal(o))
		.collect();
	let alchemy_thresholds: Vec<u32> = NODES
		.iter()
		.filter_map(|&o| crate::unlock::alchemy_reveal(o))
		.collect();
	let at = |mining, alchemy| Levels { mining, alchemy };
	let mut p = Pipeline::new(0.0, 4, 0);
	p.set_levels(at(1, 1));
	for level in 2..=crate::skills::MAX_LEVEL {
		assert_eq!(
			p.set_levels(at(level, 1)),
			mining_thresholds.contains(&level),
			"Mining stepping to {level}"
		);
	}
	for level in 2..=crate::skills::MAX_LEVEL {
		assert_eq!(
			p.set_levels(at(crate::skills::MAX_LEVEL, level)),
			alchemy_thresholds.contains(&level),
			"Alchemy stepping to {level}"
		);
	}
	// Re-setting the same levels is not a crossing, so a per-frame
	// caller costs nothing.
	let max = at(crate::skills::MAX_LEVEL, crate::skills::MAX_LEVEL);
	assert!(!p.set_levels(max));
	// And a single jump over every threshold on both axes sweeps
	// exactly once.
	let mut q = Pipeline::new(0.0, 4, 0);
	assert!(q.set_levels(max));
	assert!(!q.set_levels(max));
	// Losing levels (a fresh world, a rejoin as somebody else) crosses
	// back down and re-meshes too — the veins have to go away.
	assert!(q.set_levels(at(1, 1)));
}

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

	// Walk far away: the edited column hard-evicts...
	let far = ChunkPos::new(40, 0, 0);
	quiesce(&mut p, far);
	assert!(p.block_at(8, 8, 8).is_none(), "column evicted");
	// ...and coming back regenerates it WITH the edit.
	quiesce(&mut p, center);
	assert_eq!(p.block_at(8, 8, 8), Some(new), "edit survived eviction");
	// Regeneration re-applies the overlay but records nothing new.
	assert!(p.take_recorded_edits().is_empty());
}

/// The multiplayer half of the same guarantee, and the reason it is its own test: a client with NO
/// overlay installed still keeps a mirror of everything the server told it, and still re-applies
/// it. Nothing else can put those cells back — `apply_authoritative` drops a cell from
/// `deferred_edits` the moment it lands, and the server prunes its record on a radius WIDER than
/// the client's residency. A local write is still not recorded, so a rejection marks nothing.
#[test]
fn authoritative_edits_survive_evict_and_regen_without_an_overlay() {
	let mut p = Pipeline::new(0.0, 4, 2);
	let center = ChunkPos::new(0, 0, 0);
	quiesce(&mut p, center);

	let old = p.block_at(8, 8, 8).expect("chunk ready");
	let new = if old == BlockType::Stone {
		BlockType::Dirt
	} else {
		BlockType::Stone
	};
	assert_eq!(
		p.apply_authoritative(8, 8, 8, new as u16),
		ApplyEdit::Applied
	);
	// A PREDICTION is still not recorded: only the server's answer is.
	let beside = p.block_at(9, 8, 8).expect("chunk ready");
	let guess = if beside == BlockType::Sand {
		BlockType::Gravel
	} else {
		BlockType::Sand
	};
	p.set_block(9, 8, 8, guess).expect("prediction lands");
	assert!(p.take_recorded_edits().is_empty(), "no local save path");

	let far = ChunkPos::new(40, 0, 0);
	quiesce(&mut p, far);
	assert!(p.block_at(8, 8, 8).is_none(), "column evicted");
	quiesce(&mut p, center);
	assert_eq!(
		p.block_at(8, 8, 8),
		Some(new),
		"the server's edit survived eviction"
	);
	assert_eq!(
		p.block_at(9, 8, 8),
		Some(beside),
		"the prediction did not: the server never confirmed it"
	);
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

	// A cell far above any terrain: its chunk is a shared AIR_CHUNK
	// sentinel.
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

/// In multiplayer the "has this cell been touched?" map mirrors the SERVER's journal, so a local
/// write only marks a cell edited once authority confirms it. The rollback is what forces this: it
/// reverts through `set_cell` like any other write and `EditOverlay::record` is insert-only, so a
/// recorded prediction survives its own rejection — the cell stays edited on the client and
/// pristine on the server, and the two ends answer `ore::dig_mode` differently about it forever.
#[test]
fn a_rejected_prediction_leaves_the_cell_untouched() {
	let mut p = Pipeline::new(0.0, 4, 2);
	quiesce(&mut p, ChunkPos::new(0, 0, 0));
	let old = p.cell_at(8, 8, 8).unwrap();
	let new = if crate::chunk::cell_id(old) == BlockType::Stone as u16 {
		BlockType::Dirt as u16
	} else {
		BlockType::Stone as u16
	};
	assert!(!p.cell_edited(8, 8, 8), "pristine to begin with");

	// Predict, then take it back the way an EditRejected does.
	p.set_cell(8, 8, 8, new).unwrap();
	p.set_cell(8, 8, 8, old).unwrap();
	assert!(
		!p.cell_edited(8, 8, 8),
		"a refused edit is not an edit — the server never journaled it"
	);

	// The server's own answer IS a record, and it sticks.
	p.apply_authoritative(8, 8, 8, new);
	assert!(p.cell_edited(8, 8, 8));
}

/// An authoritative write whose value already matches is still a RECORD, and the first record on a
/// cell changes what its chunk draws: `ore::revealed` keys on "edited", not on the cell value, so
/// the chunk has to re-mesh even though nothing was written. Journal replay does exactly this, and
/// `set_cell` early-returns on an unchanged value without touching `mesh_gen` — leaving a seam
/// glittering in a chunk whose dig path already calls it spent stone.
#[test]
fn a_no_op_authoritative_write_still_remeshes_once() {
	let mut p = Pipeline::new(0.0, 4, 2);
	let center = ChunkPos::new(0, 0, 0);
	quiesce(&mut p, center);
	let pos = ChunkPos::new(0, 0, 0);
	let here = p.cell_at(8, 8, 8).unwrap();
	let before = p.store.get(&pos).unwrap().mesh_gen;

	// Same value in: nothing to write, but the cell is now edited.
	assert_eq!(
		p.apply_authoritative(8, 8, 8, here),
		ApplyEdit::Applied,
		"a matching value is applied, not deferred"
	);
	assert!(p.cell_edited(8, 8, 8), "recorded all the same");
	assert!(
		p.store.get(&pos).unwrap().mesh_gen > before,
		"the reveal answer changed, so the chunk has to be re-drawn"
	);

	// And it does not keep re-meshing on every repeat.
	let settled = p.store.get(&pos).unwrap().mesh_gen;
	p.apply_authoritative(8, 8, 8, here);
	assert_eq!(
		p.store.get(&pos).unwrap().mesh_gen,
		settled,
		"only the FIRST record is a change"
	);
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
			got = Some(out.generation);
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
				uploaded.insert(out.pos, out.generation);
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
			uploaded.insert(out.pos, out.generation);
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

/// Inline drains make progress even when every frame is over budget: with an already-expired
/// deadline each maintain, the center column (nearest-first, the first job popped) still reaches
/// the store within the first frames. Regression for the web spawn fall-through, where generated
/// columns piled up undrained while the player fell through a world that never landed. Walked for
/// both allowances, because a zero allowance is the FLOOR and not a stall.
#[test]
fn inline_drain_progresses_past_expired_deadline() {
	for allowance in [None, Some(std::time::Duration::ZERO)] {
		let center = ChunkPos::new(0, 0, 0);
		let mut p = Pipeline::new(7.5, 12, 0);
		for _ in 0..3 {
			let expired = web_time::Instant::now() - std::time::Duration::from_millis(1);
			p.maintain(center, usize::MAX, Some(expired), allowance);
		}
		assert!(
			p.cell_at(8, 100, 8).is_some(),
			"center column must land within the first frames, allowance {allowance:?}"
		);
	}
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

/// Work in a pool is still work. `top_up_workers` drains BOTH heaps into the pool, so a heaps-only
/// urgency reads the busiest frame of the fill as idle and collapses the shared streaming deadline
/// to its floor — exactly as those results start landing on this thread to be absorbed. With the
/// inline executor this was unreachable: a job left the heap and finished inside the same call.
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
	// starting a mountain column that costs many times the remaining
	// budget.
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
