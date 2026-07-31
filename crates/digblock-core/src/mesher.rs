//! Chunk meshing from immutable snapshots. Replaces the Java pattern of
//! writing face bits into neighbor chunks: face visibility depends only on
//! the two adjacent block types, so a mesh job needs its own chunk plus six
//! border planes copied from neighbors — no shared mutation, no locks.
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

/// A neighbor chunk's border plane facing us, or its absence.
#[derive(Clone)]
pub enum NeighborPlane {
	/// 16×16 block types from the adjacent chunk's facing border.
	/// Indexed [a * 16 + b]: for ±x planes (a,b) = (y,z); for ±z planes
	/// (a,b) = (y,x); for ±y planes (a,b) = (z,x).
	Blocks(Box<[u8; 256]>),
	/// Neighbor not loaded: suppress border faces (Java null neighbor).
	Missing,
	/// Outside the vertical world: air above the top (Java airBlock
	/// sentinel). Use `Missing` below the bottom.
	Air,
}

/// Extract the border plane of `data` facing direction `dir`, for use as
/// the opposite neighbor's `NeighborPlane::Blocks`.
pub fn border_plane(data: &ChunkData, dir: Dir) -> Box<[u8; 256]> {
	let mut plane = Box::new([0u8; 256]);
	let n = CHUNK_SIZE;
	let blocks = &data.blocks;
	match dir {
		// ±x borders, plane (a,b) = (y,z): gather with stride n.
		Dir::West => {
			for (i, p) in plane.iter_mut().enumerate() {
				*p = blocks[i * n + (n - 1)];
			}
		}
		Dir::East => {
			for (i, p) in plane.iter_mut().enumerate() {
				*p = blocks[i * n];
			}
		}
		// ±z borders, plane (a,b) = (y,x): one contiguous n-byte row per y.
		Dir::North => {
			for y in 0..n {
				let src = (y * n + (n - 1)) * n;
				plane[y * n..][..n].copy_from_slice(&blocks[src..][..n]);
			}
		}
		Dir::South => {
			for y in 0..n {
				let src = (y * n) * n;
				plane[y * n..][..n].copy_from_slice(&blocks[src..][..n]);
			}
		}
		// ±y borders, plane (a,b) = (z,x): one contiguous n*n-byte slab.
		Dir::Top => plane.copy_from_slice(&blocks[(n - 1) * n * n..][..n * n]),
		Dir::Bottom => plane.copy_from_slice(&blocks[..n * n]),
	}
	plane
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

/// Mesh a chunk from its snapshot + neighbor planes (indexed by `Dir`).
pub fn mesh_chunk(data: &ChunkData, neighbors: &[NeighborPlane; 6]) -> MeshData {
	let n = CHUNK_SIZE;
	// Sized for a busy surface chunk; realloc past this is rare.
	let mut mesh = MeshData {
		solid: Vec::with_capacity(4096),
		water: Vec::with_capacity(512),
	};
	// Flat-index delta to the in-chunk neighbor, per Dir (layout
	// (y*n + z)*n + x).
	let neighbor_off: [isize; 6] = [
		1,                   // West  (+x)
		n as isize,          // North (+z)
		-(n as isize),       // South (−z)
		-((n * n) as isize), // Bottom (−y)
		(n * n) as isize,    // Top   (+y)
		-1,                  // East  (−x)
	];
	for y in 0..n {
		for z in 0..n {
			let row = (y * n + z) * n;
			for x in 0..n {
				let idx = row + x;
				let me = BlockType::from_u8(data.blocks[idx]);
				if me.props().class == RenderClass::None {
					continue;
				}
				for &dir in &DIRS {
					let (ox, oy, oz) = dir.offset();
					let (nx, ny, nz) = (x as i32 + ox, y as i32 + oy, z as i32 + oz);
					let neighbor = if (0..n as i32).contains(&nx)
						&& (0..n as i32).contains(&ny)
						&& (0..n as i32).contains(&nz)
					{
						let ni = (idx as isize + neighbor_off[dir as usize]) as usize;
						Some(BlockType::from_u8(data.blocks[ni]))
					} else {
						let (a, b) = match dir {
							Dir::West | Dir::East => (y, z),
							Dir::North | Dir::South => (y, x),
							Dir::Top | Dir::Bottom => (z, x),
						};
						match &neighbors[dir as usize] {
							NeighborPlane::Blocks(p) => Some(BlockType::from_u8(p[a * n + b])),
							NeighborPlane::Missing => None,
							NeighborPlane::Air => Some(BlockType::Air),
						}
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

	const ALL_MISSING: [NeighborPlane; 6] = [
		NeighborPlane::Missing,
		NeighborPlane::Missing,
		NeighborPlane::Missing,
		NeighborPlane::Missing,
		NeighborPlane::Missing,
		NeighborPlane::Missing,
	];

	#[test]
	fn lone_dirt_has_six_faces() {
		let m = mesh_chunk(&solo(B::Dirt), &ALL_MISSING);
		assert_eq!(m.solid.len(), 6 * 4);
		assert!(m.water.is_empty());
	}

	#[test]
	fn water_only_faces_air() {
		// Water block with air above → only the top face, with lip bit.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(8, 7, 8, B::Dirt); // below
		let m = mesh_chunk(&d, &ALL_MISSING);
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
		let m = mesh_chunk(&d, &ALL_MISSING);
		// Each water block: faces toward air except toward each other.
		assert_eq!(m.water.len() / 4, 2 * 5);
	}

	#[test]
	fn missing_neighbor_suppresses_border_faces() {
		let mut d = ChunkData::default();
		d.set(0, 8, 8, B::Dirt); // on the −x border
		let m = mesh_chunk(&d, &ALL_MISSING);
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
		let mut nb = ALL_MISSING;
		nb[Dir::Top as usize] = NeighborPlane::Air;
		let m = mesh_chunk(&d, &nb);
		assert_eq!(m.solid.len() / 4, 6, "top face vs air sentinel");
	}

	#[test]
	fn neighbor_plane_occludes() {
		let mut d = ChunkData::default();
		d.set(15, 8, 8, B::Dirt); // on the +x border
		let mut other = ChunkData::default();
		other.set(0, 8, 8, B::Dirt); // the neighbor's facing block
		let mut nb = ALL_MISSING;
		nb[Dir::West as usize] = NeighborPlane::Blocks(border_plane(&other, Dir::East));
		let m = mesh_chunk(&d, &nb);
		assert_eq!(m.solid.len() / 4, 5, "+x face occluded by neighbor block");
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
	fn grass_bottom_uses_dirt_texture() {
		// Per-face table: grass top/side/bottom are three distinct layers.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Grass);
		let m = mesh_chunk(&d, &ALL_MISSING);
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
