use super::*;
use crate::chunk::CHUNK_SIZE;
use crate::decoration::{Structure, structures_near};
use crate::sun::DAY_SECS;
use crate::worldjob::Scheduler;
use rustc_hash::FxHashMap;

const SEED: f64 = 7.0;
const AT: (i32, i32, i32) = (0, 64, 0);

/// A hand-built pocket of cells. `airy` answers Air everywhere unset;
/// `sealed` answers `None` there, the unloaded world.
struct Pocket {
	cells: FxHashMap<(i32, i32, i32), u16>,
	fill: Option<u16>,
}

impl Pocket {
	fn airy() -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: Some(BlockType::Air as u16),
		}
	}
	fn sealed() -> Self {
		Self {
			cells: FxHashMap::default(),
			fill: None,
		}
	}
	fn set(&mut self, at: (i32, i32, i32), b: BlockType) {
		self.cells.insert(at, b as u16);
	}
}

impl CellSource for Pocket {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.cells.get(&(x, y, z)).copied().or(self.fill)
	}
}

/// Real structures over synthesized terrain: solid below the column's
/// height, air above, blocks applied in generation order under
/// decoration's write rule — Air only, roots excepted. (Real roots
/// replace the surface block; the synth's is stone, a distinction the
/// mesher cares about and the anchor walk does not.)
struct Grown {
	cells: FxHashMap<(i32, i32, i32), u16>,
	seed: f64,
}

impl CellSource for Grown {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.cells.get(&(x, y, z)).copied().or_else(|| {
			Some(if y <= worldgen::terrain_height(x, z, self.seed) {
				BlockType::Stone as u16
			} else {
				BlockType::Air as u16
			})
		})
	}
}

/// Kinds of structure that contributed at least one leaf, in
/// [tree, landmark, bush] order.
fn grow(px: i32, pz: i32, seed: f64) -> (Grown, Vec<(i32, i32, i32)>, [bool; 3]) {
	let mut cells = FxHashMap::default();
	let mut leaves = Vec::new();
	let mut kinds = [false; 3];
	for st in structures_near(px, pz, seed) {
		st.for_each_block(|x, y, z, b| {
			let buried = y <= worldgen::terrain_height(x, z, seed);
			if (buried && !st.is_root(x, y, z)) || cells.contains_key(&(x, y, z)) {
				return;
			}
			cells.insert((x, y, z), b as u16);
			if b.is_foliage() {
				leaves.push((x, y, z));
				match st {
					Structure::Tree(_) => kinds[0] = true,
					Structure::Landmark(_) => kinds[1] = true,
					Structure::Bush(_) => kinds[2] = true,
					Structure::Boulder(_) => {}
				}
			}
		});
	}
	(Grown { cells, seed }, leaves, kinds)
}

/// Hunt outward from the origin for a region holding a landmark. The
/// step ties to `structures_near`'s footprint (a chunk plus the apron on
/// each side), so the rings tile the plane with no gap.
fn find_landmark(seed: f64) -> Option<(i32, i32)> {
	let step = CHUNK_SIZE as i32 + 2 * crate::decoration::STRUCTURE_APRON;
	for ring in 0..64i32 {
		for cz in -ring..=ring {
			for cx in -ring..=ring {
				if cx.abs().max(cz.abs()) != ring {
					continue;
				}
				let (px, pz) = (cx * step, cz * step);
				if structures_near(px, pz, seed)
					.iter()
					.any(|s| matches!(s, Structure::Landmark(_)))
				{
					return Some((px, pz));
				}
			}
		}
	}
	None
}

/// Every leaf worldgen hangs — canopy rim, jittered edge, bush — stays
/// anchored whatever the run: the cap and the anchor rules must keep
/// ahead of what decoration actually grows. Coverage is counted per
/// structure kind, or a quiet region would pass vacuously; landmarks
/// are rare enough to need hunting rather than hoping.
#[test]
fn every_pristine_structure_leaf_survives() {
	let job = LeafDecay;
	let mut seen = [false; 3];
	let mut verify = |px: i32, pz: i32| {
		let (w, leaves, kinds) = grow(px, pz, SEED);
		for at in leaves {
			for run in 0..3 {
				assert_eq!(job.apply(at, &w, SEED, run), None, "{at:?} run {run}");
			}
		}
		for (s, k) in seen.iter_mut().zip(kinds) {
			*s |= k;
		}
	};
	for gx in -4..=4 {
		for gz in -4..=4 {
			verify(gx * 48, gz * 48);
		}
	}
	let (px, pz) = find_landmark(SEED).expect("no landmark within the hunt");
	verify(px, pz);
	assert_eq!(seen, [true; 3], "[tree, landmark, bush] sampled");
}

/// Some runs rot an orphan and some spare it — decay is ragged, never a
/// slab — and what it rots to is air.
#[test]
fn an_orphan_rots_raggedly_across_runs() {
	let mut w = Pocket::airy();
	w.set(AT, BlockType::Leaves);
	let (mut rotted, mut spared) = (0u32, 0u32);
	for run in 0..32 {
		match LeafDecay.apply(AT, &w, SEED, run) {
			Some(c) => {
				assert_eq!(c, BlockType::Air as u16, "run {run}");
				rotted += 1;
			}
			None => spared += 1,
		}
	}
	assert!(rotted > 0 && spared > 0, "{rotted} rotted, {spared} spared");
}

/// Reach is DERIVED from the cap: a chain hanging off one wood block
/// keeps every leaf the walk can reach and rots the first it cannot.
#[test]
fn the_walk_reaches_exactly_the_cap() {
	let mut w = Pocket::airy();
	w.set((0, 64, 0), BlockType::Wood);
	let past = MAX_STEPS as i32 + 1;
	for x in 1..=past {
		w.set((x, 64, 0), BlockType::Leaves);
	}
	for x in 1..past {
		for run in 0..8 {
			assert_eq!(LeafDecay.apply((x, 64, 0), &w, SEED, run), None, "x {x}");
		}
	}
	assert!(
		(0..32).any(|run| LeafDecay.apply((past, 64, 0), &w, SEED, run).is_some()),
		"the leaf past the cap must rot"
	);
}

/// A leaf resting on solid ground holds — and holds its floating
/// neighbor — with no wood anywhere: the bush rule. Water is not
/// ground; a leaf over it rots.
#[test]
fn ground_holds_a_bush_and_water_does_not() {
	let mut w = Pocket::airy();
	w.set((0, 63, 0), BlockType::Grass);
	w.set((0, 64, 0), BlockType::Leaves);
	w.set((1, 64, 0), BlockType::Leaves);
	for at in [(0, 64, 0), (1, 64, 0)] {
		for run in 0..8 {
			assert_eq!(LeafDecay.apply(at, &w, SEED, run), None, "{at:?}");
		}
	}
	let mut w = Pocket::airy();
	w.set((0, 63, 0), BlockType::Water);
	w.set((0, 64, 0), BlockType::PineLeaves);
	assert!((0..32).any(|run| LeafDecay.apply((0, 64, 0), &w, SEED, run).is_some()));
}

/// Siphoning a heartwood out of a trunk must not orphan the canopy: the
/// face the draw leaves standing still reads as trunk, DERIVED from
/// `spent_block` so a retuned face moves with it.
#[test]
fn a_drained_find_still_reads_as_trunk() {
	let spent = worldgen::nodes::spent_block(BlockType::Heartwood).unwrap();
	let mut w = Pocket::airy();
	w.set((0, 64, 0), spent);
	w.set((1, 64, 0), BlockType::Leaves);
	for run in 0..8 {
		assert_eq!(
			LeafDecay.apply((1, 64, 0), &w, SEED, run),
			None,
			"run {run}"
		);
	}
}

/// Judgment against unloaded terrain is deferred, never guessed: a walk
/// that touches a `None` cell leaves the leaf alone on every run.
#[test]
fn an_unloaded_read_defers_judgment() {
	let mut w = Pocket::sealed();
	w.set(AT, BlockType::Leaves);
	for run in 0..32 {
		assert_eq!(LeafDecay.apply(AT, &w, SEED, run), None, "run {run}");
	}
}

/// The sweep hunts exactly the foliage set — swept over every block, so
/// a new leaf kind joins by carrying `is_foliage` — and finds its
/// candidates on the edit frontier with no surface shell, because
/// pristine terrain cannot orphan a leaf.
#[test]
fn the_scope_is_exactly_the_foliage_roster() {
	let scope = LeafDecay.scope();
	let mut foliage = 0u32;
	for b in BlockType::ALL {
		assert_eq!(scope.blocks.contains(&b), b.is_foliage(), "{b:?}");
		foliage += u32::from(b.is_foliage());
	}
	assert!(foliage > 0, "the roster has members to sweep");
	assert_eq!(
		scope.candidates,
		Candidates::Frontier {
			surface_shell: false
		}
	);
}

/// A host-style world for the scheduler: writes land as cells AND in the
/// edit record, exactly what keeps a cascade self-sustaining.
struct Host {
	cells: FxHashMap<(i32, i32, i32), u16>,
	edits: Vec<(i32, i32, i32)>,
}

impl Host {
	fn record(&mut self, at: (i32, i32, i32), cell: u16) {
		self.cells.insert(at, cell);
		self.edits.push(at);
	}
}

impl CellSource for Host {
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

/// The frontier is the propagation engine: felling a trunk puts only
/// the adjacent ring of leaves under judgment, and each rotted ring's
/// own writes recruit the next — so the rim needs more than one run,
/// and after enough of them the whole canopy is gone.
#[test]
fn decay_cascades_outward_from_the_cut() {
	let mut w = Host {
		cells: FxHashMap::default(),
		edits: Vec::new(),
	};
	let (top, r) = (68, 2);
	for y in 64..=top {
		w.cells.insert((0, y, 0), BlockType::Wood as u16);
	}
	for y in top - 1..=top + 1 {
		for dz in -r..=r {
			for dx in -r..=r {
				w.cells
					.entry((dx, y, dz))
					.or_insert(BlockType::Leaves as u16);
			}
		}
	}
	// Fell the whole trunk, as a dig would: write air, record the edit.
	for y in 64..=top {
		w.record((0, y, 0), BlockType::Air as u16);
	}
	let mut sched = Scheduler::new();
	let mut per_day = 0;
	for job in jobs() {
		per_day = job.schedule().per_day;
		sched.register(job);
	}
	let leaves_left = |w: &Host| {
		w.cells
			.values()
			.filter(|&&c| c == BlockType::Leaves as u16)
			.count()
	};
	let mut slots_used = 0;
	for slot in 1..=400u32 {
		let secs = f64::from(DAY_SECS) * (f64::from(slot) + 0.5) / f64::from(per_day);
		loop {
			let writes = sched.tick(secs, SEED, &[(0, 64, 0)], &w);
			for (at, val) in writes {
				w.record(at, val);
			}
			if sched.sweeping().is_none() {
				break;
			}
		}
		slots_used = slot;
		if leaves_left(&w) == 0 {
			break;
		}
	}
	assert_eq!(leaves_left(&w), 0, "the canopy never finished rotting");
	assert!(slots_used > 1, "one run must not reach the rim");
}
