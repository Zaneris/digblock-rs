//! The per-chunk slot budget. The renderer's shared index buffer holds
//! [`CHUNK_SLOTS`] entries per chunk stream, and every baked entry, a
//! block face, a shape quad, a ramp or a folded triangle, spends one. A
//! chunk's cost is a strict upper bound on what its mesh can emit: a fixed
//! reserve covers full blocks at their densest, and each shaped cell adds
//! its whole template, since culling only ever removes entries. Both ends
//! compute it from chunk contents alone, so the server refuses a place
//! that would push a chunk past it and the client predicts the refusal.
//! Removing always fits. The per-shape cap (`shapes::MAX_SHAPE_QUADS`) no
//! longer sizes anything on the chunk side: this budget does.

use crate::block::{BlockType, RenderClass};
use crate::chunk::{CHUNK_VOLUME, ChunkPos, cell_id, cell_state, split_world};

/// Slots the shared index buffer holds per chunk mesh stream. Was 4096 ×
/// the old per-shape cap of 48, and is kept at that size on purpose: it is
/// allocated once and has to stay small on mobile.
pub const CHUNK_SLOTS: usize = 196_608;

/// Entries one chunk's full blocks can emit at most. A face needs a solid
/// cell on one side and, on the other, a non-covering cell inside the
/// chunk or the chunk's border: with s solid cells that is
/// min(6s, 6(4096 - s) + 6·256), at most 13,056 (s = 2176). Rounded up to
/// four per cell for headroom.
pub const FULL_BLOCK_RESERVE: u32 = 4 * CHUNK_VOLUME as u32;

/// What shaped cells may add on top of the reserve.
pub const SHAPED_SLOTS: u32 = CHUNK_SLOTS as u32 - FULL_BLOCK_RESERVE;

/// One cell's cost beyond the reserve: a shaped cell's whole template,
/// nothing for anything else.
pub fn cell_cost(cell: u16) -> u32 {
	let b = BlockType::from_id(cell_id(cell));
	if b.props().class != RenderClass::Shaped {
		return 0;
	}
	crate::shapes::shape_of(b, cell_state(cell)).map_or(0, |v| v.quads.len() as u32)
}

/// A chunk's cost: the sum over its shaped cells.
pub fn chunk_cost(blocks: &[u16; CHUNK_VOLUME]) -> u32 {
	blocks.iter().map(|&c| cell_cost(c)).sum()
}

/// Whether a set of writes fits every chunk it touches. `cell_at` reads a
/// cell's current value and `chunk_cost` a loaded chunk's cost; an
/// unloaded chunk fits (the write fails on its own later), and so does a
/// cell outside the world.
pub fn writes_fit(
	writes: &[((i32, i32, i32), u16)],
	cell_at: impl Fn((i32, i32, i32)) -> Option<u16>,
	chunk_cost: impl Fn(ChunkPos) -> Option<u32>,
) -> bool {
	let mut deltas: Vec<(ChunkPos, i64)> = Vec::new();
	for &(c, new) in writes {
		let Some(((cx, cy, cz), _)) = split_world(c.0, c.1, c.2) else {
			continue;
		};
		let old = cell_at(c).map_or(0, cell_cost);
		let delta = i64::from(cell_cost(new)) - i64::from(old);
		let pos = ChunkPos::new(cx, cy, cz);
		match deltas.iter_mut().find(|(p, _)| *p == pos) {
			Some(d) => d.1 += delta,
			None => deltas.push((pos, delta)),
		}
	}
	deltas.iter().all(|&(pos, delta)| {
		delta <= 0
			|| chunk_cost(pos).is_none_or(|cost| i64::from(cost) + delta <= i64::from(SHAPED_SLOTS))
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ChunkData;
	use crate::block::BlockType as B;
	use crate::chunk::make_cell;
	use crate::mesher::{MeshInput, Src, mesh_chunk};
	use crate::shapes::{Facing, door_state, shape_of};

	fn entries(d: &ChunkData) -> u32 {
		let m = mesh_chunk(&MeshInput::build(d, |_, _, _| Src::Missing));
		(m.solid.len() / 8) as u32
	}

	#[test]
	fn a_checkerboard_of_full_blocks_stays_under_the_reserve() {
		let mut d = ChunkData::default();
		for y in 0..16 {
			for z in 0..16 {
				for x in 0..16 {
					if (x + y + z) % 2 == 0 {
						d.set(x, y, z, B::Stone);
					}
				}
			}
		}
		let n = entries(&d);
		assert_eq!(n, 2048 * 6, "every face of every solid cell shows");
		assert!(n <= FULL_BLOCK_RESERVE);
		assert_eq!(chunk_cost(&d.blocks), 0);
	}

	#[test]
	fn the_cost_is_the_template_and_bounds_the_mesh() {
		// The upper door half is the busiest shipped variant; a chunk
		// packed solid with it meshes under reserve + cost, and the cost
		// itself is under budget.
		let upper = make_cell(B::LumberDoor, door_state(Facing::PosX, false, true));
		assert_eq!(
			cell_cost(upper),
			shape_of(B::LumberDoor, cell_state(upper))
				.unwrap()
				.quads
				.len() as u32
		);
		assert_eq!(cell_cost(B::Stone as u16), 0);
		let mut d = ChunkData::default();
		for i in 0..CHUNK_VOLUME {
			d.blocks[i] = upper;
		}
		let cost = chunk_cost(&d.blocks);
		assert!(cost <= SHAPED_SLOTS, "{cost} > {SHAPED_SLOTS}");
		let n = entries(&d);
		assert!(
			n <= FULL_BLOCK_RESERVE + cost,
			"{n} > {FULL_BLOCK_RESERVE} + {cost}"
		);
		assert!(n as usize <= CHUNK_SLOTS);
	}

	#[test]
	fn writes_fit_charges_each_chunk_for_its_own_delta() {
		let upper = make_cell(B::LumberDoor, door_state(Facing::PosX, false, true));
		let stair = make_cell(B::WoodStairs, 0);
		let c_upper = cell_cost(upper);
		let c_stair = cell_cost(stair);
		// Chunk (0,1,0) is one shape short of full; (1,1,0) is empty.
		let full_minus = SHAPED_SLOTS - c_stair;
		let cost = |p: ChunkPos| {
			Some(if p == ChunkPos::new(0, 1, 0) {
				full_minus
			} else {
				0
			})
		};
		let air = |_c: (i32, i32, i32)| Some(0u16);
		assert!(
			writes_fit(&[((1, 16, 1), stair)], air, cost),
			"exactly fills"
		);
		assert!(
			!writes_fit(&[((1, 16, 1), upper)], air, cost),
			"one door over"
		);
		assert!(
			writes_fit(&[((17, 16, 1), upper)], air, cost),
			"next chunk is empty"
		);
		// Two writes into the full chunk add up.
		assert!(!writes_fit(
			&[((1, 16, 1), stair), ((2, 16, 1), stair)],
			air,
			cost
		));
		// Replacing a shape with a cheaper one, or with air, always fits.
		let was_door = |_c: (i32, i32, i32)| Some(upper);
		assert!(writes_fit(&[((1, 16, 1), stair)], was_door, cost));
		assert!(writes_fit(&[((1, 16, 1), 0)], was_door, cost));
		// Unloaded fits; the write fails on its own later.
		assert!(writes_fit(&[((1, 16, 1), upper)], air, |_| None));
		assert!(c_upper > c_stair);
	}
}
