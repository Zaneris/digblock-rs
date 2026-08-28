//! The CPU copy of the coarse texture, and what is derived from it: the sky ceiling (R18) and
//! the bookkeeping that lets eviction prune it. GPU-free, so the two invariants that matter —
//! the ceiling is the highest nonzero brick row, and a slot is only ever cleared by the chunk
//! that wrote it — are pinned by plain tests.

use super::*;

/// Brick rows in the window: the world's height in bricks.
const BRICK_ROWS: usize = (WORLD_H / 8) as usize;

/// One word per coarse brick, in upload order (x fastest, then y, z slowest), plus a count of
/// nonzero words per brick row and the chunk each toroidal slot was last written for.
///
/// The rows are what make the ceiling cheap: `sky_y` is the highest row with anything in it,
/// a 32-entry scan maintained word by word, where it used to be a scan of the whole mirror
/// (204,800 words at draw 20) on every upload that changed a brick.
///
/// The owners are what make eviction safe: the window is toroidal and an evicted chunk's slot
/// may already hold the chunk that replaced it, so clearing a slot on eviction alone would erase
/// live occupancy — which is why the mirror was never pruned and `sky_y` ratcheted over a
/// session. No two RESIDENT chunks share a slot (`window_chunks_for` is twice the draw distance,
/// pinned by `the_window_covers_the_streamed_radius`), so "the slot is still mine" is exactly
/// "nobody has overwritten it", and pruning on that condition is exact.
pub(super) struct CoarseMirror {
	/// Window side in chunks — the toroidal modulus.
	wc: usize,
	words: Box<[u32]>,
	rows: [u32; BRICK_ROWS],
	owners: Box<[Option<ChunkPos>]>,
}

impl CoarseMirror {
	pub(super) fn new(window_chunks: u32) -> Self {
		let wc = window_chunks as usize;
		Self {
			wc,
			words: vec![0u32; wc * 2 * BRICK_ROWS * wc * 2].into_boxed_slice(),
			rows: [0; BRICK_ROWS],
			owners: vec![None; wc * (WORLD_H as usize / 16) * wc].into_boxed_slice(),
		}
	}

	/// A chunk's toroidal slot: its lateral coordinates wrapped, its y direct (the window is
	/// the world's whole height).
	fn slot(&self, pos: ChunkPos) -> (usize, usize, usize) {
		let wc = self.wc as i32;
		(
			pos.x.rem_euclid(wc) as usize,
			pos.y as usize,
			pos.z.rem_euclid(wc) as usize,
		)
	}

	fn owner_index(&self, (tx, ty, tz): (usize, usize, usize)) -> usize {
		(tz * (WORLD_H as usize / 16) + ty) * self.wc + tx
	}

	/// The mirror index of brick `ci` (the chunk's own 2×2×2 order, `classify_chunk`'s) of the
	/// chunk in slot `(tx, ty, tz)`.
	fn word_index(&self, (tx, ty, tz): (usize, usize, usize), ci: usize) -> usize {
		let w = self.wc * 2;
		let (dz, dy, dx) = (ci / 4, (ci / 2) % 2, ci % 2);
		((tz * 2 + dz) * BRICK_ROWS + (ty * 2 + dy)) * w + tx * 2 + dx
	}

	/// Set one word, keeping the row counts in step. True if it changed.
	fn set(&mut self, index: usize, word: u32) -> bool {
		let old = self.words[index];
		if old == word {
			return false;
		}
		let row = (index / (self.wc * 2)) % BRICK_ROWS;
		if old == 0 {
			self.rows[row] += 1;
		} else if word == 0 {
			self.rows[row] -= 1;
		}
		self.words[index] = word;
		true
	}

	/// A chunk's eight words landed in its slot. True if any word changed.
	pub(super) fn write(&mut self, pos: ChunkPos, coarse: &[u32; 8]) -> bool {
		let slot = self.slot(pos);
		let mut changed = false;
		for (ci, &word) in coarse.iter().enumerate() {
			let i = self.word_index(slot, ci);
			changed |= self.set(i, word);
		}
		let o = self.owner_index(slot);
		self.owners[o] = Some(pos);
		changed
	}

	/// An evicted chunk's words are forgotten — only if its slot is still its own (see the
	/// type's docs for why that condition is exact). True if any word changed.
	pub(super) fn evict(&mut self, pos: ChunkPos) -> bool {
		let slot = self.slot(pos);
		let o = self.owner_index(slot);
		if self.owners[o] != Some(pos) {
			return false;
		}
		self.owners[o] = None;
		let mut changed = false;
		for ci in 0..8 {
			let i = self.word_index(slot, ci);
			changed |= self.set(i, 0);
		}
		changed
	}

	/// Forget everything (the F7 drill, an arrival).
	pub(super) fn clear(&mut self) {
		self.words.fill(0);
		self.rows = [0; BRICK_ROWS];
		self.owners.fill(None);
	}

	/// The sky ceiling (R18): the top cell of the highest brick row holding anything at all —
	/// a blocker or an emitter, since the ceiling gates hits of any kind. A non-descending ray
	/// above it is proven lit/sky in one float compare, which ends the sun march at the tallest
	/// local hill instead of the top of the world. Negative while the mirror is empty.
	pub(super) fn sky_y(&self) -> f32 {
		self.rows
			.iter()
			.rposition(|&n| n > 0)
			.map_or(-1.0, |y| (y * 8 + 7) as f32)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_sky_ceiling_tops_the_highest_brick() {
		let mut m = CoarseMirror::new(40);
		assert_eq!(m.sky_y(), -1.0);
		// A blocker in the top brick of chunk row 4: brick row 9.
		let low = ChunkPos::new(17, 4, 33);
		assert!(m.write(low, &[0, 0, 0, 0, 0, 0, 0b01, 0]));
		assert_eq!(m.sky_y(), (9 * 8 + 7) as f32);
		// A higher EMITTER-only brick raises it too — the ceiling gates
		// hits of any kind.
		let high = ChunkPos::new(5, 12, 2);
		assert!(m.write(high, &[0b10, 0, 0, 0, 0, 0, 0, 0]));
		assert_eq!(m.sky_y(), (24 * 8 + 7) as f32);
		assert!(
			!m.write(high, &[0b10, 0, 0, 0, 0, 0, 0, 0]),
			"a rewrite of the same words changes nothing"
		);
		// And eviction brings it back DOWN — the ratchet this type exists to remove.
		assert!(m.evict(high));
		assert_eq!(m.sky_y(), (9 * 8 + 7) as f32);
		assert!(m.evict(low));
		assert_eq!(m.sky_y(), -1.0);
		assert!(!m.evict(low), "evicting twice is a no-op");
	}

	/// The toroidal case the prune has to get right: a chunk a whole window away lands in the
	/// same slot, and the FIRST tenant's eviction must not erase the second's occupancy.
	#[test]
	fn eviction_only_clears_a_slot_the_chunk_still_owns() {
		let mut m = CoarseMirror::new(40);
		let first = ChunkPos::new(3, 7, 9);
		let second = ChunkPos::new(3 + 40, 7, 9 - 40);
		assert!(m.write(first, &[1; 8]));
		assert!(
			m.write(second, &[2; 8]),
			"the second tenant overwrites the slot"
		);
		assert!(
			!m.evict(first),
			"the first tenant's eviction touches nothing"
		);
		assert_eq!(m.sky_y(), (15 * 8 + 7) as f32);
		assert!(m.evict(second));
		assert_eq!(m.sky_y(), -1.0);
		m.write(first, &[1; 8]);
		m.clear();
		assert_eq!(m.sky_y(), -1.0);
		assert!(!m.evict(first), "a clear forgets the owners too");
	}
}
