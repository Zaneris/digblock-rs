use super::*;
use crate::block::BlockType;
use rustc_hash::{FxHashMap, FxHashSet};

const SEED: f64 = 42.0;

/// A flat slab of Dirt with one Grass cap, so a job has something to find.
struct Slab {
	cells: FxHashMap<(i32, i32, i32), u16>,
	fill: Option<u16>,
	/// Cells `record` has touched — the stand-in for a host's journal.
	edits: FxHashSet<(i32, i32, i32)>,
}

impl Slab {
	fn of(fill: BlockType) -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: Some(fill as u16),
			edits: FxHashSet::default(),
		}
	}
	fn empty() -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: None,
			edits: FxHashSet::default(),
		}
	}
	/// A host-style write: the cell changes AND the edit is recorded, so
	/// a frontier sweep can find it — including the sweep's own writes,
	/// which is the self-sustaining half the ring test leans on.
	fn record(&mut self, at: (i32, i32, i32), cell: u16) {
		self.cells.insert(at, cell);
		self.edits.insert(at);
	}
}

impl CellSource for Slab {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.cells.get(&(x, y, z)).copied().or(self.fill)
	}
	fn edited_cells(&self, cx: i32, cz: i32, f: &mut dyn FnMut(i32, i32, i32)) {
		let s = CHUNK_SIZE as i32;
		for &(x, y, z) in &self.edits {
			if x.div_euclid(s) == cx && z.div_euclid(s) == cz {
				f(x, y, z);
			}
		}
	}
}

/// Turns every Dirt it is offered into Grass. Stands in for a real rule.
struct Sprout {
	sched: Schedule,
	scope: Scope,
}

impl Sprout {
	fn boxed(per_day: u32, radius_chunks: i32, y: (i32, i32)) -> Box<dyn WorldJob> {
		Self::with(per_day, radius_chunks, y, Candidates::Band)
	}
	fn frontier(radius_chunks: i32, y: (i32, i32), surface_shell: bool) -> Box<dyn WorldJob> {
		Self::with(1, radius_chunks, y, Candidates::Frontier { surface_shell })
	}
	fn with(
		per_day: u32,
		radius_chunks: i32,
		y: (i32, i32),
		candidates: Candidates,
	) -> Box<dyn WorldJob> {
		Box::new(Self {
			sched: Schedule {
				per_day,
				offset: 0.0,
			},
			scope: Scope {
				blocks: &[BlockType::Dirt],
				radius_chunks,
				y,
				candidates,
			},
		})
	}
}

impl WorldJob for Sprout {
	fn name(&self) -> &'static str {
		"sprout"
	}
	fn schedule(&self) -> Schedule {
		self.sched
	}
	fn scope(&self) -> Scope {
		self.scope
	}
	fn apply(
		&self,
		_at: (i32, i32, i32),
		_world: &dyn CellSource,
		_seed: f64,
		_run: Run,
	) -> Option<u16> {
		Some(BlockType::Grass as u16)
	}
}

/// Run to completion and hand back everything the sweep wrote, plus how
/// many ticks it took.
fn drain(s: &mut Scheduler, secs: f64, w: &dyn CellSource) -> (Vec<Write>, usize) {
	let mut out = Vec::new();
	let mut ticks = 0;
	loop {
		out.extend(s.tick(secs, SEED, &[(0, 64, 0)], 1, w));
		ticks += 1;
		if s.sweeping().is_none() {
			return (out, ticks);
		}
		assert!(ticks < 100_000, "sweep never finished");
	}
}

/// `daily_at` speaks the sun's clock: its noon is the sun's zero, the
/// hours are a twenty-fourth of the day apart, and the slot it produces
/// comes due exactly once a day.
#[test]
fn daily_at_keeps_the_suns_clock() {
	use crate::sun::Sun;
	let noon = Schedule::daily_at(12);
	assert_eq!(noon.per_day, 1);
	let at_noon = Sun::tod_of_secs(f64::from(noon.offset) * f64::from(DAY_SECS));
	assert!(at_noon.abs() < 1e-3, "noon is {at_noon}°");
	for h in 0..24 {
		let (a, b) = (Schedule::daily_at(h), Schedule::daily_at(h + 1));
		let gap = (b.offset - a.offset).rem_euclid(1.0);
		assert!((gap - 1.0 / 24.0).abs() < 1e-5, "{h}: {gap}");
		assert_eq!(
			a.slot(f64::from(DAY_SECS) * 3.0) + 1,
			a.slot(f64::from(DAY_SECS) * 4.0)
		);
	}
}

#[test]
fn a_slot_index_rises_once_per_slot_and_never_falls() {
	let day = f64::from(DAY_SECS);
	let twice = Schedule {
		per_day: 2,
		offset: 0.0,
	};
	assert_eq!(twice.slot(0.0), 0);
	assert_eq!(twice.slot(day * 0.49), 0);
	assert_eq!(twice.slot(day * 0.51), 1);
	assert_eq!(twice.slot(day * 1.01), 2);
	let mut prev = 0;
	for i in 0..2000 {
		let n = twice.slot(day * f64::from(i) / 100.0);
		assert!(n >= prev, "slot went backwards");
		prev = n;
	}
	// The offset moves the first slot without changing the cadence.
	let late = Schedule {
		per_day: 1,
		offset: 0.5,
	};
	assert_eq!(late.slot(day * 0.4), 0);
	assert_eq!(late.slot(day * 0.6), 0);
	assert_eq!(late.slot(day * 1.6), 1);
}

#[test]
fn a_sweep_covers_its_whole_scope_exactly_once() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 0, (60, 63)));
	let (writes, ticks) = drain(&mut s, 0.0, &Slab::of(BlockType::Dirt));
	// One chunk of columns, four y rows apiece.
	let want = (CHUNK_SIZE * CHUNK_SIZE) * 4;
	assert_eq!(writes.len(), want, "every cell in the band, once");
	assert!(ticks > 1, "a sweep this size must be sliced, not one-shot");
	let unique: FxHashMap<_, _> = writes.iter().copied().collect();
	assert_eq!(unique.len(), want, "no cell visited twice");
}

#[test]
fn no_slice_ever_exceeds_the_budget() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 1, (0, 200)));
	let w = Slab::of(BlockType::Dirt);
	for _ in 0..50 {
		let n = s.tick(0.0, SEED, &[(0, 64, 0)], 1, &w).len();
		assert!(n <= CELLS_PER_TICK, "slice of {n} past the budget");
	}
	assert!(
		s.sweeping().is_some(),
		"a sweep this large is still running"
	);
}

#[test]
fn a_job_runs_once_per_slot_however_many_ticks_pass() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 0, (60, 60)));
	let w = Slab::of(BlockType::Dirt);
	let per_sweep = CHUNK_SIZE * CHUNK_SIZE;

	let (first, _) = drain(&mut s, 0.0, &w);
	assert_eq!(first.len(), per_sweep);
	// Same slot, many ticks later: nothing further.
	for _ in 0..200 {
		assert!(s.tick(1.0, SEED, &[(0, 64, 0)], 1, &w).is_empty());
	}
	// Next day, next slot.
	let (second, _) = drain(&mut s, f64::from(DAY_SECS) * 1.5, &w);
	assert_eq!(second.len(), per_sweep, "the next slot runs");
}

/// A world closed for a month must not wake up owing thirty sweeps.
#[test]
fn a_long_absence_costs_one_catch_up_sweep_not_thirty() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(2, 0, (60, 60)));
	let w = Slab::of(BlockType::Dirt);
	let per_sweep = CHUNK_SIZE * CHUNK_SIZE;

	let (caught_up, _) = drain(&mut s, f64::from(DAY_SECS) * 30.0, &w);
	assert_eq!(caught_up.len(), per_sweep, "exactly one sweep");
	for _ in 0..200 {
		assert!(
			s.tick(f64::from(DAY_SECS) * 30.0, SEED, &[(0, 64, 0)], 1, &w)
				.is_empty(),
			"and then nothing until the next slot"
		);
	}
}

/// A sweep already slicing must be skipped, not queued behind itself.
#[test]
fn a_running_sweep_is_never_started_twice() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 2, (0, 200)));
	let w = Slab::of(BlockType::Dirt);
	s.tick(0.0, SEED, &[(0, 64, 0)], 1, &w);
	assert!(s.sweeping().is_some());
	// Days pass mid-sweep. The slot moves on; the sweep does not restart.
	let mut seen = std::collections::HashSet::new();
	for i in 0..500 {
		for (at, _) in s.tick(
			f64::from(DAY_SECS) * f64::from(i),
			SEED,
			&[(0, 64, 0)],
			1,
			&w,
		) {
			assert!(seen.insert(at), "{at:?} written twice — sweeps overlapped");
		}
	}
}

/// The column set is snapshotted at the start, so a player walking during
/// a sweep cannot make coverage uneven.
#[test]
fn walking_during_a_sweep_does_not_move_it() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 0, (60, 60)));
	let w = Slab::of(BlockType::Dirt);
	let mut out = Vec::new();
	let mut eye = 0;
	while {
		out.extend(s.tick(0.0, SEED, &[(eye, 64, 0)], 1, &w));
		eye += 4096;
		s.sweeping().is_some()
	} {}
	let xs: Vec<i32> = out.iter().map(|((x, _, _), _)| *x).collect();
	assert!(
		xs.iter().all(|&x| (0..CHUNK_SIZE as i32).contains(&x)),
		"the sweep followed the player instead of its snapshot"
	);
}

#[test]
fn two_players_in_one_chunk_are_one_column_set() {
	let one = columns_near(&[(3, 64, 5)], 0);
	let two = columns_near(&[(3, 64, 5), (9, 70, 11)], 0);
	assert_eq!(one.len(), CHUNK_SIZE * CHUNK_SIZE);
	assert_eq!(two.len(), one.len(), "the overlap is deduplicated");
}

#[test]
fn nobody_online_means_no_sweep() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 0, (60, 60)));
	let w = Slab::of(BlockType::Dirt);
	assert!(s.tick(0.0, SEED, &[], 1, &w).is_empty());
	assert!(
		s.sweeping().is_none(),
		"a sweep needs somebody to sweep near"
	);
	// And the slot is still owed once somebody arrives.
	let (writes, _) = drain(&mut s, 0.0, &w);
	assert_eq!(writes.len(), CHUNK_SIZE * CHUNK_SIZE);
}

#[test]
fn unloaded_terrain_is_skipped_not_written() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 0, (60, 60)));
	let (writes, _) = drain(&mut s, 0.0, &Slab::empty());
	assert!(writes.is_empty(), "a None cell is not a Dirt cell");
}

#[test]
fn only_the_scoped_block_is_offered() {
	let mut s = Scheduler::new();
	s.register(Sprout::boxed(1, 0, (60, 60)));
	let (writes, _) = drain(&mut s, 0.0, &Slab::of(BlockType::Rubble));
	assert!(writes.is_empty(), "Rubble is not in the scope");
}

/// A scope holding one of these blocks, alongside an innocent one so the
/// guard cannot be passing on the scope's LENGTH.
struct Bad(&'static [BlockType], Candidates);

impl WorldJob for Bad {
	fn name(&self) -> &'static str {
		"bad"
	}
	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 1,
			offset: 0.0,
		}
	}
	fn scope(&self) -> Scope {
		Scope {
			blocks: self.0,
			radius_chunks: 0,
			y: (0, 0),
			candidates: self.1,
		}
	}
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: f64, _: Run) -> Option<u16> {
		None
	}
}

/// THE anti-dupe guard. A journal write sets the cell's monotone edited bit, and `nodes::revealed`
/// reads that bit as spent — so a sweep over a node's host would destroy every reveal in a band
/// around every player, irreversibly. A LIVE FACE is worse still: it is the reveal itself, stored.
/// Swept over every block rather than over a roster somebody wrote down, which is what makes a new
/// host or a new node kind land here.
#[test]
fn a_node_scope_is_refused_at_registration() {
	let mut refused = 0;
	for candidates in [
		Candidates::Band,
		Candidates::Frontier {
			surface_shell: true,
		},
	] {
		for b in BlockType::ALL {
			let blocks: &'static [BlockType] = Box::leak(Box::new([BlockType::Dirt, b]));
			let r = std::panic::catch_unwind(move || {
				Scheduler::new().register(Box::new(Bad(blocks, candidates)));
			});
			let banned = node_host(b) || b.is_node_block();
			assert_eq!(r.is_err(), banned, "{b:?} with {candidates:?}");
			refused += u32::from(banned);
		}
	}
	assert!(refused > 1, "nothing was refused — the guard is not wired");
}

/// `node_host` and `nodes::revealed` must name the same set, or the guard above is guarding the
/// wrong thing. Asserted over a scanned region in three answers rather than two: a host reveals in
/// SOME of it, a stored live face in ALL of it, everything else in none — so dropping a host fails
/// on its own witnesses and adding one fails on their absence. A fixed list of probe positions is
/// NOT enough: nodes are sparse salted hashes, and a list that dodges every node passes anything.
#[test]
fn a_node_host_is_never_sweepable() {
	use crate::decoration;
	use crate::worldgen::nodes;
	let levels = nodes::Levels {
		mining: 99,
		alchemy: 99,
	};
	let mut candidates: Vec<(i32, i32, i32)> = Vec::new();
	for x in -400i32..400 {
		for z in -400i32..400 {
			if let Some(y) = nodes::springstone_bed(x, z, SEED) {
				candidates.push((x, y, z));
			}
			if let Some(t) = decoration::trunk_at(x, z, SEED) {
				candidates.extend((t.y0 + 1..=t.y1).map(|y| (x, y, z)));
			}
		}
	}
	for x in 0..32 {
		for z in 0..32 {
			candidates.extend((1..100).map(|y| (x, y, z)));
		}
	}
	let all = candidates.len();
	assert!(all > 0, "the scan found nowhere a node could stand");
	for b in BlockType::ALL {
		let cell = crate::chunk::make_cell(b, 0);
		let witnesses = candidates
			.iter()
			.filter(|&&at| nodes::revealed(cell, at, SEED, levels, false).is_some())
			.count();
		if b.is_node_block() {
			// A stored live face IS the reveal, so it is found wherever
			// it stands rather than where a stream seeded it — which is
			// why a sweep over one would spend every cell it touched,
			// and why the registry refuses it as flatly as a host.
			assert_eq!(
				witnesses, all,
				"{b:?} is a live face but revealed in {witnesses} of {all}"
			);
		} else if node_host(b) {
			// A host reveals where its stream put a node: some of the
			// scanned cells, and never all of them.
			assert!(
				0 < witnesses && witnesses < all,
				"{b:?} hosts nodes but revealed in {witnesses} of {all}"
			);
		} else {
			assert_eq!(witnesses, 0, "{b:?} reveals but is neither host nor face");
		}
	}
}

/// A job that files the [`Run`] of every sweep it is given, so a test can
/// see what the scheduler decided rather than only what it wrote.
struct Spy {
	rolled: bool,
	runs: std::sync::Arc<std::sync::Mutex<Vec<Run>>>,
}

impl WorldJob for Spy {
	fn name(&self) -> &'static str {
		"spy"
	}
	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 1,
			offset: 0.0,
		}
	}
	fn rolled(&self) -> bool {
		self.rolled
	}
	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Dirt],
			radius_chunks: 0,
			y: (0, 0),
			candidates: Candidates::Band,
		}
	}
	fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: f64, run: Run) -> Option<u16> {
		// One entry per SWEEP, not per cell: every cell of one sweep
		// carries the same `Run`, which is the snapshot under test.
		let mut runs = self.runs.lock().expect("spy runs");
		if runs.last() != Some(&run) {
			runs.push(run);
		}
		None
	}
}

impl Spy {
	fn boxed(
		rolled: bool,
	) -> (
		Box<dyn WorldJob>,
		std::sync::Arc<std::sync::Mutex<Vec<Run>>>,
	) {
		let runs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
		(
			Box::new(Spy {
				rolled,
				runs: runs.clone(),
			}),
			runs,
		)
	}
}

/// Sweep whatever is due at `secs` to completion.
fn sweep_at(s: &mut Scheduler, secs: f64, players: u32, world: &dyn CellSource) {
	for _ in 0..64 {
		s.tick(secs, SEED, &[(0, 0, 0)], players, world);
	}
}

/// The head-count the host observes is the head-count the rule reads: `Scheduler::tick`'s argument
/// reaches [`Run::players`] unchanged and per sweep, so a rule that scales by how busy a world has
/// been is reading the world rather than a constant. The slots are checked with it, because a
/// `Run` that never advanced would satisfy the first half on its own.
#[test]
fn the_head_count_and_the_slot_reach_every_apply() {
	let (job, runs) = Spy::boxed(false);
	let mut s = Scheduler::new();
	s.register(job);
	let w = Slab::of(BlockType::Dirt);
	let counts = [1u32, 7, 0, 40];
	for (day, &players) in counts.iter().enumerate() {
		sweep_at(&mut s, f64::from(DAY_SECS) * day as f64, players, &w);
	}
	let seen = runs.lock().expect("spy runs").clone();
	assert_eq!(
		seen.iter().map(|r| r.players).collect::<Vec<_>>(),
		counts.to_vec(),
		"the head-count did not reach the rule"
	);
	assert!(
		seen.windows(2).all(|w| w[1].slot > w[0].slot),
		"the slot did not advance across sweeps: {seen:?}"
	);
}

/// A ROLLED job anchors to the slot standing when it first sees the clock and sweeps at the next
/// one. The hosts restart that clock at zero on every world entry and every server boot, so
/// without the anchor the load-time slot — and the draw it seeds — is replayed every session. A
/// settled rule is deliberately not anchored: replaying its slot is a no-op, and waiting would
/// cost it a whole slot of spread.
#[test]
fn a_rolled_job_skips_the_slot_it_loads_into() {
	let w = Slab::of(BlockType::Dirt);
	for rolled in [false, true] {
		let (job, runs) = Spy::boxed(rolled);
		let mut s = Scheduler::new();
		s.register(job);
		// The load, and the whole of the slot standing at it.
		sweep_at(&mut s, 0.0, 1, &w);
		assert_eq!(
			runs.lock().expect("spy runs").is_empty(),
			rolled,
			"rolled={rolled}: the load-time slot"
		);
		// The next boundary: both kinds sweep, and neither repeats.
		sweep_at(&mut s, f64::from(DAY_SECS), 1, &w);
		let slots: Vec<u64> = runs
			.lock()
			.expect("spy runs")
			.iter()
			.map(|r| r.slot)
			.collect();
		let want: Vec<u64> = if rolled { vec![1] } else { vec![0, 1] };
		assert_eq!(slots, want, "rolled={rolled}");
	}
}

/// Every rule the game runs, in one list both hosts register from. A pin rather than a derived
/// check, because nothing in Rust can enumerate the modules that define rules — and the bug it
/// names is the one no test of a rule can see: a job nothing registers never runs in production
/// however well its own behaviour is covered. Building the real roster is also the standing check
/// that no production scope can destroy a node reveal, since `register` refuses one.
#[test]
fn the_production_roster_is_pinned() {
	const RULES: [&str; 6] = [
		"grass-9am",
		"grass-noon",
		"grass-3pm",
		"leaf-decay",
		"node-regrow",
		"conjured-light-fade",
	];
	let mut s = Scheduler::new();
	let mut names: Vec<&'static str> = Vec::new();
	for job in production() {
		names.push(job.name());
		s.register(job);
	}
	names.sort_unstable();
	let mut want = RULES.to_vec();
	want.sort_unstable();
	assert_eq!(names, want, "a rule joined or left the production roster");
}

/// The cube shell at exactly Chebyshev distance `d` from `c` — "ring N"
/// of a spread that advances one neighborhood per run.
fn ring(c: (i32, i32, i32), d: i32) -> FxHashSet<(i32, i32, i32)> {
	let mut out = FxHashSet::default();
	for dy in -d..=d {
		for dz in -d..=d {
			for dx in -d..=d {
				if dx.abs().max(dy.abs()).max(dz.abs()) == d {
					out.insert((c.0 + dx, c.1 + dy, c.2 + dz));
				}
			}
		}
	}
	out
}

fn written(writes: &[Write]) -> FxHashSet<(i32, i32, i32)> {
	writes.iter().map(|&(at, _)| at).collect()
}

#[test]
fn a_frontier_sweep_offers_exactly_the_edit_neighborhood() {
	let mut s = Scheduler::new();
	s.register(Sprout::frontier(1, (0, 200), false));
	let mut w = Slab::of(BlockType::Dirt);
	// Two adjacent edits (overlapping neighborhoods dedup within the
	// column), one at the column border (its neighborhood spills into the
	// next chunk column), one beyond the radius (must not appear).
	let near = [(3, 64, 3), (4, 64, 3), (15, 70, 8)];
	let far = (-40, 90, 40);
	for at in near.iter().copied().chain([far]) {
		w.record(at, BlockType::Dirt as u16);
	}
	let (writes, _) = drain(&mut s, 0.0, &w);
	let mut want = FxHashSet::default();
	for at in near {
		want.extend(ring(at, 0));
		want.extend(ring(at, 1));
	}
	assert_eq!(written(&writes), want);
	assert_eq!(writes.len(), want.len(), "a cell offered twice");
	assert!(!want.is_empty(), "nothing was examined");
}

/// Turns Dirt to Grass only beside existing Grass — the shape of a real
/// spread rule, minus the sun. What it pins is the runner, not the rule:
/// judged against the pre-sweep world, its own writes cannot feed it
/// mid-run.
struct Creep;

impl WorldJob for Creep {
	fn name(&self) -> &'static str {
		"creep"
	}
	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: 1,
			offset: 0.0,
		}
	}
	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Dirt],
			radius_chunks: 1,
			y: (0, 200),
			candidates: Candidates::Frontier {
				surface_shell: false,
			},
		}
	}
	fn apply(&self, at: (i32, i32, i32), world: &dyn CellSource, _: f64, _: Run) -> Option<u16> {
		ring(at, 1)
			.into_iter()
			.any(|(x, y, z)| {
				world
					.cell_at(x, y, z)
					.is_some_and(|c| cell_id(c) == BlockType::Grass as u16)
			})
			.then_some(BlockType::Grass as u16)
	}
}

#[test]
fn a_frontier_run_advances_one_ring_never_two() {
	let mut s = Scheduler::new();
	s.register(Box::new(Creep));
	let mut w = Slab::of(BlockType::Dirt);
	let seed_cell = (0, 64, 0);
	w.record(seed_cell, BlockType::Grass as u16);

	// Run 1: exactly the seed's neighbors. Per-slice application would
	// let ring 1's writes pull ring 2 in within the same run.
	let (writes, _) = drain(&mut s, 0.0, &w);
	assert_eq!(written(&writes), ring(seed_cell, 1));

	// Apply as a host would — recorded, so the frontier sees them.
	for &(at, val) in &writes {
		w.record(at, val);
	}

	// Run 2, next slot: exactly one ring further.
	let (writes2, _) = drain(&mut s, f64::from(DAY_SECS) * 1.5, &w);
	assert_eq!(written(&writes2), ring(seed_cell, 2));
}

#[test]
fn judgment_reads_the_presweep_world_and_emission_rechecks_it() {
	let mut s = Scheduler::new();
	s.register(Sprout::frontier(0, (0, 200), false));
	let mut w = Slab::of(BlockType::Dirt);
	let stomped = (2, 64, 2);
	w.record(stomped, BlockType::Dirt as u16);

	// One column, few candidates: judgment fits in the first slice and
	// emits nothing.
	let first = s.tick(0.0, SEED, &[(0, 64, 0)], 1, &w);
	assert!(first.is_empty(), "judgment slice must not emit");
	assert!(s.sweeping().is_some(), "emission still owed");

	// A player edit lands between the phases; its cell's write must die.
	w.record(stomped, BlockType::Stone as u16);
	let (rest, _) = drain(&mut s, 0.0, &w);
	let mut want = ring(stomped, 1);
	want.extend(ring(stomped, 0));
	want.remove(&stomped);
	assert_eq!(written(&rest), want);
}

#[test]
fn abandon_between_phases_emits_nothing() {
	let mut s = Scheduler::new();
	s.register(Sprout::frontier(0, (0, 200), false));
	let mut w = Slab::of(BlockType::Dirt);
	w.record((2, 64, 2), BlockType::Dirt as u16);

	assert!(s.tick(0.0, SEED, &[(0, 64, 0)], 1, &w).is_empty());
	assert!(s.sweeping().is_some(), "writes are judged and waiting");
	s.abandon();
	// Same slot: the run was consumed at start, so nothing resumes and
	// the pending writes died with the sweep.
	for _ in 0..50 {
		assert!(s.tick(0.0, SEED, &[(0, 64, 0)], 1, &w).is_empty());
	}
}

#[test]
fn no_frontier_slice_ever_exceeds_the_budget() {
	let mut s = Scheduler::new();
	s.register(Sprout::frontier(1, (0, 200), true));
	let w = Slab::of(BlockType::Dirt);
	let mut total = 0;
	loop {
		let n = s.tick(0.0, SEED, &[(0, 64, 0)], 1, &w).len();
		assert!(n <= CELLS_PER_TICK, "emitted {n} past the budget");
		total += n;
		if s.sweeping().is_none() {
			break;
		}
	}
	// The shell alone offers several columns' subsoil — enough that a
	// single-slice emission could not have carried it.
	assert!(
		total > CELLS_PER_TICK,
		"shell sweep too small to prove slicing"
	);
}

#[test]
fn an_unedited_world_costs_a_frontier_sweep_nothing() {
	let mut s = Scheduler::new();
	s.register(Sprout::frontier(2, (0, 200), false));
	let (writes, _) = drain(&mut s, 0.0, &Slab::of(BlockType::Dirt));
	assert!(writes.is_empty(), "no edits, no shell, no candidates");
}

#[test]
fn the_shell_offers_exactly_the_subsoil_span() {
	let mut s = Scheduler::new();
	s.register(Sprout::frontier(0, (0, 200), true));
	let (writes, _) = drain(&mut s, 0.0, &Slab::of(BlockType::Dirt));
	// The eye's own chunk column, every block column's span, clamped to
	// the job's band — derived from the same function the runner asks.
	let mut want = FxHashSet::default();
	for z in 0..CHUNK_SIZE as i32 {
		for x in 0..CHUNK_SIZE as i32 {
			let (lo, hi) = crate::worldgen::subsoil_span(x, z, SEED);
			for y in lo.max(0)..=hi.min(200) {
				want.insert((x, y, z));
			}
		}
	}
	assert_eq!(written(&writes), want);
	assert!(!want.is_empty(), "the span sweep examined nothing");
}
