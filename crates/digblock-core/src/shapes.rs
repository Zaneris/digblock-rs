//! Sub-block shape registry: static geometry per (block id, state nibble) on a
//! cubic sub-grid (`GRID` sub-cells per block edge, chosen so halves, thirds,
//! quarters and sixths are all exact). Shapes are authored once as sub-box
//! lists in a canonical facing and every variant is baked from them at first
//! use — rasterize to a bitset, emit exposed sub-faces, greedy-merge into
//! maximal quads — all pure integer ops, so the registry is deterministic
//! across platforms and both sides of the wire.
//!
//! Layering: above block.rs/dir.rs, below the mesher. Corner-slot and UV
//! conventions deliberately mirror the mesher's `face_corners`, so a baked
//! full-cube face is bit-identical to the mesher's full-block output — pinned
//! here by `full_cube_bakes_to_the_six_block_faces` and cross-checked in
//! mesher.rs.

use crate::block::{BlockType, ShapeKind};
use crate::chunk::{STATE_MASK, STATE_SHIFT, cell_id, cell_state};
use crate::dir::{DIRS, Dir};
use glam::Vec3;
use std::sync::LazyLock;

/// Sub-cells per block edge.
pub const GRID: u8 = 12;
const G: i32 = GRID as i32;
const GU: usize = GRID as usize;

/// Ceiling on any variant's baked quad count — sizes the renderer's shared
/// index buffer (`MAX_QUADS`); pinned per-variant by a test here. Worst
/// today: the upper door half (lites + hinge).
pub const MAX_SHAPE_QUADS: usize = 48;

/// Sub-units one whole block converts into — an even division into thirds
/// on each axis.
pub const SUBS_PER_BLOCK: u32 = 27;
/// Filled twelfths³ per sub-unit: the volume of one cube of that division.
pub const SUB_UNIT_VOLUME: u32 = 1728 / SUBS_PER_BLOCK;

// --- state nibble -----------------------------------------------------

/// Horizontal facing, axis-named (compass names are a trap here:
/// `Dir::West` is +x). Values are the k in "k × 90° CCW-about-+y
/// rotations of the canonical +x shape" — and the state nibble's low bits.
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
	/// door's leaf sits set back from the cell face it points at, leaving
	/// room for the handle's protrusion).
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

/// Stairs: state = facing only (the rest of the nibble stays clear on the
/// wire — `state_valid` enforces it; decode is forgiving and masks).
#[inline]
pub fn stair_state(f: Facing) -> u8 {
	f as u8
}

/// Door: facing, then the open flag, then the upper flag — exactly fills
/// the nibble.
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

/// The door cell with its open flag flipped, in place inside the state
/// nibble.
#[inline]
pub fn door_toggled(cell: u16) -> u16 {
	cell ^ (0b0100 << STATE_SHIFT)
}

/// Slab: the size bit set = two-thirds fill (clear = one third), then the
/// anchor bit set = anchored to the cell's ceiling instead of its floor.
/// The rest of the nibble stays clear — `state_valid` enforces it.
#[inline]
pub fn slab_state(two_thirds: bool, top: bool) -> u8 {
	u8::from(two_thirds) | (u8::from(top) << 1)
}

/// Whether a slab cell hangs from its ceiling (the anchor bit).
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
/// still carries the slab in its canonical default state.
#[inline]
pub fn slab_place_state(normal: (i32, i32, i32)) -> u8 {
	slab_state(false, normal.1 < 0)
}

/// A slab cell one third taller: one third → two thirds → the plain base
/// block.
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

/// A slab cell one third shorter: two thirds → one third → Air. The
/// dig-shrink
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

/// Axis-aligned sub-box in twelfths, min inclusive and max exclusive, in
/// sub-grid coordinates.
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

/// Sub-grid coverage of one block-boundary plane, in the face's NEUTRAL
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
		ShapeKind::Table => Some(&TABLES[0]),
		ShapeKind::Chair => Some(&CHAIRS[(state & 3) as usize]),
		ShapeKind::Bed => Some(&BEDS[(state & 3) as usize]),
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
/// physics test or the alchemy screen pays for the whole family — well
/// under a millisecond natively, several times that on wasm — in whatever
/// frame happens to touch it first. That is a guaranteed one-frame hitch at an arbitrary moment of
/// gameplay, so spend it where a pause is already expected instead. Warm
/// calls are single-digit nanoseconds, so this is purely about WHEN.
pub fn prewarm() {
	let _ = LazyLock::force(&STAIRS);
	let _ = LazyLock::force(&DOORS);
	let _ = LazyLock::force(&SLABS);
	let _ = LazyLock::force(&TORCHES);
	let _ = LazyLock::force(&TABLES);
	let _ = LazyLock::force(&CHAIRS);
	let _ = LazyLock::force(&BEDS);
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

/// The cell a whole OBJECT is anchored to, plus the box of best fit around all
/// of it, in twelfths **of that anchor cell** — so a door hands back its LOWER
/// half and a box two cells tall, and every single-cell block hands back itself
/// and [`draw_bounds`]. **The caller must not assume the box fits inside one
/// cell.**
///
/// This is the crosshair outline's box, and it exists because a door is one
/// object wearing two cells: boxing the half under the crosshair drew a frame
/// around whichever panel you aimed at, which is not the thing the click acts
/// on — dig either half and both go.
///
/// The partner's geometry is DERIVED (same id, facing and open bit, other half)
/// rather than read out of the world, exactly as [`door_partner`] derives its
/// cell, so this stays a pure function of one cell value. The union is a real
/// union, not the lower half stretched: the handle protrudes past the leaf and
/// the lites do not.
pub fn object_bounds(cell: (i32, i32, i32), value: u16) -> ((i32, i32, i32), Option<SubBox>) {
	let b = BlockType::from_id(cell_id(value));
	if !b.is_door() {
		return (cell, draw_bounds(value));
	}
	let state = cell_state(value);
	let (facing, open, upper) = door_parts(state);
	let half = |up: bool| shape_of(b, door_state(facing, open, up)).map(|v| v.bounds);
	let (Some(lower), Some(top)) = (half(false), half(true)) else {
		return (cell, draw_bounds(value));
	};
	// Both halves in the anchor's frame: the lower at its own height, the
	// upper one whole cell up.
	let mut u = lower;
	for a in 0..3 {
		let lift = if a == 1 { GRID } else { 0 };
		u.min[a] = u.min[a].min(top.min[a] + lift);
		u.max[a] = u.max[a].max(top.max[a] + lift);
	}
	let anchor = if upper {
		door_partner(cell, state)
	} else {
		cell
	};
	(anchor, Some(u))
}

/// Whether a cell takes its per-block tint from the cell BELOW instead of
/// its own: the upper half of a door, so a two-cell leaf wears ONE tint
/// and reads as one object rather than as two planks sawn from different
/// timber. Rides the mesh as a flag in the solid material word — see
/// `mesher::solid_word1` and chunk.wgsl's `block_tint`.
///
/// A cell, not a block type, because it is the door's *half* that decides
/// — and it is deliberately a hash-anchor shift rather than a "doors are
/// exempt" rule, so the leaf still varies from the door next to it.
#[inline]
pub fn tint_from_below(cell: u16) -> bool {
	BlockType::from_id(cell_id(cell)).is_door() && door_parts(cell_state(cell)).2
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
		Some(ShapeKind::Torch) | Some(ShapeKind::Table) => state == 0,
		Some(ShapeKind::Chair) | Some(ShapeKind::Bed) => state & !3 == 0,
	}
}

// --- alchemy ----------------------------------------------------------

/// How many items one craft produces (the essence redesign's
/// feedstock-and-catalyst shape). A feedstock recipe consumes ONE cube of
/// matter, so it yields as many of the shape as the cube's volume cuts
/// into — floored, the remainder being the waste of the cut — which is
/// what makes matter conservation structural: a craft can only ever
/// output at most the volume it consumed. Essence-only recipes buy one
/// at a time, exactly as they always did.
pub fn craft_batch(b: BlockType) -> u32 {
	match b.grid_cell() {
		Some((m, s)) if m.feedstock().is_some() => (SUBS_PER_BLOCK / s.units()).max(1),
		_ => 1,
	}
}

// --- authored geometry ------------------------------------------------

/// Canonical stairs, ascending toward +x: three plain steps of equal
/// rise, each tread running the full depth of the cell. (Two
/// nosing/reveal detail passes were tried and
/// reverted: an overhanging tread lip reads asymmetric — the top step
/// has nothing overhanging it — and an inset-tread reveal didn't look
/// right either. Plain steps it is.)
const STAIR_BOXES: [SubBox; 3] = [
	SubBox::new([0, 0, 0], [12, 4, 12]),
	SubBox::new([4, 4, 0], [12, 8, 12]),
	SubBox::new([8, 8, 0], [12, 12, 12]),
];

/// Canonical closed leaf (facing +x): a thin slab set back from the −x
/// face, leaving room for the handle to protrude past either leaf face,
/// covering the cell's whole elevation.
const LEAF: SubBox = SubBox::new([1, 0, 0], [3, 12, 12]);

/// Door handle (lower half only): a small square knob through the leaf
/// near the latch stile, protruding past each leaf face.
const HANDLE: SubBox = SubBox::new([0, 9, 9], [4, 11, 11]);

/// Door hinges, one per half, mounted on the BACK of the closed leaf at
/// the pivot edge. They ride the leaf through [`swing_open`] — the open
/// pose lands them in the cell's corner, bridging the setback gap to
/// whatever the open door hangs against — so the door and its knuckles
/// read as one rigid rotation.
const HINGE_LOWER: SubBox = SubBox::new([3, 1, 0], [4, 3, 1]);
const HINGE_UPPER: SubBox = SubBox::new([3, 9, 0], [4, 11, 1]);

/// Canonical upper-half panel: a divided-lite window laid out the same
/// way in both axes (width = z here) — rails, stiles, and a cross mullion
/// leaving four square lites. Overlaps are fine; the raster unions them.
const DOOR_UPPER_BOXES: [SubBox; 6] = [
	SubBox::new([1, 0, 0], [3, 2, 12]),   // bottom rail
	SubBox::new([1, 10, 0], [3, 12, 12]), // top rail
	SubBox::new([1, 2, 0], [3, 10, 2]),   // stile (hinge side)
	SubBox::new([1, 2, 10], [3, 10, 12]), // stile (latch side)
	SubBox::new([1, 2, 5], [3, 10, 7]),   // vertical mullion
	SubBox::new([1, 5, 2], [3, 7, 10]),   // horizontal mullion
];

/// Slab fills by state, indexed by the size bit and then the anchor bit:
/// one third or two thirds of the cell's height, anchored to its floor or
/// to its ceiling. The one-third rise is deliberately one stair tread —
/// the auto-step threshold serves both. A ceiling slab is the floor one
/// mirrored about the cell's mid-plane, which is what keeps every pair
/// the same volume (and so the same alchemy cost and the same dig).
const SLAB_BOXES: [SubBox; 4] = [
	SubBox::new([0, 0, 0], [12, 4, 12]),
	SubBox::new([0, 0, 0], [12, 8, 12]),
	SubBox::new([0, 8, 0], [12, 12, 12]),
	SubBox::new([0, 4, 0], [12, 12, 12]),
];

/// The torch: one centered post, square in section and stopping short of the
/// cell's top. Floor-mounted, stateless — wall mounting is a state nibble away
/// when wanted. Small enough that auto-step walking OVER it reads fine.
///
/// The post is as slim as a CENTERED box gets on this grid (an odd width can't
/// straddle the middle), and it carries no head: a flare over the top read as a
/// mallet rather than a flame, and the flame is the TEXTURE's business anyway —
/// `torch_side` paints its upper rows as glow, so one box carries wood below
/// and fire above.
const TORCH_BOXES: [SubBox; 1] = [SubBox::new([5, 0, 5], [7, 10, 7])];

/// A table: a full-footprint top on four corner legs.
/// Stateless — a square top has no facing to lose, and it means tables
/// pushed together read as one continuous surface rather than a grid of
/// rotations that don't line up.
///
/// The top is deliberately the cell's whole footprint, not inset: an
/// inset top
/// leaves a visible gap between adjacent tables at exactly the seam a
/// player builds a long table across.
const TABLE_BOXES: [SubBox; 5] = [
	SubBox::new([0, 10, 0], [12, 12, 12]),
	SubBox::new([1, 0, 1], [3, 10, 3]),
	SubBox::new([9, 0, 1], [11, 10, 3]),
	SubBox::new([1, 0, 9], [3, 10, 11]),
	SubBox::new([9, 0, 9], [11, 10, 11]),
];

/// A chair, canonically BACKED toward +x: seat at mid-height, back rising
/// from it, four legs. Placing uses the look direction unrotated, so a chair
/// dropped while facing +x has its back away and its seat turned toward
/// the placer — which is what you want when setting one at a table.
///
/// The seat sits above `AUTO_STEP_MAX`, so a walk does not climb onto
/// furniture; you jump onto a chair like anything else waist-high.
const CHAIR_BOXES: [SubBox; 6] = [
	SubBox::new([2, 6, 2], [10, 8, 10]),
	SubBox::new([8, 8, 2], [10, 12, 10]),
	SubBox::new([2, 0, 2], [4, 6, 4]),
	SubBox::new([8, 0, 2], [10, 6, 4]),
	SubBox::new([2, 0, 8], [4, 6, 10]),
	SubBox::new([8, 0, 8], [10, 6, 10]),
];

/// A bed, canonically HEADED toward +x: a mattress on a low frame with a
/// raised pillow at the head end, so the facing is readable from across a
/// room rather than only up close.
///
/// One cell, not two. A two-cell frame is the obvious next version, but a
/// horizontal pair is genuinely new machinery — the door's pair is
/// VERTICAL, and `door_partner`, `object_bounds`, the place path's derived
/// second cell and the server's assembled `writes` all special-case that
/// axis. A cot is a real object today; the long frame can arrive with the
/// feature that would justify the cost (setting your spawn), which is when
/// somebody actually has to answer where a sleeper wakes up.
const BED_BOXES: [SubBox; 6] = [
	SubBox::new([0, 3, 1], [12, 6, 11]),
	SubBox::new([8, 6, 2], [11, 8, 10]),
	SubBox::new([0, 0, 1], [2, 3, 3]),
	SubBox::new([0, 0, 9], [2, 3, 11]),
	SubBox::new([10, 0, 1], [12, 3, 3]),
	SubBox::new([10, 0, 9], [12, 3, 11]),
];

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

static TABLES: LazyLock<[ShapeVariant; 1]> = LazyLock::new(|| [furniture(&TABLE_BOXES, 0)]);

static CHAIRS: LazyLock<[ShapeVariant; 4]> =
	LazyLock::new(|| Facing::ALL.map(|f| furniture(&CHAIR_BOXES, f as u8)));

static BEDS: LazyLock<[ShapeVariant; 4]> =
	LazyLock::new(|| Facing::ALL.map(|f| furniture(&BED_BOXES, f as u8)));

/// One furniture variant: rotate the canonical boxes, bake, and let
/// collision and the interact ray share them. Furniture has no swung
/// pose and no ramp — the whole family is "solid where it is drawn".
fn furniture(canonical: &[SubBox], k: u8) -> ShapeVariant {
	let boxes = rotate_boxes(canonical, k);
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
}

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
		// leaf's vertical pivot axis — a true rigid rotation, applied
		// BEFORE the facing rotation so every facing opens with the same
		// handedness.
		let mut panel: Vec<SubBox> = if upper {
			let mut p = DOOR_UPPER_BOXES.to_vec();
			p.push(HINGE_UPPER);
			p
		} else {
			vec![LEAF, HANDLE, HINGE_LOWER]
		};
		// Collision stays the uncut slab (the knob is cosmetic to
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

/// Rotate boxes k × 90° CCW about +y: point (x, z) → (GRID − z, x).
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

/// The door swing: a rigid 90° rotation about the leaf's vertical pivot
/// axis, in the corner of the cell the hinge stands in. Valid for door
/// parts only — they all live inside the swing envelope on x, so their
/// images stay inside the cell (the `debug_assert` below IS that
/// envelope). The leaf/handle images match the old x↔z mirror, their
/// x-extents being symmetric about the pivot; the hinge, mounted
/// off-center on the leaf's back, is what makes the rotation visibly
/// rigid.
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
/// coordinates, and back from (u, v) LATTICE coordinates (one more per
/// axis than there are cells) + the plane coordinate q to a position in
/// twelfths.
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
mod tests;
