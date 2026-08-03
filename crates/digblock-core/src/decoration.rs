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
use crate::simplex::fbm2;
use crate::worldgen::{carved, column_shape};

/// Horizontal reach guarantee: no structure block may sit further than
/// this from its origin column. Chunks scan candidates this far outside
/// their own footprint; future structures must respect (or raise) it.
pub const STRUCTURE_APRON: i32 = 8;

/// Candidate grid pitch: at most one tree candidate per CELL×CELL area.
const CELL: i32 = 8;
/// Forest-cluster field: candidate density swings from open meadow (5%)
/// to dense grove (80%) over ~170-block patches, so woods read as places
/// instead of a uniform sprinkle.
const FOREST_FREQ: f64 = 0.006;
const FOREST_SEED_OFF: f64 = 92401.0;
pub const MAX_STRUCTURE_HEIGHT: i32 = 10; // trunk (≤7) + canopy cap (+1) + headroom

/// Tree silhouette family, chosen per candidate from its hash bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeKind {
	/// The classic round broadleaf: two wide rings and a cap.
	Blob,
	/// Poplar-like column: a tall stack of 3×3 rings and a tip.
	TallNarrow,
	/// Conifer: alternating wide/narrow tiers tapering to a tip.
	Pine,
	/// Low spreading oak: radius-3 rings under a wide cap.
	SquatWide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
	/// Trunk base: the block above the surface. The trunk also roots one
	/// block down, replacing the surface block itself.
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pub trunk_h: i32,
	pub kind: TreeKind,
	/// Per-tree canopy-jitter salt (hash bits), so each tree drops a
	/// different set of rim leaves.
	salt: u16,
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

fn smoothstep(t: f64) -> f64 {
	let t = t.clamp(0.0, 1.0);
	t * t * (3.0 - 2.0 * t)
}

/// Per-cell candidate density in percent, sampled at the cell center — a
/// pure function of (cell, seed), identical from every chunk that scans
/// this cell.
fn cell_density_pct(cell_x: i32, cell_z: i32, seed: f64) -> u64 {
	let x = f64::from(cell_x * CELL + CELL / 2);
	let z = f64::from(cell_z * CELL + CELL / 2);
	let f = (fbm2(x, z, FOREST_FREQ, 2, seed + FOREST_SEED_OFF) * 2.2).clamp(-1.0, 1.0);
	let t = smoothstep(((f + 1.0) * 0.5 - 0.40) / 0.35);
	5 + (t * 75.0) as u64
}

/// Deterministic per-cell canopy roll: a pure function of the tree's salt
/// and the block's tree-relative offset, so every chunk that renders this
/// tree drops the identical rim leaves (apron determinism).
fn jroll(salt: u16, dx: i32, dz: i32, dy: i32) -> bool {
	let k = (dx + 8) as u64 | ((dz + 8) as u64) << 5 | ((dy + 8) as u64) << 10;
	let mut v = u64::from(salt) ^ (k << 16) ^ k;
	v ^= v >> 33;
	v = v.wrapping_mul(0xff51_afd7_ed55_8ccd);
	v ^= v >> 29;
	v & 1 == 0
}

/// The candidate (if any) of one candidate-grid cell, validated against
/// the terrain: plains grass surface, not a cave mouth, headroom below
/// the world ceiling.
fn cell_tree(cell_x: i32, cell_z: i32, seed: f64) -> Option<Tree> {
	let h = hash(cell_x, cell_z, seed);
	if h % 100 >= cell_density_pct(cell_x, cell_z, seed) {
		return None;
	}
	let x = cell_x * CELL + ((h >> 8) % CELL as u64) as i32;
	let z = cell_z * CELL + ((h >> 16) % CELL as u64) as i32;
	let shape = column_shape(x, z, seed);
	if !shape.surface.props().trees {
		return None; // trees grow on grass only — never sand, rock, or sea
	}
	if carved(x, shape.height, z, seed) {
		return None; // surface swallowed by a cave mouth
	}
	if shape.height + MAX_STRUCTURE_HEIGHT >= WORLD_VBLOCK {
		return None;
	}
	let kind = match (h >> 32) % 4 {
		0 => TreeKind::Blob,
		1 => TreeKind::TallNarrow,
		2 => TreeKind::Pine,
		_ => TreeKind::SquatWide,
	};
	let trunk_h = match kind {
		TreeKind::Blob => 4 + ((h >> 24) % 3) as i32,
		TreeKind::TallNarrow | TreeKind::Pine => 6 + ((h >> 24) % 2) as i32,
		TreeKind::SquatWide => 4 + ((h >> 24) % 2) as i32,
	};
	Some(Tree {
		x,
		y: shape.height + 1,
		z,
		trunk_h,
		kind,
		salt: ((h >> 40) & 0xFFFF) as u16,
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
	/// Exclusive upper bound on the world y this tree occupies — every
	/// canopy tops out at `y + trunk_h`, one below this. `build_column`'s
	/// sky-chunk skipping relies on the bound staying exact.
	pub fn top(&self) -> i32 {
		self.y + self.trunk_h + 1
	}

	/// Every block of the tree in world coordinates. The trunk roots one
	/// block into the surface cell and wins over leaves; canopy layers are
	/// listed per kind relative to the trunk top, with rim leaves of wide
	/// (r ≥ 2) layers dropped 50/50 by the per-tree jitter roll.
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		f(self.x, self.y - 1, self.z, BlockType::Wood);
		for dy in 0..self.trunk_h {
			f(self.x, self.y + dy, self.z, BlockType::Wood);
		}
		let t = self.y + self.trunk_h - 1;
		// (dy relative to the trunk top, radius, clip corners). The trunk
		// center is skipped automatically for dy <= 0; r = 0 is a single
		// leaf above the trunk (tip/cap center).
		let layers: &[(i32, i32, bool)] = match self.kind {
			TreeKind::Blob => &[(-1, 2, true), (0, 2, true), (1, 1, false)],
			TreeKind::TallNarrow => &[
				(-3, 1, false),
				(-2, 1, false),
				(-1, 1, false),
				(0, 1, false),
				(1, 0, false),
			],
			TreeKind::Pine => &[
				(-4, 2, true),
				(-3, 1, false),
				(-2, 2, true),
				(-1, 1, false),
				(0, 1, false),
				(1, 0, false),
			],
			TreeKind::SquatWide => &[(-1, 3, true), (0, 3, true), (1, 2, true)],
		};
		for &(dy, r, clip) in layers {
			for dz in -r..=r {
				for dx in -r..=r {
					if clip && dx.abs() == r && dz.abs() == r {
						continue; // clipped corners
					}
					if dx == 0 && dz == 0 && dy <= 0 {
						continue; // trunk occupies the center
					}
					if r >= 2 && dx.abs().max(dz.abs()) == r && jroll(self.salt, dx, dz, dy) {
						continue; // jittered rim leaf
					}
					f(self.x + dx, t + dy, self.z + dz, BlockType::Leaves);
				}
			}
		}
	}
}

/// Apply every reachable tree to one chunk's freshly filled data.
/// Structures never erase terrain — only Air cells are written — with one
/// exception: the trunk's base block replaces the Grass/Dirt surface cell
/// of its own column, rooting the tree into the ground. Base cells are
/// unique per tree (one candidate per grid cell), and leaves still never
/// overwrite solids, so trees cannot clobber each other.
pub fn decorate_chunk(data: &mut ChunkData, pos: ChunkPos, trees: &[Tree]) {
	let n = CHUNK_SIZE as i32;
	let (px, py, pz) = (pos.x * n, pos.y * n, pos.z * n);
	for tree in trees {
		let base = (tree.x, tree.y - 1, tree.z);
		tree.for_each_block(|x, y, z, block| {
			let (lx, ly, lz) = (x - px, y - py, z - pz);
			if (0..n).contains(&lx) && (0..n).contains(&ly) && (0..n).contains(&lz) {
				let (lx, ly, lz) = (lx as usize, ly as usize, lz as usize);
				let cur = data.get(lx, ly, lz);
				let writable = cur == BlockType::Air
					|| ((x, y, z) == base
						&& matches!(cur, BlockType::Grass | BlockType::Dirt));
				if writable {
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
		let mut found = [false; 4];
		for cz in -64..64 {
			for cx in -64..64 {
				let Some(t) = cell_tree(cx, cz, seed) else {
					continue;
				};
				found[t.kind as usize] = true;
				// Per-kind trunk range and leaf-count bounds: the maximum
				// is the full layer list; the minimum drops every
				// jitterable rim cell of the wide (r >= 2) layers.
				let (trunk, leaves_bounds) = match t.kind {
					TreeKind::Blob => (4..=6, 25..=49),
					TreeKind::TallNarrow => (6..=7, 33..=33),
					TreeKind::Pine => (6..=7, 41..=65),
					TreeKind::SquatWide => (4..=5, 57..=109),
				};
				assert!(trunk.contains(&t.trunk_h), "{:?} trunk {}", t.kind, t.trunk_h);
				assert!(t.top() < WORLD_VBLOCK);
				let mut wood = 0;
				let mut leaves = 0;
				t.for_each_block(|x, y, z, b| {
					assert!((x - t.x).abs() <= STRUCTURE_APRON, "apron x");
					assert!((z - t.z).abs() <= STRUCTURE_APRON, "apron z");
					assert!(y >= t.y - 1 && y < t.top(), "vertical bounds");
					match b {
						BlockType::Wood => wood += 1,
						BlockType::Leaves => leaves += 1,
						other => panic!("unexpected {other:?}"),
					}
				});
				assert_eq!(wood, t.trunk_h + 1, "{:?} wood incl. root", t.kind);
				assert!(leaves_bounds.contains(&leaves), "{:?} leaves {leaves}", t.kind);
			}
		}
		assert!(found.iter().all(|&k| k), "all four kinds in sample: {found:?}");
	}
}
