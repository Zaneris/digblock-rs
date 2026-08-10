//! Stage-B voxel light field (LIGHT-DESIGN.md): a client-side, derived,
//! never-serialized map of per-cell light, filled by a budgeted two-queue
//! BFS. Occlusion correctness is structural — light only reaches a cell by
//! stepping through open cells, so "no light through walls" needs no
//! special case. The field lives BESIDE the chunk store (a sibling map on
//! `Pipeline`), because the BFS reads blocks while writing light, and
//! because shared AIR_CHUNK sentinels still carry per-cell light.
//!
//! One byte per cell: low nibble blocklight, high nibble skylight
//! (skylight is P3; everything here is written nibble-aware from day one).
//! A missing chunk entry reads as `Uniform(0)`.

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::block::BlockType;
use crate::chunk::{cell_id, split_world, ChunkPos, CHUNK_SIZE, CHUNK_VOLUME};

pub const MAX_LIGHT: u8 = 15;

/// BFS pops per `Pipeline::maintain` call. A count, not a deadline: pops
/// are uniform-cost, and a count keeps settlement deterministic (pinned by
/// `any_step_budget_settles_to_the_same_field`). One torch floods ~5k
/// cells, so a single edit settles within a frame; only mass streaming
/// carries over, and dirty-gating makes carry-over late, never wrong.
pub const LIGHT_BUDGET: u32 = 16 * 1024;

/// Per-chunk light storage. `Uniform` never allocates — dark chunks (and,
/// in P3, full-sky chunks) stay a tag; `Mixed` allocates on the first
/// divergent write.
pub enum LightChunk {
	Uniform(u8),
	Mixed(Box<[u8; CHUNK_VOLUME]>),
}

impl LightChunk {
	#[inline]
	fn get(&self, lx: usize, ly: usize, lz: usize) -> u8 {
		match self {
			LightChunk::Uniform(v) => *v,
			LightChunk::Mixed(cells) => cells[(ly * CHUNK_SIZE + lz) * CHUNK_SIZE + lx],
		}
	}
}

/// The block-side facts the BFS needs about one cell. `None` = chunk not
/// resident: the BFS treats it as a wall (never writes into or reads
/// through it) — arrival seeding repairs the boundary when the chunk
/// lands.
#[derive(Clone, Copy)]
pub struct CellFacts {
	/// The mesher's AO rule, shared verbatim: `RenderClass::Opaque` blocks
	/// light; Cutout/Translucent/Shaped pass.
	pub opaque: bool,
	/// `BlockType::light_level()` — the emission this cell re-seeds with
	/// if a removal wave strips it.
	pub emits: u8,
}

/// Read facts for a raw cell value.
#[inline]
pub fn facts_of(cell: u16) -> CellFacts {
	let b = BlockType::from_id(cell_id(cell));
	CellFacts {
		opaque: b.props().class == crate::block::RenderClass::Opaque,
		emits: b.light_level(),
	}
}

pub struct LightField {
	chunks: FxHashMap<ChunkPos, LightChunk>,
	add: VecDeque<(i32, i32, i32)>,
	/// Removal wavefront: the position and the light value it HELD, which
	/// bounds how far the darkness spreads.
	remove: VecDeque<((i32, i32, i32), u8)>,
	/// Chunks whose padded mesh shell saw a light write since the last
	/// `take_dirty` — the remesh set. Same shell rule as `set_cell`'s
	/// neighbor bumps.
	dirty: FxHashSet<ChunkPos>,
}

impl Default for LightField {
	fn default() -> Self {
		Self::new()
	}
}

impl LightField {
	pub fn new() -> Self {
		Self {
			chunks: FxHashMap::default(),
			add: VecDeque::new(),
			remove: VecDeque::new(),
			dirty: FxHashSet::default(),
		}
	}

	/// Blocklight at a world cell (0 outside the world or in dark/absent
	/// chunks).
	pub fn block_light(&self, x: i32, y: i32, z: i32) -> u8 {
		let Some(((cx, cy, cz), (lx, ly, lz))) = split_world(x, y, z) else {
			return 0;
		};
		self.chunks
			.get(&ChunkPos::new(cx, cy, cz))
			.map_or(0, |c| c.get(lx, ly, lz))
			& 0x0F
	}

	/// Queue depth (both wavefronts) — settled means zero.
	pub fn pending(&self) -> usize {
		self.add.len() + self.remove.len()
	}

	/// Write the blocklight nibble, allocating `Mixed` on divergence and
	/// recording the shell-dirty set.
	fn set_block_light(&mut self, x: i32, y: i32, z: i32, v: u8) {
		let Some(((cx, cy, cz), (lx, ly, lz))) = split_world(x, y, z) else {
			return;
		};
		let pos = ChunkPos::new(cx, cy, cz);
		let entry = self.chunks.entry(pos).or_insert(LightChunk::Uniform(0));
		if let LightChunk::Uniform(u) = entry {
			if *u & 0x0F == v {
				return; // no divergence — stay unallocated
			}
			*entry = LightChunk::Mixed(Box::new([*u; CHUNK_VOLUME]));
		}
		let LightChunk::Mixed(cells) = entry else {
			unreachable!("uniform was just promoted")
		};
		let cell = &mut cells[(ly * CHUNK_SIZE + lz) * CHUNK_SIZE + lx];
		if *cell & 0x0F == v {
			return;
		}
		*cell = (*cell & 0xF0) | v;
		// The write's own chunk plus every neighbor whose padded shell
		// contains the cell (face/edge/corner) — the mesh-visibility rule.
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
					self.dirty
						.insert(ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz));
				}
			}
		}
	}

	/// The single edit hook, called from the local-write funnel
	/// (`Pipeline::set_cell`) with the previous and new cell values. One
	/// uniform rule covers torch place, torch dig, wall place, wall dig,
	/// and fluid writes:
	/// 1. stored light above the new emission → strip it (removal wave);
	/// 2. the new cell emits → seed and spread;
	/// 3. the new cell is open → pull neighboring light in.
	pub fn on_cell_changed(&mut self, x: i32, y: i32, z: i32, _prev: u16, new: u16) {
		let new = facts_of(new);
		let cur = self.block_light(x, y, z);
		if cur > new.emits {
			self.set_block_light(x, y, z, 0);
			self.remove.push_back(((x, y, z), cur));
		}
		if new.emits > cur {
			self.set_block_light(x, y, z, new.emits);
			self.add.push_back((x, y, z));
		}
		if !new.opaque {
			// A dug wall: surrounding light floods back in. Stale or dim
			// entries self-discard at pop, so over-queuing is cheap.
			for (dx, dy, dz) in NEIGHBORS {
				self.add.push_back((x + dx, y + dy, z + dz));
			}
		}
	}

	/// Seed a freshly-landed column: every emitting cell in the stack
	/// starts a wave, and the four horizontal neighbor columns' facing
	/// light borders spread inward (the light mirror of the arrival
	/// re-dirty rule). Runs AFTER the overlay re-applied its cells, so a
	/// restored torch is in the scanned data.
	pub fn seed_column(
		&mut self,
		x: i32,
		z: i32,
		stack: impl Iterator<Item = (i32, std::sync::Arc<crate::chunk::ChunkData>)>,
	) {
		let (bx, bz) = (x * CHUNK_SIZE as i32, z * CHUNK_SIZE as i32);
		for (cy, data) in stack {
			let by = cy * CHUNK_SIZE as i32;
			for (i, &cell) in data.blocks.iter().enumerate() {
				let emits = BlockType::from_id(cell_id(cell)).light_level();
				if emits > 0 {
					let (lx, lyz) = (i % CHUNK_SIZE, i / CHUNK_SIZE);
					let (lz, ly) = (lyz % CHUNK_SIZE, lyz / CHUNK_SIZE);
					let (wx, wy, wz) = (bx + lx as i32, by + ly as i32, bz + lz as i32);
					if self.block_light(wx, wy, wz) < emits {
						self.set_block_light(wx, wy, wz, emits);
					}
					self.add.push_back((wx, wy, wz));
				}
			}
		}
		// Border pull: any lit cell (≥ 2 — a 1 has nothing to give) in a
		// neighbor column's facing layer re-spreads; pops into cells this
		// column can't accept discard themselves.
		let s = CHUNK_SIZE as i32;
		for (nx, nz, fx, fz) in [
			(x - 1, z, bx - 1, i32::MIN),
			(x + 1, z, bx + s, i32::MIN),
			(x, z - 1, i32::MIN, bz - 1),
			(x, z + 1, i32::MIN, bz + s),
		] {
			for cy in 0..crate::chunk::WORLD_VCHUNK {
				let pos = ChunkPos::new(nx, cy, nz);
				let Some(chunk) = self.chunks.get(&pos) else {
					continue;
				};
				if matches!(chunk, LightChunk::Uniform(u) if u & 0x0F < 2) {
					continue;
				}
				let by = cy * s;
				for a in 0..s {
					for b in 0..s {
						let (wx, wz) = if fz == i32::MIN {
							(fx, bz + a)
						} else {
							(bx + a, fz)
						};
						let (wx, wy, wz) = (wx, by + b, wz);
						if self.block_light(wx, wy, wz) >= 2 {
							self.add.push_back((wx, wy, wz));
						}
					}
				}
			}
		}
	}

	/// Drop an evicted chunk's light. Queue entries pointing into it
	/// self-discard at pop (`facts` answers `None` there).
	pub fn drop_chunk(&mut self, pos: ChunkPos) {
		self.chunks.remove(&pos);
		self.dirty.remove(&pos);
	}

	/// Run the BFS for up to `budget` pops against the world read `facts`
	/// (`None` = not resident = wall). Removals drain before adds — the
	/// two-queue algorithm's ordering requirement. Returns pops consumed.
	pub fn step(
		&mut self,
		budget: u32,
		mut facts: impl FnMut(i32, i32, i32) -> Option<CellFacts>,
	) -> u32 {
		let mut pops = 0;
		while pops < budget {
			if let Some(((x, y, z), old)) = self.remove.pop_front() {
				pops += 1;
				for (dx, dy, dz) in NEIGHBORS {
					let (nx, ny, nz) = (x + dx, y + dy, z + dz);
					let nl = self.block_light(nx, ny, nz);
					if nl == 0 {
						continue;
					}
					if nl < old {
						self.set_block_light(nx, ny, nz, 0);
						self.remove.push_back(((nx, ny, nz), nl));
						// An emitter caught in the wave re-seeds itself.
						if let Some(f) = facts(nx, ny, nz) {
							if f.emits > 0 {
								self.set_block_light(nx, ny, nz, f.emits);
								self.add.push_back((nx, ny, nz));
							}
						}
					} else {
						// Independently supported: the relight frontier.
						self.add.push_back((nx, ny, nz));
					}
				}
			} else if let Some((x, y, z)) = self.add.pop_front() {
				pops += 1;
				let l = self.block_light(x, y, z);
				if l <= 1 {
					continue; // stale entry, or nothing left to give
				}
				for (dx, dy, dz) in NEIGHBORS {
					let (nx, ny, nz) = (x + dx, y + dy, z + dz);
					let Some(f) = facts(nx, ny, nz) else {
						continue; // not resident: a wall for now
					};
					if f.opaque {
						continue;
					}
					if self.block_light(nx, ny, nz) + 1 < l {
						self.set_block_light(nx, ny, nz, l - 1);
						self.add.push_back((nx, ny, nz));
					}
				}
			} else {
				break;
			}
		}
		pops
	}

	/// Drain the chunks whose shell saw a light write since the last call.
	pub fn take_dirty(&mut self) -> impl Iterator<Item = ChunkPos> + '_ {
		self.dirty.drain()
	}
}

const NEIGHBORS: [(i32, i32, i32); 6] = [
	(-1, 0, 0),
	(1, 0, 0),
	(0, -1, 0),
	(0, 1, 0),
	(0, 0, -1),
	(0, 0, 1),
];

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType;
	use std::collections::HashMap;

	type Cell = (i32, i32, i32);

	/// A tiny world: cells default to Air, with explicit overrides. The
	/// closure world keeps these tests independent of the Pipeline.
	struct World {
		cells: HashMap<Cell, BlockType>,
		/// Cells outside this half-open box read as not-resident.
		bounds: Option<(Cell, Cell)>,
	}

	impl World {
		fn open() -> Self {
			Self {
				cells: HashMap::new(),
				bounds: None,
			}
		}

		fn set(&mut self, p: (i32, i32, i32), b: BlockType) {
			self.cells.insert(p, b);
		}

		fn facts(&self) -> impl FnMut(i32, i32, i32) -> Option<CellFacts> + '_ {
			move |x, y, z| {
				if let Some((lo, hi)) = self.bounds {
					if x < lo.0 || y < lo.1 || z < lo.2 || x >= hi.0 || y >= hi.1 || z >= hi.2 {
						return None;
					}
				}
				let b = self
					.cells
					.get(&(x, y, z))
					.copied()
					.unwrap_or(BlockType::Air);
				Some(facts_of(b as u16))
			}
		}
	}

	/// Place a block through the edit hook, mirroring `Pipeline::set_cell`.
	fn place(f: &mut LightField, w: &mut World, p: (i32, i32, i32), b: BlockType) {
		let prev = w.cells.get(&p).copied().unwrap_or(BlockType::Air);
		w.set(p, b);
		f.on_cell_changed(p.0, p.1, p.2, prev as u16, b as u16);
	}

	fn settle(f: &mut LightField, w: &World) {
		while f.pending() > 0 {
			f.step(u32::MAX, w.facts());
		}
	}

	const TORCH_AT: (i32, i32, i32) = (100, 100, 100);

	#[test]
	fn torch_in_open_air_pins_the_field() {
		let mut w = World::open();
		let mut f = LightField::new();
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		settle(&mut f, &w);
		assert_eq!(f.block_light(100, 100, 100), 14, "the source cell");
		// Manhattan falloff in open air: 14 − distance.
		assert_eq!(f.block_light(101, 100, 100), 13);
		assert_eq!(f.block_light(103, 102, 100), 9);
		assert_eq!(f.block_light(100, 100, 113), 1, "the last lit cell");
		assert_eq!(f.block_light(100, 100, 114), 0, "beyond the reach");
	}

	#[test]
	fn no_light_through_an_opaque_wall() {
		let mut w = World::open();
		let mut f = LightField::new();
		// A 5×5 stone wall two cells east of the torch, so the only paths
		// past it detour around the rim.
		for dy in -2..=2 {
			for dz in -2..=2 {
				w.set((102, 100 + dy, 100 + dz), BlockType::Stone);
			}
		}
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		settle(&mut f, &w);
		assert_eq!(f.block_light(102, 100, 100), 0, "inside the wall");
		// Straight behind the wall's center: the shortest OPEN path goes
		// 2 east of the torch is blocked, so light walks around the rim —
		// 3 out to clear the wall (dy=3), across (2), back (3), plus the
		// 2 east: torch→(100,103,100)=3, →(103,103,100)... shortest open
		// path to (103,100,100) is 9 steps → level 5.
		assert_eq!(f.block_light(103, 100, 100), 14 - 9, "pays the detour");
		// A probe past the wall's rim crosses monotonically at y=103
		// (open): 3 up, 1 east before, 1 across, 1 east after = 6 steps.
		assert_eq!(f.block_light(103, 103, 100), 14 - 6);
	}

	#[test]
	fn removing_the_torch_returns_all_zero() {
		let mut w = World::open();
		let mut f = LightField::new();
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		settle(&mut f, &w);
		place(&mut f, &mut w, TORCH_AT, BlockType::Air);
		settle(&mut f, &w);
		for dx in -15..=15 {
			for dy in -3..=3 {
				assert_eq!(
					f.block_light(100 + dx, 100 + dy, 100),
					0,
					"cell {dx},{dy} still lit after removal"
				);
			}
		}
	}

	#[test]
	fn removing_one_of_two_torches_leaves_the_other_torch_field() {
		let mut w = World::open();
		let mut f = LightField::new();
		let other = (106, 100, 100);
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		place(&mut f, &mut w, other, BlockType::Torch);
		settle(&mut f, &w);
		place(&mut f, &mut w, TORCH_AT, BlockType::Air);
		settle(&mut f, &w);
		// The survivor's pure field, no residue of the removed one.
		let mut expect = LightField::new();
		let mut w2 = World::open();
		place(&mut expect, &mut w2, other, BlockType::Torch);
		settle(&mut expect, &w2);
		for dx in -16..=16 {
			for dy in -2..=2 {
				for dz in -2..=2 {
					let (x, y, z) = (100 + dx, 100 + dy, 100 + dz);
					assert_eq!(
						f.block_light(x, y, z),
						expect.block_light(x, y, z),
						"cell {dx},{dy},{dz} differs from the survivor's pure field"
					);
				}
			}
		}
	}

	#[test]
	fn placing_a_wall_casts_a_correct_shadow() {
		let mut w = World::open();
		let mut f = LightField::new();
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		settle(&mut f, &w);
		let before = f.block_light(103, 100, 100);
		assert_eq!(before, 11);
		// Wall up AFTER the field settled: remove-then-relight must darken
		// the far side to detour values.
		for dy in -2..=2 {
			for dz in -2..=2 {
				place(&mut f, &mut w, (102, 100 + dy, 100 + dz), BlockType::Stone);
			}
		}
		settle(&mut f, &w);
		assert_eq!(f.block_light(102, 100, 100), 0, "inside the wall");
		assert_eq!(
			f.block_light(103, 100, 100),
			14 - 9,
			"shadow, not stale light"
		);
		// And digging the wall's center floods light straight through.
		place(&mut f, &mut w, (102, 100, 100), BlockType::Air);
		settle(&mut f, &w);
		assert_eq!(f.block_light(102, 100, 100), 12);
		assert_eq!(f.block_light(103, 100, 100), 11, "restored after the dig");
	}

	#[test]
	fn any_step_budget_settles_to_the_same_field() {
		for budget in [1, 7, 64, u32::MAX] {
			let mut w = World::open();
			let mut f = LightField::new();
			for dy in -2..=2 {
				for dz in -2..=2 {
					w.set((102, 100 + dy, 100 + dz), BlockType::Stone);
				}
			}
			place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
			while f.pending() > 0 {
				f.step(budget, w.facts());
			}
			assert_eq!(f.block_light(103, 100, 100), 5, "budget {budget} diverged");
			assert_eq!(f.block_light(100, 100, 113), 1, "budget {budget} diverged");
		}
	}

	#[test]
	fn unloaded_chunks_are_walls_until_seeded() {
		let mut w = World::open();
		// Resident box ends just east of the torch; beyond is unloaded.
		w.bounds = Some(((0, 0, 0), (104, 200, 200)));
		let mut f = LightField::new();
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		settle(&mut f, &w);
		assert_eq!(f.block_light(103, 100, 100), 11, "inside residency");
		assert_eq!(
			f.block_light(104, 100, 100),
			0,
			"never written into the void"
		);
	}

	#[test]
	fn an_emitter_survives_a_removal_wave() {
		// A dim emitter (its own light) sitting inside a brighter torch's
		// field: stored value is the torch's. Removing the torch must
		// re-seed the dim source, not zero it. No dim block row exists,
		// so use two torches at range where one dominates — covered — and
		// additionally pin the emitter re-seed path directly: a torch 3
		// cells away holds 14 (own) not 11 (neighbor's spread).
		let mut w = World::open();
		let mut f = LightField::new();
		place(&mut f, &mut w, TORCH_AT, BlockType::Torch);
		place(&mut f, &mut w, (103, 100, 100), BlockType::Torch);
		settle(&mut f, &w);
		assert_eq!(f.block_light(103, 100, 100), 14, "own emission wins");
		place(&mut f, &mut w, TORCH_AT, BlockType::Air);
		settle(&mut f, &w);
		assert_eq!(f.block_light(103, 100, 100), 14);
		assert_eq!(f.block_light(100, 100, 100), 11, "relit by the survivor");
	}
}
