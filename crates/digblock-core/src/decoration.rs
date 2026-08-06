//! Cross-chunk structures via pure apron evaluation. Candidate origins
//! come from a position hash of world coordinates, and validity is judged
//! by pure per-column functions (`column_shape`, `carved`) — so the chunk
//! that owns a structure and every neighbor it spills into compute the
//! *identical* structure independently, with no pipeline state and no
//! generation-order dependence. Each structure family draws from its own
//! salted hash stream: ordinary trees on the fine candidate grid, rare
//! landmark trees on a coarse grid, boulders on rock, bushes on grass.
//! Any future structure is a new candidate rule + block list that
//! respects `STRUCTURE_APRON`.

use crate::block::BlockType;
use crate::chunk::{ChunkData, ChunkPos, CHUNK_SIZE, WORLD_VBLOCK};
use crate::worldgen::{carved, column_shape, TreeDensity, TreeSpecies};

/// Horizontal reach guarantee: no structure block may sit further than
/// this from its origin column. Chunks scan candidates this far outside
/// their own footprint; future structures must respect (or raise) it.
pub const STRUCTURE_APRON: i32 = 8;

/// Candidate grid pitch: at most one tree candidate per CELL×CELL area.
const CELL: i32 = 8;
/// The densest per-cell candidate percentage any surface reaches
/// (`TreeDensity::Dense`) — a cheap pre-gate, so most cells reject on
/// their hash roll before paying for a `column_shape` evaluation.
const MAX_TREE_DENSITY_PCT: u64 = 55;
pub const MAX_STRUCTURE_HEIGHT: i32 = 10; // trunk (≤7) + canopy cap (+1) + headroom

/// Landmark trees: one candidate per 64×64 cell at 2.5% — mean spacing
/// ~400 blocks, so meeting one feels like an event. Max horizontal reach
/// is 5 (canopy −4..=5 around a 2×2 trunk) ≤ STRUCTURE_APRON.
const LANDMARK_CELL: i32 = 64;
const LANDMARK_PCT: u64 = 25; // of 1000
pub const MAX_LANDMARK_HEIGHT: i32 = 13; // 2×2 trunk (≤10) + cap (+1) + headroom
/// Boulder / bush per-cell candidate percentages.
const BOULDER_PCT: u64 = 10;
const BUSH_PCT: u64 = 15;

/// Per-family hash salts: full independent 64-bit streams, no bit-window
/// bookkeeping. TREE = 0 bit-preserves the original tree stream.
const TREE_SALT: u64 = 0;
const LANDMARK_SALT: u64 = 0x9e37_79b9_97f4_2c15;
const BOULDER_SALT: u64 = 0xc2b2_ae3d_27d4_eb4f;
const BUSH_SALT: u64 = 0x1656_67b1_9e37_79f9;

/// Tree silhouette family: broadleaf surfaces pick one of the first
/// three per candidate from hash bits; conifer and acacia surfaces grow
/// their dedicated forms.
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
	/// Savanna acacia: a bare trunk under an umbrella disc.
	FlatTop,
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

/// Deterministic position hash (splitmix64 over cell coords + seed bits),
/// salted per structure family so streams stay independent.
fn hash(cx: i32, cz: i32, seed: f64, salt: u64) -> u64 {
	let mut h = (u64::from(cx as u32) << 32 | u64::from(cz as u32)) ^ seed.to_bits() ^ salt;
	h ^= h >> 30;
	h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
	h ^= h >> 27;
	h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
	h ^= h >> 31;
	h
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
/// the terrain: the surface's `SurfaceProps` row decides density and
/// species; the candidate must not be a cave mouth and needs headroom
/// below the world ceiling.
fn cell_tree(cell_x: i32, cell_z: i32, seed: f64) -> Option<Tree> {
	let h = hash(cell_x, cell_z, seed, TREE_SALT);
	if h % 100 >= MAX_TREE_DENSITY_PCT {
		return None; // no surface is denser — skip the column_shape cost
	}
	let x = cell_x * CELL + ((h >> 8) % CELL as u64) as i32;
	let z = cell_z * CELL + ((h >> 16) % CELL as u64) as i32;
	let shape = column_shape(x, z, seed);
	let props = shape.surface.props();
	let pct: u64 = match props.tree_density {
		TreeDensity::None => return None,
		TreeDensity::Sparse => 6,
		TreeDensity::Normal => 30,
		TreeDensity::Dense => MAX_TREE_DENSITY_PCT,
	};
	if h % 100 >= pct {
		return None;
	}
	if carved(x, shape.height, z, seed) {
		return None; // surface swallowed by a cave mouth
	}
	if shape.height + MAX_STRUCTURE_HEIGHT >= WORLD_VBLOCK {
		return None;
	}
	let kind = match props.tree_species {
		TreeSpecies::Broadleaf => match (h >> 32) % 3 {
			0 => TreeKind::Blob,
			1 => TreeKind::TallNarrow,
			_ => TreeKind::SquatWide,
		},
		TreeSpecies::Pine => TreeKind::Pine,
		TreeSpecies::FlatTop => TreeKind::FlatTop,
	};
	let trunk_h = match kind {
		TreeKind::Blob | TreeKind::FlatTop => 4 + ((h >> 24) % 3) as i32,
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

/// A rare 2×2-trunk giant with a wide umbrella canopy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landmark {
	/// North-west trunk column; the trunk occupies (x..=x+1, z..=z+1).
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pub trunk_h: i32,
	salt: u16,
}

/// A small stone clump on rock surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boulder {
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pattern: u8,
}

/// One or two leaf blocks on grassy ground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bush {
	pub x: i32,
	pub y: i32,
	pub z: i32,
	leaf: BlockType,
	second: Option<(i32, i32)>,
}

/// Every decoration structure, dispatched by variant. Deterministic
/// generation order (landmarks first, then per fine cell tree → boulder
/// → bush) plus first-writer-wins in `decorate_chunk` keeps every chunk
/// agreeing about overlaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Structure {
	Tree(Tree),
	Landmark(Landmark),
	Boulder(Boulder),
	Bush(Bush),
}

impl Structure {
	/// Exclusive upper bound on the world y this structure occupies —
	/// `build_column`'s sky-chunk skipping relies on exactness.
	pub fn top(&self) -> i32 {
		match self {
			Structure::Tree(t) => t.top(),
			Structure::Landmark(l) => l.y + l.trunk_h + 2,
			Structure::Boulder(b) => b.y + if b.pattern == 3 { 2 } else { 1 },
			Structure::Bush(b) => b.y + 1,
		}
	}

	pub fn for_each_block(&self, f: impl FnMut(i32, i32, i32, BlockType)) {
		match self {
			Structure::Tree(t) => t.for_each_block(f),
			Structure::Landmark(l) => l.for_each_block(f),
			Structure::Boulder(b) => b.for_each_block(f),
			Structure::Bush(b) => b.for_each_block(f),
		}
	}

	/// The origin column (its candidate position) — every emitted block
	/// stays within `STRUCTURE_APRON` of it.
	pub fn origin(&self) -> (i32, i32) {
		match self {
			Structure::Tree(t) => (t.x, t.z),
			Structure::Landmark(l) => (l.x, l.z),
			Structure::Boulder(b) => (b.x, b.z),
			Structure::Bush(b) => (b.x, b.z),
		}
	}

	/// Whether (x, y, z) is a trunk root cell — the one place a structure
	/// may replace the topsoil surface block instead of writing into Air.
	pub fn is_root(&self, x: i32, y: i32, z: i32) -> bool {
		match self {
			Structure::Tree(t) => (x, y, z) == (t.x, t.y - 1, t.z),
			Structure::Landmark(l) => {
				y == l.y - 1 && (0..=1).contains(&(x - l.x)) && (0..=1).contains(&(z - l.z))
			}
			_ => false,
		}
	}
}

/// The landmark candidate (if any) of one coarse-grid cell: all four
/// trunk columns must be tree-valid grass within one block of height,
/// uncarved, with headroom.
fn cell_landmark(cell_x: i32, cell_z: i32, seed: f64) -> Option<Landmark> {
	let h = hash(cell_x, cell_z, seed, LANDMARK_SALT);
	if h % 1000 >= LANDMARK_PCT {
		return None;
	}
	let x = cell_x * LANDMARK_CELL + ((h >> 8) % LANDMARK_CELL as u64) as i32;
	let z = cell_z * LANDMARK_CELL + ((h >> 16) % LANDMARK_CELL as u64) as i32;
	let mut hmin = i32::MAX;
	let mut hmax = i32::MIN;
	for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
		let shape = column_shape(x + dx, z + dz, seed);
		if shape.surface.props().tree_density == TreeDensity::None {
			return None;
		}
		if carved(x + dx, shape.height, z + dz, seed) {
			return None;
		}
		hmin = hmin.min(shape.height);
		hmax = hmax.max(shape.height);
	}
	if hmax - hmin > 1 || hmax + MAX_LANDMARK_HEIGHT >= WORLD_VBLOCK {
		return None;
	}
	Some(Landmark {
		x,
		y: hmax + 1,
		z,
		trunk_h: 8 + ((h >> 24) % 3) as i32,
		salt: ((h >> 40) & 0xffff) as u16,
	})
}

impl Landmark {
	/// Trunk roots one block into each of the four surface cells; the
	/// canopy is two wide 10×10-minus-corners layers under a radius-2
	/// layer and a 2×2 cap. Rim leaves of the wide layers jitter away
	/// 50/50 like ordinary trees.
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
			for dy in -1..self.trunk_h {
				f(self.x + dx, self.y + dy, self.z + dz, BlockType::Wood);
			}
		}
		// (dy relative to y, x/z span around the 2×2 footprint, jitter).
		let layers: [(i32, i32, bool); 3] = [
			(self.trunk_h - 2, 4, true),
			(self.trunk_h - 1, 4, true),
			(self.trunk_h, 2, false),
		];
		for (dy, r, jitter) in layers {
			for dz in -r..=r + 1 {
				for dx in -r..=r + 1 {
					let corner = (dx == -r || dx == r + 1) && (dz == -r || dz == r + 1);
					if corner {
						continue;
					}
					if (0..=1).contains(&dx) && (0..=1).contains(&dz) && dy < self.trunk_h {
						continue; // trunk occupies the center 2×2
					}
					let rim = dx == -r || dx == r + 1 || dz == -r || dz == r + 1;
					if jitter && rim && jroll(self.salt, dx, dz, dy - self.trunk_h) {
						continue;
					}
					f(self.x + dx, self.y + dy, self.z + dz, BlockType::Leaves);
				}
			}
		}
		// 2×2 cap over the trunk.
		for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
			f(
				self.x + dx,
				self.y + self.trunk_h + 1,
				self.z + dz,
				BlockType::Leaves,
			);
		}
	}
}

/// The boulder candidate (if any) of one fine-grid cell: rock surfaces
/// only (`props().boulders`), dry, uncarved.
fn cell_boulder(cell_x: i32, cell_z: i32, seed: f64) -> Option<Boulder> {
	let h = hash(cell_x, cell_z, seed, BOULDER_SALT);
	if h % 100 >= BOULDER_PCT {
		return None;
	}
	let x = cell_x * CELL + ((h >> 8) % CELL as u64) as i32;
	let z = cell_z * CELL + ((h >> 16) % CELL as u64) as i32;
	let shape = column_shape(x, z, seed);
	if !shape.surface.props().boulders || shape.water > shape.height {
		return None;
	}
	if carved(x, shape.height, z, seed) {
		return None;
	}
	if shape.height + 4 >= WORLD_VBLOCK {
		return None;
	}
	Some(Boulder {
		x,
		y: shape.height + 1,
		z,
		pattern: ((h >> 24) % 4) as u8,
	})
}

impl Boulder {
	/// 2–3 stone cells; partial burial falls out of the Air-only write
	/// rule on slopes (cells inside higher neighbor terrain don't write).
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		const PATTERNS: [&[(i32, i32, i32)]; 4] = [
			&[(0, 0, 0), (1, 0, 0)],
			&[(0, 0, 0), (0, 0, 1)],
			&[(0, 0, 0), (1, 0, 0), (0, 0, 1)],
			&[(0, 0, 0), (1, 0, 0), (0, 1, 0)],
		];
		for &(dx, dy, dz) in PATTERNS[self.pattern as usize] {
			f(self.x + dx, self.y + dy, self.z + dz, BlockType::Stone);
		}
	}
}

/// The bush candidate (if any) of one fine-grid cell: grassy tree-valid
/// ground, uncarved. The caller skips bushes in cells that already grew
/// a tree.
fn cell_bush(cell_x: i32, cell_z: i32, seed: f64) -> Option<Bush> {
	let h = hash(cell_x, cell_z, seed, BUSH_SALT);
	if h % 100 >= BUSH_PCT {
		return None;
	}
	let x = cell_x * CELL + ((h >> 8) % CELL as u64) as i32;
	let z = cell_z * CELL + ((h >> 16) % CELL as u64) as i32;
	let shape = column_shape(x, z, seed);
	let props = shape.surface.props();
	if props.tree_density == TreeDensity::None {
		return None;
	}
	if carved(x, shape.height, z, seed) {
		return None;
	}
	if shape.height + 3 >= WORLD_VBLOCK {
		return None;
	}
	let leaf = match props.tree_species {
		TreeSpecies::Pine => BlockType::PineLeaves,
		_ => BlockType::Leaves,
	};
	const OFFSETS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
	let second = ((h >> 24) % 5).checked_sub(1).map(|i| OFFSETS[i as usize]);
	Some(Bush {
		x,
		y: shape.height + 1,
		z,
		leaf,
		second,
	})
}

impl Bush {
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		f(self.x, self.y, self.z, self.leaf);
		if let Some((dx, dz)) = self.second {
			f(self.x + dx, self.y, self.z + dz, self.leaf);
		}
	}
}

/// All valid structures whose blocks could reach a chunk with x/z origin
/// `(px, pz)` — candidates within the apron of its footprint. Landmarks
/// first (coarse grid, row-major), then per fine cell: tree → boulder →
/// bush (bushes skip cells that grew a tree). This order is part of the
/// determinism contract — `decorate_chunk` is first-writer-wins.
pub fn structures_near(px: i32, pz: i32, seed: f64) -> Vec<Structure> {
	let n = CHUNK_SIZE as i32;
	structures_in(px, px + n - 1, pz, pz + n - 1, seed)
}

/// All valid structures whose candidate cell is within `STRUCTURE_APRON`
/// of the inclusive column footprint [x0, x1] × [z0, z1]. Same
/// candidate-order contract as `structures_near`.
fn structures_in(x0: i32, x1: i32, z0: i32, z1: i32, seed: f64) -> Vec<Structure> {
	let (x0, x1) = (x0 - STRUCTURE_APRON, x1 + STRUCTURE_APRON);
	let (z0, z1) = (z0 - STRUCTURE_APRON, z1 + STRUCTURE_APRON);
	let mut out = Vec::new();
	for cell_z in z0.div_euclid(LANDMARK_CELL)..=z1.div_euclid(LANDMARK_CELL) {
		for cell_x in x0.div_euclid(LANDMARK_CELL)..=x1.div_euclid(LANDMARK_CELL) {
			if let Some(l) = cell_landmark(cell_x, cell_z, seed) {
				out.push(Structure::Landmark(l));
			}
		}
	}
	for cell_z in z0.div_euclid(CELL)..=z1.div_euclid(CELL) {
		for cell_x in x0.div_euclid(CELL)..=x1.div_euclid(CELL) {
			let tree = cell_tree(cell_x, cell_z, seed);
			let grew_tree = tree.is_some();
			if let Some(t) = tree {
				out.push(Structure::Tree(t));
			}
			if let Some(b) = cell_boulder(cell_x, cell_z, seed) {
				out.push(Structure::Boulder(b));
			}
			if !grew_tree {
				if let Some(b) = cell_bush(cell_x, cell_z, seed) {
					out.push(Structure::Bush(b));
				}
			}
		}
	}
	out
}

/// Whether any decoration block lands in the inclusive box
/// [x0, x1] × [y0, y1] × [z0, z1] — the spawn walk's clearance probe.
/// Pristine terrain never reaches a spawn candidate's capsule cells;
/// decoration (trees, boulders, bushes, landmarks) can, and this is how
/// the walk avoids it.
pub fn box_obstructed(x0: i32, x1: i32, y0: i32, y1: i32, z0: i32, z1: i32, seed: f64) -> bool {
	structures_in(x0, x1, z0, z1, seed).iter().any(|s| {
		let mut hit = false;
		s.for_each_block(|bx, by, bz, _| {
			hit |= (x0..=x1).contains(&bx) && (y0..=y1).contains(&by) && (z0..=z1).contains(&bz);
		});
		hit
	})
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
	/// (r ≥ 2) layers dropped 50/50 by the per-tree jitter roll. Conifers
	/// carry PineLeaves; every other kind carries Leaves.
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		f(self.x, self.y - 1, self.z, BlockType::Wood);
		for dy in 0..self.trunk_h {
			f(self.x, self.y + dy, self.z, BlockType::Wood);
		}
		let t = self.y + self.trunk_h - 1;
		let leaf = match self.kind {
			TreeKind::Pine => BlockType::PineLeaves,
			_ => BlockType::Leaves,
		};
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
			TreeKind::FlatTop => &[(0, 2, true), (1, 1, false)],
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
					f(self.x + dx, t + dy, self.z + dz, leaf);
				}
			}
		}
	}
}

/// Apply every reachable structure to one chunk's freshly filled data.
/// Structures never erase terrain — only Air cells are written — with one
/// exception: trunk root cells replace the Grass/Dirt/DryGrass surface
/// block of their own columns, rooting trees into the ground. Root cells
/// are unique per structure (one candidate per grid cell), and every
/// other block still never overwrites solids, so structures cannot
/// clobber each other beyond first-writer-wins in Air.
pub fn decorate_chunk(data: &mut ChunkData, pos: ChunkPos, structures: &[Structure]) {
	let n = CHUNK_SIZE as i32;
	let (px, py, pz) = (pos.x * n, pos.y * n, pos.z * n);
	for st in structures {
		st.for_each_block(|x, y, z, block| {
			let (lx, ly, lz) = (x - px, y - py, z - pz);
			if (0..n).contains(&lx) && (0..n).contains(&ly) && (0..n).contains(&lz) {
				let (lx, ly, lz) = (lx as usize, ly as usize, lz as usize);
				let cur = data.get(lx, ly, lz);
				let writable = cur == BlockType::Air
					|| (st.is_root(x, y, z)
						&& matches!(
							cur,
							BlockType::Grass | BlockType::Dirt | BlockType::DryGrass
						));
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
		let a = structures_near(0, 0, seed);
		let b = structures_near(0, 0, seed);
		assert_eq!(a, b, "deterministic");
		// Neighboring chunks agree about shared-apron candidates.
		let right = structures_near(16, 0, seed);
		for s in &a {
			if s.origin().0 >= 16 - STRUCTURE_APRON {
				assert!(right.contains(s), "border structure {s:?} seen by both");
			}
		}
	}

	/// Every structure keeps the apron contract and reports an EXACT
	/// top(): build_column's sky-chunk skipping truncates canopies at
	/// chunk-stack seams if top() ever under-reports.
	#[test]
	fn structures_respect_apron_and_exact_tops() {
		let seed = 1234.5678;
		let mut seen = 0u32;
		for ccz in -16..16 {
			for ccx in -16..16 {
				for s in structures_near(ccx * 16, ccz * 16, seed) {
					seen += 1;
					let (ox, oz) = s.origin();
					let mut max_y = i32::MIN;
					s.for_each_block(|x, y, z, _| {
						assert!((x - ox).abs() <= STRUCTURE_APRON, "apron x {s:?}");
						assert!((z - oz).abs() <= STRUCTURE_APRON, "apron z {s:?}");
						max_y = max_y.max(y);
					});
					assert_eq!(max_y, s.top() - 1, "exact top for {s:?}");
				}
			}
		}
		assert!(seen > 0, "sample contains structures");
	}

	#[test]
	fn landmark_shape_sane() {
		for trunk_h in 8..=10 {
			for salt in [0u16, 0xbeef, 0xffff] {
				let l = Landmark {
					x: 0,
					y: 60,
					z: 0,
					trunk_h,
					salt,
				};
				let (mut wood, mut leaves) = (0, 0);
				l.for_each_block(|x, y, z, b| {
					assert!((-4..=5).contains(&x) && (-4..=5).contains(&z));
					assert!(y >= l.y - 1 && y <= l.y + trunk_h + 1);
					match b {
						BlockType::Wood => wood += 1,
						BlockType::Leaves => leaves += 1,
						other => panic!("unexpected {other:?}"),
					}
				});
				// 4 trunk columns, each rooted one block down.
				assert_eq!(wood, 4 * (trunk_h + 1));
				// Two 10×10-minus-corners-minus-trunk layers (92 max, 60
				// after full rim jitter), a 6×6-minus-corners layer (32),
				// and the 2×2 cap.
				assert!((156..=220).contains(&leaves), "leaves {leaves}");
			}
		}
	}

	#[test]
	fn boulder_and_bush_shapes() {
		for pattern in 0..4u8 {
			let b = Boulder {
				x: 0,
				y: 10,
				z: 0,
				pattern,
			};
			let mut count = 0;
			let mut max_y = i32::MIN;
			b.for_each_block(|x, y, z, block| {
				assert_eq!(block, BlockType::Stone);
				assert!((0..=1).contains(&x) && (0..=1).contains(&z));
				max_y = max_y.max(y);
				count += 1;
			});
			assert!((2..=3).contains(&count));
			assert_eq!(max_y, Structure::Boulder(b).top() - 1);
		}
		for (leaf, second) in [
			(BlockType::Leaves, None),
			(BlockType::PineLeaves, Some((1, 0))),
			(BlockType::Leaves, Some((0, -1))),
		] {
			let b = Bush {
				x: 0,
				y: 10,
				z: 0,
				leaf,
				second,
			};
			let mut count = 0;
			b.for_each_block(|_, y, _, block| {
				assert_eq!(block, leaf);
				assert_eq!(y, 10);
				count += 1;
			});
			assert_eq!(count, 1 + second.is_some() as i32);
		}
	}

	/// Terrain-placed candidates land where their rules say: boulders on
	/// rock surfaces, bushes on tree-valid ground, landmarks on flat
	/// tree-valid 2×2 footprints.
	#[test]
	fn placements_match_surface_rules() {
		let seed = 42.0;
		let (mut boulders, mut bushes) = (0u32, 0u32);
		for cz in -128..128 {
			for cx in -128..128 {
				if let Some(b) = cell_boulder(cx, cz, seed) {
					let s = column_shape(b.x, b.z, seed);
					assert!(s.surface.props().boulders, "boulder on {:?}", s.surface);
					assert_eq!(b.y, s.height + 1);
					boulders += 1;
				}
				if let Some(b) = cell_bush(cx, cz, seed) {
					let s = column_shape(b.x, b.z, seed);
					assert!(s.surface.props().tree_density != TreeDensity::None);
					bushes += 1;
				}
			}
		}
		assert!(boulders > 0, "sample contains boulders");
		assert!(bushes > 0, "sample contains bushes");
	}

	#[test]
	fn landmarks_exist() {
		let seed = 1234.5678;
		let mut found = 0u32;
		for cz in -96..96 {
			for cx in -96..96 {
				if let Some(l) = cell_landmark(cx, cz, seed) {
					assert!((8..=10).contains(&l.trunk_h));
					assert!(Structure::Landmark(l).top() < WORLD_VBLOCK);
					found += 1;
				}
			}
		}
		assert!(found > 0, "coarse sample contains landmarks");
	}

	/// Per-kind trunk range and leaf-count bounds: the maximum is the full
	/// layer list; the minimum drops every jitterable rim cell of the wide
	/// (r >= 2) layers.
	fn expect_shape(
		kind: TreeKind,
	) -> (std::ops::RangeInclusive<i32>, std::ops::RangeInclusive<i32>) {
		match kind {
			TreeKind::Blob => (4..=6, 25..=49),
			TreeKind::TallNarrow => (6..=7, 33..=33),
			TreeKind::Pine => (6..=7, 41..=65),
			TreeKind::SquatWide => (4..=5, 57..=109),
			TreeKind::FlatTop => (4..=6, 17..=29),
		}
	}

	fn assert_sane(t: &Tree) {
		let (trunk, leaves_bounds) = expect_shape(t.kind);
		assert!(
			trunk.contains(&t.trunk_h),
			"{:?} trunk {}",
			t.kind,
			t.trunk_h
		);
		assert!(t.top() < WORLD_VBLOCK);
		let expect_leaf = match t.kind {
			TreeKind::Pine => BlockType::PineLeaves,
			_ => BlockType::Leaves,
		};
		let mut wood = 0;
		let mut leaves = 0;
		t.for_each_block(|x, y, z, b| {
			assert!((x - t.x).abs() <= STRUCTURE_APRON, "apron x");
			assert!((z - t.z).abs() <= STRUCTURE_APRON, "apron z");
			assert!(y >= t.y - 1 && y < t.top(), "vertical bounds");
			match b {
				BlockType::Wood => wood += 1,
				b if b == expect_leaf => leaves += 1,
				other => panic!("unexpected {other:?}"),
			}
		});
		assert_eq!(wood, t.trunk_h + 1, "{:?} wood incl. root", t.kind);
		assert!(
			leaves_bounds.contains(&leaves),
			"{:?} leaves {leaves}",
			t.kind
		);
	}

	/// Terrain-placed candidates are sane, and the broadleaf sample shows
	/// all three of its silhouettes. Pine/FlatTop placement depends on
	/// which climates the sample area holds, so their shapes are pinned
	/// by `conifer_and_acacia_shapes` on synthetic trees instead.
	#[test]
	fn trees_have_sane_shape() {
		let seed = 42.0;
		let mut broadleaf = [false; 3];
		for cz in -128..128 {
			for cx in -128..128 {
				let Some(t) = cell_tree(cx, cz, seed) else {
					continue;
				};
				match t.kind {
					TreeKind::Blob => broadleaf[0] = true,
					TreeKind::TallNarrow => broadleaf[1] = true,
					TreeKind::SquatWide => broadleaf[2] = true,
					_ => {}
				}
				assert_sane(&t);
			}
		}
		assert!(
			broadleaf.iter().all(|&k| k),
			"all broadleaf kinds in sample: {broadleaf:?}"
		);
	}

	#[test]
	fn conifer_and_acacia_shapes() {
		for (kind, trunks) in [(TreeKind::Pine, 6..=7), (TreeKind::FlatTop, 4..=6)] {
			for trunk_h in trunks {
				for salt in [0u16, 0xbeef, 0xffff] {
					assert_sane(&Tree {
						x: 0,
						y: 60,
						z: 0,
						trunk_h,
						kind,
						salt,
					});
				}
			}
		}
	}
}
