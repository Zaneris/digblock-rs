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

/// Directions in the Java normal-table order (VertTex.glsl normal()):
/// 0=West(+x) 1=North(+z) 2=South(−z) 3=Bottom(−y) 4=Top(+y) 5=East(−x).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Dir {
	West = 0,
	North = 1,
	South = 2,
	Bottom = 3,
	Top = 4,
	East = 5,
}

pub const DIRS: [Dir; 6] = [
	Dir::West,
	Dir::North,
	Dir::South,
	Dir::Bottom,
	Dir::Top,
	Dir::East,
];

impl Dir {
	#[inline]
	pub fn opposite(self) -> Dir {
		match self {
			Dir::West => Dir::East,
			Dir::East => Dir::West,
			Dir::North => Dir::South,
			Dir::South => Dir::North,
			Dir::Top => Dir::Bottom,
			Dir::Bottom => Dir::Top,
		}
	}

	#[inline]
	pub fn offset(self) -> (i32, i32, i32) {
		match self {
			Dir::West => (1, 0, 0),
			Dir::North => (0, 0, 1),
			Dir::South => (0, 0, -1),
			Dir::Bottom => (0, -1, 0),
			Dir::Top => (0, 1, 0),
			Dir::East => (-1, 0, 0),
		}
	}
}

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
	pub water: Vec<u32>,
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

/// Pack a vertex: bits 0-4 x, 5-9 y, 10-14 z, 15-17 normal, 20-21 corner
/// (0=TL 1=TR 2=BL 3=BR), 22 water-lip, 23-28 texture layer (18-19 unused).
#[inline]
fn pack(x: u32, y: u32, z: u32, normal: Dir, tex: u32, corner: u32, lip: bool) -> u32 {
	debug_assert!(x <= 16 && y <= 16 && z <= 16 && tex < 64 && corner < 4);
	x | (y << 5)
		| (z << 10)
		| ((normal as u32) << 15)
		| (corner << 20)
		| (u32::from(lip) << 22)
		| (tex << 23)
}

/// Emit one face quad. Geometry is the Java ChunkMeshGenerator's, in
/// chunk-local coordinates: per-face origin offset + diagonal vector `d`,
/// corners TL/TR/BL/BR, triangles (BR,TR,TL)+(BR,TL,BL) via the shared
/// index pattern [3,1,0, 3,0,2].
fn emit_face(out: &mut Vec<u32>, bx: usize, by: usize, bz: usize, dir: Dir, tex: u32, lip: bool) {
	let (x, y, z) = (bx as i32, by as i32, bz as i32);
	// (origin offset, diagonal d) per face — Java addFaces.
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
	// TL, TR, BL, BR
	let corners = [
		(x + dx, y + dy, z + dz),
		(x, y + dy, if vertical { z + dz } else { z }),
		(x + dx, y, if vertical { z } else { z + dz }),
		(x, y, z),
	];
	for (ci, &(cx, cy, cz)) in corners.iter().enumerate() {
		out.push(pack(
			cx as u32, cy as u32, cz as u32, dir, tex, ci as u32, lip,
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

/// Mesh a chunk from its padded snapshot.
pub fn mesh_chunk(input: &MeshInput) -> MeshData {
	let n = CHUNK_SIZE;
	// Sized for a busy surface chunk; realloc past this is rare.
	let mut mesh = MeshData {
		solid: Vec::with_capacity(4096),
		water: Vec::with_capacity(512),
	};
	for y in 0..n {
		for z in 0..n {
			for x in 0..n {
				let pi = pidx(x as i32, y as i32, z as i32);
				let me = BlockType::from_u8(input.blocks[pi]);
				if me.props().class == RenderClass::None {
					continue;
				}
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
					let lip = class == RenderClass::Translucent && dir == Dir::Top;
					let out = match class {
						RenderClass::Translucent => &mut mesh.water,
						_ => &mut mesh.solid,
					};
					emit_face(out, x, y, z, dir, tex, lip);
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

	#[test]
	fn lone_dirt_has_six_faces() {
		let m = mesh_chunk(&isolated(&solo(B::Dirt)));
		assert_eq!(m.solid.len(), 6 * 4);
		assert!(m.water.is_empty());
	}

	#[test]
	fn water_only_faces_air() {
		// Water block with air above → only the top face, with lip bit.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(8, 7, 8, B::Dirt); // below
		let m = mesh_chunk(&isolated(&d));
		// Water: 5 air-adjacent sides + top = faces toward air on all open
		// sides. Dirt below: 5 visible faces (top hidden? no — top borders
		// water, water is not solid → dirt top IS visible through water).
		let water_faces = m.water.len() / 4;
		assert_eq!(water_faces, 5, "water faces air on 4 sides + top");
		assert!(
			m.water.iter().any(|v| v & (1 << 22) != 0),
			"top face has lip bit"
		);
		let solid_faces = m.solid.len() / 4;
		assert_eq!(solid_faces, 6, "dirt visible through water above");
	}

	#[test]
	fn water_water_interface_emits_nothing() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(9, 8, 8, B::Water);
		let m = mesh_chunk(&isolated(&d));
		// Each water block: faces toward air except toward each other.
		assert_eq!(m.water.len() / 4, 2 * 5);
	}

	#[test]
	fn missing_neighbor_suppresses_border_faces() {
		let mut d = ChunkData::default();
		d.set(0, 8, 8, B::Dirt); // on the −x border
		let m = mesh_chunk(&isolated(&d));
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
		let m = mesh_chunk(&input);
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
		let m = mesh_chunk(&input);
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
		let v = pack(16, 7, 3, Dir::East, 37, 2, true);
		assert_eq!(v & 0x1f, 16);
		assert_eq!((v >> 5) & 0x1f, 7);
		assert_eq!((v >> 10) & 0x1f, 3);
		assert_eq!((v >> 15) & 0x7, Dir::East as u32);
		assert_eq!((v >> 20) & 0x3, 2);
		assert_eq!((v >> 22) & 0x1, 1);
		assert_eq!((v >> 23) & 0x3f, 37);
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
		let m = mesh_chunk(&isolated(&d));
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
		let m = mesh_chunk(&isolated(&d));
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
