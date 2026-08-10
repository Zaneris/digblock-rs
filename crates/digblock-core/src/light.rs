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

/// The level a bounce seed starts at — an AIR cell with full sky sitting
/// on an opaque cell is "sunlit ground", the one place the sun dumps
/// energy for free all day, and its bounce reaches ~12 blocks into
/// whatever shade adjoins it. 12 is deliberately below MAX_LIGHT: no
/// spread value can ever collide with it, so a stored 12 uniquely
/// identifies a seed (stale-seed cleanup keys on that).
pub const BOUNCE_SEED: u8 = 12;

/// BFS pops per `step_light` call (two calls per maintain). A count, not
/// a deadline: pops are uniform-cost, and a count keeps settlement
/// deterministic (pinned by `any_step_budget_settles_to_the_same_field`).
/// Sized so a torch edit (~5k flooded cells) settles within a frame or
/// two, but a mass-streaming backlog can't blow the frame budget: a pop
/// costs ~6 hash-map neighbor reads, and 16k×2 per maintain measurably
/// moved the bench's p99 (+4ms); 4k×2 keeps the worst maintain ~1ms.
/// Carry-over is late, never wrong — dirty-gating remeshes whatever
/// settles later.
pub const LIGHT_BUDGET: u32 = 4 * 1024;

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
	/// True only for Air. Bounce seeds require a genuinely empty cell
	/// above ground — a water column reaching the sea floor is
	/// non-opaque with full sky, but it is not a sunlit meadow.
	pub air: bool,
}

/// Read facts for a raw cell value.
#[inline]
pub fn facts_of(cell: u16) -> CellFacts {
	let b = BlockType::from_id(cell_id(cell));
	CellFacts {
		opaque: b.props().class == crate::block::RenderClass::Opaque,
		emits: b.light_level(),
		air: b == BlockType::Air,
	}
}

/// Which nibble a queue entry belongs to. Blocklight spreads uniformly;
/// skylight adds the column rule — straight DOWN at level 15 costs no
/// decrement, which is what makes an open shaft bright to the floor and
/// a roof's removal cascade all the way down it. Bounce spreads like
/// blocklight but is seeded by GEOMETRY (sunlit ground), not by blocks —
/// its sources are maintained by the seed-check queue, never by
/// `light_level()`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
	Block,
	Sky,
	Bounce,
}

const CHANNELS: usize = 3;

impl Channel {
	/// Nibble shift within the channel's backing map: block and sky share
	/// a byte in the main map; bounce lives in its own sparse map's low
	/// nibble (near-surface chunks only — deep chunks never allocate).
	#[inline]
	fn shift(self) -> u8 {
		match self {
			Channel::Block => 0,
			Channel::Sky => 4,
			Channel::Bounce => 0,
		}
	}
}

/// A world cell position on a wavefront.
type Cell = (i32, i32, i32);

pub struct LightField {
	chunks: FxHashMap<ChunkPos, LightChunk>,
	/// Bounce's own sparse storage (low nibble). Separate from `chunks`
	/// so the two-nibble byte layout there survives untouched and deep
	/// chunks — where bounce is always zero — never allocate an entry.
	bounce: FxHashMap<ChunkPos, LightChunk>,
	/// Per-channel add wavefronts (blocklight, skylight, bounce).
	add: [VecDeque<Cell>; CHANNELS],
	/// Per-channel removal wavefronts: the position and the light value
	/// it HELD, which bounds how far the darkness spreads.
	remove: [VecDeque<(Cell, u8)>; CHANNELS],
	/// Cells whose bounce-SEED status may have changed (an edit, a sky
	/// nibble reaching or losing 15). Drained first in `step`, where the
	/// facts closure is available to evaluate the seed rule.
	seed_check: VecDeque<Cell>,
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
			bounce: FxHashMap::default(),
			add: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
			remove: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
			seed_check: VecDeque::new(),
			dirty: FxHashSet::default(),
		}
	}

	fn nibble(&self, x: i32, y: i32, z: i32, ch: Channel) -> u8 {
		let Some(((cx, cy, cz), (lx, ly, lz))) = split_world(x, y, z) else {
			return 0;
		};
		let map = match ch {
			Channel::Bounce => &self.bounce,
			_ => &self.chunks,
		};
		(map.get(&ChunkPos::new(cx, cy, cz))
			.map_or(0, |c| c.get(lx, ly, lz))
			>> ch.shift())
			& 0x0F
	}

	/// Blocklight at a world cell (0 outside the world or in dark/absent
	/// chunks).
	pub fn block_light(&self, x: i32, y: i32, z: i32) -> u8 {
		self.nibble(x, y, z, Channel::Block)
	}

	/// Skylight at a world cell.
	pub fn sky_light(&self, x: i32, y: i32, z: i32) -> u8 {
		self.nibble(x, y, z, Channel::Sky)
	}

	/// Bounce light at a world cell (sun-fill from nearby sunlit ground).
	pub fn bounce_light(&self, x: i32, y: i32, z: i32) -> u8 {
		self.nibble(x, y, z, Channel::Bounce)
	}

	/// Queue depth (all wavefronts) — settled means zero.
	pub fn pending(&self) -> usize {
		self.add.iter().map(VecDeque::len).sum::<usize>()
			+ self.remove.iter().map(VecDeque::len).sum::<usize>()
			+ self.seed_check.len()
	}

	/// Per-chunk storage, for the mesh shell copy (`MeshInput::fill_light`).
	/// `None` means uniformly dark.
	pub fn chunk(&self, pos: ChunkPos) -> Option<&LightChunk> {
		self.chunks.get(&pos)
	}

	/// Per-chunk bounce storage (low nibble). `None` means zero bounce.
	pub fn bounce_chunk(&self, pos: ChunkPos) -> Option<&LightChunk> {
		self.bounce.get(&pos)
	}

	/// Write one channel's nibble, allocating `Mixed` on divergence and
	/// recording the shell-dirty set.
	fn set_nibble(&mut self, x: i32, y: i32, z: i32, v: u8, ch: Channel) {
		let Some(((cx, cy, cz), (lx, ly, lz))) = split_world(x, y, z) else {
			return;
		};
		let (sh, keep) = (ch.shift(), !(0x0F_u8 << ch.shift()));
		let pos = ChunkPos::new(cx, cy, cz);
		let map = match ch {
			Channel::Bounce => &mut self.bounce,
			_ => &mut self.chunks,
		};
		let entry = map.entry(pos).or_insert(LightChunk::Uniform(0));
		if let LightChunk::Uniform(u) = entry {
			if (*u >> sh) & 0x0F == v {
				return; // no divergence — stay unallocated
			}
			*entry = LightChunk::Mixed(Box::new([*u; CHUNK_VOLUME]));
		}
		let LightChunk::Mixed(cells) = entry else {
			unreachable!("uniform was just promoted")
		};
		let cell = &mut cells[(ly * CHUNK_SIZE + lz) * CHUNK_SIZE + lx];
		if (*cell >> sh) & 0x0F == v {
			return;
		}
		*cell = (*cell & keep) | (v << sh);
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
	/// roof place/dig, and fluid writes:
	/// 1. stored light above the new emission → strip it (removal wave —
	///    for skylight the emission is always 0, so any occlusion strips);
	/// 2. the new cell emits (blocklight only) → seed and spread;
	/// 3. the new cell is open → pull neighboring light in on BOTH
	///    channels (a dug roof descends via the add rule's down-at-15).
	pub fn on_cell_changed(&mut self, x: i32, y: i32, z: i32, _prev: u16, new: u16) {
		let new = facts_of(new);
		let cur = self.block_light(x, y, z);
		if cur > new.emits {
			self.set_nibble(x, y, z, 0, Channel::Block);
			self.remove[Channel::Block as usize].push_back(((x, y, z), cur));
		}
		if new.emits > cur {
			self.set_nibble(x, y, z, new.emits, Channel::Block);
			self.add[Channel::Block as usize].push_back((x, y, z));
		}
		if new.opaque {
			let sky = self.sky_light(x, y, z);
			if sky > 0 {
				self.set_nibble(x, y, z, 0, Channel::Sky);
				self.remove[Channel::Sky as usize].push_back(((x, y, z), sky));
			}
			let b = self.bounce_light(x, y, z);
			if b > 0 {
				self.set_nibble(x, y, z, 0, Channel::Bounce);
				self.remove[Channel::Bounce as usize].push_back(((x, y, z), b));
			}
		} else {
			// A dug wall or roof: surrounding light floods back in. Stale
			// or dim entries self-discard at pop, so over-queuing is cheap.
			for (dx, dy, dz) in NEIGHBORS {
				self.add[Channel::Block as usize].push_back((x + dx, y + dy, z + dz));
				self.add[Channel::Sky as usize].push_back((x + dx, y + dy, z + dz));
				self.add[Channel::Bounce as usize].push_back((x + dx, y + dy, z + dz));
			}
			// The world's top row has no in-world cell above to descend
			// from — above the world IS the sky.
			if y == crate::chunk::WORLD_VBLOCK {
				self.set_nibble(x, y, z, MAX_LIGHT, Channel::Sky);
				self.add[Channel::Sky as usize].push_back((x, y, z));
			}
		}
		// Any edit can create or destroy a bounce seed here or one cell
		// up (place a block on open ground → its lid is the new seed; dig
		// the ground under one → the old seed is stale). The rule needs
		// the facts closure, so it is evaluated in `step`.
		self.seed_check.push_back((x, y, z));
		self.seed_check.push_back((x, y + 1, z));
	}

	/// Seed a freshly-landed column: the skylight DESCEND pass (level 15
	/// straight down through non-opaque cells, per-chunk `Uniform(0xF0)`
	/// for all-air chunks with every column still open), blocklight seeds
	/// from every emitting cell, lateral seeds where a lit column meets a
	/// blocked one (a pit wall, an overhang lip), and a TWO-WAY border
	/// exchange with the four horizontal neighbor columns (the light
	/// mirror of the arrival re-dirty rule). Runs AFTER the overlay
	/// re-applied its cells, so a restored torch is in the scanned data.
	pub fn seed_column(
		&mut self,
		x: i32,
		z: i32,
		stack: &[(i32, std::sync::Arc<crate::chunk::ChunkData>)],
	) {
		use crate::chunk::is_air_chunk;
		let s = CHUNK_SIZE as i32;
		let (bx, bz) = (x * s, z * s);
		// Blocklight: emitter scan (air chunks hold no emitters).
		for (cy, data) in stack {
			if is_air_chunk(data) {
				continue;
			}
			let by = cy * s;
			for (i, &cell) in data.blocks.iter().enumerate() {
				let emits = BlockType::from_id(cell_id(cell)).light_level();
				if emits > 0 {
					let (lx, lyz) = (i % CHUNK_SIZE, i / CHUNK_SIZE);
					let (lz, ly) = (lyz % CHUNK_SIZE, lyz / CHUNK_SIZE);
					let (wx, wy, wz) = (bx + lx as i32, by + ly as i32, bz + lz as i32);
					if self.block_light(wx, wy, wz) < emits {
						self.set_nibble(wx, wy, wz, emits, Channel::Block);
					}
					self.add[Channel::Block as usize].push_back((wx, wy, wz));
				}
			}
		}
		// Skylight descend, top chunk first. `stop[lx][lz]` is the world
		// y of the first opaque cell (the column's sky floor); i32::MIN =
		// open to the world bottom. `last_air` tracks whether the LAST
		// non-opaque cell walked in each column was Air — i.e. whether
		// the cell resting on the sky floor qualifies as a bounce seed
		// (a water column reaching the sea floor does not).
		let mut stop = [[i32::MIN; CHUNK_SIZE]; CHUNK_SIZE];
		let mut last_air = [[false; CHUNK_SIZE]; CHUNK_SIZE];
		let mut open_cols = CHUNK_SIZE * CHUNK_SIZE;
		for (cy, data) in stack.iter().rev() {
			let by = cy * s;
			if is_air_chunk(data) {
				if open_cols == CHUNK_SIZE * CHUNK_SIZE {
					// Whole footprint still open: tag, never allocate.
					self.chunks
						.entry(ChunkPos::new(x, *cy, z))
						.or_insert(LightChunk::Uniform(0xF0));
					self.dirty.insert(ChunkPos::new(x, *cy, z));
					last_air = [[true; CHUNK_SIZE]; CHUNK_SIZE];
					continue;
				}
				// Partially shadowed footprint: per-column fill.
				let mut fills = Vec::new();
				for (lx, col) in stop.iter().enumerate() {
					for (lz, &st) in col.iter().enumerate() {
						if st == i32::MIN {
							fills.push((lx, lz));
						}
					}
				}
				for (lx, lz) in fills {
					last_air[lx][lz] = true;
					for ly in 0..s {
						self.set_nibble(
							bx + lx as i32,
							by + ly,
							bz + lz as i32,
							MAX_LIGHT,
							Channel::Sky,
						);
					}
				}
				continue;
			}
			let mut writes = Vec::new();
			for (lx, col) in stop.iter_mut().enumerate() {
				for (lz, st) in col.iter_mut().enumerate() {
					if *st != i32::MIN {
						continue; // already blocked above
					}
					for ly in (0..CHUNK_SIZE).rev() {
						let cell = data.blocks[(ly * CHUNK_SIZE + lz) * CHUNK_SIZE + lx];
						let f = facts_of(cell);
						if f.opaque {
							*st = by + ly as i32;
							open_cols -= 1;
							break;
						}
						last_air[lx][lz] = f.air;
						writes.push((bx + lx as i32, by + ly as i32, bz + lz as i32));
					}
				}
			}
			for (wx, wy, wz) in writes {
				self.set_nibble(wx, wy, wz, MAX_LIGHT, Channel::Sky);
			}
		}
		// Bounce fill: each sunlit-ground column gets its settled vertical
		// profile written directly (seed 12 on the ground, decaying up) —
		// on flat terrain that IS the answer, so the one add queued per
		// seed discards immediately and only slopes propagate. The exact
		// mirror of the descend's write-then-let-BFS-refine trick.
		for lx in 0..CHUNK_SIZE {
			for lz in 0..CHUNK_SIZE {
				let st = stop[lx][lz];
				if st == i32::MIN || !last_air[lx][lz] {
					continue;
				}
				let (wx, wz) = (bx + lx as i32, bz + lz as i32);
				for h in 1..=i32::from(BOUNCE_SEED) {
					let v = BOUNCE_SEED + 1 - h as u8;
					if self.nibble(wx, st + h, wz, Channel::Bounce) < v {
						self.set_nibble(wx, st + h, wz, v, Channel::Bounce);
					}
				}
				self.add[Channel::Bounce as usize].push_back((wx, st + 1, wz));
			}
		}
		// Lateral seeds: where a lit column stands beside a blocked one,
		// its 15-cells in the neighbor's shadow band spread sideways (pit
		// walls, cave mouths, overhang rims).
		for lx in 0..CHUNK_SIZE {
			for lz in 0..CHUNK_SIZE {
				let own = stop[lx][lz];
				let mut deepest = own;
				for (nx, nz) in [
					(lx.wrapping_sub(1), lz),
					(lx + 1, lz),
					(lx, lz.wrapping_sub(1)),
					(lx, lz + 1),
				] {
					if nx < CHUNK_SIZE && nz < CHUNK_SIZE {
						deepest = deepest.max(stop[nx][nz]);
					}
				}
				let base = if own == i32::MIN { 0 } else { own + 1 };
				for wy in base..=deepest {
					self.add[Channel::Sky as usize].push_back((bx + lx as i32, wy, bz + lz as i32));
				}
			}
		}
		// Two-way border exchange: lit cells (≥ 2 — a 1 has nothing to
		// give) on EITHER side of each of the four borders re-spread, in
		// whichever channel is lit. Pops into cells that can't accept
		// discard themselves. Chunk pairs that are uniformly EQUAL (open
		// sky facing open sky, dark facing dark) exchange nothing and are
		// skipped wholesale — without that filter every surface column
		// would queue tens of thousands of no-op sky pops at arrival.
		for (nx, nz, fx, fz) in [
			(x - 1, z, bx - 1, i32::MIN),
			(x + 1, z, bx + s, i32::MIN),
			(x, z - 1, i32::MIN, bz - 1),
			(x, z + 1, i32::MIN, bz + s),
		] {
			for cy in 0..crate::chunk::WORLD_VCHUNK {
				let uniform = |m: &FxHashMap<ChunkPos, LightChunk>, p: ChunkPos| match m.get(&p) {
					None => Some(0u8),
					Some(LightChunk::Uniform(v)) => Some(*v),
					Some(LightChunk::Mixed(_)) => None,
				};
				let (tp, op) = (ChunkPos::new(nx, cy, nz), ChunkPos::new(x, cy, z));
				// Both maps must agree uniformly for the wholesale skip —
				// two open-sky air chunks are main-map-equal while their
				// low cells can hold diverging bounce from the ground below.
				if let (Some(a), Some(b), Some(ba), Some(bb)) = (
					uniform(&self.chunks, tp),
					uniform(&self.chunks, op),
					uniform(&self.bounce, tp),
					uniform(&self.bounce, op),
				) {
					if a == b && ba == bb {
						continue; // identical uniform faces: nothing to trade
					}
				}
				let by = cy * s;
				for a in 0..s {
					for b in 0..s {
						// The neighbor's facing cell and our own opposite it.
						let (theirs, ours) = if fz == i32::MIN {
							let inward = if fx < bx { bx } else { bx + s - 1 };
							((fx, by + b, bz + a), (inward, by + b, bz + a))
						} else {
							let inward = if fz < bz { bz } else { bz + s - 1 };
							((bx + a, by + b, fz), (bx + a, by + b, inward))
						};
						// Spread only ever crosses a ≥2 difference (the
						// brighter side raises the dimmer to bright−1),
						// so equal-height sky on both sides trades zero
						// pops — the flat-terrain common case.
						for ch in [Channel::Block, Channel::Sky, Channel::Bounce] {
							let tv = self.nibble(theirs.0, theirs.1, theirs.2, ch);
							let ov = self.nibble(ours.0, ours.1, ours.2, ch);
							if tv >= ov + 2 {
								self.add[ch as usize].push_back(theirs);
							} else if ov >= tv + 2 {
								self.add[ch as usize].push_back(ours);
							}
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
		self.bounce.remove(&pos);
		self.dirty.remove(&pos);
	}

	/// Run the BFS for up to `budget` pops against the world read `facts`
	/// (`None` = not resident = wall). Removals drain before adds — the
	/// two-queue algorithm's ordering requirement — on both channels.
	/// Skylight adds the column rule in both directions: straight down
	/// from a 15 costs no decrement, and a 15 directly below a removed 15
	/// is DEPENDENT (the shaft darkens all the way down), never a relight
	/// source. Returns pops consumed.
	pub fn step(
		&mut self,
		budget: u32,
		mut facts: impl FnMut(i32, i32, i32) -> Option<CellFacts>,
	) -> u32 {
		let mut pops = 0;
		// The bounce-seed rule, evaluated wherever a seed might have
		// appeared or gone stale: an AIR cell on an opaque floor under
		// full sky. A stored 12 is only ever a seed (spread caps at 11),
		// so a 12 that fails the rule is stale and strips cleanly.
		while pops < budget {
			if let Some((x, y, z)) = self.seed_check.pop_front() {
				pops += 1;
				let is_seed = matches!(
					(facts(x, y, z), facts(x, y - 1, z)),
					(Some(f), Some(b)) if f.air && b.opaque
				) && self.nibble(x, y, z, Channel::Sky) == MAX_LIGHT;
				let cur = self.nibble(x, y, z, Channel::Bounce);
				if is_seed && cur < BOUNCE_SEED {
					self.set_nibble(x, y, z, BOUNCE_SEED, Channel::Bounce);
					self.add[Channel::Bounce as usize].push_back((x, y, z));
				} else if !is_seed && cur == BOUNCE_SEED {
					self.set_nibble(x, y, z, 0, Channel::Bounce);
					self.remove[Channel::Bounce as usize].push_back(((x, y, z), BOUNCE_SEED));
				}
				continue;
			}
			let busy = |f: &Self, ch: Channel| {
				!f.remove[ch as usize].is_empty() || !f.add[ch as usize].is_empty()
			};
			let ch = if busy(self, Channel::Block) {
				Channel::Block
			} else if busy(self, Channel::Sky) {
				Channel::Sky
			} else {
				Channel::Bounce
			};
			if let Some(((x, y, z), old)) = self.remove[ch as usize].pop_front() {
				pops += 1;
				for (dx, dy, dz) in NEIGHBORS {
					let (nx, ny, nz) = (x + dx, y + dy, z + dz);
					let nl = self.nibble(nx, ny, nz, ch);
					if nl == 0 {
						continue;
					}
					let down_column =
						ch == Channel::Sky && dy == -1 && old == MAX_LIGHT && nl == MAX_LIGHT;
					if nl < old || down_column {
						self.set_nibble(nx, ny, nz, 0, ch);
						self.remove[ch as usize].push_back(((nx, ny, nz), nl));
						match ch {
							// An emitter caught in the wave re-seeds itself
							// (blocklight only — nothing emits sky).
							Channel::Block => {
								if let Some(f) = facts(nx, ny, nz) {
									if f.emits > 0 {
										self.set_nibble(nx, ny, nz, f.emits, ch);
										self.add[ch as usize].push_back((nx, ny, nz));
									}
								}
							}
							// A cell losing its sky 15 may have been a
							// bounce seed's sky support.
							Channel::Sky => {
								if nl == MAX_LIGHT {
									self.seed_check.push_back((nx, ny, nz));
								}
							}
							// A bounce seed caught in the wave re-seeds
							// exactly like an emitter.
							Channel::Bounce => {
								let reseed = matches!(
									(facts(nx, ny, nz), facts(nx, ny - 1, nz)),
									(Some(f), Some(b)) if f.air && b.opaque
								) && self.nibble(nx, ny, nz, Channel::Sky)
									== MAX_LIGHT;
								if reseed {
									self.set_nibble(nx, ny, nz, BOUNCE_SEED, ch);
									self.add[ch as usize].push_back((nx, ny, nz));
								}
							}
						}
					} else {
						// Independently supported: the relight frontier.
						self.add[ch as usize].push_back((nx, ny, nz));
					}
				}
			} else if let Some((x, y, z)) = self.add[ch as usize].pop_front() {
				pops += 1;
				let l = self.nibble(x, y, z, ch);
				if l == 0 {
					continue; // stale entry
				}
				for (dx, dy, dz) in NEIGHBORS {
					// Sky at 15 descends undecremented; every other spread
					// (and every blocklight spread) pays 1.
					let target = if ch == Channel::Sky && dy == -1 && l == MAX_LIGHT {
						MAX_LIGHT
					} else {
						l - 1
					};
					if target == 0 {
						continue;
					}
					let (nx, ny, nz) = (x + dx, y + dy, z + dz);
					let Some(f) = facts(nx, ny, nz) else {
						continue; // not resident: a wall for now
					};
					if f.opaque {
						continue;
					}
					if self.nibble(nx, ny, nz, ch) < target {
						self.set_nibble(nx, ny, nz, target, ch);
						self.add[ch as usize].push_back((nx, ny, nz));
						// Sky descending to 15 can mint a bounce seed
						// (the flooded shaft's floor).
						if ch == Channel::Sky && target == MAX_LIGHT {
							self.seed_check.push_back((nx, ny, nz));
						}
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

	/// Seed a horizontal sky plane (the unit-test stand-in for
	/// `seed_column`'s descend): every cell of the plane starts at 15 and
	/// the down-at-15 rule does the rest.
	fn seed_sky_plane(
		f: &mut LightField,
		y: i32,
		xs: std::ops::RangeInclusive<i32>,
		zs: std::ops::RangeInclusive<i32>,
	) {
		for x in xs {
			for z in zs.clone() {
				f.set_nibble(x, y, z, MAX_LIGHT, Channel::Sky);
				f.add[Channel::Sky as usize].push_back((x, y, z));
			}
		}
	}

	#[test]
	fn sky_descends_undecremented_and_a_cave_mouth_attenuates_sideways() {
		let mut w = World::open();
		// Resident box; outside is wall. Stone plate at y=105 shadows
		// x 95..=115 across the FULL z extent, so the only mouths are the
		// x rims at 94 and 116.
		w.bounds = Some(((90, 90, 90), (121, 112, 111)));
		for x in 95..=115 {
			for z in 90..=110 {
				w.set((x, 105, z), BlockType::Stone);
			}
		}
		let mut f = LightField::new();
		seed_sky_plane(&mut f, 110, 90..=120, 90..=110);
		settle(&mut f, &w);
		assert_eq!(f.sky_light(105, 107, 100), 15, "open air above the plate");
		assert_eq!(
			f.sky_light(94, 91, 100),
			15,
			"open column: full to the floor"
		);
		assert_eq!(f.sky_light(105, 105, 100), 0, "the plate itself");
		// Under the plate, light walks in from the west rim at x=94 and
		// pays 1 per step: (95)=14 … (105)=4.
		assert_eq!(f.sky_light(95, 100, 100), 14, "first cell inside the mouth");
		assert_eq!(
			f.sky_light(105, 100, 100),
			4,
			"attenuated deep under the plate"
		);
	}

	#[test]
	fn a_roof_zeroes_skylight_beneath_and_digging_it_floods_the_shaft() {
		let mut w = World::open();
		// The stone plate spans the WHOLE resident footprint, so nothing
		// leaks around a rim — below it must be pitch dark.
		w.bounds = Some(((95, 90, 95), (106, 112, 106)));
		for x in 95..=105 {
			for z in 95..=105 {
				w.set((x, 105, z), BlockType::Stone);
			}
		}
		let mut f = LightField::new();
		seed_sky_plane(&mut f, 110, 95..=105, 95..=105);
		settle(&mut f, &w);
		assert_eq!(f.sky_light(100, 107, 100), 15, "above the roof");
		for y in 90..105 {
			assert_eq!(f.sky_light(100, y, 100), 0, "sealed below at y={y}");
		}
		// Dig one roof cell: the shaft floods at 15 all the way down
		// (descend costs nothing), neighbors get lateral 14s.
		place(&mut f, &mut w, (100, 105, 100), BlockType::Air);
		settle(&mut f, &w);
		for y in 90..=105 {
			assert_eq!(f.sky_light(100, y, 100), 15, "shaft lit at y={y}");
		}
		assert_eq!(
			f.sky_light(101, 100, 100),
			14,
			"lateral spread off the shaft"
		);
		// Roof it again: the down-dependence rule darkens the whole
		// shaft — a 15 below a removed 15 is dependent, never a relight
		// source.
		place(&mut f, &mut w, (100, 105, 100), BlockType::Stone);
		settle(&mut f, &w);
		for y in 90..105 {
			assert_eq!(f.sky_light(100, y, 100), 0, "resealed at y={y}");
		}
		assert_eq!(f.sky_light(101, 100, 100), 0, "laterals cleaned up too");
	}

	/// The half-roof world: a stone floor across the whole residency,
	/// open sky on the west half, a stone roof (with its own sunlit lid)
	/// over the east half. The mouths are only at the x=99/100 seam.
	fn half_roof() -> (World, LightField) {
		let mut w = World::open();
		w.bounds = Some(((90, 95, 90), (111, 112, 111)));
		for x in 90..111 {
			for z in 90..111 {
				w.set((x, 99, z), BlockType::Stone);
			}
		}
		for x in 100..111 {
			for z in 90..111 {
				w.set((x, 103, z), BlockType::Stone);
			}
		}
		let mut f = LightField::new();
		seed_sky_plane(&mut f, 110, 90..=110, 90..=110);
		(w, f)
	}

	#[test]
	fn sunlit_ground_seeds_bounce_and_shade_keeps_a_gradient() {
		let (w, mut f) = half_roof();
		settle(&mut f, &w);
		// Open ground: the air cell on the stone is a seed, decaying up.
		assert_eq!(f.bounce_light(95, 100, 100), 12, "the seed itself");
		assert_eq!(f.bounce_light(95, 103, 100), 9, "vertical profile");
		// The roof's own lid is sunlit ground too.
		assert_eq!(f.bounce_light(105, 104, 100), 12, "the roof lid");
		// Under the roof: bounce walks in from the sunlit west with plain
		// Manhattan decay — floor AND ceiling hold a real gradient.
		assert_eq!(f.bounce_light(100, 100, 100), 11, "floor at the mouth");
		assert_eq!(f.bounce_light(105, 100, 100), 6, "floor deep in");
		assert_eq!(f.bounce_light(100, 102, 100), 9, "ceiling-adjacent");
		// Under-roof ground is NOT a seed (its sky is lateral, < 15).
		assert!(f.sky_light(105, 100, 100) < MAX_LIGHT);
	}

	#[test]
	fn bounce_settles_identically_at_any_budget() {
		for budget in [1, 7, 64, u32::MAX] {
			let (w, mut f) = half_roof();
			while f.pending() > 0 {
				f.step(budget, w.facts());
			}
			assert_eq!(f.bounce_light(105, 100, 100), 6, "budget {budget}");
			assert_eq!(f.bounce_light(100, 102, 100), 9, "budget {budget}");
		}
	}

	#[test]
	fn water_on_sunlit_ground_is_not_a_seed() {
		let (mut w, mut f) = half_roof();
		settle(&mut f, &w);
		place(&mut f, &mut w, (95, 100, 100), BlockType::Water);
		settle(&mut f, &w);
		// The stale 12 strips (12 is seed-only) and relight restores the
		// value its sunlit neighbors actually support.
		assert_eq!(f.bounce_light(95, 100, 100), 11, "wet cell demoted");
		assert_eq!(f.bounce_light(94, 100, 100), 12, "dry neighbor unaffected");
	}

	#[test]
	fn sealing_the_sky_strips_the_seeds_below() {
		let mut w = World::open();
		w.bounds = Some(((95, 95, 95), (106, 112, 106)));
		for x in 95..106 {
			for z in 95..106 {
				w.set((x, 99, z), BlockType::Stone);
			}
		}
		let mut f = LightField::new();
		seed_sky_plane(&mut f, 110, 95..=105, 95..=105);
		settle(&mut f, &w);
		assert_eq!(f.bounce_light(100, 100, 100), 12);
		// Roof the ENTIRE residency: no rim, no sky, no seeds, no bounce.
		for x in 95..106 {
			for z in 95..106 {
				place(&mut f, &mut w, (x, 103, z), BlockType::Stone);
			}
		}
		settle(&mut f, &w);
		assert_eq!(f.bounce_light(100, 100, 100), 0, "sealed floor");
		assert_eq!(f.bounce_light(100, 102, 100), 0, "sealed ceiling");
		assert_eq!(f.bounce_light(100, 104, 100), 12, "the new lid seeds");
	}

	#[test]
	fn digging_the_ground_under_a_seed_relocates_it() {
		let mut w = World::open();
		w.bounds = Some(((95, 95, 95), (106, 112, 106)));
		for y in [98, 99] {
			for x in 95..106 {
				for z in 95..106 {
					w.set((x, y, z), BlockType::Stone);
				}
			}
		}
		let mut f = LightField::new();
		seed_sky_plane(&mut f, 110, 95..=105, 95..=105);
		settle(&mut f, &w);
		place(&mut f, &mut w, (100, 99, 100), BlockType::Air);
		settle(&mut f, &w);
		// The hole floods with sky and its floor is the new seed; the old
		// seed cell above is stale-stripped and relit to spread values.
		assert_eq!(f.bounce_light(100, 99, 100), 12, "the hole floor seeds");
		assert_eq!(f.bounce_light(100, 100, 100), 11, "old seed demoted");
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
