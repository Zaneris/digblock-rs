//! Sub-block shape registry: static geometry per (block id, state nibble) on a
//! cubic sub-grid (`GRID` sub-cells per block edge, chosen so halves, thirds,
//! quarters and sixths are all exact). Shapes are authored as `.shape` text
//! assets (embedded and parsed by [`def`]) in a canonical facing, and every
//! variant is baked from them at first use — rasterize to a bitset, emit
//! exposed sub-faces, greedy-merge into maximal quads — all pure integer ops,
//! so the registry is deterministic across platforms and both sides of the
//! wire. The LEGACY_* pins in the tests prove each ported file bakes
//! byte-identical to the retired hardcoded consts.
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

pub mod def;
mod ramp;

/// Sub-cells per block edge.
pub const GRID: u8 = 12;
const G: i32 = GRID as i32;
const GU: usize = GRID as usize;

/// Ceiling on any variant's baked entry count (quads, ramps and folded
/// triangles alike); pinned per-variant by a test here. Sizes the item,
/// viewmodel and icon buffers, and is what the editor reports against. The
/// chunk index buffer is NOT sized from it any more: `budget` charges each
/// placed shape's template against a per-chunk budget instead, so this is
/// a sanity ceiling, not a memory line. Worst shipped today: the upper
/// door half (lites + hinge), at 48.
pub const MAX_SHAPE_QUADS: usize = 144;

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

/// The other half of a door pair (upper ↔ lower, same column) — a thin
/// wrapper over the generic `object_cells` machinery, kept for its many
/// callers. Client prediction and the server's pair writes share this so
/// they can never disagree about which cell is the partner.
#[inline]
pub fn door_partner(cell: (i32, i32, i32), state: u8) -> (i32, i32, i32) {
	let value = crate::chunk::make_cell(BlockType::LumberDoor, state);
	let other = usize::from(part_index(value) == 0);
	object_cells(cell, value)[other].0
}

// --- multi-cell objects -----------------------------------------------

/// How a kind's state nibble is laid out: which bits are a facing, an open
/// flag, and the part index of a multi-cell footprint. Declared in code —
/// the wire's grammar — and cross-checked against the kind's `.shape` file
/// at load. Growing a footprint past what the nibble holds means drawing
/// the cell's spare bits into state (chunk.rs reserves the right); that is
/// additive for existing cells, whose spare bits are zero.
pub struct StateLayout {
	/// Bits 0-1 are a `Facing`.
	pub facing: bool,
	/// An open/closed flag (the door's bit 2).
	pub open_bit: Option<u8>,
	/// Where the part index lives (meaningful only when parts > 1).
	pub part_shift: u8,
	/// Cells of the footprint, in canonical facing: W, H, D.
	pub footprint: [u8; 3],
}

impl StateLayout {
	/// Cells this object wears.
	#[inline]
	pub fn parts(&self) -> u8 {
		self.footprint[0] * self.footprint[1] * self.footprint[2]
	}

	/// Bits the part index occupies.
	#[inline]
	fn part_bits(&self) -> u8 {
		match self.parts() {
			1 => 0,
			2 => 1,
			_ => 2,
		}
	}
}

/// The nibble layout of a kind. The door's rows are FROZEN — facing, open
/// bit 2, part bit 3 (part 1 = upper) — so every journaled door cell keeps
/// today's meaning.
pub fn state_layout(kind: ShapeKind) -> StateLayout {
	let (facing, open_bit, part_shift, footprint) = match kind {
		ShapeKind::Stairs | ShapeKind::Chair | ShapeKind::Bed => (true, None, 2, [1, 1, 1]),
		ShapeKind::Door => (true, Some(2), 3, [1, 2, 1]),
		ShapeKind::Slab | ShapeKind::Torch | ShapeKind::Table => (false, None, 0, [1, 1, 1]),
	};
	StateLayout {
		facing,
		open_bit,
		part_shift,
		footprint,
	}
}

/// Whether this block is one object wearing several cells.
#[inline]
pub fn is_multi_cell(b: BlockType) -> bool {
	b.props().shape.is_some_and(|k| state_layout(k).parts() > 1)
}

/// Which cell of its object's footprint this cell is (0 for the anchor and
/// for every single-cell shape).
#[inline]
pub fn part_index(value: u16) -> u8 {
	let Some(kind) = BlockType::from_id(cell_id(value)).props().shape else {
		return 0;
	};
	let l = state_layout(kind);
	(cell_state(value) >> l.part_shift) & ((1 << l.part_bits()) - 1)
}

/// A part's cell offset from the anchor: canonical order
/// `part = cx + W*(cz + D*cy)`, the x/z pair turned with the facing.
fn part_offset(l: &StateLayout, part: u8, facing_k: u8) -> (i32, i32, i32) {
	let [w, _, d] = l.footprint;
	let (mut x, y, mut z) = (
		i32::from(part % w),
		i32::from(part / (w * d)),
		i32::from((part / w) % d),
	);
	for _ in 0..(facing_k & 3) {
		(x, z) = (-z, x);
	}
	(x, y, z)
}

/// Every (cell, expected cell value) of the object this cell belongs to,
/// derived purely from the one cell's own value — [0] is the anchor
/// (part 0). Client prediction and the server's assembled writes share
/// this, so a door still places, digs and toggles as one object, and any
/// future footprint does too.
pub fn object_cells(cell: (i32, i32, i32), value: u16) -> Vec<((i32, i32, i32), u16)> {
	let Some(kind) = BlockType::from_id(cell_id(value)).props().shape else {
		return vec![(cell, value)];
	};
	let l = state_layout(kind);
	let parts = l.parts();
	if parts == 1 {
		return vec![(cell, value)];
	}
	let k = if l.facing { cell_state(value) & 3 } else { 0 };
	let mine = part_offset(&l, part_index(value), k);
	let anchor = (cell.0 - mine.0, cell.1 - mine.1, cell.2 - mine.2);
	let part_mask =
		u16::from((1u8 << l.part_bits()) - 1) << (STATE_SHIFT + u32::from(l.part_shift));
	(0..parts)
		.map(|p| {
			let o = part_offset(&l, p, k);
			let v =
				(value & !part_mask) | (u16::from(p) << (STATE_SHIFT + u32::from(l.part_shift)));
			((anchor.0 + o.0, anchor.1 + o.1, anchor.2 + o.2), v)
		})
		.collect()
}

/// Whether a value is the canonical ANCHOR form a placement may carry on
/// the wire: part 0, and closed if the kind has an open flag. The server
/// derives every other cell itself.
#[inline]
pub fn place_canonical(value: u16) -> bool {
	let Some(kind) = BlockType::from_id(cell_id(value)).props().shape else {
		return true;
	};
	let l = state_layout(kind);
	let open = l
		.open_bit
		.is_some_and(|bit| cell_state(value) & (1 << bit) != 0);
	part_index(value) == 0 && !open
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShapeQuad {
	pub dir: Dir,
	pub corners: [[u8; 3]; 4],
	/// Per-corner UV in twelfths (matches the corner-code UV of a full
	/// block face: TL=(u0,v0) … BR=(u1,v1)).
	pub uv: [[u8; 2]; 4],
	/// Texture layer override from the owning authored box, resolved for
	/// this quad's dir at bake; `None` = the block row's top/side/bottom.
	pub tex: Option<u16>,
	/// Lies on the block-boundary plane toward `dir` → subject to
	/// neighbor culling. Interior quads always emit.
	pub boundary: bool,
	/// Boundary quads: the covered rect [a0, b0, a1, b1) in the neutral
	/// tangent frame, for `FaceMask::covers`. Zeroed for interior quads.
	pub rect: [u8; 4],
}

impl ShapeQuad {
	/// A folded quad: two corners coincide, so one of its two triangles is
	/// degenerate and only the other draws (the ramp pass's side pieces).
	pub fn is_tri(&self) -> bool {
		let c = &self.corners;
		c[0] == c[1] || c[1] == c[3] || c[3] == c[2] || c[2] == c[0]
	}

	/// A ramp: the corners do not share the coordinate along `dir`'s axis.
	pub fn is_ramp(&self) -> bool {
		let (ox, oy, _) = self.dir.offset();
		let axis = if ox != 0 {
			0
		} else if oy != 0 {
			1
		} else {
			2
		};
		self.corners
			.iter()
			.any(|c| c[axis] != self.corners[0][axis])
	}
}

/// Everything baked for one (shape, state) combination.
pub struct ShapeVariant {
	/// Physics boxes (world edit validation shares them). Open doors:
	/// EMPTY — the leaf stops blocking entirely.
	pub collision: Vec<SubBox>,
	/// What light meets (`gi` builds a shaped cell's covers from these): equals `collision`
	/// except a fixture a body walks THROUGH but that still stands in the way of light — the
	/// torch. The open door is empty here as it is in `collision`, on purpose: it lets light
	/// through.
	pub light: Vec<SubBox>,
	/// Picking boxes for the interact ray: equals `collision` except an
	/// open door, whose swung leaf stays clickable to close it.
	pub interaction: Vec<SubBox>,
	/// Render template, stamped per instance by the mesher.
	pub quads: Vec<ShapeQuad>,
	/// Boundary coverage per `Dir`, for culling BOTH directions.
	pub face_masks: [FaceMask; 6],
	/// `face_masks[d].is_full()`, precomputed.
	pub occludes_full: [bool; 6],
	/// Union AABB of the DRAWN geometry, in twelfths — the box of best fit around the shape. The
	/// crosshair outline wraps this instead of the cell, so a slab or a door leaf is boxed at its
	/// own size; a stair's is the full cube, because that already IS its best fit. Taken from the
	/// authored boxes, which is exactly what the raster fills, rather than from `collision` — a
	/// closed door's knob protrudes past the leaf physics uses.
	pub bounds: SubBox,
	/// Stairs: the ascending facing — physics resolves a diagonal ramp
	/// instead of the sub-boxes when approaching from the low side.
	pub ramp: Option<Facing>,
}

// --- registry ---------------------------------------------------------

/// The shape for a block + state, if the block is shaped.
#[inline]
pub fn shape_of(b: BlockType, state: u8) -> Option<&'static ShapeVariant> {
	if let Some(v) = preview_of(b, state) {
		return Some(v);
	}
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

/// Force every variant table to bake NOW, during load. Baking is lazy per family and each family
/// bakes ALL its variants at once, so the first door to reach a mesher, a `neighbor_cover` probe,
/// a physics test or the alchemy screen pays for the whole family — a one-frame hitch at an
/// arbitrary moment of gameplay. Warm calls are nanoseconds: this is only about WHEN.
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

/// Boxes light meets ([] for full cubes and for an open door; the torch's post, which a body
/// walks through but light does not).
#[inline]
pub fn light_boxes(cell: u16) -> &'static [SubBox] {
	shape_of_cell(cell).map_or(&[], |v| &v.light)
}

/// Interact-ray boxes (open door = swung leaf).
#[inline]
pub fn interaction_boxes(cell: u16) -> &'static [SubBox] {
	shape_of_cell(cell).map_or(&[], |v| &v.interaction)
}

/// Whether a BODY collides with this cell — THE definition, per state: solid matter that is a
/// full cube, or a shaped one with collision boxes (an open door and the torch have none). Every
/// physics and placement question asks this; `BlockProps::solid` is matter, which is a different
/// question (the ray and fluid respect a torch; the feet pass it).
#[inline]
pub fn collides(cell: u16) -> bool {
	let b = BlockType::from_id(cell_id(cell));
	b.props().solid && (b.props().shape.is_none() || !collision_boxes(cell).is_empty())
}

/// Box of best fit around a cell's drawn geometry, in twelfths — `None`
/// for a full cube, which needs no box because it IS the cell. See
/// [`ShapeVariant::bounds`].
#[inline]
pub fn draw_bounds(cell: u16) -> Option<SubBox> {
	shape_of_cell(cell).map(|v| v.bounds)
}

/// The cell a whole OBJECT's box is measured from, plus the box of best fit around all of it, in
/// twelfths **of that cell** — so a door hands back its LOWER half and a box two cells tall. **The
/// caller must not assume the box fits inside one cell.** This is the crosshair outline's box: a
/// door is one object wearing two cells, and boxing the half under the crosshair framed whichever
/// panel you aimed at. Every partner's geometry is DERIVED, so this stays pure in one cell value.
/// The measuring cell is the footprint's minimum corner (a rotated footprint can put parts on the
/// anchor's negative side, and the box's twelfths are unsigned) — for a door, exactly the lower.
pub fn object_bounds(cell: (i32, i32, i32), value: u16) -> ((i32, i32, i32), Option<SubBox>) {
	let b = BlockType::from_id(cell_id(value));
	if !is_multi_cell(b) {
		return (cell, draw_bounds(value));
	}
	let cells = object_cells(cell, value);
	let corner = (
		cells.iter().map(|&(c, _)| c.0).min().unwrap_or(cell.0),
		cells.iter().map(|&(c, _)| c.1).min().unwrap_or(cell.1),
		cells.iter().map(|&(c, _)| c.2).min().unwrap_or(cell.2),
	);
	let mut u: Option<SubBox> = None;
	for (c, v) in cells {
		let Some(part) = draw_bounds(v) else {
			return (cell, draw_bounds(value));
		};
		let lift = [
			(c.0 - corner.0) as u8 * GRID,
			(c.1 - corner.1) as u8 * GRID,
			(c.2 - corner.2) as u8 * GRID,
		];
		let acc = u.get_or_insert(SubBox::new([u8::MAX; 3], [0; 3]));
		for (a, l) in lift.into_iter().enumerate() {
			acc.min[a] = acc.min[a].min(part.min[a] + l);
			acc.max[a] = acc.max[a].max(part.max[a] + l);
		}
	}
	(corner, u)
}

/// Whether a cell takes its per-block tint from the cell BELOW instead of its own: the upper half
/// of a door, so a two-cell leaf wears ONE tint and reads as one object rather than two planks
/// sawn from different timber. Rides the mesh as a flag in the solid material word — see
/// `mesher::solid_word1` and chunk.wgsl's `block_tint`. A cell, not a block type, because the
/// door's *half* decides; a hash-anchor shift, so the leaf still varies from the door beside it.
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

/// How many items one craft produces (the essence redesign's feedstock-and-catalyst shape). A
/// feedstock recipe consumes ONE cube of matter, so it yields as many of the shape as the cube's
/// volume cuts into — floored, the remainder being the waste of the cut — which is what makes
/// matter conservation structural: a craft can only ever output at most the volume it consumed.
/// Essence-only recipes buy one at a time.
pub fn craft_batch(b: BlockType) -> u32 {
	match b.grid_cell() {
		Some((m, s)) if m.feedstock().is_some() => (SUBS_PER_BLOCK / s.units()).max(1),
		_ => 1,
	}
}

// --- variant building ---------------------------------------------------

static TORCHES: LazyLock<[ShapeVariant; 1]> =
	LazyLock::new(|| [build_variant(ShapeKind::Torch, 0)]);

static TABLES: LazyLock<[ShapeVariant; 1]> = LazyLock::new(|| [build_variant(ShapeKind::Table, 0)]);

static CHAIRS: LazyLock<[ShapeVariant; 4]> =
	LazyLock::new(|| Facing::ALL.map(|f| build_variant(ShapeKind::Chair, f as u8)));

static BEDS: LazyLock<[ShapeVariant; 4]> =
	LazyLock::new(|| Facing::ALL.map(|f| build_variant(ShapeKind::Bed, f as u8)));

static SLABS: LazyLock<[ShapeVariant; 4]> =
	LazyLock::new(|| std::array::from_fn(|state| build_variant(ShapeKind::Slab, state as u8)));

static STAIRS: LazyLock<[ShapeVariant; 4]> =
	LazyLock::new(|| Facing::ALL.map(|f| build_variant(ShapeKind::Stairs, f as u8)));

static DOORS: LazyLock<[ShapeVariant; 16]> =
	LazyLock::new(|| std::array::from_fn(|state| build_variant(ShapeKind::Door, state as u8)));

/// The authored boxes falling inside one footprint cell, clipped and
/// translated to cell-local twelfths (authored order preserved — it is the
/// overlap paint order). A box wholly outside the cell drops out.
fn slice_cell(list: &[def::DefBox], co: (i32, i32, i32)) -> Vec<def::DefBox> {
	let lo = [co.0 * G, co.1 * G, co.2 * G];
	list.iter()
		.filter_map(|b| {
			let mut min = [0u8; 3];
			let mut max = [0u8; 3];
			for a in 0..3 {
				let l = i32::from(b.sub.min[a]).max(lo[a]);
				let h = i32::from(b.sub.max[a]).min(lo[a] + G);
				if l >= h {
					return None;
				}
				min[a] = (l - lo[a]) as u8;
				max[a] = (h - lo[a]) as u8;
			}
			Some(def::DefBox {
				sub: SubBox::new(min, max),
				..b.clone()
			})
		})
		.collect()
}

/// One role's boxes from a cell-local list, posed: filter, swing if open
/// (BEFORE the facing rotation, so every facing opens with the same
/// handedness), then rotate by k.
fn role_boxes(
	list: &[def::DefBox],
	open: bool,
	k: u8,
	pick: impl Fn(def::Roles) -> bool,
) -> Vec<SubBox> {
	let picked: Vec<SubBox> = list
		.iter()
		.filter(|b| pick(b.roles))
		.map(|b| b.sub)
		.map(|b| if open { swing_open(&b) } else { b })
		.collect();
	rotate_boxes(&picked, k)
}

/// The posed visual boxes of a cell-local list WITH their texture triples,
/// for the override-aware bake (triples are facing-invariant: top/bottom
/// survive a y-rotation and all four sides share one slot).
fn visual_pairs(list: &[def::DefBox], open: bool, k: u8) -> Vec<(SubBox, [Option<u16>; 3])> {
	let visual: Vec<&def::DefBox> = list.iter().filter(|b| b.roles.visual).collect();
	let posed = role_boxes(list, open, k, |r| r.visual);
	posed
		.into_iter()
		.zip(visual)
		.map(|(sub, b)| (sub, b.tex))
		.collect()
}

/// Bake one (kind, state) variant from its loaded `.shape` definition:
/// slice the state's footprint cell out of the authored volume, split by
/// role, pose (the door's swing, then the facing rotation), bake the
/// visual boxes into quads and bounds. An OPEN door keeps only its swung
/// visual and interaction — collision and light go empty, on purpose.
fn build_variant(kind: ShapeKind, state: u8) -> ShapeVariant {
	let d = def::of(kind);
	let l = state_layout(kind);
	let (open, k) = match d.states {
		def::StateModel::Door => {
			let (f, open, _) = door_parts(state);
			(open, f as u8)
		}
		def::StateModel::Facing => (false, state & 3),
		_ => (false, 0),
	};
	let part = (state >> l.part_shift) & ((1 << l.part_bits()) - 1);
	let list = slice_cell(d.state_boxes(state), part_offset(&l, part, 0));
	let baked = bake_tex(&visual_pairs(&list, open, k));
	let closed_role = |pick: fn(def::Roles) -> bool| {
		if open {
			Vec::new()
		} else {
			role_boxes(&list, false, k, pick)
		}
	};
	ShapeVariant {
		collision: closed_role(|r| r.collision),
		light: closed_role(|r| r.light),
		// The interact ray still finds an open door on its swung leaf.
		interaction: role_boxes(&list, open, k, |r| r.interaction),
		quads: baked.quads,
		face_masks: baked.face_masks,
		occludes_full: baked.occludes_full,
		bounds: baked.bounds,
		ramp: d.ramp.then(|| Facing::from_bits(k)),
	}
}

/// Rotate boxes k × 90° CCW about +y: point (x, z) → (GRID − z, x).
/// Public for the shape editor's facing preview; the registry's own
/// variants ride it through `build_variant`.
pub fn rotate_boxes(boxes: &[SubBox], k: u8) -> Vec<SubBox> {
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

/// The door swing: a rigid 90° rotation about the leaf's vertical pivot axis, in the corner of the
/// cell the hinge stands in. Valid for door parts only — they all live inside the swing envelope
/// on x, so their images stay inside the cell (the `debug_assert` below IS that envelope). The
/// leaf/handle images match the old x↔z mirror, their x-extents being symmetric about the pivot;
/// the hinge, mounted off-center on the leaf's back, is what makes the rotation visibly rigid.
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

/// Bake with no texture overrides — every quad falls back to the block row.
/// Live code always bakes through a def's texture pairs; the legacy pins
/// and the full-cube convention test bake bare box lists.
#[cfg(test)]
fn bake(boxes: &[SubBox]) -> Baked {
	let pairs: Vec<(SubBox, [Option<u16>; 3])> = boxes.iter().map(|&b| (b, [None; 3])).collect();
	bake_tex(&pairs)
}

// --- editor preview -----------------------------------------------------

/// Bake one footprint cell of an ARBITRARY authored list into a full
/// variant, the exact pipeline the game runs on a `.shape` file — the
/// editor's preview, paired with [`set_preview_override`].
pub fn preview_variant(list: &[def::DefBox], cell_offset: (i32, i32, i32), k: u8) -> ShapeVariant {
	let sliced = slice_cell(list, cell_offset);
	let baked = bake_tex(&visual_pairs(&sliced, false, k));
	ShapeVariant {
		collision: role_boxes(&sliced, false, k, |r| r.collision),
		light: role_boxes(&sliced, false, k, |r| r.light),
		interaction: role_boxes(&sliced, false, k, |r| r.interaction),
		quads: baked.quads,
		face_masks: baked.face_masks,
		occludes_full: baked.occludes_full,
		bounds: baked.bounds,
		ramp: None,
	}
}

/// EDITOR-ONLY registry overrides: while any is set, `shape_of` answers it
/// for that (block, state) before the shipped tables, so a sculpted work in
/// progress renders through the game's own mesher as real world cells.
/// Never used in a networked session — an override under `Player::tick`
/// would diverge from the server. Each set leaks its variant (a dev tool's
/// churn, not a player's). Off = one relaxed atomic load in `shape_of`.
static PREVIEW_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PREVIEW: std::sync::Mutex<Vec<((BlockType, u8), &'static ShapeVariant)>> =
	std::sync::Mutex::new(Vec::new());

/// Install (or replace) the preview variant one (block, state) answers.
pub fn set_preview_override(b: BlockType, state: u8, v: ShapeVariant) {
	let leaked: &'static ShapeVariant = Box::leak(Box::new(v));
	let mut p = PREVIEW.lock().unwrap();
	match p.iter_mut().find(|(key, _)| *key == (b, state)) {
		Some(slot) => slot.1 = leaked,
		None => p.push(((b, state), leaked)),
	}
	PREVIEW_ON.store(true, std::sync::atomic::Ordering::Release);
}

/// Drop every preview override (the shipped tables answer again).
pub fn clear_preview_overrides() {
	PREVIEW_ON.store(false, std::sync::atomic::Ordering::Release);
	PREVIEW.lock().unwrap().clear();
}

#[inline]
fn preview_of(b: BlockType, state: u8) -> Option<&'static ShapeVariant> {
	if !PREVIEW_ON.load(std::sync::atomic::Ordering::Acquire) {
		return None;
	}
	let p = PREVIEW.lock().unwrap();
	p.iter()
		.find(|(key, _)| *key == (b, state))
		.map(|&(_, v)| v)
}

/// The texture-triple slot a quad direction samples — the same split
/// `BlockType::texture` uses (top, bottom, everything else is side).
#[inline]
fn tex_slot(dir: Dir) -> usize {
	match dir {
		Dir::Top => 0,
		Dir::Bottom => 2,
		_ => 1,
	}
}

fn bake_tex(boxes: &[(SubBox, [Option<u16>; 3])]) -> Baked {
	let plain: Vec<SubBox> = boxes.iter().map(|&(b, _)| b).collect();
	let occ = rasterize(&plain);
	// Exposure comes from the UNION (interior faces between boxes still
	// vanish); each filled sub-cell's texture comes from the LAST box that
	// painted it — the overlap rule the .shape grammar documents.
	let mut owner = vec![0u16; (G * G * G) as usize];
	for (i, (b, _)) in boxes.iter().enumerate() {
		for y in b.min[1]..b.max[1] {
			for z in b.min[2]..b.max[2] {
				for x in b.min[0]..b.max[0] {
					owner[occ_idx(i32::from(x), i32::from(y), i32::from(z))] = i as u16;
				}
			}
		}
	}
	let occ_at = |x: i32, y: i32, z: i32| -> bool {
		if !(0..G).contains(&x) || !(0..G).contains(&y) || !(0..G).contains(&z) {
			return false;
		}
		occ[occ_idx(x, y, z)]
	};
	// Runs of twelfth steps draw as ramps first; the greedy pass skips the
	// face cells they replaced.
	let joined = ramp::join(&occ, &|p, dir| {
		boxes[owner[occ_idx(p[0], p[1], p[2])] as usize].1[tex_slot(dir)]
	});
	let mut quads = Vec::new();
	let mut face_masks = [FaceMask::EMPTY; 6];
	for dir in DIRS {
		let (ox, oy, oz) = dir.offset();
		let slot = tex_slot(dir);
		// Sub-cell layers along the normal axis; the face plane of layer
		// L is at L (negative dirs) or L+1 (positive dirs).
		for layer in 0..G {
			let positive = ox + oy + oz > 0;
			let q = if positive { layer + 1 } else { layer };
			let boundary = q == 0 || q == G;
			// Exposed faces of this layer as a (u, v) cell grid, each cell
			// tagged with its owner's resolved layer for this dir.
			let mut grid: [[Option<Option<u16>>; GU]; GU] = [[None; GU]; GU];
			let mut values: Vec<Option<u16>> = Vec::new();
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
					if boundary {
						let (na, nb) = neutral_axes(dir);
						let p = [x, y, z];
						face_masks[dir as usize].0[p[nb] as usize] |= 1 << p[na];
					}
					// Coverage counts the cell either way; only the draw
					// is the ramp's.
					if joined.consumed[dir as usize][occ_idx(x, y, z)] {
						continue;
					}
					let t = boxes[owner[occ_idx(x, y, z)] as usize].1[slot];
					let (u, v) = uv_cell(dir, (x, y, z));
					grid[v as usize][u as usize] = Some(t);
					if !values.contains(&t) {
						values.push(t);
					}
				}
			}
			// One greedy pass per distinct texture (deterministic order:
			// the row default first, then layers ascending) — a quad never
			// spans two textures. Override-free shapes have ONE value, so
			// this is the old single pass, bit for bit.
			values.sort_unstable();
			for tex in values {
				let mut sub = [[false; GU]; GU];
				for v in 0..GU {
					for u in 0..GU {
						sub[v][u] = grid[v][u] == Some(tex);
					}
				}
				greedy_rects(&sub, |u0, v0, u1, v1| {
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
						boundary_rect(dir, &corners)
					} else {
						[0; 4]
					};
					quads.push(ShapeQuad {
						dir,
						corners,
						uv,
						tex,
						boundary,
						rect,
					});
				});
			}
		}
	}
	quads.extend(joined.quads);
	let occludes_full = std::array::from_fn(|d| face_masks[d].is_full());
	Baked {
		quads,
		face_masks,
		occludes_full,
		bounds: union_bounds(&plain),
	}
}

/// A boundary quad's covered rect [a0, b0, a1, b1) in the neutral tangent
/// frame, for `FaceMask::covers`: the corners' bounding box.
fn boundary_rect(dir: Dir, corners: &[[u8; 3]; 4]) -> [u8; 4] {
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
