//! Chunk meshing from immutable snapshots. Replaces the Java pattern of
//! writing face bits into neighbor chunks: a mesh job gets a padded 18³
//! copy of its chunk plus a one-block shell from all 26 neighbors — no
//! shared mutation, no locks, and diagonal neighbors are available for
//! per-vertex AO and water corner heights.
//!
//! Face rules (originally distilled from Chunk.setFlags in the Java, now
//! driven by each block's RenderClass — see block.rs), verified by tests:
//! - an Opaque neighbor occludes any face
//! - Opaque emits toward any non-occluding neighbor; Cutout the same except
//!   against its own type; Translucent emits only toward class-None (air)
//!   and never against its own type — so water|water yields nothing
//! - a missing (unloaded) neighbor suppresses border faces — whichever
//!   chunk generates second re-dirties this one, matching the Java
//! - above the top of the world there is an air sentinel; below the bottom,
//!   missing (the Java null) — so no bottom faces at world bottom

use crate::block::{BlockType, RenderClass};
use crate::chunk::{ChunkData, CHUNK_SIZE};

mod water;
use water::{emit_water_face, WaterState};

// Face directions moved to crate::dir; re-exported for existing users.
pub use crate::dir::{Dir, DIRS};

/// Padded snapshot edge: the chunk plus a one-block shell from its
/// neighbors.
pub const PADDED: usize = CHUNK_SIZE + 2;
const PADDED_VOLUME: usize = PADDED * PADDED * PADDED;

/// Where a neighbor chunk's blocks come from when building a `MeshInput`,
/// keyed by chunk offset (dx, dy, dz) ∈ {-1,0,1}³.
pub enum Src<'a> {
	/// A generated, non-sentinel neighbor chunk.
	Chunk(&'a ChunkData),
	/// All air: the sky sentinel, or above the world top.
	Air,
	/// Not generated (or below the world bottom): shell cells read as air;
	/// on a face offset it also suppresses that border's faces (Java null
	/// neighbor — whichever chunk generates second re-dirties this one).
	Missing,
}

/// A mesh job's complete input: 18³ block ids (the chunk plus a one-block
/// shell copied from every existing neighbor — faces, edges, corners) and
/// a missing flag per face.
pub struct MeshInput {
	/// Indexed by `pidx` over coords in -1..=16; shell cells with no
	/// backing chunk are air (0).
	pub blocks: Box<[u8; PADDED_VOLUME]>,
	/// Face neighbor not loaded: suppress that border's faces.
	pub missing: [bool; 6],
}

/// Padded flat index for coords in -1..=16 — same y,z,x nesting as
/// `ChunkData::index`, x contiguous.
#[inline]
pub fn pidx(x: i32, y: i32, z: i32) -> usize {
	debug_assert!((-1..=16).contains(&x) && (-1..=16).contains(&y) && (-1..=16).contains(&z));
	let n = PADDED as i32;
	(((y + 1) * n + (z + 1)) * n + (x + 1)) as usize
}

/// The face `Dir` for an axis-aligned unit chunk offset; None for
/// edge/corner offsets.
fn face_dir(dx: i32, dy: i32, dz: i32) -> Option<Dir> {
	match (dx, dy, dz) {
		(1, 0, 0) => Some(Dir::West),
		(-1, 0, 0) => Some(Dir::East),
		(0, 0, 1) => Some(Dir::North),
		(0, 0, -1) => Some(Dir::South),
		(0, 1, 0) => Some(Dir::Top),
		(0, -1, 0) => Some(Dir::Bottom),
		_ => None,
	}
}

impl MeshInput {
	/// Assemble from the center chunk plus a lookup over the 26 neighbor
	/// chunk offsets. Air and Missing regions stay zero-filled (air) —
	/// only the face missing flags distinguish them.
	pub fn build<'a>(
		center: &ChunkData,
		mut neighbor: impl FnMut(i32, i32, i32) -> Src<'a>,
	) -> MeshInput {
		let n = CHUNK_SIZE;
		let mut input = MeshInput {
			blocks: Box::new([0u8; PADDED_VOLUME]),
			missing: [false; 6],
		};
		for y in 0..n {
			for z in 0..n {
				let src = (y * n + z) * n;
				let dst = pidx(0, y as i32, z as i32);
				input.blocks[dst..dst + n].copy_from_slice(&center.blocks[src..src + n]);
			}
		}
		// The local index range a neighbor at axis offset d contributes:
		// its far border row for -1, its near border row for +1.
		let span = |d: i32| match d {
			-1 => 15..16,
			1 => 0..1,
			_ => 0..16,
		};
		for dy in -1..=1 {
			for dz in -1..=1 {
				for dx in -1..=1 {
					if (dx, dy, dz) == (0, 0, 0) {
						continue;
					}
					let d = match neighbor(dx, dy, dz) {
						Src::Chunk(d) => d,
						Src::Air => continue,
						Src::Missing => {
							if let Some(dir) = face_dir(dx, dy, dz) {
								input.missing[dir as usize] = true;
							}
							continue;
						}
					};
					for ly in span(dy) {
						for lz in span(dz) {
							for lx in span(dx) {
								input.blocks[pidx(
									lx as i32 + 16 * dx,
									ly as i32 + 16 * dy,
									lz as i32 + 16 * dz,
								)] = d.blocks[(ly * n + lz) * n + lx];
							}
						}
					}
				}
			}
		}
		input
	}

	/// Block id at padded coords (each in -1..=16).
	#[inline]
	pub fn get(&self, x: i32, y: i32, z: i32) -> u8 {
		self.blocks[pidx(x, y, z)]
	}
}

pub struct MeshData {
	/// Packed u32 vertices, 4 per face (TL,TR,BL,BR).
	pub solid: Vec<u32>,
	/// Water vertices are TWO u32 words each (8 per face): word 0 is the
	/// solid packing; word 1 carries bits 0-3 surface drop code d (the
	/// shader lowers the vertex by d/9 — 0 = full height), bit 4 falling,
	/// bits 5-7 flow dir, bits 8-9 the splash code (3 = fall impact
	/// cell, 2/1 = pool cells at Chebyshev distance 1/2 — expanding
	/// foam rings), and bits 10-13 the vertex fall distance (blocks of
	/// falling water above; bottom-plane verts +1 so the interpolated
	/// "blocks fallen" is continuous down a column; saturates at 15).
	pub water: Vec<u32>,
	/// Water-fall impact cells (the splash-code-3 predicate), packed
	/// chunk-local `x | y<<4 | z<<8` — spray-emitter metadata. Rides the
	/// mesh so every fall-changing path (worldgen stream-in, dig/place,
	/// fluid sim, remesh-all) refreshes emitters with no extra scans.
	pub impacts: Vec<u16>,
}

impl MeshData {
	pub fn is_empty(&self) -> bool {
		self.solid.is_empty() && self.water.is_empty()
	}
}

#[inline]
fn face_visible(me: BlockType, neighbor: Option<BlockType>) -> bool {
	let Some(nb) = neighbor else { return false }; // Missing neighbor
	if nb.props().class == RenderClass::Opaque {
		return false;
	}
	match me.props().class {
		RenderClass::None => false,
		RenderClass::Opaque => true,
		RenderClass::Cutout => nb != me,
		RenderClass::Translucent => nb.props().class == RenderClass::None,
	}
}

/// Pack a vertex: bits 0-4 x, 5-9 y, 10-14 z, 15-17 normal, 18-19 ao
/// (0 darkest .. 3 open), 20-21 corner (0=TL 1=TR 2=BL 3=BR), 22 unused,
/// 23-28 texture layer, 29-31 tint.
#[inline]
fn pack(v: Vert, normal: Dir, corner: u32) -> u32 {
	let (x, y, z) = v.pos;
	debug_assert!(x <= 16 && y <= 16 && z <= 16 && v.tex < 64 && corner < 4 && v.ao < 4);
	debug_assert!(v.tint < 8);
	x | (y << 5)
		| (z << 10)
		| ((normal as u32) << 15)
		| (v.ao << 18)
		| (corner << 20)
		| (v.tex << 23)
		| (v.tint << 29)
}

/// One vertex's non-positional attributes on their way into `pack`.
#[derive(Clone, Copy)]
struct Vert {
	pos: (u32, u32, u32),
	tex: u32,
	ao: u32,
	tint: u32,
}

/// Per-block brightness jitter: a 3-bit step from a splitmix64 hash of
/// the world block coords, breaking texture repetition on large uniform
/// faces. World coords (not chunk-local) so the pattern doesn't tile
/// with a 16-block period.
#[inline]
fn tint(wx: i64, wy: i64, wz: i64) -> u32 {
	let mut h = (wx as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
		^ (wy as u64).rotate_left(21)
		^ (wz as u64).rotate_left(42);
	h ^= h >> 30;
	h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
	h ^= h >> 27;
	h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
	h ^= h >> 31;
	(h & 7) as u32
}

/// Corner lattice positions [TL, TR, BL, BR] of the face of block
/// (x, y, z) toward `dir` — the Java addFaces geometry (per-face origin
/// offset + diagonal vector).
fn face_corners(x: i32, y: i32, z: i32, dir: Dir) -> [(i32, i32, i32); 4] {
	let ((ox, oy, oz), (dx, dy, dz)) = match dir {
		Dir::South => ((0, 0, 0), (1, 1, 0)),
		Dir::East => ((0, 0, 1), (0, 1, -1)),
		Dir::North => ((1, 0, 1), (-1, 1, 0)),
		Dir::West => ((1, 0, 0), (0, 1, 1)),
		Dir::Top => ((0, 1, 0), (1, 0, 1)),
		Dir::Bottom => ((0, 0, 1), (1, 0, -1)),
	};
	let (x, y, z) = (x + ox, y + oy, z + oz);
	let vertical = matches!(dir, Dir::Top | Dir::Bottom);
	[
		(x + dx, y + dy, z + dz),
		(x, y + dy, if vertical { z + dz } else { z }),
		(x + dx, y, if vertical { z } else { z + dz }),
		(x, y, z),
	]
}

/// Whether a block id casts ambient occlusion. Opaque only — cutout and
/// translucent neighbors don't occlude faces (`face_visible`), so they
/// shouldn't darken corners you can see through them.
#[inline]
fn occludes(id: u8) -> bool {
	BlockType::from_u8(id).props().class == RenderClass::Opaque
}

/// Classic 4-level voxel AO for the face of block b toward `dir`: each
/// corner is darkened by its two edge neighbors and their shared diagonal
/// in the face's neighbor layer — `3 − side1 − side2 − corner`, floored
/// to 0 when both sides occlude (the diagonal is then invisible).
fn face_ao(input: &MeshInput, bx: i32, by: i32, bz: i32, dir: Dir) -> [u32; 4] {
	let (ox, oy, oz) = dir.offset();
	let b = [bx, by, bz];
	let c = [bx + ox, by + oy, bz + oz]; // face's neighbor-layer cell
									  // The two tangent axes of the face plane.
	let (u, v) = match dir {
		Dir::West | Dir::East => (1, 2),
		Dir::Top | Dir::Bottom => (0, 2),
		Dir::North | Dir::South => (0, 1),
	};
	let corners = face_corners(bx, by, bz, dir);
	let mut ao = [3u32; 4];
	for (i, p) in corners.iter().enumerate() {
		let p = [p.0, p.1, p.2];
		// Corner at the block's low lattice plane on a tangent axis looks
		// −1 along it, at the high plane +1.
		let su = if p[u] == b[u] { -1 } else { 1 };
		let sv = if p[v] == b[v] { -1 } else { 1 };
		let cell = |du: i32, dv: i32| -> bool {
			let mut q = c;
			q[u] += du;
			q[v] += dv;
			occludes(input.get(q[0], q[1], q[2]))
		};
		let s1 = cell(su, 0);
		let s2 = cell(0, sv);
		ao[i] = if s1 && s2 {
			0
		} else {
			3 - u32::from(s1) - u32::from(s2) - u32::from(cell(su, sv))
		};
	}
	ao
}

/// Emit one face quad with per-corner AO. Triangulation runs the quad
/// diagonal between emit slots 0 and 3 (shared index pattern
/// [3,1,0, 3,0,2]); the emit order flips so the diagonal passes through
/// the lower-AO-sum corner pair — a single dark corner then shades a
/// radial gradient instead of a hard crease across the face. Corner
/// codes travel with their vertices, so uv is order-independent.
fn emit_face(
	out: &mut Vec<u32>,
	b: (usize, usize, usize),
	dir: Dir,
	tex: u32,
	ao: [u32; 4],
	tint: u32,
) {
	let corners = face_corners(b.0 as i32, b.1 as i32, b.2 as i32, dir);
	let order: [usize; 4] = if ao[0] + ao[3] > ao[1] + ao[2] {
		[1, 3, 0, 2] // TR, BR, TL, BL — diagonal TR–BL
	} else {
		[0, 1, 2, 3] // TL, TR, BL, BR — diagonal TL–BR
	};
	for &ci in &order {
		let (cx, cy, cz) = corners[ci];
		out.push(pack(
			Vert {
				pos: (cx as u32, cy as u32, cz as u32),
				tex,
				ao: ao[ci],
				tint,
			},
			dir,
			ci as u32,
		));
	}
}

/// Flat-index delta to the padded neighbor cell, per `Dir`.
const PDELTA: [isize; 6] = [
	1,                             // West  (+x)
	PADDED as isize,               // North (+z)
	-(PADDED as isize),            // South (−z)
	-((PADDED * PADDED) as isize), // Bottom (−y)
	(PADDED * PADDED) as isize,    // Top   (+y)
	-1,                            // East  (−x)
];

/// Mesh a chunk from its padded snapshot. `base` is the chunk's world
/// block origin — the tint hash needs world coordinates.
pub fn mesh_chunk(input: &MeshInput, base: (i32, i32, i32)) -> MeshData {
	let n = CHUNK_SIZE;
	// Sized for a busy surface chunk; realloc past this is rare.
	let mut mesh = MeshData {
		solid: Vec::with_capacity(4096),
		water: Vec::with_capacity(512),
		impacts: Vec::new(),
	};
	for y in 0..n {
		for z in 0..n {
			for x in 0..n {
				let pi = pidx(x as i32, y as i32, z as i32);
				let me = BlockType::from_u8(input.blocks[pi]);
				if me.props().class == RenderClass::None {
					continue;
				}
				// Cell-level water state, shared by all its faces.
				let mut wstate: Option<WaterState> = None;
				for &dir in &DIRS {
					// A missing face neighbor suppresses this border's
					// faces; the padded ring makes every other lookup a
					// uniform flat index.
					let on_border = match dir {
						Dir::West => x == n - 1,
						Dir::North => z == n - 1,
						Dir::South => z == 0,
						Dir::Bottom => y == 0,
						Dir::Top => y == n - 1,
						Dir::East => x == 0,
					};
					let neighbor = if on_border && input.missing[dir as usize] {
						None
					} else {
						let ni = (pi as isize + PDELTA[dir as usize]) as usize;
						Some(BlockType::from_u8(input.blocks[ni]))
					};
					if !face_visible(me, neighbor) {
						continue;
					}
					let class = me.props().class;
					let tex = me.texture(dir);
					if class == RenderClass::Translucent {
						// Not get_or_insert_with: recording the impact cell
						// must happen exactly once, and the closure can't
						// reach mesh.impacts.
						let state = match wstate {
							Some(s) => s,
							None => {
								let s = WaterState::of(input, x as i32, y as i32, z as i32, me);
								if s.splash == 3 {
									mesh.impacts.push((x | y << 4 | z << 8) as u16);
								}
								wstate = Some(s);
								s
							}
						};
						emit_water_face(&mut mesh.water, input, (x, y, z), dir, tex, state);
					} else {
						let ao = face_ao(input, x as i32, y as i32, z as i32, dir);
						let bt = tint(
							i64::from(base.0) + x as i64,
							i64::from(base.1) + y as i64,
							i64::from(base.2) + z as i64,
						);
						emit_face(&mut mesh.solid, (x, y, z), dir, tex, ao, bt);
					}
				}
			}
		}
	}
	mesh
}

/// The static index pattern shared by every chunk mesh: for quad q with
/// vertex base q*4, indices [base+3, base+1, base+0, base+3, base+0,
/// base+2] — the Java triangle order (BR,TR,TL)+(BR,TL,BL), CCW.
pub fn quad_indices(max_quads: usize) -> Vec<u32> {
	let mut idx = Vec::with_capacity(max_quads * 6);
	for q in 0..max_quads as u32 {
		let b = q * 4;
		idx.extend_from_slice(&[b + 3, b + 1, b, b + 3, b, b + 2]);
	}
	idx
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;

	fn solo(block: B) -> ChunkData {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, block);
		d
	}

	/// All 26 neighbors missing — the old ALL_MISSING fixture.
	fn isolated(d: &ChunkData) -> MeshInput {
		MeshInput::build(d, |_, _, _| Src::Missing)
	}

	/// World origin for tests that don't care about the tint pattern.
	const ZERO: (i32, i32, i32) = (0, 0, 0);

	#[test]
	fn lone_dirt_has_six_faces() {
		let m = mesh_chunk(&isolated(&solo(B::Dirt)), ZERO);
		assert_eq!(m.solid.len(), 6 * 4);
		assert!(m.water.is_empty());
	}

	#[test]
	fn water_only_faces_air() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(8, 7, 8, B::Dirt); // below
		let m = mesh_chunk(&isolated(&d), ZERO);
		// Water: 5 air-adjacent sides + top = faces toward air on all open
		// sides. Dirt below: 5 visible faces (top hidden? no — top borders
		// water, water is not solid → dirt top IS visible through water).
		let water_faces = m.water.len() / 8; // two words per water vertex
		assert_eq!(water_faces, 5, "water faces air on 4 sides + top");
		let solid_faces = m.solid.len() / 4;
		assert_eq!(solid_faces, 6, "dirt visible through water above");
	}

	#[test]
	fn water_water_interface_emits_nothing() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(9, 8, 8, B::Water);
		let m = mesh_chunk(&isolated(&d), ZERO);
		// Each water block: faces toward air except toward each other.
		assert_eq!(m.water.len() / 8, 2 * 5);
	}

	#[test]
	fn missing_neighbor_suppresses_border_faces() {
		let mut d = ChunkData::default();
		d.set(0, 8, 8, B::Dirt); // on the −x border
		let m = mesh_chunk(&isolated(&d), ZERO);
		assert_eq!(
			m.solid.len() / 4,
			5,
			"border face toward missing neighbor suppressed"
		);
	}

	#[test]
	fn air_sentinel_above_world_top() {
		let mut d = ChunkData::default();
		d.set(8, 15, 8, B::Dirt);
		let input = MeshInput::build(&d, |dx, dy, dz| {
			if (dx, dy, dz) == (0, 1, 0) {
				Src::Air
			} else {
				Src::Missing
			}
		});
		let m = mesh_chunk(&input, ZERO);
		assert_eq!(m.solid.len() / 4, 6, "top face vs air sentinel");
	}

	#[test]
	fn neighbor_chunk_occludes() {
		let mut d = ChunkData::default();
		d.set(15, 8, 8, B::Dirt); // on the +x border
		let mut other = ChunkData::default();
		other.set(0, 8, 8, B::Dirt); // the neighbor's facing block
		let input = MeshInput::build(&d, |dx, dy, dz| {
			if (dx, dy, dz) == (1, 0, 0) {
				Src::Chunk(&other)
			} else {
				Src::Missing
			}
		});
		let m = mesh_chunk(&input, ZERO);
		assert_eq!(m.solid.len() / 4, 5, "+x face occluded by neighbor block");
	}

	#[test]
	fn shell_regions_land_at_padded_coords() {
		// One marked block per source kind: a face neighbor (below), an
		// edge neighbor (+x+z), and a corner neighbor (+x+y+z).
		let mut below = ChunkData::default();
		below.set(5, 15, 7, B::Stone); // → padded (5, -1, 7)
		let mut edge = ChunkData::default();
		edge.set(0, 8, 0, B::Dirt); // offset (1,0,1) → padded (16, 8, 16)
		let mut corner = ChunkData::default();
		corner.set(0, 0, 0, B::Sand); // offset (1,1,1) → padded (16,16,16)
		let input = MeshInput::build(&ChunkData::default(), |dx, dy, dz| match (dx, dy, dz) {
			(0, -1, 0) => Src::Chunk(&below),
			(1, 0, 1) => Src::Chunk(&edge),
			(1, 1, 1) => Src::Chunk(&corner),
			_ => Src::Air,
		});
		assert_eq!(input.get(5, -1, 7), B::Stone as u8);
		assert_eq!(input.get(16, 8, 16), B::Dirt as u8);
		assert_eq!(input.get(16, 16, 16), B::Sand as u8);
		// Everything else in the shell is air.
		assert_eq!(input.get(-1, -1, -1), 0);
		assert_eq!(input.get(8, 8, 8), 0, "empty center");
	}

	#[test]
	fn missing_face_sets_flag_missing_diagonal_does_not() {
		let input = MeshInput::build(&ChunkData::default(), |dx, dy, dz| {
			// Top face missing, all edges/corners missing, other faces Air.
			if (dx, dy, dz) == (0, 1, 0) || dx.abs() + dy.abs() + dz.abs() > 1 {
				Src::Missing
			} else {
				Src::Air
			}
		});
		let mut expect = [false; 6];
		expect[Dir::Top as usize] = true;
		assert_eq!(input.missing, expect);
	}

	#[test]
	fn packed_vertex_roundtrip() {
		let v = pack(
			Vert {
				pos: (16, 7, 3),
				tex: 37,
				ao: 1,
				tint: 5,
			},
			Dir::East,
			2,
		);
		assert_eq!(v & 0x1f, 16);
		assert_eq!((v >> 5) & 0x1f, 7);
		assert_eq!((v >> 10) & 0x1f, 3);
		assert_eq!((v >> 15) & 0x7, Dir::East as u32);
		assert_eq!((v >> 18) & 0x3, 1);
		assert_eq!((v >> 20) & 0x3, 2);
		assert_eq!((v >> 23) & 0x3f, 37);
		assert_eq!(v >> 29, 5);
	}

	fn tint_of(v: u32) -> u32 {
		v >> 29
	}

	#[test]
	fn tint_varies_by_world_position_and_is_stable() {
		// The same local block in two different chunks gets different
		// tints (world-coord hash, no 16-block tiling), and meshing twice
		// is bit-identical.
		let d = solo(B::Dirt);
		let a = mesh_chunk(&isolated(&d), ZERO);
		let b = mesh_chunk(&isolated(&d), (16, 0, 0));
		let c = mesh_chunk(&isolated(&d), ZERO);
		assert_eq!(a.solid, c.solid, "meshing is deterministic");
		assert_ne!(
			tint_of(a.solid[0]),
			tint_of(b.solid[0]),
			"world offset changes the tint"
		);
		// Across a full chunk of stone, tints are non-constant.
		let mut full = ChunkData::default();
		for y in 0..4 {
			for z in 0..16 {
				for x in 0..16 {
					full.set(x, y, z, B::Stone);
				}
			}
		}
		let m = mesh_chunk(&isolated(&full), ZERO);
		let first = tint_of(m.solid[0]);
		assert!(
			m.solid.iter().any(|v| tint_of(*v) != first),
			"tint varies across the chunk"
		);
	}

	#[test]
	fn water_word_count_is_even() {
		let m = mesh_chunk(&isolated(&solo(B::Water)), (48, 16, -32));
		assert_eq!(m.water.len() % 8, 0, "whole two-word quads only");
		assert!(m.solid.is_empty());
	}

	/// The (word0, word1) vertex pairs of a water face, matched like
	/// `quad_of` but over the interleaved two-word stream.
	fn water_quad(m: &[u32], bx: i32, by: i32, bz: i32, dir: Dir) -> [(u32, u32); 4] {
		let want: std::collections::HashSet<(u32, u32, u32)> = face_corners(bx, by, bz, dir)
			.iter()
			.map(|&(x, y, z)| (x as u32, y as u32, z as u32))
			.collect();
		for q in m.chunks(8) {
			let got: std::collections::HashSet<(u32, u32, u32)> = q
				.chunks(2)
				.map(|p| (p[0] & 0x1f, (p[0] >> 5) & 0x1f, (p[0] >> 10) & 0x1f))
				.collect();
			if (q[0] >> 15) & 0x7 == dir as u32 && got == want {
				return [(q[0], q[1]), (q[2], q[3]), (q[4], q[5]), (q[6], q[7])];
			}
		}
		panic!("water face of ({bx},{by},{bz}) toward {dir:?} not found");
	}

	fn drop_of(w1: u32) -> u32 {
		w1 & 0xf
	}

	fn falling_of(w1: u32) -> bool {
		(w1 >> 4) & 1 == 1
	}

	#[test]
	fn source_surface_drops_one_ninth() {
		// Lone source with air above: top face all d = 9 − 8 = 1; side
		// faces drop only their top-plane vertices; nothing is falling.
		let m = mesh_chunk(&isolated(&solo(B::Water)), ZERO);
		let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
		for (_, w1) in top {
			assert_eq!(drop_of(w1), 1);
			assert!(!falling_of(w1));
		}
		let side = water_quad(&m.water, 8, 8, 8, Dir::West);
		for (w0, w1) in side {
			let y = (w0 >> 5) & 0x1f;
			assert_eq!(
				drop_of(w1),
				u32::from(y == 9),
				"top verts drop, bottom full"
			);
		}
	}

	#[test]
	fn submerged_water_renders_full_cube() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(8, 9, 8, B::Water);
		let m = mesh_chunk(&isolated(&d), ZERO);
		// Lower cell: water above → its side faces stay full height.
		let side = water_quad(&m.water, 8, 8, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!(drop_of(w1), 0);
		}
		// Upper cell is the surface: top face drops.
		let top = water_quad(&m.water, 8, 9, 8, Dir::Top);
		for (_, w1) in top {
			assert_eq!(drop_of(w1), 1);
		}
	}

	#[test]
	fn waterfall_top_drops_like_a_surface_cell() {
		let m = mesh_chunk(&isolated(&solo(B::WaterFall)), ZERO);
		// Floating fall: 4 sides + top + bottom toward air. Air above, so
		// its top-plane vertices take the corner surface (amount 8 → d=1)
		// like any surface water; everything is flagged falling.
		assert_eq!(m.water.len() / 8, 6);
		for pair in m.water.chunks(2) {
			let y = (pair[0] >> 5) & 0x1f;
			assert_eq!(drop_of(pair[1]), u32::from(y == 9));
			assert!(falling_of(pair[1]));
		}
		// Mid-column (falling water above): a complete cube, no drop.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFall);
		d.set(8, 9, 8, B::WaterFall);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let side = water_quad(&m.water, 8, 8, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!(drop_of(w1), 0, "submerged fall stays full");
		}
	}

	#[test]
	fn cascade_steps_ramp_into_each_other() {
		// The fluid sim's staircase shape: flow on the upper step, flow
		// one down and one across on the lower step. The upper surface's
		// downhill corners plunge the whole block (d = 9, air with water
		// below) and the lower cell's uphill corners pin full (water
		// hangs over the stone step), so the two surfaces meet exactly
		// at the lattice — a connected ramp, no cliff sliver between.
		let mut d = ChunkData::default();
		d.set(8, 9, 8, B::WaterFlow7);
		d.set(8, 8, 8, B::Stone);
		d.set(9, 8, 8, B::WaterFlow7);
		d.set(9, 7, 8, B::Stone);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let upper = water_quad(&m.water, 8, 9, 8, Dir::Top);
		for (w0, w1) in upper {
			let expect = if w0 & 0x1f == 9 { 9 } else { 2 };
			assert_eq!(drop_of(w1), expect, "downhill edge plunges");
		}
		let lower = water_quad(&m.water, 9, 8, 8, Dir::Top);
		for (w0, w1) in lower {
			let expect = if w0 & 0x1f == 9 { 0 } else { 2 };
			assert_eq!(drop_of(w1), expect, "uphill edge pins full");
		}
		// The upper cell's downhill side face collapsed to zero area and
		// is skipped: upper emits N/S/E/Top, lower W/N/S/Top (E faces
		// the stone step, bottoms sit on stone).
		assert_eq!(m.water.len() / 8, 4 + 4);
	}

	#[test]
	fn waterfall_lip_knits_with_the_feeding_pool() {
		// Top fall block beside the upper pool's surface source — the
		// worldgen lip arrangement. The interface face is culled, so the
		// two top surfaces must agree on their shared corners or the rim
		// is a see-through crack. The fall surfaces one level below its
		// feeder (amount 8 → 7), so the shared corners average to d = 1
		// while the lip's outer edge dips to d = 2 — the surface tips
		// over the edge instead of popping back up to full height.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFall);
		d.set(7, 8, 8, B::Water);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let pool_top = water_quad(&m.water, 7, 8, 8, Dir::Top);
		for (_, w1) in pool_top {
			assert_eq!(drop_of(w1), 1, "pool surface undisturbed");
		}
		let fall_top = water_quad(&m.water, 8, 8, 8, Dir::Top);
		for (w0, w1) in fall_top {
			let expect = if w0 & 0x1f == 9 { 2 } else { 1 };
			assert_eq!(drop_of(w1), expect, "shared corners knit, outer dip");
		}
	}

	#[test]
	fn fall_lip_steps_below_its_feeder() {
		// A thin flow feeding a fall: the lip continues the downhill
		// slope one level below the feeder (amount 5 → lip 4).
		let mut d = ChunkData::default();
		d.set(7, 8, 8, B::WaterFlow5);
		d.set(8, 8, 8, B::WaterFall);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let fall_top = water_quad(&m.water, 8, 8, 8, Dir::Top);
		for (w0, w1) in fall_top {
			let expect = if w0 & 0x1f == 9 { 5 } else { 4 };
			assert_eq!(drop_of(w1), expect, "lip = feeder − 1");
		}
	}

	#[test]
	fn flow_direction_points_at_the_emptiest_side() {
		// Flow5 walled in by water except +x: flows toward +x (code 1).
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFlow5);
		d.set(7, 8, 8, B::WaterFlow4);
		d.set(8, 8, 9, B::WaterFlow4);
		d.set(8, 8, 7, B::WaterFlow4);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
		for (_, w1) in top {
			assert_eq!((w1 >> 5) & 7, 1, "flows toward +x");
		}
		// Sources rest even with open sides.
		let m = mesh_chunk(&isolated(&solo(B::Water)), ZERO);
		let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
		for (_, w1) in top {
			assert_eq!((w1 >> 5) & 7, 0, "sources are still");
		}
	}

	#[test]
	fn landed_fall_carries_the_foam_bit() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFall);
		d.set(8, 7, 8, B::Stone);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let side = water_quad(&m.water, 8, 8, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!((w1 >> 8) & 1, 1, "splash cell foams");
		}
		// A fall still in the air doesn't.
		let m = mesh_chunk(&isolated(&solo(B::WaterFall)), ZERO);
		let side = water_quad(&m.water, 8, 8, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!((w1 >> 8) & 1, 0, "mid-air fall has no foam");
		}
	}

	fn fall_of(w1: u32) -> u32 {
		(w1 >> 10) & 0xf
	}

	fn splash_of(w1: u32) -> u32 {
		(w1 >> 8) & 3
	}

	#[test]
	fn impact_cells_ride_mesh_data() {
		// A landed fall records its cell in MeshData.impacts once (not
		// per face); mid-air falls record nothing.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFall);
		d.set(8, 7, 8, B::Stone);
		d.set(10, 8, 8, B::WaterFall);
		d.set(10, 7, 8, B::Water);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let mut impacts = m.impacts.clone();
		impacts.sort_unstable();
		assert_eq!(impacts, vec![8 | 8 << 4 | 8 << 8, 10 | 8 << 4 | 8 << 8]);
		let m = mesh_chunk(&isolated(&solo(B::WaterFall)), ZERO);
		assert!(m.impacts.is_empty(), "mid-air fall is not an emitter");
	}

	#[test]
	fn worldgen_style_pool_landing_foams() {
		// A fall ending on resting water (the worldgen arrangement —
		// falls land in pools, not on stone) is an impact cell. The old
		// foam predicate required an opaque block below and never fired
		// for these.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFall);
		d.set(8, 7, 8, B::Water);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let side = water_quad(&m.water, 8, 8, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!(splash_of(w1), 3, "pool landing is an impact");
		}
	}

	#[test]
	fn pool_cells_get_splash_rings() {
		// Fall into the middle of a 7×7 pool: surface cells ring the
		// impact by Chebyshev distance — 1 → code 2, 2 → code 1,
		// further → 0. Mid-fall cells above the impact stay ring-free.
		let mut d = ChunkData::default();
		for z in 5..=11 {
			for x in 5..=11 {
				d.set(x, 7, z, B::Water);
			}
		}
		d.set(8, 8, 8, B::WaterFall);
		d.set(8, 9, 8, B::WaterFall);
		let m = mesh_chunk(&isolated(&d), ZERO);
		for (dist, expect) in [(1, 2), (2, 1), (3, 0)] {
			let top = water_quad(&m.water, 8 + dist, 7, 8, Dir::Top);
			for (_, w1) in top {
				assert_eq!(splash_of(w1), expect, "ring at distance {dist}");
			}
		}
		let mid_fall = water_quad(&m.water, 8, 9, 8, Dir::West);
		for (_, w1) in mid_fall {
			assert_eq!(splash_of(w1), 0, "mid-air sheet cell has no splash");
		}
	}

	#[test]
	fn fall_distance_counts_from_lip() {
		// Fall column y 8..=12: each cell counts the falling cells above
		// it; side-face top-plane verts carry that count, bottom-plane
		// verts one more, so interpolation is continuous down the sheet.
		let mut d = ChunkData::default();
		for y in 8..=12 {
			d.set(8, y, 8, B::WaterFall);
		}
		let m = mesh_chunk(&isolated(&d), ZERO);
		for (cell_y, expect) in [(12, 0), (8, 4)] {
			let side = water_quad(&m.water, 8, cell_y, 8, Dir::West);
			for (w0, w1) in side {
				let vy = (w0 >> 5) & 0x1f;
				let top_plane = vy == cell_y as u32 + 1;
				assert_eq!(fall_of(w1), expect + u32::from(!top_plane));
			}
		}
		// Non-falling water carries zero fall bits.
		let m = mesh_chunk(&isolated(&solo(B::Water)), ZERO);
		for pair in m.water.chunks(2) {
			assert_eq!(fall_of(pair[1]), 0);
		}
	}

	#[test]
	fn fall_distance_saturates() {
		// A full-height column: the deepest cells cap at 15 instead of
		// wrapping the 4-bit field.
		let mut d = ChunkData::default();
		for y in 0..16 {
			d.set(8, y, 8, B::WaterFall);
		}
		let m = mesh_chunk(&isolated(&d), ZERO);
		let side = water_quad(&m.water, 8, 0, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!(fall_of(w1), 15, "deep cells saturate");
		}
		// A fall entering through the top shell (still falling above the
		// snapshot ceiling) saturates too — the partial count would reset
		// the sheet to a glassy lip at every vertical chunk seam.
		let mut d = ChunkData::default();
		d.set(8, 15, 8, B::WaterFall);
		let mut above = ChunkData::default();
		above.set(8, 0, 8, B::WaterFall);
		let input = MeshInput::build(&d, |dx, dy, dz| {
			if (dx, dy, dz) == (0, 1, 0) {
				Src::Chunk(&above)
			} else {
				Src::Air
			}
		});
		let m = mesh_chunk(&input, ZERO);
		let side = water_quad(&m.water, 8, 15, 8, Dir::West);
		for (_, w1) in side {
			assert_eq!(fall_of(w1), 15, "unknown height above the seam");
		}
	}

	#[test]
	fn adjacent_flows_share_corner_heights() {
		// Flow3 | Flow5 side by side: no interface faces, and the two top
		// faces agree on their shared corners (mean amount 4 → d = 5).
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFlow3);
		d.set(9, 8, 8, B::WaterFlow5);
		let m = mesh_chunk(&isolated(&d), ZERO);
		assert_eq!(m.water.len() / 8, 2 * 5, "interface culled");
		let top3 = water_quad(&m.water, 8, 8, 8, Dir::Top);
		let top5 = water_quad(&m.water, 9, 8, 8, Dir::Top);
		let d_at = |quad: &[(u32, u32); 4], lx: u32| -> Vec<u32> {
			quad.iter()
				.filter(|(w0, _)| w0 & 0x1f == lx)
				.map(|(_, w1)| drop_of(*w1))
				.collect()
		};
		assert_eq!(d_at(&top3, 9), vec![5, 5], "shared corners from both");
		assert_eq!(d_at(&top5, 9), vec![5, 5], "identical on the other face");
		assert_eq!(d_at(&top3, 8), vec![6, 6], "flow3 alone: d = 9 − 3");
		assert_eq!(d_at(&top5, 10), vec![4, 4], "flow5 alone: d = 9 − 5");
	}

	#[test]
	fn corner_heights_knit_across_chunk_borders() {
		// Source at the +x border; the neighbor chunk holds Flow4 across
		// the seam. The border corners average both (mean 6 → d = 3),
		// proving the padded shell feeds corner heights.
		let mut d = ChunkData::default();
		d.set(15, 8, 8, B::Water);
		let mut other = ChunkData::default();
		other.set(0, 8, 8, B::WaterFlow4);
		let input = MeshInput::build(&d, |dx, dy, dz| {
			if (dx, dy, dz) == (1, 0, 0) {
				Src::Chunk(&other)
			} else {
				Src::Air
			}
		});
		let m = mesh_chunk(&input, ZERO);
		let top = water_quad(&m.water, 15, 8, 8, Dir::Top);
		for (w0, w1) in top {
			let expect = if w0 & 0x1f == 16 { 3 } else { 1 };
			assert_eq!(drop_of(w1), expect, "border corners mix in shell water");
		}
	}

	fn ao_of(v: u32) -> u32 {
		(v >> 18) & 0x3
	}

	fn corner_of(v: u32) -> u32 {
		(v >> 20) & 0x3
	}

	/// The emitted quad (4 consecutive vertices) for the given block's
	/// face toward `dir`, matched by normal + corner lattice positions.
	fn quad_of(m: &[u32], bx: i32, by: i32, bz: i32, dir: Dir) -> [u32; 4] {
		let want: std::collections::HashSet<(u32, u32, u32)> = face_corners(bx, by, bz, dir)
			.iter()
			.map(|&(x, y, z)| (x as u32, y as u32, z as u32))
			.collect();
		for q in m.chunks(4) {
			let got: std::collections::HashSet<(u32, u32, u32)> = q
				.iter()
				.map(|v| (v & 0x1f, (v >> 5) & 0x1f, (v >> 10) & 0x1f))
				.collect();
			if (q[0] >> 15) & 0x7 == dir as u32 && got == want {
				return [q[0], q[1], q[2], q[3]];
			}
		}
		panic!("face of ({bx},{by},{bz}) toward {dir:?} not found");
	}

	#[test]
	fn open_field_face_has_full_ao() {
		let m = mesh_chunk(&isolated(&solo(B::Dirt)), ZERO);
		for v in &m.solid {
			assert_eq!(ao_of(*v), 3, "no occluders anywhere");
		}
	}

	#[test]
	fn side_occluder_darkens_touching_corners() {
		// Dirt at (8,8,8); occluder diagonally above at (9,9,8) sits in
		// the top face's neighbor layer, +x of the face cell — exactly
		// the two corners on the x=9 lattice edge (TL, BL) drop to 2.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Dirt);
		d.set(9, 9, 8, B::Stone);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let top = quad_of(&m.solid, 8, 8, 8, Dir::Top);
		assert_eq!(top.iter().filter(|v| ao_of(**v) == 2).count(), 2);
		assert_eq!(top.iter().filter(|v| ao_of(**v) == 3).count(), 2);
	}

	#[test]
	fn both_sides_floor_ao_to_zero() {
		// Top-face corner at lattice (9,9,9): side1 (9,9,8) and side2
		// (8,9,9) both occluded → that corner floors to 0 even though the
		// diagonal cell (9,9,9) is empty.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Dirt);
		d.set(9, 9, 8, B::Stone);
		d.set(8, 9, 9, B::Stone);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let top = quad_of(&m.solid, 8, 8, 8, Dir::Top);
		assert_eq!(top.iter().filter(|v| ao_of(**v) == 0).count(), 1);
	}

	#[test]
	fn cross_chunk_occluder_darkens_border_corners() {
		// The seam regression: the occluder exists only in the +x
		// neighbor chunk, above the face plane — the old border-plane
		// snapshot couldn't see it.
		let mut d = ChunkData::default();
		d.set(15, 8, 8, B::Dirt);
		let mut other = ChunkData::default();
		other.set(0, 9, 8, B::Stone); // padded (16, 9, 8)
		let input = MeshInput::build(&d, |dx, dy, dz| {
			if (dx, dy, dz) == (1, 0, 0) {
				Src::Chunk(&other)
			} else {
				Src::Air
			}
		});
		let m = mesh_chunk(&input, ZERO);
		let top = quad_of(&m.solid, 15, 8, 8, Dir::Top);
		assert_eq!(
			top.iter().filter(|v| ao_of(**v) == 2).count(),
			2,
			"the two x=16 corners see the neighbor-chunk occluder"
		);
		assert_eq!(top.iter().filter(|v| ao_of(**v) == 3).count(), 2);
	}

	#[test]
	fn quad_flips_toward_dark_tr_bl_diagonal() {
		// Top face of (8,8,8): TL=(9,9,9) TR=(8,9,9) BL=(9,9,8) BR=(8,9,8).
		// An occluder at TR's diagonal cell (7,9,9) darkens only TR →
		// TL+BR = 6 > TR+BL = 5 → emit order rotates to [TR,BR,TL,BL].
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Dirt);
		d.set(7, 9, 9, B::Stone);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let top = quad_of(&m.solid, 8, 8, 8, Dir::Top);
		let order: Vec<u32> = top.iter().map(|v| corner_of(*v)).collect();
		assert_eq!(order, vec![1, 3, 0, 2], "diagonal through TR–BL");
		// The dark-at-TL mirror keeps the default diagonal (which already
		// runs through the dark corner).
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Dirt);
		d.set(9, 9, 9, B::Stone);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let top = quad_of(&m.solid, 8, 8, 8, Dir::Top);
		let order: Vec<u32> = top.iter().map(|v| corner_of(*v)).collect();
		assert_eq!(order, vec![0, 1, 2, 3], "default diagonal TL–BR");
	}

	#[test]
	fn water_never_reorders_and_skips_ao() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(9, 9, 8, B::Stone); // would darken and flip a solid face
		let m = mesh_chunk(&isolated(&d), ZERO);
		let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
		let order: Vec<u32> = top.iter().map(|(w0, _)| corner_of(*w0)).collect();
		assert_eq!(order, vec![0, 1, 2, 3], "water keeps the default order");
		for (w0, _) in top {
			assert_eq!(ao_of(w0), 3, "water skips AO");
		}
	}

	#[test]
	fn cutout_culls_same_type_but_not_others() {
		// Two adjacent leaves over dirt: the shared leaf|leaf face pair is
		// culled, leaves never occlude the dirt below, and dirt (Opaque)
		// occludes nothing of itself.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Leaves);
		d.set(9, 8, 8, B::Leaves);
		d.set(8, 7, 8, B::Dirt);
		let m = mesh_chunk(&isolated(&d), ZERO);
		assert!(m.water.is_empty(), "cutout meshes into the solid bucket");
		let layer_of = |v: u32| (v >> 23) & 0x3f;
		let leaf_faces = m.solid.iter().filter(|v| layer_of(**v) == 8).count() / 4;
		let dirt_faces = m.solid.iter().filter(|v| layer_of(**v) == 1).count() / 4;
		// Leaf A: 4 (leaf neighbor culled, dirt below occludes). Leaf B: 5.
		assert_eq!(leaf_faces, 9);
		// Dirt: 6 — its top face shows through the non-occluding leaf.
		assert_eq!(dirt_faces, 6);
	}

	#[test]
	fn grass_bottom_uses_dirt_texture() {
		// Per-face table: grass top/side/bottom are three distinct layers.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Grass);
		let m = mesh_chunk(&isolated(&d), ZERO);
		let layer_of = |v: u32| (v >> 23) & 0x3f;
		let normal_of = |v: u32| (v >> 15) & 0x7;
		for v in &m.solid {
			let expect = match normal_of(*v) as usize {
				n if n == Dir::Top as usize => 3,    // GrassTop
				n if n == Dir::Bottom as usize => 1, // Dirt
				_ => 2,                              // GrassSide
			};
			assert_eq!(layer_of(*v), expect);
		}
	}
}
