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
				if node_at((x, y, z), SEED) == Some(BlockType::Heartwood) {
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

#[test]
fn the_chance_climbs_with_the_head_count_and_caps() {
	assert_eq!(chance_percent(0), 0);
	assert_eq!(chance_percent(1), PERCENT_PER_PLAYER);
	let mut prev = 0;
	let mut capped = 0;
	for players in 0..64 {
		let c = chance_percent(players);
		assert!(c >= prev, "{players} players: {c} < {prev}");
		assert!(c <= MAX_PERCENT);
		if c == MAX_PERCENT {
			capped += 1;
		}
		prev = c;
	}
	assert!(capped > 1, "the cap is never reached in the sampled range");
	assert_eq!(chance_percent(u32::MAX), MAX_PERCENT);
}

#[test]
fn the_scope_is_every_spent_face_and_no_host() {
	let scope = NodeRegrow.scope();
	let want: FxHashSet<u16> = NODES
		.iter()
		.filter_map(|&n| spent_block(n))
		.map(|b| b as u16)
		.collect();
	let got: FxHashSet<u16> = scope.blocks.iter().map(|&b| b as u16).collect();
	assert_eq!(got, want);
	assert_eq!(got.len(), NODES.len(), "two kinds share a spent face");
	for &b in scope.blocks {
		assert!(!node_host(b), "{b:?} hosts nodes");
	}
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
