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
	let p = Pipeline::new(Seed::current(0.0), 4, 0);
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
	let mut p = Pipeline::new(Seed::current(0.0), 6, 0);
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
	let mut p = Pipeline::new(Seed::current(0.0), 4, 0);
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
	let mut p = Pipeline::new(Seed::current(0.0), 4, 0);
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
