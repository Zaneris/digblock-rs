use super::*;
use crate::block::BlockType;
use rustc_hash::FxHashMap;

const SEED: f64 = 42.0;

/// A flat slab of Dirt with one Grass cap, so a job has something to find.
struct Slab {
	cells: FxHashMap<(i32, i32, i32), u16>,
	fill: Option<u16>,
}

impl Slab {
	fn of(fill: BlockType) -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: Some(fill as u16),
		}
	}
	fn empty() -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: None,
		}
	}
}

impl CellSource for Slab {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.cells.get(&(x, y, z)).copied().or(self.fill)
	}
}

/// Turns every Dirt it is offered into Grass. Stands in for a real rule.
struct Sprout {
	sched: Schedule,
	scope: Scope,
}

impl Sprout {
	fn boxed(per_day: u32, radius_chunks: i32, y: (i32, i32)) -> Box<dyn WorldJob> {
		Box::new(Self {
			sched: Schedule {
				per_day,
				offset: 0.0,
			},
			scope: Scope {
				blocks: &[BlockType::Dirt],
				radius_chunks,
				y,
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
		_run: u64,
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
		out.extend(s.tick(secs, SEED, &[(0, 64, 0)], w));
		ticks += 1;
		if s.sweeping().is_none() {
			return (out, ticks);
		}
		assert!(ticks < 100_000, "sweep never finished");
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
		let n = s.tick(0.0, SEED, &[(0, 64, 0)], &w).len();
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
		assert!(s.tick(1.0, SEED, &[(0, 64, 0)], &w).is_empty());
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
			s.tick(f64::from(DAY_SECS) * 30.0, SEED, &[(0, 64, 0)], &w)
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
	s.tick(0.0, SEED, &[(0, 64, 0)], &w);
	assert!(s.sweeping().is_some());
	// Days pass mid-sweep. The slot moves on; the sweep does not restart.
	let mut seen = std::collections::HashSet::new();
	for i in 0..500 {
		for (at, _) in s.tick(f64::from(DAY_SECS) * f64::from(i), SEED, &[(0, 64, 0)], &w) {
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
		out.extend(s.tick(0.0, SEED, &[(eye, 64, 0)], &w));
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
	assert!(s.tick(0.0, SEED, &[], &w).is_empty());
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

/// THE anti-dupe guard. A journal write sets the cell's monotone edited
/// bit, and `nodes::revealed` reads that bit as spent — so a sweep over a
/// node's host would destroy every reveal in a band around every player,
/// irreversibly. The registry refuses rather than trusting the author.
#[test]
fn a_node_host_scope_is_refused_at_registration() {
	for host in [BlockType::Stone, BlockType::Wood, BlockType::Sand] {
		struct Bad(BlockType);
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
					blocks: match self.0 {
						BlockType::Stone => &[BlockType::Stone],
						BlockType::Wood => &[BlockType::Dirt, BlockType::Wood],
						_ => &[BlockType::Sand],
					},
					radius_chunks: 0,
					y: (0, 0),
				}
			}
			fn apply(&self, _: (i32, i32, i32), _: &dyn CellSource, _: f64, _: u64) -> Option<u16> {
				None
			}
		}
		let r = std::panic::catch_unwind(move || {
			Scheduler::new().register(Box::new(Bad(host)));
		});
		assert!(r.is_err(), "{host:?} is a node host and must be refused");
	}
}

/// `node_host` and `nodes::revealed` must name the same set, or the guard
/// above is guarding the wrong thing.
#[test]
fn a_node_host_is_never_sweepable() {
	use crate::worldgen::nodes;
	let levels = nodes::Levels {
		mining: 99,
		alchemy: 99,
	};
	for b in BlockType::ALL {
		if node_host(b) {
			continue;
		}
		// A non-host must be unable to reveal anything, anywhere.
		for at in [(0, 20, 0), (13, 60, -7), (128, 100, 256), (-64, 8, 31)] {
			assert!(
				nodes::revealed(b as u16, at, SEED, levels, false).is_none(),
				"{b:?} can host a node but `node_host` says otherwise"
			);
		}
	}
}
