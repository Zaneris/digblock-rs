//! Cross-chunk structures via pure apron evaluation. Candidate origins
//! come from a position hash of world coordinates, and validity is judged
//! by pure per-column functions (`column_shape`, `carved`) — so the chunk
//! that owns a structure and every neighbor it spills into compute the
//! *identical* structure independently, with no pipeline state and no
//! generation-order dependence. Trees are the first client; any future
//! structure (boulders, ruins, cacti) is a new candidate rule + block
//! list that respects `STRUCTURE_APRON`.

use crate::block::BlockType;
use crate::chunk::{ChunkData, ChunkPos, CHUNK_SIZE, WORLD_VBLOCK};
use crate::worldgen::{carved, column_shape, SurfaceKind};

/// Horizontal reach guarantee: no structure block may sit further than
/// this from its origin column. Chunks scan candidates this far outside
/// their own footprint; future structures must respect (or raise) it.
pub const STRUCTURE_APRON: i32 = 8;

/// Candidate grid pitch: at most one tree candidate per CELL×CELL area.
const CELL: i32 = 8;
/// Percent of cells that host a candidate.
const TREE_DENSITY_PCT: u64 = 30;
/// Leaf blob radius; trunk max height + canopy bound the vertical reach.
const LEAF_R: i32 = 2;
pub const MAX_STRUCTURE_HEIGHT: i32 = 9; // trunk (≤6) + canopy top (+2) + 1

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
	/// Trunk base: the block above the surface.
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pub trunk_h: i32,
}

/// Deterministic position hash (splitmix64 over cell coords + seed bits).
fn hash(cx: i32, cz: i32, seed: f64) -> u64 {
	let mut h = (u64::from(cx as u32) << 32 | u64::from(cz as u32)) ^ seed.to_bits();
	h ^= h >> 30;
	h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
	h ^= h >> 27;
	h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
	h ^= h >> 31;
	h
}

/// The candidate (if any) of one candidate-grid cell, validated against
/// the terrain: plains grass surface, not a cave mouth, headroom below
/// the world ceiling.
fn cell_tree(cell_x: i32, cell_z: i32, seed: f64) -> Option<Tree> {
	let h = hash(cell_x, cell_z, seed);
	if h % 100 >= TREE_DENSITY_PCT {
		return None;
	}
	let x = cell_x * CELL + ((h >> 8) % CELL as u64) as i32;
	let z = cell_z * CELL + ((h >> 16) % CELL as u64) as i32;
	let shape = column_shape(x, z, seed);
	if shape.surface != SurfaceKind::Plains {
		return None; // trees grow on grass only — never sand, rock, or sea
	}
	if carved(x, shape.height, z, seed) {
		return None; // surface swallowed by a cave mouth
	}
	if shape.height + MAX_STRUCTURE_HEIGHT >= WORLD_VBLOCK {
		return None;
	}
	Some(Tree {
		x,
		y: shape.height + 1,
		z,
		trunk_h: 4 + ((h >> 24) % 3) as i32,
	})
}

/// All valid trees whose blocks could reach a chunk with x/z origin
/// `(px, pz)` — i.e. candidates within the apron of its footprint.
pub fn trees_near(px: i32, pz: i32, seed: f64) -> Vec<Tree> {
	let n = CHUNK_SIZE as i32;
	let (x0, x1) = (px - STRUCTURE_APRON, px + n - 1 + STRUCTURE_APRON);
	let (z0, z1) = (pz - STRUCTURE_APRON, pz + n - 1 + STRUCTURE_APRON);
	let mut trees = Vec::new();
	for cell_z in z0.div_euclid(CELL)..=z1.div_euclid(CELL) {
		for cell_x in x0.div_euclid(CELL)..=x1.div_euclid(CELL) {
			if let Some(t) = cell_tree(cell_x, cell_z, seed) {
				trees.push(t);
			}
		}
	}
	trees
}

impl Tree {
	/// Highest world y this tree occupies.
	pub fn top(&self) -> i32 {
		self.y + self.trunk_h + 1
	}

	/// Every block of the tree in world coordinates. Trunk wins over
	/// leaves; the canopy is two 5×5 layers minus corners around the
	/// trunk top plus a 3×3 cap above it.
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		for dy in 0..self.trunk_h {
			f(self.x, self.y + dy, self.z, BlockType::Wood);
		}
		let top = self.y + self.trunk_h - 1;
		for dy in -1..=1 {
			let r = if dy == 1 { 1 } else { LEAF_R };
			for dz in -r..=r {
				for dx in -r..=r {
					if r == LEAF_R && dx.abs() == LEAF_R && dz.abs() == LEAF_R {
						continue; // clipped corners
					}
					if dx == 0 && dz == 0 && dy <= 0 {
						continue; // trunk occupies the center
					}
					f(self.x + dx, top + dy, self.z + dz, BlockType::Leaves);
				}
			}
		}
	}
}

/// Apply every reachable tree to one chunk's freshly filled data.
/// Structures never erase terrain: only Air cells are written.
pub fn decorate_chunk(data: &mut ChunkData, pos: ChunkPos, trees: &[Tree]) {
	let n = CHUNK_SIZE as i32;
	let (px, py, pz) = (pos.x * n, pos.y * n, pos.z * n);
	for tree in trees {
		tree.for_each_block(|x, y, z, block| {
			let (lx, ly, lz) = (x - px, y - py, z - pz);
			if (0..n).contains(&lx) && (0..n).contains(&ly) && (0..n).contains(&lz) {
				let (lx, ly, lz) = (lx as usize, ly as usize, lz as usize);
				if data.get(lx, ly, lz) == BlockType::Air {
					data.set(lx, ly, lz, block);
				}
			}
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn candidates_are_deterministic_and_spaced() {
		let seed = 1234.5678;
		let a = trees_near(0, 0, seed);
		let b = trees_near(0, 0, seed);
		assert_eq!(a, b, "deterministic");
		// Neighboring chunks agree about shared-apron candidates.
		let right = trees_near(16, 0, seed);
		for t in &a {
			if t.x >= 16 - STRUCTURE_APRON {
				assert!(right.contains(t), "border tree {t:?} seen by both");
			}
		}
	}

	#[test]
	fn trees_have_sane_shape() {
		let seed = 42.0;
		let mut found = false;
		for cz in -32..32 {
			for cx in -32..32 {
				let Some(t) = cell_tree(cx, cz, seed) else {
					continue;
				};
				found = true;
				assert!((4..=6).contains(&t.trunk_h));
				assert!(t.top() < WORLD_VBLOCK);
				let mut wood = 0;
				let mut leaves = 0;
				t.for_each_block(|_, _, _, b| match b {
					BlockType::Wood => wood += 1,
					BlockType::Leaves => leaves += 1,
					other => panic!("unexpected {other:?}"),
				});
				assert_eq!(wood, t.trunk_h);
				// two 5×5-minus-corners rings minus trunk cells + 3×3 cap
				assert_eq!(leaves, (21 - 1) * 2 + 9);
			}
		}
		assert!(found, "sample area contains trees");
	}
}
