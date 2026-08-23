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

use std::cell::RefCell;
use std::thread::LocalKey;

use crate::block::BlockType;
use crate::chunk::{CHUNK_SIZE, ChunkData, ChunkPos, WORLD_VBLOCK};
use crate::worldgen::{
	ColumnShape, SEA_LEVEL, TreeDensity, TreeSpecies, carved, column_shape_above,
};

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
pub const MAX_STRUCTURE_HEIGHT: i32 = 10; // trunk + canopy cap + headroom

/// Landmark trees: one rare candidate per grid cell, spaced far enough
/// apart that meeting one feels like an event. The canopy's horizontal
/// reach around the trunk stays within STRUCTURE_APRON.
const LANDMARK_CELL: i32 = 64;
/// Fallback-scan stride: the probe grid a rolled cell is searched on
/// when its first position fails validation.
const LANDMARK_STRIDE: i32 = 8;
const LANDMARK_PCT: u64 = 25; // of 1000
pub const MAX_LANDMARK_HEIGHT: i32 = 13; // trunk + cap + headroom
/// Boulder / bush per-cell candidate percentages.
const BOULDER_PCT: u64 = 10;
const BUSH_PCT: u64 = 15;

/// Per-family hash salts: full independent 64-bit streams, no bit-window
/// bookkeeping. The tree family's salt is the identity, which
/// bit-preserves the original tree stream.
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
	/// Poplar-like column: a tall stack of narrow rings and a tip.
	TallNarrow,
	/// Conifer: alternating wide/narrow tiers tapering to a tip.
	Pine,
	/// Low spreading oak: broad rings under a wide cap.
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

/// The candidate column of a decoration cell, or `None` when it sits at or below the waterline
/// band. Nothing grows or piles there: every surface a structure family accepts classifies strictly
/// above the band, since at or below it a column is ocean, river, lake, or shore. Rejecting on
/// altitude costs one field evaluation where a classification costs five.
fn candidate_column(x: i32, z: i32, seed: f64) -> Option<ColumnShape> {
	column_shape_above(x, z, seed, SEA_LEVEL + 1)
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
	let shape = candidate_column(x, z, seed)?;
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

/// A rare giant on a square multi-column trunk, with a wide umbrella
/// canopy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landmark {
	/// North-west trunk column; the trunk fills the square footprint
	/// anchored here.
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

	/// Inclusive lower bound on the world y this structure occupies — the trunk ROOT for anything
	/// that has one, since that is the single cell reaching below the surface. `decorate_chunk`
	/// tests `base..top` against a chunk before enumerating, so a bound that sat above the lowest
	/// emitted block would silently drop it; `structures_respect_apron_and_their_exact_y_span`
	/// walks real worldgen to keep both ends exact, and
	/// `a_structure_emits_into_a_chunk_exactly_when_its_span_reaches_it` pins the skip itself.
	pub fn base(&self) -> i32 {
		match self {
			Structure::Tree(t) => t.y - 1,
			Structure::Landmark(l) => l.y - 1,
			Structure::Boulder(b) => b.y,
			Structure::Bush(b) => b.y,
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

/// The landmark candidate (if any) of one coarse-grid cell. Two-stage placement, and the ORDER is
/// load-bearing: the first attempt is the original single-roll position, verbatim, so every
/// landmark that stood before the fallback existed still stands at its exact spot — which is what
/// lets the fallback ride a protocol bump with saves kept (`landmark_fallback_only_adds` is the
/// proof). The scan runs only where that attempt fails, so it can only ADD trees.
fn cell_landmark(cell_x: i32, cell_z: i32, seed: f64) -> Option<Landmark> {
	let h = hash(cell_x, cell_z, seed, LANDMARK_SALT);
	if h % 1000 >= LANDMARK_PCT {
		return None;
	}
	let x0 = cell_x * LANDMARK_CELL + ((h >> 8) % LANDMARK_CELL as u64) as i32;
	let z0 = cell_z * LANDMARK_CELL + ((h >> 16) % LANDMARK_CELL as u64) as i32;
	if let Some(l) = landmark_at(x0, z0, h, seed) {
		return Some(l);
	}
	// Half the single rolls died to water or bare ground the rest of
	// the cell didn't share, so the cell gets a full look on a
	// `LANDMARK_STRIDE` grid before it is given up on. The hash jitter
	// keeps cells off one global lattice.
	let jx = ((h >> 8) % LANDMARK_STRIDE as u64) as i32;
	let jz = ((h >> 16) % LANDMARK_STRIDE as u64) as i32;
	for sx in 0..LANDMARK_CELL / LANDMARK_STRIDE {
		for sz in 0..LANDMARK_CELL / LANDMARK_STRIDE {
			let x = cell_x * LANDMARK_CELL + jx + sx * LANDMARK_STRIDE;
			let z = cell_z * LANDMARK_CELL + jz + sz * LANDMARK_STRIDE;
			if let Some(l) = landmark_at(x, z, h, seed) {
				return Some(l);
			}
		}
	}
	None
}

/// One position validated: all four trunk columns must be tree-valid
/// grass within one block of height, uncarved, with headroom. `h` is
/// the CELL's roll — trunk height and salt ride it, not the position,
/// so a fallback tree is the same tree the first position would have
/// grown.
fn landmark_at(x: i32, z: i32, h: u64, seed: f64) -> Option<Landmark> {
	let mut hmin = i32::MAX;
	let mut hmax = i32::MIN;
	for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
		let shape = candidate_column(x + dx, z + dz, seed)?;
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
	/// Trunk roots one block into each of its surface cells; the canopy is
	/// two wide corner-clipped layers under a smaller one, capped over the
	/// trunk. Rim leaves of the wide layers jitter away like ordinary
	/// trees.
	pub fn for_each_block(&self, mut f: impl FnMut(i32, i32, i32, BlockType)) {
		for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
			for dy in -1..self.trunk_h {
				f(self.x + dx, self.y + dy, self.z + dz, BlockType::Wood);
			}
		}
		// (dy relative to y, x/z span around the trunk footprint, jitter).
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
						continue; // the trunk occupies the centre cells
					}
					let rim = dx == -r || dx == r + 1 || dz == -r || dz == r + 1;
					if jitter && rim && jroll(self.salt, dx, dz, dy - self.trunk_h) {
						continue;
					}
					f(self.x + dx, self.y + dy, self.z + dz, BlockType::Leaves);
				}
			}
		}
		// The cap over the trunk.
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
	let shape = candidate_column(x, z, seed)?;
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
	/// A couple of stone cells; partial burial falls out of the Air-only
	/// write rule on slopes (cells inside higher neighbor terrain don't
	/// write).
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
	let shape = candidate_column(x, z, seed)?;
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

/// Every family's validated candidate for one fine grid cell — bushes
/// already yield to a tree in the same cell, as the candidate order says.
#[derive(Clone, Copy, Default)]
struct CellCandidates {
	tree: Option<Tree>,
	boulder: Option<Boulder>,
	bush: Option<Bush>,
}

fn cell_candidates(cell_x: i32, cell_z: i32, seed: f64) -> CellCandidates {
	let tree = cell_tree(cell_x, cell_z, seed);
	let boulder = cell_boulder(cell_x, cell_z, seed);
	let bush = match tree {
		Some(_) => None,
		None => cell_bush(cell_x, cell_z, seed),
	};
	CellCandidates {
		tree,
		boulder,
		bush,
	}
}

/// Slots of the candidate memos below. A power of two, indexed by the low
/// bits of the cell coordinates rather than a hash, so a square window of
/// cells — wider than any single job's apron and its neighbours' — never
/// self-evicts.
const MEMO_SLOTS: usize = 256;

/// A bounded, direct-mapped memo of validated candidate cells. Aprons of neighbouring chunks
/// overlap on each axis, so the same cell is validated by several column jobs while validation — a
/// terrain probe per candidate — is the expensive part. The key carries the entire input, so a hit
/// is by construction what a miss would have computed and worldgen stays a pure function of
/// `(x, z, seed)`. Thread-local, so workers never share one, and fixed-size, so it cannot grow.
struct CellMemo<T> {
	keys: [Option<(i32, i32, u64)>; MEMO_SLOTS],
	vals: [T; MEMO_SLOTS],
}

impl<T: Copy + Default> CellMemo<T> {
	fn new() -> Self {
		Self {
			keys: [None; MEMO_SLOTS],
			vals: [T::default(); MEMO_SLOTS],
		}
	}
}

fn memoized<T: Copy + Default>(
	memo: &'static LocalKey<RefCell<CellMemo<T>>>,
	cell_x: i32,
	cell_z: i32,
	seed: f64,
	miss: impl FnOnce() -> T,
) -> T {
	let key = (cell_x, cell_z, seed.to_bits());
	let slot = (cell_x as usize & 15) | (cell_z as usize & 15) << 4;
	let hit = memo.with_borrow(|m| (m.keys[slot] == Some(key)).then(|| m.vals[slot]));
	if let Some(v) = hit {
		return v;
	}
	// Computed outside the borrow: validation reaches back into worldgen,
	// which must stay free to use these memos itself.
	let v = miss();
	memo.with_borrow_mut(|m| {
		m.keys[slot] = Some(key);
		m.vals[slot] = v;
	});
	v
}

thread_local! {
	static FINE_MEMO: RefCell<CellMemo<CellCandidates>> = RefCell::new(CellMemo::new());
	static LANDMARK_MEMO: RefCell<CellMemo<Option<Landmark>>> = RefCell::new(CellMemo::new());
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
			let landmark = memoized(&LANDMARK_MEMO, cell_x, cell_z, seed, || {
				cell_landmark(cell_x, cell_z, seed)
			});
			if let Some(l) = landmark {
				out.push(Structure::Landmark(l));
			}
		}
	}
	for cell_z in z0.div_euclid(CELL)..=z1.div_euclid(CELL) {
		for cell_x in x0.div_euclid(CELL)..=x1.div_euclid(CELL) {
			let c = memoized(&FINE_MEMO, cell_x, cell_z, seed, || {
				cell_candidates(cell_x, cell_z, seed)
			});
			if let Some(t) = c.tree {
				out.push(Structure::Tree(t));
			}
			if let Some(b) = c.boulder {
				out.push(Structure::Boulder(b));
			}
			if let Some(b) = c.bush {
				out.push(Structure::Bush(b));
			}
		}
	}
	out
}

/// A worldgen trunk standing at one column: its inclusive y-span (root
/// cell included) and the tree's identity — what `worldgen::nodes` needs
/// to derive heartwood the way `ore_at` derives a vein.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrunkInfo {
	/// Inclusive world-y span of the trunk cells in this column, the root
	/// cell at the bottom.
	pub y0: i32,
	pub y1: i32,
	/// The column that identifies the TREE (a landmark's north-west trunk
	/// column) — one coordinate per tree, whatever column of it is asked
	/// about, so a per-tree roll hashes the same input from every cell.
	pub anchor: (i32, i32),
	pub landmark: bool,
}

/// The trunk whose cells stand in column `(x, z)`, if any — landmarks first, matching
/// `decorate_chunk`'s write order, so where a landmark and a fine tree could both claim a column
/// the answer is the one whose wood actually stands there. A pure function of `(x, z, seed)`;
/// callers needing the CELL to really hold wood — a canopy may have covered a trunk cell, a cliff
/// may have left it unwritten — must check the live cell themselves, the `ore::revealed` shape.
pub fn trunk_at(x: i32, z: i32, seed: f64) -> Option<TrunkInfo> {
	for cell_z in (z - 1).div_euclid(LANDMARK_CELL)..=z.div_euclid(LANDMARK_CELL) {
		for cell_x in (x - 1).div_euclid(LANDMARK_CELL)..=x.div_euclid(LANDMARK_CELL) {
			let l = memoized(&LANDMARK_MEMO, cell_x, cell_z, seed, || {
				cell_landmark(cell_x, cell_z, seed)
			});
			if let Some(l) = l
				&& (0..=1).contains(&(x - l.x))
				&& (0..=1).contains(&(z - l.z))
			{
				return Some(TrunkInfo {
					y0: l.y - 1,
					y1: l.y + l.trunk_h - 1,
					anchor: (l.x, l.z),
					landmark: true,
				});
			}
		}
	}
	let (cell_x, cell_z) = (x.div_euclid(CELL), z.div_euclid(CELL));
	let c = memoized(&FINE_MEMO, cell_x, cell_z, seed, || {
		cell_candidates(cell_x, cell_z, seed)
	});
	let t = c.tree?;
	((t.x, t.z) == (x, z)).then_some(TrunkInfo {
		y0: t.y - 1,
		y1: t.y + t.trunk_h - 1,
		anchor: (t.x, t.z),
		landmark: false,
	})
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
	/// listed per kind relative to the trunk top, with rim leaves of the
	/// wide layers dropped by the per-tree jitter roll. Conifers
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

/// Apply every reachable structure to one chunk's freshly filled data. Structures never erase
/// terrain — only Air cells are written — with one exception: trunk root cells replace the surface
/// block of their own columns, rooting trees into the ground. Root cells are unique per structure,
/// and every other block still never overwrites solids, so structures cannot clobber each other
/// beyond first-writer-wins in Air.
pub fn decorate_chunk(data: &mut ChunkData, pos: ChunkPos, structures: &[Structure]) {
	let n = CHUNK_SIZE as i32;
	let (px, py, pz) = (pos.x * n, pos.y * n, pos.z * n);
	for st in structures {
		// A column's whole vertical stack is decorated against the SAME candidate list, and a
		// structure stands in one or two of its chunks — so without this every other chunk
		// enumerates every block of every structure only to range-test it away.
		if st.top() <= py || st.base() >= py + n {
			continue;
		}
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
mod tests;
