//! Writing cells. Copy-on-write, bumping only the neighbours whose
//! shell contains the edit; edits to non-resident columns defer until
//! the column lands.

use super::*;

impl Pipeline {
	/// Set one block, remeshing the chunk and every Ready neighbor whose padded shell can see the
	/// edited cell (faces, and the edge/corner diagonals at touched borders). Returns the previous
	/// block; None if outside the world, the chunk isn't Ready, or the edit is a no-op. In-flight
	/// meshes of the old data are dropped by the generation check in `drain_results`.
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

	/// Apply an AUTHORITATIVE block value (a server edit): unlike [`set_block`], an edit into a
	/// not-yet-generated chunk is not dropped — it is held and applied (pre-mesh) when the chunk
	/// lands, so join-sync edits racing generation can never be lost. Every world consumer gets
	/// this by owning a `Pipeline`, instead of re-inventing a retry map of its own.
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

	/// Re-mesh the chunk owning this cell, without writing anything. For the one case where a
	/// chunk's MESH depends on something other than its cells: the ore reveal reads the edit
	/// overlay, so recording a cell can change what its column draws while every value stays put.
	/// Neighbours are deliberately not bumped — the reveal is a per-cell stamp inside the owning
	/// chunk.
	pub(super) fn remesh_at(&mut self, x: i32, y: i32, z: i32) {
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
	pub(super) fn try_apply(&mut self, cell: (i32, i32, i32), v: u16) -> bool {
		self.set_cell(cell.0, cell.1, cell.2, v).is_some()
			|| self.cell_at(cell.0, cell.1, cell.2) == Some(v)
	}

	/// Deferred authoritative edits whose column just generated: apply
	/// through `set_block` (COW + remesh + border bumps) now that the
	/// chunks are Ready. Queued-but-undispatched meshes snapshot at
	/// dispatch time, so they see these writes.
	pub(super) fn apply_deferred_for(&mut self, landed: &[(i32, i32)]) {
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
