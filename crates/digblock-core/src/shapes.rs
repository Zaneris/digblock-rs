//! Sub-block shape registry: static geometry per (block id, state nibble)
//! on a 12×12×12 sub-grid. Shapes are authored once as sub-box lists in a
//! canonical facing and every variant (rotations, door open/half) is baked
//! from them at first use — rasterize to a bitset, emit exposed sub-faces,
//! greedy-merge into maximal quads — all pure integer ops, so the registry
//! is deterministic across platforms and both sides of the wire.
//!
//! Layering: above block.rs/dir.rs, below the mesher. The mesher stamps
//! the baked quad templates; physics consumes the collision boxes and the
//! stair ramp tag; the server's edit validation consumes `state_valid`.
//!
//! Corner-slot and UV conventions deliberately mirror the mesher's
//! `face_corners` (TL/TR/BL/BR slots, corner-code UV): the baker works in
//! each face's (u, v) frame and inverts it back to positions, so a baked
//! full-cube face is bit-identical to the mesher's full-block output —
//! pinned by `full_cube_bakes_to_the_six_block_faces` here and a
//! cross-check test in mesher.rs.

use crate::block::{BlockType, Essence, ShapeKind};
use crate::chunk::{cell_id, cell_state, STATE_MASK, STATE_SHIFT};
use crate::dir::{Dir, DIRS};
use glam::Vec3;
use std::sync::LazyLock;

/// Sub-cells per block edge.
pub const GRID: u8 = 12;
const G: i32 = GRID as i32;
const GU: usize = GRID as usize;

/// Ceiling on any variant's baked quad count — sizes the renderer's shared
/// index buffer (`MAX_QUADS`); pinned per-variant by a test here. Worst
/// today: the upper door half (lites + hinge) at 43.
pub const MAX_SHAPE_QUADS: usize = 48;

/// Sub-units one whole block converts into (a 3×3×3 division).
pub const SUBS_PER_BLOCK: u32 = 27;
/// Filled twelfths³ per sub-unit (one 4×4×4-twelfth cube).
pub const SUB_UNIT_VOLUME: u32 = 1728 / SUBS_PER_BLOCK;

// --- state nibble -----------------------------------------------------

/// Horizontal facing, axis-named (compass names are a trap here:
/// `Dir::West` is +x). Values are the k in "k × 90° CCW-about-+y
/// rotations of the canonical +x shape" — and the low 2 state bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facing {
	PosX = 0,
	PosZ = 1,
	NegX = 2,
	NegZ = 3,
}

impl Facing {
	pub const ALL: [Facing; 4] = [Facing::PosX, Facing::PosZ, Facing::NegX, Facing::NegZ];

	#[inline]
	pub fn from_bits(v: u8) -> Facing {
		Self::ALL[(v & 3) as usize]
	}

	/// The unit offset of this facing (stairs ascend toward it; a closed
	/// door's leaf sits one twelfth in from the cell face it points at,
	/// leaving room for the handle's protrusion).
	#[inline]
	pub fn offset(self) -> (i32, i32) {
		match self {
			Facing::PosX => (1, 0),
			Facing::PosZ => (0, 1),
			Facing::NegX => (-1, 0),
			Facing::NegZ => (0, -1),
		}
	}
}

/// Stairs: state = facing only (bits 2-3 stay clear on the wire —
/// `state_valid` enforces it; decode is forgiving and masks).
#[inline]
pub fn stair_state(f: Facing) -> u8 {
	f as u8
}

/// Door: facing | open<<2 | upper<<3 — exactly fills the nibble.
#[inline]
pub fn door_state(f: Facing, open: bool, upper: bool) -> u8 {
	f as u8 | (u8::from(open) << 2) | (u8::from(upper) << 3)
}

/// (facing, open, upper) of a door state.
#[inline]
pub fn door_parts(state: u8) -> (Facing, bool, bool) {
	(
		Facing::from_bits(state),
		state & 0b0100 != 0,
		state & 0b1000 != 0,
	)
}

/// The other half of a door pair (upper ↔ lower, same column). Client
/// prediction and the server's pair writes share this so they can never
/// disagree about which cell is the partner.
#[inline]
pub fn door_partner(cell: (i32, i32, i32), state: u8) -> (i32, i32, i32) {
	let (_, _, upper) = door_parts(state);
	(cell.0, cell.1 + if upper { -1 } else { 1 }, cell.2)
}

/// The door cell with its open bit flipped (state nibble bit 2 = cell
/// bit 14).
#[inline]
pub fn door_toggled(cell: u16) -> u16 {
	cell ^ (0b0100 << STATE_SHIFT)
}

/// Slab: state bit 0 set = two-thirds fill (clear = one-third), bit 1
/// set = anchored to the cell's ceiling instead of its floor. Bits 2-3
/// stay clear — `state_valid` enforces it.
#[inline]
pub fn slab_state(two_thirds: bool, top: bool) -> u8 {
	u8::from(two_thirds) | (u8::from(top) << 1)
}

/// Whether a slab cell hangs from its ceiling (state bit 1).
#[inline]
pub fn slab_is_top(cell: u16) -> bool {
	cell_state(cell) & 0b10 != 0
}

/// The face normal a slab grows from: its OWN open side — up for a
/// floor slab, down for a ceiling one. Clicking the other face is
/// clicking the boundary it is already flush against, so that places
/// into the neighbouring cell the way a full cube would. Client
/// prediction and the server's grow derivation share this.
#[inline]
pub fn slab_grow_normal(cell: u16) -> (i32, i32, i32) {
	(0, if slab_is_top(cell) { -1 } else { 1 }, 0)
}

/// The state a freshly placed slab lands in, from the face it was built
/// against: a slab placed onto a ceiling hangs from it, everything else
/// (floors and side faces alike) sits on the floor. Derived, never sent
/// — the server re-derives it from its own verified normal, so the wire
/// still carries the canonical state-0 slab.
#[inline]
pub fn slab_place_state(normal: (i32, i32, i32)) -> u8 {
	slab_state(false, normal.1 < 0)
}

/// A slab cell one third taller: 1/3 → 2/3, 2/3 → the plain base block.
/// It grows into its cell from whichever end it is anchored to, so the
/// anchor rides along. Client prediction and the server's grow write
/// share this so they can never disagree about the grown value.
#[inline]
pub fn slab_grown(cell: u16) -> u16 {
	let b = BlockType::from_id(cell_id(cell));
	debug_assert!(b.is_slab());
	if cell_state(cell) & 1 == 0 {
		cell | (1 << STATE_SHIFT)
	} else {
		b.slab_base().map_or(cell, |base| base as u16)
	}
}

/// A slab cell one third shorter: 2/3 → 1/3, 1/3 → Air. The dig-shrink
/// counterpart of [`slab_grown`], shared the same way — and it keeps the
/// anchor for the same reason, so a dug ceiling slab stays on its
/// ceiling instead of dropping to the floor.
#[inline]
pub fn slab_shrunk(cell: u16) -> u16 {
	debug_assert!(BlockType::from_id(cell_id(cell)).is_slab());
	if cell_state(cell) & 1 == 0 {
		BlockType::Air as u16
	} else {
		cell & !(1 << STATE_SHIFT)
	}
}

// --- geometry types ---------------------------------------------------

/// Axis-aligned sub-box in twelfths, min inclusive, max exclusive
/// (coordinates 0..=12).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SubBox {
	pub min: [u8; 3],
	pub max: [u8; 3],
}

impl SubBox {
	pub const fn new(min: [u8; 3], max: [u8; 3]) -> Self {
		Self { min, max }
	}

	/// World-space AABB of this box inside block cell `c`.
	#[inline]
	pub fn aabb(&self, c: (i32, i32, i32)) -> (Vec3, Vec3) {
		let s = 1.0 / f32::from(GRID);
		let lo = Vec3::new(
			c.0 as f32 + f32::from(self.min[0]) * s,
			c.1 as f32 + f32::from(self.min[1]) * s,
			c.2 as f32 + f32::from(self.min[2]) * s,
		);
		let hi = Vec3::new(
			c.0 as f32 + f32::from(self.max[0]) * s,
			c.1 as f32 + f32::from(self.max[1]) * s,
			c.2 as f32 + f32::from(self.max[2]) * s,
		);
		(lo, hi)
	}
}

/// 12×12 coverage of one block-boundary plane, in the face's NEUTRAL
/// tangent frame — the same ascending-axis pairs `face_ao` uses (x-faces:
/// (y, z), y-faces: (x, z), z-faces: (x, y)); bit = first axis, row =
/// second. A face's dir-D mask and its neighbor's opposite(D) mask share
/// this frame, so coverage compares bit-for-bit with no flips.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FaceMask(pub [u16; GU]);

impl FaceMask {
	pub const EMPTY: FaceMask = FaceMask([0; GU]);
	const ROW: u16 = (1 << GRID) - 1;

	#[inline]
	pub fn is_full(&self) -> bool {
		self.0.iter().all(|&r| r == Self::ROW)
	}

	/// All bits of the rect [a0, a1) × [b0, b1) set (neutral frame).
	#[inline]
	pub fn covers(&self, a0: u8, b0: u8, a1: u8, b1: u8) -> bool {
		let bits = row_bits(a0, a1);
		(b0..b1).all(|b| self.0[b as usize] & bits == bits)
	}
}

#[inline]
fn row_bits(a0: u8, a1: u8) -> u16 {
	debug_assert!(a0 < a1 && a1 <= GRID);
	(((1u32 << (a1 - a0)) - 1) << a0) as u16
}

/// One baked template quad. Corners are [TL, TR, BL, BR] in twelfths —
/// the same slot semantics as the mesher's `face_corners`, so the shared
/// index pattern and the corner codes carry over unchanged.
#[derive(Clone, Copy, Debug)]
pub struct ShapeQuad {
	pub dir: Dir,
	pub corners: [[u8; 3]; 4],
	/// Per-corner UV in twelfths (matches the corner-code UV of a full
	/// block face: TL=(u0,v0) … BR=(u1,v1)).
	pub uv: [[u8; 2]; 4],
	/// Lies on the block-boundary plane toward `dir` → subject to
	/// neighbor culling. Interior quads always emit.
	pub boundary: bool,
	/// Boundary quads: the covered rect [a0, b0, a1, b1) in the neutral
	/// tangent frame, for `FaceMask::covers`. Zeroed for interior quads.
	pub rect: [u8; 4],
}

/// Everything baked for one (shape, state) combination.
pub struct ShapeVariant {
	/// Physics boxes (world edit validation shares them). Open doors:
	/// EMPTY — the leaf stops blocking entirely.
	pub collision: Vec<SubBox>,
	/// Picking boxes for the interact ray: equals `collision` except an
	/// open door, whose swung leaf stays clickable to close it.
	pub interaction: Vec<SubBox>,
	/// Render template, stamped per instance by the mesher.
	pub quads: Vec<ShapeQuad>,
	/// Boundary coverage per `Dir`, for culling BOTH directions.
	pub face_masks: [FaceMask; 6],
	/// `face_masks[d].is_full()`, precomputed.
	pub occludes_full: [bool; 6],
	/// Union AABB of the DRAWN geometry, in twelfths — the box of best
	/// fit around the shape. The crosshair outline wraps this instead of
	/// the cell, so a slab or a door leaf is boxed at its own size; a
	/// stair's is the full cube, because that already IS its best fit.
	/// Taken from the authored boxes (which is exactly what the raster
	/// fills, so no drawn quad can fall outside it) rather than from
	/// `collision` — a closed door's knob protrudes past the leaf
	/// physics uses, and an outline that cut through it would read as a
	/// bug.
	pub bounds: SubBox,
	/// Stairs: the ascending facing — physics resolves a diagonal ramp
	/// instead of the sub-boxes when approaching from the low side.
	pub ramp: Option<Facing>,
}

// --- registry ---------------------------------------------------------

/// The shape for a block + state, if the block is shaped.
#[inline]
pub fn shape_of(b: BlockType, state: u8) -> Option<&'static ShapeVariant> {
	match b.props().shape? {
		ShapeKind::Stairs => Some(&STAIRS[(state & 3) as usize]),
		ShapeKind::Door => Some(&DOORS[(state & 0xF) as usize]),
		ShapeKind::Slab => Some(&SLABS[(state & 3) as usize]),
		ShapeKind::Torch => Some(&TORCHES[0]),
	}
}

/// The shape for a raw cell (id + state), if shaped.
#[inline]
pub fn shape_of_cell(cell: u16) -> Option<&'static ShapeVariant> {
	shape_of(BlockType::from_id(cell_id(cell)), cell_state(cell))
}

/// Force every variant table to bake NOW, during load.
///
/// Baking is lazy per family, and each family bakes ALL its variants at
/// once: the first door to reach a mesher, a `neighbor_cover` probe, a
/// physics test or the alchemy screen pays for all 16 (measured 720µs
/// natively, several ms on wasm) in whatever frame happens to touch it
/// first. That is a guaranteed one-frame hitch at an arbitrary moment of
/// gameplay, so spend it where a pause is already expected instead. Warm
/// calls are single-digit nanoseconds, so this is purely about WHEN.
pub fn prewarm() {
	let _ = LazyLock::force(&STAIRS);
	let _ = LazyLock::force(&DOORS);
	let _ = LazyLock::force(&SLABS);
	let _ = LazyLock::force(&TORCHES);
}

/// No sub-block geometry — physics/raycast take the unit-cube fast path.
#[inline]
pub fn is_full_cube(cell: u16) -> bool {
	BlockType::from_id(cell_id(cell)).props().shape.is_none()
}

/// Stairs: the ascending facing of this cell, if it is a stair.
#[inline]
pub fn stair_ramp(cell: u16) -> Option<Facing> {
	shape_of_cell(cell)?.ramp
}

/// Physics boxes of a cell ([] for full cubes too — callers branch on
/// `is_full_cube` first; empty for open doors).
#[inline]
pub fn collision_boxes(cell: u16) -> &'static [SubBox] {
	shape_of_cell(cell).map_or(&[], |v| &v.collision)
}

/// Interact-ray boxes (open door = swung leaf).
#[inline]
pub fn interaction_boxes(cell: u16) -> &'static [SubBox] {
	shape_of_cell(cell).map_or(&[], |v| &v.interaction)
}

/// Box of best fit around a cell's drawn geometry, in twelfths — `None`
/// for a full cube, which needs no box because it IS the cell. See
/// [`ShapeVariant::bounds`].
#[inline]
pub fn draw_bounds(cell: u16) -> Option<SubBox> {
	shape_of_cell(cell).map(|v| v.bounds)
}

/// THE wire gate for cell values (relaxes the server's old all-state-
/// bits-are-BadBlock rule): known id, spare bits clear, and a state the
/// id's shape actually defines (zero for stateless blocks; facing-only
/// for stairs; any facing×open×half for doors — the door *placement*
/// rule "lower+closed only" is the server's, not this one's).
#[inline]
pub fn state_valid(cell: u16) -> bool {
	const SPARE_MASK: u16 = !(crate::chunk::ID_MASK | STATE_MASK);
	if cell & SPARE_MASK != 0 {
		return false;
	}
	let Some(b) = BlockType::try_from_id(cell_id(cell)) else {
		return false;
	};
	let state = cell_state(cell);
	match b.props().shape {
		None => state == 0,
		Some(ShapeKind::Stairs) => state & !3 == 0,
		Some(ShapeKind::Door) => true,
		Some(ShapeKind::Slab) => state & !3 == 0,
		Some(ShapeKind::Torch) => state == 0,
	}
}

// --- alchemy ----------------------------------------------------------

/// One alchemy craft: `count` items out of `cost` units of `material`.
/// Batched so fractional-unit shapes stay exact (a future half-slab at
/// 13.5 units crafts as 2-for-27); both launch shapes are whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CraftRecipe {
	pub material: Essence,
	pub count: u32,
	pub cost: u32,
}

/// The craft table. Costs are the shape's filled 12ths volume rounded up
/// to whole sub-units (pinned against the rasterized volumes by a test):
/// stairs 1152/64 → 18 (exactly 2/3 block), door 588/64 → 10 (uncut
/// leaves — the window lites don't discount — plus the handle and the
/// two hinges), slab 576/64 → 9 (exactly a third — three slabs equal
/// one block, closing the grow-to-base-block economy).
pub fn craft_recipe(b: BlockType) -> Option<CraftRecipe> {
	let (material, count, cost) = match b {
		BlockType::WoodStairs => (Essence::Flora, 1, 18),
		BlockType::StoneStairs => (Essence::Earth, 1, 18),
		BlockType::WoodDoor => (Essence::Flora, 1, 10),
		BlockType::WoodSlab => (Essence::Flora, 1, 9),
		BlockType::StoneSlab => (Essence::Earth, 1, 9),
		_ => return None,
	};
	Some(CraftRecipe {
		material,
		count,
		cost,
	})
}

/// Every craftable shaped item, in display order.
pub const CRAFTABLE: [BlockType; 5] = [
	BlockType::WoodStairs,
	BlockType::StoneStairs,
	BlockType::WoodDoor,
	BlockType::WoodSlab,
	BlockType::StoneSlab,
];

// --- authored geometry ------------------------------------------------

/// Canonical stairs, ascending toward +x: three plain steps, risers at
/// 4/8/12 twelfths. (Two nosing/reveal detail passes were tried and
/// reverted: an overhanging tread lip reads asymmetric — the top step
/// has nothing overhanging it — and an inset-tread reveal didn't look
/// right either. Plain steps it is.)
const STAIR_BOXES: [SubBox; 3] = [
	SubBox::new([0, 0, 0], [12, 4, 12]),
	SubBox::new([4, 4, 0], [12, 8, 12]),
	SubBox::new([8, 8, 0], [12, 12, 12]),
];

/// Canonical closed leaf (facing +x): 2-twelfths slab set back one
/// twelfth from the −x face (room for the handle to protrude one
/// twelfth on either side), full 12×12 elevation.
const LEAF: SubBox = SubBox::new([1, 0, 0], [3, 12, 12]);

/// Door handle (lower half only): a 2×2 knob through the leaf near the
/// latch stile, protruding one twelfth past each leaf face.
const HANDLE: SubBox = SubBox::new([0, 9, 9], [4, 11, 11]);

/// Door hinges, one per half, mounted on the BACK of the closed leaf
/// (x 3..4) at the pivot edge (z 0..1). They ride the leaf through
/// [`swing_open`] — the open pose lands them at the corner (x 0..1,
/// z 0..1), bridging the setback gap to whatever the open door hangs
/// against — so the door and its knuckles read as one rigid rotation.
const HINGE_LOWER: SubBox = SubBox::new([3, 1, 0], [4, 3, 1]);
const HINGE_UPPER: SubBox = SubBox::new([3, 9, 0], [4, 11, 1]);

/// Canonical upper-half panel: divided-lite window, 2/3/2/3/2 in both
/// axes (w = z here) — rails, stiles, and a cross mullion leaving four
/// 3×3 lites. Overlaps are fine; the raster unions them.
const DOOR_UPPER_BOXES: [SubBox; 6] = [
	SubBox::new([1, 0, 0], [3, 2, 12]),   // bottom rail
	SubBox::new([1, 10, 0], [3, 12, 12]), // top rail
	SubBox::new([1, 2, 0], [3, 10, 2]),   // stile (hinge side)
	SubBox::new([1, 2, 10], [3, 10, 12]), // stile (latch side)
	SubBox::new([1, 2, 5], [3, 10, 7]),   // vertical mullion
	SubBox::new([1, 5, 2], [3, 7, 10]),   // horizontal mullion
];

/// Slab fills by state, indexed `two_thirds | top<<1`: one third
/// (12×12×4) or two thirds (12×12×8), anchored to the cell's floor or to
/// its ceiling. The 4/12 rise is deliberately one stair tread — the
/// auto-step threshold serves both. A ceiling slab is the floor one
/// mirrored about the cell's mid-plane, which is what keeps every pair
/// the same volume (and so the same alchemy cost and the same dig).
const SLAB_BOXES: [SubBox; 4] = [
	SubBox::new([0, 0, 0], [12, 4, 12]),
	SubBox::new([0, 0, 0], [12, 8, 12]),
	SubBox::new([0, 8, 0], [12, 12, 12]),
	SubBox::new([0, 4, 0], [12, 12, 12]),
];

/// The torch: one centered post, 2/12 square and 10/12 tall.
/// Floor-mounted, stateless — wall mounting is a state nibble away when
/// wanted. Small enough that auto-step walks OVER it reads fine.
///
/// 2/12 is as slim as a CENTERED box gets on this grid (an odd width
/// can't straddle the middle), and the head is what came off to get
/// here: it used to flare to 4/12 over the last 3/12 of the height,
/// overhanging the post a twelfth on every side, which read as a mallet
/// rather than a flame. Nothing is lost by dropping it — the flame is
/// the TEXTURE's business, not the silhouette's: `torch_side` paints its
/// top 5/12 of rows as glow, so the same box carries wood below and fire
/// above, and the top face is `TORCH_FLAME` either way.
const TORCH_BOXES: [SubBox; 1] = [SubBox::new([5, 0, 5], [7, 10, 7])];

static TORCHES: LazyLock<[ShapeVariant; 1]> = LazyLock::new(|| {
	let boxes = TORCH_BOXES.to_vec();
	let baked = bake(&boxes);
	[ShapeVariant {
		collision: boxes.clone(),
		interaction: boxes,
		quads: baked.quads,
		face_masks: baked.face_masks,
		occludes_full: baked.occludes_full,
		bounds: baked.bounds,
		ramp: None,
	}]
});

static SLABS: LazyLock<[ShapeVariant; 4]> = LazyLock::new(|| {
	SLAB_BOXES.map(|b| {
		let boxes = vec![b];
		let baked = bake(&boxes);
		ShapeVariant {
			collision: boxes.clone(),
			interaction: boxes,
			quads: baked.quads,
			face_masks: baked.face_masks,
			occludes_full: baked.occludes_full,
			bounds: baked.bounds,
			ramp: None,
		}
	})
});

static STAIRS: LazyLock<[ShapeVariant; 4]> = LazyLock::new(|| {
	Facing::ALL.map(|f| {
		let boxes = rotate_boxes(&STAIR_BOXES, f as u8);
		let baked = bake(&boxes);
		ShapeVariant {
			collision: boxes.clone(),
			interaction: boxes,
			quads: baked.quads,
			face_masks: baked.face_masks,
			occludes_full: baked.occludes_full,
			bounds: baked.bounds,
			ramp: Some(f),
		}
	})
});

static DOORS: LazyLock<[ShapeVariant; 16]> = LazyLock::new(|| {
	std::array::from_fn(|state| {
		let (facing, open, upper) = door_parts(state as u8);
		let k = facing as u8;
		// Closed canonical panel; the open pose swings 90° about the
		// vertical pivot axis through (x, z) = (2, 2) — a true rigid
		// rotation, applied BEFORE the facing rotation so every facing
		// opens with the same handedness.
		let mut panel: Vec<SubBox> = if upper {
			let mut p = DOOR_UPPER_BOXES.to_vec();
			p.push(HINGE_UPPER);
			p
		} else {
			vec![LEAF, HANDLE, HINGE_LOWER]
		};
		// Collision stays the uncut slab (the 1/12 knob is cosmetic to
		// physics); the interact ray also finds the protruding handle.
		let mut slab = vec![LEAF];
		let mut interact = if upper {
			vec![LEAF]
		} else {
			vec![LEAF, HANDLE]
		};
		if open {
			panel = panel.iter().map(swing_open).collect();
			slab = slab.iter().map(swing_open).collect();
			interact = interact.iter().map(swing_open).collect();
		}
		let panel = rotate_boxes(&panel, k);
		let slab = rotate_boxes(&slab, k);
		let interact = rotate_boxes(&interact, k);
		let baked = bake(&panel);
		ShapeVariant {
			// Collision is always the UNCUT leaf (lites don't carve) —
			// and nothing at all while open.
			collision: if open { Vec::new() } else { slab },
			// The interact ray still finds an open door on its swung leaf.
			interaction: interact,
			quads: baked.quads,
			face_masks: baked.face_masks,
			occludes_full: baked.occludes_full,
			bounds: baked.bounds,
			ramp: None,
		}
	})
});

/// Rotate boxes k × 90° CCW about +y: point (x, z) → (12 − z, x).
fn rotate_boxes(boxes: &[SubBox], k: u8) -> Vec<SubBox> {
	let mut out: Vec<SubBox> = boxes.to_vec();
	for _ in 0..(k & 3) {
		out = out
			.iter()
			.map(|b| SubBox {
				min: [GRID - b.max[2], b.min[1], b.min[0]],
				max: [GRID - b.min[2], b.max[1], b.max[0]],
			})
			.collect();
	}
	out
}

/// The door swing: a rigid 90° rotation about the vertical pivot axis
/// through (x, z) = (2, 2) — (x, z) → (z, 4 − x). Valid for door parts
/// only (all live within x 0..4, so images stay inside the cell). The
/// leaf/handle images match the old x↔z mirror (their x-extents are
/// symmetric about 2); the hinge, mounted off-center on the leaf's
/// back, is what makes the rotation visibly rigid.
fn swing_open(b: &SubBox) -> SubBox {
	debug_assert!(b.max[0] <= 4, "door part outside the swing envelope");
	SubBox {
		min: [b.min[2], b.min[1], 4 - b.max[0]],
		max: [b.max[2], b.max[1], 4 - b.min[0]],
	}
}

// --- baking -----------------------------------------------------------

struct Baked {
	quads: Vec<ShapeQuad>,
	face_masks: [FaceMask; 6],
	occludes_full: [bool; 6],
	bounds: SubBox,
}

/// Union AABB of an authored box list, in twelfths. An empty list has no
/// geometry to bound, so it reports the whole cell — nothing draws there
/// anyway, and a zero-size box would be a degenerate outline.
fn union_bounds(boxes: &[SubBox]) -> SubBox {
	let mut min = [GRID; 3];
	let mut max = [0u8; 3];
	for b in boxes {
		for i in 0..3 {
			min[i] = min[i].min(b.min[i]);
			max[i] = max[i].max(b.max[i]);
		}
	}
	if boxes.is_empty() {
		SubBox::new([0; 3], [GRID; 3])
	} else {
		SubBox::new(min, max)
	}
}

#[inline]
fn occ_idx(x: i32, y: i32, z: i32) -> usize {
	((y * G + z) * G + x) as usize
}

fn rasterize(boxes: &[SubBox]) -> Vec<bool> {
	let mut occ = vec![false; (G * G * G) as usize];
	for b in boxes {
		for y in b.min[1]..b.max[1] {
			for z in b.min[2]..b.max[2] {
				for x in b.min[0]..b.max[0] {
					occ[occ_idx(i32::from(x), i32::from(y), i32::from(z))] = true;
				}
			}
		}
	}
	occ
}

/// Filled sub-cell volume of a box union, in twelfths³ (overlap-safe).
pub fn raster_volume(boxes: &[SubBox]) -> u32 {
	rasterize(boxes).iter().map(|&c| u32::from(c)).sum()
}

/// The face's (u, v) frame — mirrors `face_corners` exactly. Maps a
/// face-cell at block-local sub-cell (cx, cy, cz) to its (u, v) CELL
/// coordinates (0..12), and back from (u, v) LATTICE coordinates
/// (0..=12) + the plane coordinate q to a position in twelfths.
#[inline]
fn uv_cell(dir: Dir, c: (i32, i32, i32)) -> (i32, i32) {
	let (x, y, z) = c;
	match dir {
		Dir::West => (G - 1 - z, G - 1 - y),
		Dir::North => (x, G - 1 - y),
		Dir::South => (G - 1 - x, G - 1 - y),
		Dir::East => (z, G - 1 - y),
		Dir::Top => (G - 1 - x, G - 1 - z),
		Dir::Bottom => (G - 1 - x, z),
	}
}

#[inline]
fn uv_pos(dir: Dir, q: i32, u: i32, v: i32) -> [u8; 3] {
	let p = match dir {
		Dir::West => [q, G - v, G - u],
		Dir::North => [u, G - v, q],
		Dir::South => [G - u, G - v, q],
		Dir::East => [q, G - v, u],
		Dir::Top => [G - u, q, G - v],
		Dir::Bottom => [G - u, q, v],
	};
	[p[0] as u8, p[1] as u8, p[2] as u8]
}

/// The neutral tangent-frame axes of a face (`face_ao`'s (u, v) pairs):
/// indices into an [x, y, z] triple.
#[inline]
fn neutral_axes(dir: Dir) -> (usize, usize) {
	match dir {
		Dir::West | Dir::East => (1, 2),
		Dir::Top | Dir::Bottom => (0, 2),
		Dir::North | Dir::South => (0, 1),
	}
}

fn bake(boxes: &[SubBox]) -> Baked {
	let occ = rasterize(boxes);
	let occ_at = |x: i32, y: i32, z: i32| -> bool {
		if !(0..G).contains(&x) || !(0..G).contains(&y) || !(0..G).contains(&z) {
			return false;
		}
		occ[occ_idx(x, y, z)]
	};
	let mut quads = Vec::new();
	let mut face_masks = [FaceMask::EMPTY; 6];
	for dir in DIRS {
		let (ox, oy, oz) = dir.offset();
		// Sub-cell layers along the normal axis; the face plane of layer
		// L is at L (negative dirs) or L+1 (positive dirs).
		for layer in 0..G {
			let positive = ox + oy + oz > 0;
			let q = if positive { layer + 1 } else { layer };
			let boundary = q == 0 || q == G;
			// Exposed faces of this layer as a (u, v) cell grid.
			let mut grid = [[false; GU]; GU];
			let mut any = false;
			for a in 0..G {
				for b in 0..G {
					// (a, b) sweep the two non-normal axes.
					let (x, y, z) = if ox != 0 {
						(layer, a, b)
					} else if oy != 0 {
						(a, layer, b)
					} else {
						(a, b, layer)
					};
					if !occ_at(x, y, z) || occ_at(x + ox, y + oy, z + oz) {
						continue;
					}
					let (u, v) = uv_cell(dir, (x, y, z));
					grid[v as usize][u as usize] = true;
					any = true;
					if boundary {
						let (na, nb) = neutral_axes(dir);
						let p = [x, y, z];
						face_masks[dir as usize].0[p[nb] as usize] |= 1 << p[na];
					}
				}
			}
			if !any {
				continue;
			}
			greedy_rects(&grid, |u0, v0, u1, v1| {
				let corners = [
					uv_pos(dir, q, u0, v0), // TL
					uv_pos(dir, q, u1, v0), // TR
					uv_pos(dir, q, u0, v1), // BL
					uv_pos(dir, q, u1, v1), // BR
				];
				let uv = [
					[u0 as u8, v0 as u8],
					[u1 as u8, v0 as u8],
					[u0 as u8, v1 as u8],
					[u1 as u8, v1 as u8],
				];
				let rect = if boundary {
					let (na, nb) = neutral_axes(dir);
					let mut lo = [G; 3];
					let mut hi = [0; 3];
					for c in corners {
						for i in 0..3 {
							lo[i] = lo[i].min(i32::from(c[i]));
							hi[i] = hi[i].max(i32::from(c[i]));
						}
					}
					[lo[na] as u8, lo[nb] as u8, hi[na] as u8, hi[nb] as u8]
				} else {
					[0; 4]
				};
				quads.push(ShapeQuad {
					dir,
					corners,
					uv,
					boundary,
					rect,
				});
			});
		}
	}
	let occludes_full = std::array::from_fn(|d| face_masks[d].is_full());
	Baked {
		quads,
		face_masks,
		occludes_full,
		bounds: union_bounds(boxes),
	}
}

/// Greedy maximal-rectangle cover of a boolean grid: grow right, then
/// down, then consume. Emits (u0, v0, u1, v1) in LATTICE coords (cell
/// range [u0, u1) × [v0, v1)).
fn greedy_rects(grid: &[[bool; GU]; GU], mut emit: impl FnMut(i32, i32, i32, i32)) {
	let mut used = [[false; GU]; GU];
	for v0 in 0..GU {
		for u0 in 0..GU {
			if !grid[v0][u0] || used[v0][u0] {
				continue;
			}
			let mut u1 = u0 + 1;
			while u1 < GU && grid[v0][u1] && !used[v0][u1] {
				u1 += 1;
			}
			let mut v1 = v0 + 1;
			'rows: while v1 < GU {
				for u in u0..u1 {
					if !grid[v1][u] || used[v1][u] {
						break 'rows;
					}
				}
				v1 += 1;
			}
			for row in used.iter_mut().take(v1).skip(v0) {
				for cell in row.iter_mut().take(u1).skip(u0) {
					*cell = true;
				}
			}
			emit(u0 as i32, v0 as i32, u1 as i32, v1 as i32);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rotation_is_exact_and_cyclic() {
		// 4 × 90° = identity, and each step preserves volume.
		let v0 = raster_volume(&STAIR_BOXES);
		for k in 0..=4u8 {
			let r = rotate_boxes(&STAIR_BOXES, k);
			assert_eq!(raster_volume(&r), v0, "k={k} preserves volume");
		}
		let full = rotate_boxes(&STAIR_BOXES, 4);
		assert_eq!(&full[..], &STAIR_BOXES[..], "4 rotations = identity");
	}

	#[test]
	fn stair_masks_and_boxes() {
		// Canonical stairs ascend +x: the +x back face (Dir::West!) and
		// the bottom are full boundary planes; everything else is partial.
		let v = shape_of(BlockType::WoodStairs, stair_state(Facing::PosX)).unwrap();
		assert!(v.occludes_full[Dir::West as usize], "+x back face full");
		assert!(v.occludes_full[Dir::Bottom as usize], "bottom is full");
		assert!(!v.occludes_full[Dir::East as usize]);
		assert!(!v.occludes_full[Dir::Top as usize]);
		assert!(!v.occludes_full[Dir::North as usize]);
		assert!(!v.occludes_full[Dir::South as usize]);
		assert_eq!(v.collision.len(), 3);
		assert_eq!(v.ramp, Some(Facing::PosX));
		// The −x boundary is the first riser's face (y 0..4, all z).
		let m = &v.face_masks[Dir::East as usize];
		assert!(m.covers(0, 0, 4, 12), "riser face covers y 0..4 (all z)");
		assert!(!m.covers(4, 0, 5, 1), "nothing above y=4 on −x");
	}

	#[test]
	fn every_facing_bakes_and_stays_under_the_quad_cap() {
		let mut max = 0;
		for f in Facing::ALL {
			let v = shape_of(BlockType::StoneStairs, stair_state(f)).unwrap();
			assert!(!v.quads.is_empty());
			max = max.max(v.quads.len());
		}
		for state in 0..16u8 {
			let v = shape_of(BlockType::WoodDoor, state).unwrap();
			assert!(!v.quads.is_empty());
			max = max.max(v.quads.len());
		}
		for state in 0..4u8 {
			let v = shape_of(BlockType::WoodSlab, state).unwrap();
			assert!(!v.quads.is_empty());
			max = max.max(v.quads.len());
		}
		{
			let v = shape_of(BlockType::Torch, 0).unwrap();
			assert!(!v.quads.is_empty());
			assert_eq!(v.ramp, None);
			assert!(
				v.occludes_full.iter().all(|f| !f),
				"a torch hides no neighbor face"
			);
			max = max.max(v.quads.len());
		}
		assert!(
			max <= MAX_SHAPE_QUADS,
			"worst variant emits {max} quads (cap {MAX_SHAPE_QUADS})"
		);
	}

	/// A slab is one box the height its size bit says, flush against the
	/// end its anchor bit says — and the plane it is flush against is
	/// exactly the one boundary it fully covers, which is what lets a
	/// ceiling slab hide the face above it the way a floor slab hides
	/// the one below.
	#[test]
	fn slab_variants() {
		for two_thirds in [false, true] {
			for top in [false, true] {
				let h = if two_thirds { 8 } else { 4 };
				let v = shape_of(BlockType::WoodSlab, slab_state(two_thirds, top)).unwrap();
				assert_eq!(v.collision.len(), 1);
				let want = if top {
					SubBox::new([0, 12 - h, 0], [12, 12, 12])
				} else {
					SubBox::new([0, 0, 0], [12, h, 12])
				};
				assert_eq!(v.collision[0], want, "two_thirds={two_thirds} top={top}");
				assert_eq!(v.interaction, v.collision);
				assert_eq!(v.ramp, None, "slabs are boxes, never ramps");
				// A plain box: 6 quads, only the anchored plane fully covered.
				assert_eq!(v.quads.len(), 6);
				let (closed, open) = if top {
					(Dir::Top, Dir::Bottom)
				} else {
					(Dir::Bottom, Dir::Top)
				};
				assert!(v.occludes_full[closed as usize]);
				for d in [open, Dir::West, Dir::East, Dir::North, Dir::South] {
					assert!(!v.occludes_full[d as usize], "{d:?} partial or open");
				}
			}
		}
	}

	/// The two anchors are mirror images: same volume, same alchemy
	/// cost, same dig — a ceiling slab is not a second, cheaper item.
	#[test]
	fn a_ceiling_slab_is_a_floor_slab_mirrored() {
		for two_thirds in [false, true] {
			let bottom = SLAB_BOXES[slab_state(two_thirds, false) as usize];
			let top = SLAB_BOXES[slab_state(two_thirds, true) as usize];
			assert_eq!(bottom.min[1], 12 - top.max[1]);
			assert_eq!(bottom.max[1], 12 - top.min[1]);
			assert_eq!(raster_volume(&[bottom]), raster_volume(&[top]));
		}
	}

	/// The crosshair outline's box. Two properties, both load-bearing:
	/// it never clips the drawn geometry (every authored box is inside
	/// it), and it is TIGHT, so a slab and a door read as their own size
	/// instead of as the cell they sit in. A stair is the deliberate
	/// no-op — the full cube already is its box of best fit.
	#[test]
	fn draw_bounds_are_the_tight_box_around_the_drawn_geometry() {
		use crate::chunk::make_cell;
		let full = SubBox::new([0, 0, 0], [12, 12, 12]);
		assert_eq!(
			draw_bounds(BlockType::Stone as u16),
			None,
			"a full cube has no box to shrink to"
		);
		for f in Facing::ALL {
			assert_eq!(
				draw_bounds(make_cell(BlockType::StoneStairs, stair_state(f))),
				Some(full),
				"a stair fills its cell corner to corner"
			);
		}
		// Slabs: thirds anchored to one end, so only the other shrinks.
		for (two_thirds, h) in [(false, 4u8), (true, 8)] {
			assert_eq!(
				draw_bounds(make_cell(
					BlockType::WoodSlab,
					slab_state(two_thirds, false)
				)),
				Some(SubBox::new([0, 0, 0], [12, h, 12]))
			);
			assert_eq!(
				draw_bounds(make_cell(BlockType::WoodSlab, slab_state(two_thirds, true))),
				Some(SubBox::new([0, 12 - h, 0], [12, 12, 12]))
			);
		}
		// A closed door is a plate: 4 twelfths deep on its facing axis
		// (the leaf plus the knob protruding either side), full in the
		// other two. Depth lands on x for a ±x facing, z for ±z.
		for f in Facing::ALL {
			for upper in [false, true] {
				let b = draw_bounds(make_cell(BlockType::WoodDoor, door_state(f, false, upper)))
					.unwrap();
				let span = |i: usize| b.max[i] - b.min[i];
				let (thin, wide) = match f {
					Facing::PosX | Facing::NegX => (0, 2),
					Facing::PosZ | Facing::NegZ => (2, 0),
				};
				assert!(span(thin) <= 4, "{f:?} upper={upper}: {b:?} is a plate");
				assert_eq!(span(1), 12, "{f:?} upper={upper}: full height");
				assert_eq!(span(wide), 12, "{f:?} upper={upper}: full width");
			}
		}
		// Nothing drawn may fall outside the box, in any variant.
		for (b, states) in [
			(BlockType::StoneStairs, 4u8),
			(BlockType::WoodDoor, 16),
			(BlockType::WoodSlab, 4),
		] {
			for state in 0..states {
				let v = shape_of(b, state).unwrap();
				for q in &v.quads {
					for c in q.corners {
						for i in 0..3 {
							assert!(
								c[i] >= v.bounds.min[i] && c[i] <= v.bounds.max[i],
								"{b:?} state {state}: quad corner {c:?} outside {:?}",
								v.bounds
							);
						}
					}
				}
			}
		}
	}

	/// Growing and shrinking move the free end and never the anchor: a
	/// ceiling slab stays on its ceiling all the way to the full block
	/// and back down to air.
	#[test]
	fn slab_grow_shrink_roundtrip() {
		use crate::chunk::make_cell;
		for (slab, base) in [
			(BlockType::WoodSlab, BlockType::Wood),
			(BlockType::StoneSlab, BlockType::Stone),
		] {
			for top in [false, true] {
				let third = make_cell(slab, slab_state(false, top));
				let two = make_cell(slab, slab_state(true, top));
				assert_eq!(slab_grown(third), two);
				assert_eq!(slab_grown(two), base as u16, "third grow → base block");
				assert_eq!(slab_shrunk(two), third);
				assert_eq!(slab_shrunk(third), BlockType::Air as u16);
				assert_eq!(slab_is_top(third), top);
				assert_eq!(slab_is_top(two), top);
			}
			assert_eq!(slab.slab_base(), Some(base));
		}
	}

	/// A slab grows from the face it does NOT sit flush against, and a
	/// fresh one anchors to whatever it was built against. Together
	/// those two make the ceiling case the exact mirror of the floor
	/// case, which is why one grow rule serves both.
	#[test]
	fn slabs_grow_from_their_open_face_and_anchor_to_the_face_placed_against() {
		use crate::chunk::make_cell;
		let floor = make_cell(BlockType::WoodSlab, slab_state(false, false));
		let ceiling = make_cell(BlockType::WoodSlab, slab_state(false, true));
		assert_eq!(slab_grow_normal(floor), (0, 1, 0));
		assert_eq!(slab_grow_normal(ceiling), (0, -1, 0));
		// The anchor bit survives the grow, so a 2/3 grows the same way.
		assert_eq!(slab_grow_normal(slab_grown(ceiling)), (0, -1, 0));
		// Placed against a ceiling → hangs; against a floor or a wall →
		// sits. A wall slab has no up/down cue, so it sits, as before.
		assert!(slab_is_top(make_cell(
			BlockType::WoodSlab,
			slab_place_state((0, -1, 0))
		)));
		for n in [(0, 1, 0), (1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1)] {
			assert!(!slab_is_top(make_cell(
				BlockType::WoodSlab,
				slab_place_state(n)
			)));
		}
	}

	#[test]
	fn door_variants() {
		for f in Facing::ALL {
			for upper in [false, true] {
				let closed = shape_of(BlockType::WoodDoor, door_state(f, false, upper)).unwrap();
				let open = shape_of(BlockType::WoodDoor, door_state(f, true, upper)).unwrap();
				// Closed: one uncut leaf box, set back a twelfth from the
				// boundary — no face of a door ever fully covers a
				// boundary plane. The interact set adds the handle knob on
				// the lower half. Open: no collision at all, but the
				// swung leaf (and handle) stays clickable.
				assert_eq!(closed.collision.len(), 1);
				let boxes = if upper { 1 } else { 2 };
				assert_eq!(closed.interaction.len(), boxes, "upper={upper}");
				assert_eq!(closed.interaction[0], closed.collision[0]);
				assert!(closed.occludes_full.iter().all(|&f| !f), "upper={upper}");
				assert!(open.collision.is_empty());
				assert_eq!(open.interaction.len(), boxes);
				assert_ne!(
					open.interaction, closed.interaction,
					"open leaf is the swung pose"
				);
			}
		}
		// The upper closed panel has see-through lites: strictly fewer
		// filled sub-cells than the uncut lower leaf; the handle adds its
		// two protruding twelfths and each half carries a 1×2×1 hinge on
		// the leaf's back at the pivot edge.
		let lower = raster_volume(&[LEAF, HANDLE, HINGE_LOWER]);
		let mut up_boxes = DOOR_UPPER_BOXES.to_vec();
		up_boxes.push(HINGE_UPPER);
		let upper = raster_volume(&up_boxes);
		assert_eq!(raster_volume(&[LEAF]), 288);
		// Leaf 288 + handle knob 2×2 poking 1 out each side + hinge 2.
		assert_eq!(lower, 298);
		assert!(upper < lower, "lites carved: {upper} < {lower}");
		// Four 3×3 lites, 2 deep: 288 − 4·9·2 = 216; + hinge 2.
		assert_eq!(upper, 218);
	}

	#[test]
	fn full_cube_bakes_to_the_six_block_faces() {
		// A debug full-cube shape must reproduce the mesher's full-block
		// convention exactly: 6 quads, corner UVs (0,0)/(12,0)/(0,12)/
		// (12,12), corners on the block lattice.
		let baked = bake(&[SubBox::new([0, 0, 0], [12, 12, 12])]);
		assert_eq!(baked.quads.len(), 6);
		for q in &baked.quads {
			assert!(q.boundary);
			assert_eq!(q.uv, [[0, 0], [12, 0], [0, 12], [12, 12]]);
			for c in q.corners {
				for a in c {
					assert!(a == 0 || a == 12, "lattice corner: {c:?}");
				}
			}
			assert_eq!(q.rect, [0, 0, 12, 12]);
		}
		assert!(baked.occludes_full.iter().all(|&f| f));
	}

	#[test]
	fn craft_costs_derive_from_collision_volume() {
		for b in CRAFTABLE {
			let r = craft_recipe(b).unwrap();
			// Cost = ceil(filled volume / 64), from the canonical closed
			// geometry; doors price BOTH halves (uncut leaves — lites
			// don't discount) plus the handle and both hinges.
			let volume = match b.props().shape.unwrap() {
				ShapeKind::Stairs => raster_volume(&STAIR_BOXES) * r.count,
				ShapeKind::Door => {
					(raster_volume(&[LEAF, HANDLE, HINGE_LOWER])
						+ raster_volume(&[LEAF, HINGE_UPPER]))
						* r.count
				}
				// Cost prices the PLACED 1/3 item; grows debit one item each.
				ShapeKind::Slab => raster_volume(&SLAB_BOXES[..1]) * r.count,
				// Functional items are priced by hand (§7.3), never by
				// volume — the torch has no `craft_recipe` row at all.
				ShapeKind::Torch => unreachable!("torches are priced by hand"),
			};
			assert_eq!(
				r.cost,
				volume.div_ceil(SUB_UNIT_VOLUME),
				"{b:?} cost matches volume {volume}"
			);
			assert_eq!(r.count, 1, "launch shapes craft singly");
		}
		assert_eq!(craft_recipe(BlockType::Stone), None);
		assert_eq!(SUBS_PER_BLOCK * SUB_UNIT_VOLUME, 1728, "exact division");
	}

	#[test]
	fn state_validity() {
		use crate::chunk::make_cell;
		// Stateless blocks: zero only.
		assert!(state_valid(BlockType::Stone as u16));
		assert!(!state_valid(make_cell(BlockType::Stone, 1)));
		// Stairs: facing only.
		for f in Facing::ALL {
			assert!(state_valid(make_cell(
				BlockType::WoodStairs,
				stair_state(f)
			)));
		}
		assert!(!state_valid(make_cell(BlockType::WoodStairs, 0b0100)));
		// Doors: the whole nibble decodes.
		for s in 0..16u8 {
			assert!(state_valid(make_cell(BlockType::WoodDoor, s)));
		}
		// Slabs: size bit 0 and anchor bit 1.
		for b in [BlockType::WoodSlab, BlockType::StoneSlab] {
			for s in 0..4u8 {
				assert!(state_valid(make_cell(b, s)));
			}
			assert!(!state_valid(make_cell(b, 0b0100)));
			assert!(!state_valid(make_cell(b, 0b1000)));
		}
		// Unknown id, and spare bits 10-11, always refuse.
		assert!(!state_valid(999));
		assert!(!state_valid(BlockType::Stone as u16 | 1 << 10));
		assert!(!state_valid(BlockType::WoodStairs as u16 | 1 << 11));
	}

	#[test]
	fn interior_quads_exist_and_boundary_rects_are_sane() {
		// A stair has interior riser faces (non-boundary) and boundary
		// treads; every boundary rect must be a valid neutral-frame rect.
		let v = shape_of(BlockType::WoodStairs, 0).unwrap();
		assert!(v.quads.iter().any(|q| !q.boundary), "risers are interior");
		assert!(v.quads.iter().any(|q| q.boundary));
		for q in &v.quads {
			if q.boundary {
				let [a0, b0, a1, b1] = q.rect;
				assert!(a0 < a1 && a1 <= GRID && b0 < b1 && b1 <= GRID, "{q:?}");
			}
		}
	}

	#[test]
	fn baking_is_deterministic() {
		let a = bake(&STAIR_BOXES);
		let b = bake(&STAIR_BOXES);
		assert_eq!(a.quads.len(), b.quads.len());
		for (qa, qb) in a.quads.iter().zip(&b.quads) {
			assert_eq!(qa.corners, qb.corners);
			assert_eq!(qa.uv, qb.uv);
		}
	}
}
