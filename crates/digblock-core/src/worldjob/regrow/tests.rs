use super::*;
use crate::chunk::CHUNK_SIZE;
use crate::sun::DAY_SECS;
use crate::worldjob::{Scheduler, node_host};
use rustc_hash::{FxHashMap, FxHashSet};

const SEED: f64 = 42.0;

/// Sparse world: every unnamed cell is Air, every named cell is an edit.
#[derive(Default)]
struct Sparse {
	cells: FxHashMap<(i32, i32, i32), u16>,
	edits: FxHashSet<(i32, i32, i32)>,
}

impl Sparse {
	fn record(&mut self, at: (i32, i32, i32), b: BlockType) {
		self.cells.insert(at, b as u16);
		self.edits.insert(at);
	}
}

impl CellSource for Sparse {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		Some(
			self.cells
				.get(&(x, y, z))
				.copied()
				.unwrap_or(BlockType::Air as u16),
		)
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

/// One cell worldgen puts a heartwood in, found by walking — so a retune
/// of the tree roll moves the test with it.
fn find_heartwood() -> (i32, i32, i32) {
	for x in -512..512 {
		for z in -512..512 {
			let Some(t) = crate::decoration::trunk_at(x, z, SEED) else {
				continue;
			};
			for y in t.y0..=t.y1 {
				if node_at(BlockType::Wood, (x, y, z), SEED) == Some(BlockType::Heartwood) {
					return (x, y, z);
				}
			}
		}
	}
	panic!("no heartwood in the sampled world");
}

fn run(slot: u64, players: u32) -> Run {
	Run { slot, players }
}

/// The odds climb with the head-count, stop climbing, and never reach
/// certainty — a find that came back every night would not be a find.
#[test]
fn the_chance_climbs_with_the_head_count_and_caps() {
	// Nobody online, nothing comes back: the sweep needs a player near
	// the face to run at all, so an empty world's rate is the one rate
	// that has to be zero.
	assert_eq!(chance_percent(0), 0);
	let mut prev = chance_percent(0);
	let (mut climbed, mut capped) = (0, 0);
	for players in 1..64 {
		let c = chance_percent(players);
		assert!(c >= prev, "{players} players: {c} < {prev}");
		climbed += u32::from(c > prev);
		capped += u32::from(c == MAX_PERCENT);
		prev = c;
	}
	assert!(climbed > 1, "the head-count never moves the odds");
	assert!(capped > 1, "the cap is never reached in the sampled range");
	assert_eq!(chance_percent(u32::MAX), MAX_PERCENT, "the cap is a cap");
}

/// Slots rolled per cell where a hit is wanted. At the capped odds a
/// face the rule accepts cannot go unhit this many times by chance.
const ROLLS: u64 = 200;

/// The scope and the rule name one set of faces: every kind's draw
/// leaves one the sweep is offered, the scope holds nothing else, and at
/// a real find's coordinates the rule accepts that kind's face alone —
/// swept over every block, so a face the scope offers and the rule
/// refuses is caught as surely as one it never sees.
#[test]
fn the_scope_and_the_rule_name_one_set_of_faces() {
	let scope = NodeRegrow.scope().blocks;
	let mut kinds = 0;
	for &n in &NODES {
		let face = spent_block(n).expect("a node with no spent face");
		assert_eq!(
			scope.iter().filter(|&&b| b == face).count(),
			1,
			"a draw on {n:?} leaves {face:?}, which the sweep is not offered exactly once"
		);
		assert!(
			!node_host(face) && !face.is_node_block(),
			"{face:?} may not be swept at all"
		);
		kinds += 1;
	}
	assert_eq!(
		scope.len(),
		kinds,
		"the scope names a face no draw leaves, or two kinds share one"
	);

	let at = find_heartwood();
	let mine = spent_block(BlockType::Heartwood).expect("the heart's own face");
	for b in BlockType::ALL {
		let mut w = Sparse::default();
		w.record(at, b);
		let ever = (0..ROLLS).any(|slot| {
			NodeRegrow
				.apply(at, &w, SEED, run(slot, u32::MAX))
				.is_some()
		});
		assert_eq!(ever, b == mine, "{b:?} standing where a heart was");
	}
}

/// Reopening a world must not hand the finds straight back. The host's
/// clock restarts at zero, so the slot standing at load is one an
/// earlier session may already have drawn from — the sweep anchors there
/// and waits for the next boundary instead. Driven across that whole
/// span at the capped odds, then one tick past it.
#[test]
fn a_reload_draws_nothing_before_the_next_slot() {
	let at = find_heartwood();
	let mut w = Sparse::default();
	w.record(at, BlockType::DrainedHeartwood);
	let mut s = Scheduler::new();
	for job in jobs() {
		s.register(job);
	}
	// The clock the load-time slot ends at: one whole day past the
	// schedule's own offset into it.
	let boundary = f64::from(DAY_SECS) * (1.0 + f64::from(NodeRegrow.schedule().offset));
	const SAMPLES: u32 = 200;
	for step in 0..SAMPLES {
		let secs = boundary * f64::from(step) / f64::from(SAMPLES);
		assert!(
			s.tick(secs, SEED, &[at], u32::MAX, &w).is_empty(),
			"a find came back at {secs}s, before the first boundary"
		);
		assert!(s.sweeping().is_none(), "a sweep started at {secs}s");
	}
	s.tick(boundary, SEED, &[at], u32::MAX, &w);
	assert!(
		s.sweeping().is_some(),
		"the sweep never came due after the boundary"
	);
}

#[test]
fn only_a_spent_face_standing_where_its_find_was_regrows() {
	let at = find_heartwood();
	let off = (at.0, at.1 + 40, at.2);
	let mut w = Sparse::default();
	w.record(at, BlockType::DrainedHeartwood);
	w.record(off, BlockType::DrainedHeartwood);
	let wrong = (at.0, at.1, at.2 + 1);
	w.record(wrong, BlockType::SpentLeystone);
	let mut regrew = 0;
	for slot in 0..400 {
		let r = run(slot, u32::MAX);
		if let Some(v) = NodeRegrow.apply(at, &w, SEED, r) {
			assert_eq!(v, BlockType::Heartwood as u16);
			regrew += 1;
		}
		assert_eq!(
			NodeRegrow.apply(off, &w, SEED, r),
			None,
			"a carried face regrew"
		);
		assert_eq!(
			NodeRegrow.apply(wrong, &w, SEED, r),
			None,
			"a face of another kind regrew"
		);
	}
	assert!(regrew > 0, "the find never came back at the capped rate");
}

/// More players never lowers a cell's odds: a slot that regrows for a
/// quieter world regrows for a busier one, and the observed rate tracks
/// the stated percentage.
#[test]
fn a_busier_world_regrows_a_superset_at_the_stated_rate() {
	let at = find_heartwood();
	let mut w = Sparse::default();
	w.record(at, BlockType::DrainedHeartwood);
	const SLOTS: u64 = 4000;
	for players in [1, 2, 3, 10, 50] {
		let mut hits = 0;
		for slot in 0..SLOTS {
			let busy = NodeRegrow.apply(at, &w, SEED, run(slot, players)).is_some();
			let quiet = NodeRegrow.apply(at, &w, SEED, run(slot, 1)).is_some();
			assert!(
				busy || !quiet,
				"slot {slot}: a quiet world regrew where a busy one did not"
			);
			hits += u64::from(busy);
		}
		let want = u64::from(chance_percent(players)) * SLOTS / 100;
		let slack = SLOTS / 50;
		assert!(
			hits.abs_diff(want) <= slack,
			"{players} players: {hits} of {SLOTS} regrew, expected about {want}"
		);
	}
}

/// Through the scheduler: the sweep finds the face as an edit, writes
/// the live node back, and the roster shared with the server registers.
#[test]
fn the_sweep_hands_the_live_face_back() {
	let at = find_heartwood();
	let mut w = Sparse::default();
	w.record(at, BlockType::DrainedHeartwood);
	let mut s = Scheduler::new();
	for job in jobs() {
		s.register(job);
	}
	let eye = (at.0, at.1, at.2);
	let mut writes = Vec::new();
	for day in 0..200 {
		let secs =
			f64::from(DAY_SECS) * (f64::from(day) + NodeRegrow.schedule().offset as f64 + 0.01);
		for _ in 0..64 {
			writes.extend(s.tick(secs, SEED, &[eye], 50, &w));
		}
		if !writes.is_empty() {
			break;
		}
	}
	assert!(!writes.is_empty(), "no regrowth over the sampled days");
	for &(cell, v) in &writes {
		assert_eq!(cell, at);
		assert_eq!(v, BlockType::Heartwood as u16);
	}
}
