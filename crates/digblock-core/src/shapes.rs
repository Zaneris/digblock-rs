//! Sub-block shape registry: static geometry per (block id, state nibble)
//! on a cubic sub-grid (`GRID` sub-cells per block edge, chosen so halves,
//! thirds, quarters and sixths are all exact). Shapes are authored once as
//! sub-box lists in a
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

use crate::block::{BlockType, ShapeKind};
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

/// The cell a whole OBJECT is anchored to, plus the box of best fit
/// around all of it, in twelfths **of that anchor cell** — so a door
/// hands back its LOWER half and a box two cells tall, and every
/// single-cell block hands back itself and [`draw_bounds`].
/// The caller must not assume the box fits inside one cell.
///
/// This is the crosshair outline's box, and it exists because a door is
/// one object wearing two cells: boxing the half under the crosshair
/// drew a frame around whichever panel you happened to aim at, which is
/// not the thing the click acts on — dig either half and both go.
///
/// The partner's geometry is DERIVED (same id, same facing, same open
/// bit, other half) rather than read out of the world, exactly as
/// [`door_partner`] derives its cell: the pair is written in one tick and
/// replayed per-cell LWW, so the two halves cannot disagree about which
/// variant they are, and this stays a pure function of one cell value.
///
/// The union is a real union, not the lower half stretched: the handle
/// protrudes past the leaf and the lites do not, so the two halves have
/// different x extents and only one of them reaches the box's edge.
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

/// How many items one craft produces. Batched so a fractional-unit shape
/// stays exact (a future half-slab, worth half a sub-unit more than a
/// whole number, would craft two at a time out of one block's worth of
/// essence); every shape authored so far is whole, so the batch is one
/// throughout.
///
/// The COST is not here: it is the shape's volume
/// ([`crate::block::ShapeFamily::units`]) prorated by the material's
/// essence split (`crate::material`), which is what makes a new
/// material × shape pair a block row rather than another table entry.
pub fn craft_batch(_b: BlockType) -> u32 {
	1
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

/// The torch: one centered post, square in section and stopping short of
/// the cell's top. Floor-mounted, stateless — wall mounting is a state
/// nibble away when wanted. Small enough that auto-step walks OVER it
/// reads fine.
///
/// The post is as slim as a CENTERED box gets on this grid (an odd width
/// can't straddle the middle), and the head is what came off to get
/// here: it used to flare to twice that width over the top of the post,
/// overhanging it on every side, which read as a mallet rather than a
/// flame. Nothing is lost by dropping it — the flame is the TEXTURE's
/// business, not the silhouette's: `torch_side` paints its upper rows as
/// glow, so the same box carries wood below and fire above, and the top
/// face is `TORCH_FLAME` either way.
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
		// The −x boundary is only the bottom step's rise, across all z.
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
			let v = shape_of(BlockType::LumberDoor, state).unwrap();
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
				// A plain box: one quad per face, only the anchored plane
				// fully covered.
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
		// A closed door is a plate: shallow on its facing axis (the leaf
		// plus the knob protruding either side), full in the other two.
		// Depth lands on x for a ±x facing, z for ±z.
		for f in Facing::ALL {
			for upper in [false, true] {
				let b = draw_bounds(make_cell(
					BlockType::LumberDoor,
					door_state(f, false, upper),
				))
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
			(BlockType::LumberDoor, 16),
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

	/// A door is one object across two cells, so `object_bounds` answers
	/// the same anchor and the same box from either half: the pair's lower
	/// cell, and a box two cells tall.
	///
	/// The union has to be a real union rather than the lower half
	/// stretched — the handle protrudes past the leaf on the lower half
	/// and the lites do not, so it is the LOWER half that sets the thin
	/// axis and the UPPER half's box does not reach it.
	#[test]
	fn a_door_bounds_as_one_object() {
		use crate::chunk::make_cell;
		let lower = (3, 40, 7);
		let upper = (lower.0, lower.1 + 1, lower.2);
		for f in Facing::ALL {
			for open in [false, true] {
				let at = |c, up| {
					object_bounds(c, make_cell(BlockType::LumberDoor, door_state(f, open, up)))
				};
				let a = at(lower, false);
				let b = at(upper, true);
				assert_eq!(a, b, "{f:?} open={open}: both halves, one object");
				let (anchor, bounds) = a;
				assert_eq!(anchor, lower, "anchored on the lower half");
				let u = bounds.unwrap();
				assert_eq!((u.min[1], u.max[1]), (0, 24), "two cells tall");
				// Every part of both halves is inside the union.
				for up in [false, true] {
					let half =
						draw_bounds(make_cell(BlockType::LumberDoor, door_state(f, open, up)))
							.unwrap();
					let lift = u8::from(up) * GRID;
					for i in 0..3 {
						let (mn, mx) = if i == 1 {
							(half.min[1] + lift, half.max[1] + lift)
						} else {
							(half.min[i], half.max[i])
						};
						assert!(
							mn >= u.min[i] && mx <= u.max[i],
							"{f:?} open={open} upper={up}: {half:?} escapes {u:?} on axis {i}"
						);
					}
				}
			}
		}
	}

	/// Everything that is not a door is its own object: same cell, same
	/// box `draw_bounds` already gave.
	#[test]
	fn a_single_cell_block_is_its_own_object() {
		use crate::chunk::make_cell;
		let c = (3, 40, 7);
		for v in [
			BlockType::Stone as u16,
			make_cell(BlockType::StoneStairs, stair_state(Facing::PosZ)),
			make_cell(BlockType::WoodSlab, slab_state(true, true)),
			make_cell(BlockType::Torch, 0),
		] {
			assert_eq!(object_bounds(c, v), (c, draw_bounds(v)), "cell {v:#x}");
		}
	}

	/// Only a door's UPPER half tints from the cell below. The lower half
	/// must not, or the leaf takes its tint from whatever it stands on.
	#[test]
	fn only_a_doors_upper_half_tints_from_below() {
		use crate::chunk::make_cell;
		for f in Facing::ALL {
			for open in [false, true] {
				assert!(tint_from_below(make_cell(
					BlockType::LumberDoor,
					door_state(f, open, true)
				)));
				assert!(!tint_from_below(make_cell(
					BlockType::LumberDoor,
					door_state(f, open, false)
				)));
			}
		}
		for v in [
			BlockType::Stone as u16,
			make_cell(BlockType::StoneStairs, stair_state(Facing::PosX)),
			make_cell(BlockType::WoodSlab, slab_state(false, true)),
		] {
			assert!(!tint_from_below(v), "cell {v:#x} is one cell");
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
		// The anchor bit survives the grow, so a two-thirds slab grows
		// the same way.
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
				let closed = shape_of(BlockType::LumberDoor, door_state(f, false, upper)).unwrap();
				let open = shape_of(BlockType::LumberDoor, door_state(f, true, upper)).unwrap();
				// Closed: one uncut leaf box, set back from the
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
		// filled sub-cells than the uncut lower leaf. The handle adds the
		// sub-cells it pokes out either side, and each half carries a
		// small hinge on the leaf's back at the pivot edge.
		let lower = raster_volume(&[LEAF, HANDLE, HINGE_LOWER]);
		let mut up_boxes = DOOR_UPPER_BOXES.to_vec();
		up_boxes.push(HINGE_UPPER);
		let upper = raster_volume(&up_boxes);
		assert_eq!(raster_volume(&[LEAF]), 288);
		// The uncut leaf, plus the knob's protrusion either side, plus the
		// hinge.
		assert_eq!(lower, 298);
		assert!(upper < lower, "lites carved: {upper} < {lower}");
		// The same leaf with four square lites cut clear through, plus the
		// hinge.
		assert_eq!(upper, 218);
	}

	#[test]
	fn full_cube_bakes_to_the_six_block_faces() {
		// A debug full-cube shape must reproduce the mesher's full-block
		// convention exactly: one quad per face, corner UVs at the face's
		// four extremes, every corner on the block lattice.
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

	/// §4.2's volume rule, now stated once per SHAPE rather than once per
	/// authored block: every material × shape pair prorates the material's
	/// whole-block split by these numbers, so pinning the shape axis covers
	/// every pair the catalog will ever grow — including the ones that do
	/// not exist yet.
	#[test]
	fn craft_costs_derive_from_collision_volume() {
		use crate::block::ShapeFamily;
		// Cost = the filled volume over `SUB_UNIT_VOLUME`, rounded up,
		// from the canonical closed geometry; doors price BOTH halves
		// (uncut leaves — the lites don't discount) plus the handle and
		// both hinges; a slab prices the PLACED one-third item, so grows
		// debit one item each.
		let volume = |s: ShapeFamily| match s {
			ShapeFamily::Block => SUBS_PER_BLOCK * SUB_UNIT_VOLUME,
			ShapeFamily::Stairs => raster_volume(&STAIR_BOXES),
			ShapeFamily::Door => {
				raster_volume(&[LEAF, HANDLE, HINGE_LOWER]) + raster_volume(&[LEAF, HINGE_UPPER])
			}
			ShapeFamily::Slab => raster_volume(&SLAB_BOXES[..1]),
		};
		for s in ShapeFamily::ALL {
			assert_eq!(
				s.units(),
				volume(s).div_ceil(SUB_UNIT_VOLUME),
				"{s:?} units match volume {}",
				volume(s)
			);
		}
		// A whole cube is `SUBS_PER_BLOCK` units exactly — the identity the
		// raw-block recipes prorate through, and the reason three slabs
		// make a block.
		assert_eq!(ShapeFamily::Block.units(), SUBS_PER_BLOCK);
		assert_eq!(ShapeFamily::Slab.units() * 3, SUBS_PER_BLOCK);
		// Functional items are priced by hand (§7.3), never by volume —
		// the torch has no shape FAMILY at all, which is how it opts out.
		assert_eq!(BlockType::Torch.shape_family(), None);
		for &b in &BlockType::ALL {
			assert_eq!(craft_batch(b), 1, "{b:?} — launch shapes craft singly");
		}
		assert_eq!(SUBS_PER_BLOCK * SUB_UNIT_VOLUME, 1728, "exact division");
	}

	/// The furniture family, which is where a piece's FRONT has to survive
	/// the rotation: a chair whose back stayed on +x for every facing
	/// would still pass every generic bake test and be useless.
	#[test]
	fn furniture_turns_and_stands() {
		use crate::chunk::make_cell;
		// A chair's back is the far half along its facing axis, and moves
		// with it — the union of the boxes above the seat is what a player
		// reads as the front/back, so that is what this checks.
		let back_of = |f: Facing| {
			let boxes = collision_boxes(make_cell(BlockType::Chair, stair_state(f)));
			let high: Vec<&SubBox> = boxes.iter().filter(|b| b.max[1] > 8).collect();
			assert_eq!(high.len(), 1, "{f:?}: one back");
			*high[0]
		};
		assert!(back_of(Facing::PosX).min[0] >= 6, "back on +x");
		assert!(back_of(Facing::NegX).max[0] <= 6, "back on -x");
		assert!(back_of(Facing::PosZ).min[2] >= 6, "back on +z");
		assert!(back_of(Facing::NegZ).max[2] <= 6, "back on -z");
		// The bed's pillow is the same story at the head end.
		let pillow_of = |f: Facing| {
			let boxes = collision_boxes(make_cell(BlockType::Bed, stair_state(f)));
			*boxes
				.iter()
				.find(|b| b.min[1] >= 6)
				.expect("the raised pillow")
		};
		assert!(pillow_of(Facing::PosX).min[0] >= 6, "head on +x");
		assert!(pillow_of(Facing::NegZ).max[2] <= 6, "head on -z");
		// The table is stateless, so every nibble resolves to one shape —
		// which is what lets tables pushed together read as one surface.
		let top = collision_boxes(BlockType::Table as u16);
		for s in 0..16u8 {
			assert_eq!(collision_boxes(make_cell(BlockType::Table, s)), top);
		}
		// Every piece is solid where it is drawn, never occludes a
		// SIDEWAYS boundary (a rank of chairs must not hide the room
		// behind it), and stands clear of the auto-step window —
		// furniture is climbed onto, not walked over.
		for b in [BlockType::Table, BlockType::Chair, BlockType::Bed] {
			let v = shape_of(b, 0).expect("shaped");
			assert!(!v.collision.is_empty(), "{b:?}");
			for d in [Dir::West, Dir::East, Dir::North, Dir::South] {
				assert!(!v.occludes_full[d as usize], "{b:?} walls off {d:?}");
			}
			// A walk mounts any top within `AUTO_STEP_MAX` of the feet.
			// Furniture must not be walked onto — but the bed's legs top
			// out inside that window, and that is fine BECAUSE the
			// mattress sits straight on them: the mount's headroom probe
			// refuses. So the property is not "no low surface", it is "no
			// low surface a capsule could actually stand on".
			for low in v.collision.iter().filter(|c| c.max[1] <= 4) {
				let capped = v.collision.iter().any(|c| {
					c.min[1] <= low.max[1]
						&& c.max[1] > low.max[1]
						&& c.min[0] < low.max[0]
						&& c.max[0] > low.min[0]
						&& c.min[2] < low.max[2]
						&& c.max[2] > low.min[2]
				});
				assert!(capped, "{b:?} offers a bare {}/12 step", low.max[1]);
			}
			let walkable = v.collision.iter().map(|c| c.max[1]).max().unwrap();
			assert!(u32::from(walkable) > 4, "{b:?} is a doormat");
		}
		// The table's top DOES cover its ceiling boundary, which is what
		// lets a floor of tables cull like a floor of slabs.
		assert!(shape_of(BlockType::Table, 0).unwrap().occludes_full[Dir::Top as usize]);
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
			assert!(state_valid(make_cell(BlockType::LumberDoor, s)));
		}
		// Slabs: the size bit and the anchor bit.
		for b in [BlockType::WoodSlab, BlockType::StoneSlab] {
			for s in 0..4u8 {
				assert!(state_valid(make_cell(b, s)));
			}
			assert!(!state_valid(make_cell(b, 0b0100)));
			assert!(!state_valid(make_cell(b, 0b1000)));
		}
		// Furniture: the table is stateless like the torch; the chair and
		// the bed take a facing and nothing else.
		assert!(state_valid(BlockType::Table as u16));
		assert!(!state_valid(make_cell(BlockType::Table, 1)));
		for b in [BlockType::Chair, BlockType::Bed] {
			for f in Facing::ALL {
				assert!(state_valid(make_cell(b, stair_state(f))), "{b:?}");
			}
			assert!(!state_valid(make_cell(b, 0b0100)), "{b:?}");
			assert!(!state_valid(make_cell(b, 0b1000)), "{b:?}");
		}
		// Unknown id, and the cell's spare bits, always refuse.
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
