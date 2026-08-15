//! GI-class classification for the RTGI voxel window (GI-DESIGN.md).
//!
//! The window is an R8Uint 3D texture, one texel per block, that the
//! probe rays march. Classification is a pure function of the cell so
//! every consumer (arrival uploads, edit re-uploads, tests) agrees by
//! construction. For full cubes the class byte IS the block id (ids fit
//! u8 with room to spare — pinned below); everything a ray needs per
//! class — opacity, albedo, emission — lives in the render side's
//! material LUT, one entry per class, so the window never re-uploads when
//! a material's look is tuned.
//!
//! Shaped cells get SYNTHETIC classes from [`SHAPE_CLASS_BASE`] up, one
//! per distinct (block, occlusion geometry) pair, because the v1 rule —
//! "Shaped blocks rays whole" — is exactly why a thin slab cast a
//! full-block shadow onto the slab beside it and a torch post shaded
//! like a pillar. Each synthetic class carries a TWO-BOX COVER of the
//! shape's collision boxes (in sub-grid units): the first box is
//! collision box 0 exactly, the second the AABB union of the rest. For
//! slabs and torches that cover is the exact geometry; a stair's L gains
//! only its top rear quarter; a door leaf keeps its thinness. The cover
//! can only ever OVER-block — never under — and the worst case is the
//! old full cell, so every shape strictly improves. The DDA tests the boxes
//! against the ray's in-cell span (Laine & Karras' ESVO "contours");
//! a state whose collision is EMPTY (an open door) classifies as air,
//! which is what lets light through a doorway.

use std::sync::LazyLock;

use crate::block::{BlockType, RenderClass};
use crate::chunk::{CHUNK_SIZE, CHUNK_VOLUME, ChunkData};
use crate::shapes::{self, SubBox};

/// What a ray does when it enters a cell of this class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GiOpacity {
	Air,
	Translucent,
	Opaque,
}

/// One classified chunk, ready for the GPU: texel bytes in
/// write_texture order, the coarse occupancy bytes, and the cells
/// that emit (for the light list).
pub struct GiChunk {
	/// `[z][y][x]` — depth slices in `write_texture` layout, x fastest
	/// (NOT ChunkData's y-major order).
	pub bytes: Box<[u8; CHUNK_VOLUME]>,
	/// One byte per coarse cell of the chunk, `[z][y][x]`; the low bits
	/// flag, in order, any ray-blocking cell and any emitting cell.
	pub coarse: [u8; 8],
	/// Emitting cells as (local x, y, z, class).
	pub emitters: Vec<(u8, u8, u8, u8)>,
}

/// First class byte the shaped-variant classes occupy. Everything below
/// is a plain block id (or air); everything from here up indexes
/// [`shape_classes`]. The two ranges can never collide —
/// `shape_classes_fit_the_byte` pins both ends.
///
/// The window texture is `R8Uint`, so one byte of classes is the whole
/// budget and this constant is where it is SPLIT. It sat far higher while
/// there were only a handful of shaped rows — and SKILLS-DESIGN §7.2's
/// grid spends the shaped side per MATERIAL (a timber's stairs, slab and
/// door between them), so the third timber overflowed it. Full cubes need
/// far less room than that split gave them: it moved down to leave the
/// shaped side the space the catalog actually grows into. Both halves are
/// pinned, and the pin reports the headroom rather than merely passing.
///
/// The price to know before adding a material: **a dozen-odd classes per
/// shaped material** (a cover per stair facing, per slab anchor and
/// thickness, and per closed door facing) — `shape_classes_fit_the_byte`
/// prints where the roster currently stands.
///
/// The free slots left are NOT a material's worth of room. The pin holds a
/// margin back deliberately, so the build fails while there is still space
/// to think in rather than at the panic — which leaves **less spendable
/// than one more full material costs**. The next shaped material therefore
/// has to move the split FIRST, and there is plenty to move: the full-cube
/// half is only used up to the last block id, so every id between that and
/// the base sits idle. A NEW shape family is the expensive case, since it
/// costs its own covers in every material at once — a fence or a pane is
/// far more than a fourth timber.
pub const SHAPE_CLASS_BASE: u8 = 112;

/// One synthetic class: the block whose material the LUT entry borrows,
/// and the ≤2-box occlusion cover in sub-grid units.
pub struct ShapeClass {
	pub base: BlockType,
	pub boxes: [Option<SubBox>; 2],
}

fn union_box(boxes: &[SubBox]) -> Option<SubBox> {
	let mut it = boxes.iter();
	let first = *it.next()?;
	Some(it.fold(first, |a, b| SubBox {
		min: [
			a.min[0].min(b.min[0]),
			a.min[1].min(b.min[1]),
			a.min[2].min(b.min[2]),
		],
		max: [
			a.max[0].max(b.max[0]),
			a.max[1].max(b.max[1]),
			a.max[2].max(b.max[2]),
		],
	}))
}

/// The class table and the (id, state) → class byte index. Index slots
/// hold the class byte, or 0 for "occludes nothing" (an open door).
///
/// The index is sized by the roster rather than by a fixed id space, so
/// shaped rows are limited only by the class budget above — the old
/// fixed-size table silently capped shaped ids at a number the grid's
/// materials reach.
static SHAPE_CLASSES: LazyLock<(Vec<ShapeClass>, Vec<u8>)> = LazyLock::new(|| {
	let mut classes: Vec<ShapeClass> = Vec::new();
	let mut index = vec![0u8; BlockType::COUNT * 16];
	for b in BlockType::ALL {
		if b.props().shape.is_none() {
			continue;
		}
		// All 16 nibbles, not just canonically-valid states: `shape_of`
		// masks a nibble down to a real variant, and the window must
		// answer for whatever a journal or an edit actually stored.
		for state in 0..16u8 {
			let cell = crate::chunk::make_cell(b, state);
			let boxes = shapes::collision_boxes(cell);
			let key = ((b as usize) << 4) | state as usize;
			if boxes.is_empty() {
				index[key] = 0;
				continue;
			}
			let cover = [Some(boxes[0]), union_box(&boxes[1..])];
			let found = classes
				.iter()
				.position(|c| c.base == b && c.boxes == cover)
				.unwrap_or_else(|| {
					classes.push(ShapeClass {
						base: b,
						boxes: cover,
					});
					classes.len() - 1
				});
			let class = SHAPE_CLASS_BASE as usize + found;
			assert!(class < 256, "shape classes overflow the class byte");
			index[key] = class as u8;
		}
	}
	(classes, index)
});

/// Every synthetic class, indexed by `class - SHAPE_CLASS_BASE`. The
/// render side builds the LUT entries from `SHAPE_CLASS_BASE` up out of
/// this.
pub fn shape_classes() -> &'static [ShapeClass] {
	&SHAPE_CLASSES.0
}

/// The emissive band of a shaped emitter, as a fraction of its cell
/// measured DOWN FROM THE CELL TOP.
///
/// texgen's `torch_side` paints its top rows as fire and leaves the rest
/// as wood — this constant is that band — and shaped UVs are cut from the
/// CELL's face (`shapes::uv_cell`: `v = G - 1 - y`), so the band is
/// cell-relative, not box-relative: the part of it above the shape's own
/// top simply isn't drawn. For the torch, whose post stops short of the
/// cell top, that leaves a visible flame over the top of the post.
const EMISSIVE_BAND: f32 = 5.0 / 12.0;

/// Where inside its cell an emitting class actually radiates from, in
/// block units (0..1 per axis).
///
/// A full cube emits from its center. A shaped emitter does NOT: a torch
/// is a slim post whose fire is the top of its height, so a center origin
/// puts the point source down in the wooden handle, well below the flame
/// the eye reads as the source. Under `LIGHT_NEAR` that costs no
/// brightness — irradiance is flat in there — but the hard point shadows
/// it casts project from the wrong height.
///
/// Horizontally the box center and the cell center agree for anything
/// symmetric (the torch's post is centered), so this only really moves y
/// today; it is written per-axis so a wall-mounted variant works when
/// the state nibble arrives.
pub fn light_origin(class: u8) -> [f32; 3] {
	const MID: [f32; 3] = [0.5, 0.5, 0.5];
	if class < SHAPE_CLASS_BASE {
		return MID;
	}
	let Some(sc) = shape_classes().get((class - SHAPE_CLASS_BASE) as usize) else {
		return MID;
	};
	let boxes: Vec<SubBox> = sc.boxes.iter().flatten().copied().collect();
	let Some(b) = union_box(&boxes) else {
		return MID;
	};
	let g = f32::from(shapes::GRID);
	let ctr = |lo: u8, hi: u8| (f32::from(lo) + f32::from(hi)) * 0.5 / g;
	// Clip the cell's emissive band to the shape's own extent. A shape
	// that doesn't reach the band at all (a slab, were one ever to emit)
	// keeps its box center rather than an origin outside its geometry.
	let band_lo = (1.0 - EMISSIVE_BAND) * g;
	let y = if f32::from(b.max[1]) > band_lo {
		(band_lo.max(f32::from(b.min[1])) + f32::from(b.max[1])) * 0.5 / g
	} else {
		ctr(b.min[1], b.max[1])
	};
	[ctr(b.min[0], b.max[0]), y, ctr(b.min[2], b.max[2])]
}

/// How far below the eye a light a player is CARRYING hangs.
///
/// The eye sits [`crate::player::EYE_TO_CROWN`] below the crown of a
/// [`crate::player::HEIGHT`] body, so this hangs around chest height —
/// comfortably inside the capsule, which is the whole point. See
/// [`carried_origin`].
pub const CARRY_DROP: f32 = 0.5;

/// Where a light carried by a player whose eye is at `eye` radiates
/// from.
///
/// Deliberately a function of the player's POSITION alone and not of
/// their facing, which costs one thing and buys two.
///
/// What it costs: the light does not sit where the item is drawn. The
/// viewmodel hangs its item off the hand and `players.wgsl` offsets the
/// remote one by a yaw-rotated half block, so the light and the flame
/// you can see are about that far apart. Against `LIGHT_RANGE` that is
/// nothing.
///
/// What it buys, and neither is cosmetic. **Mouse-look never moves the
/// light**, so a player standing still and turning is not a moving
/// emitter: no invalidation fires, the resolve's screen history climbs
/// back to `N_MAX`, and a lit room settles as deeply as one lit by a
/// torch on the floor. A hand-anchored origin swings through half a
/// block on every look and would hold the near field permanently short.
/// And **the cell is guaranteed non-solid**: the player's own capsule
/// occupies it, so nothing can bury the light inside geometry when they
/// brush a wall — which would otherwise put the room out.
pub fn carried_origin(eye: glam::Vec3) -> glam::Vec3 {
	eye - glam::Vec3::Y * CARRY_DROP
}

/// The class byte for one cell: 0 for air, the block id for full cubes
/// and fluids, a synthetic [`shape_classes`] entry for shaped cells —
/// which is the one place the state nibble matters (a thin floor slab
/// and a thick ceiling slab occlude differently).
#[inline]
pub fn gi_class(cell: u16) -> u8 {
	let b = BlockType::from_id(crate::chunk::cell_id(cell));
	if b == BlockType::Air {
		return 0;
	}
	if b.props().shape.is_some() {
		let key = ((b as usize) << 4) | crate::chunk::cell_state(cell) as usize;
		return SHAPE_CLASSES.1[key];
	}
	b as u8
}

/// How a full-cube class attenuates rays (LUT-side mirror; also the
/// coarse rule). Shaped cells don't route through this — their
/// synthetic classes are partial by construction.
#[inline]
pub fn opacity(b: BlockType) -> GiOpacity {
	match b.props().class {
		RenderClass::None => GiOpacity::Air,
		RenderClass::Translucent => GiOpacity::Translucent,
		_ => GiOpacity::Opaque,
	}
}

/// Pack one sub-box into the LUT's box word: the three min coordinates
/// then the three max, a nibble each (a sub-grid coordinate fits). 0 is
/// the "no box" sentinel — a real box always has a nonzero max. The WGSL
/// decode in gi_common.wgsl's `gi_box_hit` is this formula's mirror,
/// pinned by `box_pack_matches_the_shader_decode`.
#[inline]
pub fn pack_box(b: SubBox) -> u32 {
	u32::from(b.min[0])
		| u32::from(b.min[1]) << 4
		| u32::from(b.min[2]) << 8
		| u32::from(b.max[0]) << 12
		| u32::from(b.max[1]) << 16
		| u32::from(b.max[2]) << 20
}

/// Classify a whole chunk. Air chunks classify to all-zero bytes —
/// callers still upload them, because the window is toroidal and the
/// region may hold a stale previous tenant.
pub fn classify_chunk(data: &ChunkData) -> GiChunk {
	let n = CHUNK_SIZE;
	let mut bytes = Box::new([0u8; CHUNK_VOLUME]);
	let mut coarse = [0u8; 8];
	let mut emitters = Vec::new();
	for z in 0..n {
		for y in 0..n {
			for x in 0..n {
				let cell = data.get_cell(x, y, z);
				let class = gi_class(cell);
				bytes[(z * n + y) * n + x] = class;
				if class == 0 {
					continue;
				}
				let b = BlockType::from_id(crate::chunk::cell_id(cell));
				let emits = b.props().light > 0;
				if emits {
					emitters.push((x as u8, y as u8, z as u8, class));
				}
				// A partial cell sets the blocker bit too: the coarse
				// texture only licenses SKIPPING a brick, and a brick
				// holding a slab must still be marched.
				let blocks_rays = class >= SHAPE_CLASS_BASE || opacity(b) == GiOpacity::Opaque;
				if blocks_rays || emits {
					let ci = (z / 8) * 4 + (y / 8) * 2 + (x / 8);
					coarse[ci] |= u8::from(blocks_rays) | (u8::from(emits) << 1);
				}
			}
		}
	}
	GiChunk {
		bytes,
		coarse,
		emitters,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::make_cell;
	use crate::shapes::{Facing, door_state, slab_state};

	#[test]
	fn every_block_id_fits_the_class_byte() {
		// Full-cube classes are the id, with the synthetic range stacked on
		// top of them in the same byte. A block catalog crossing
		// SHAPE_CLASS_BASE needs the base moved first.
		#[allow(clippy::assertions_on_constants)]
		{
			assert!(BlockType::COUNT <= SHAPE_CLASS_BASE as usize);
		}
		for b in BlockType::ALL {
			if b.props().shape.is_some() {
				continue;
			}
			assert_eq!(
				gi_class(b as u16),
				if b == BlockType::Air { 0 } else { b as u8 }
			);
		}
	}

	#[test]
	fn a_torch_radiates_from_its_flame_not_its_handle() {
		let o = light_origin(gi_class(make_cell(BlockType::Torch, 0)));
		// The cell-relative emissive band clipped to the post leaves the
		// flame at the top of the post, and the origin is the middle of
		// what is left. Horizontally the centered post agrees with the cell
		// center.
		assert!((o[0] - 0.5).abs() < 1e-6, "{o:?}");
		assert!((o[2] - 0.5).abs() < 1e-6, "{o:?}");
		assert!((o[1] - 8.5 / 12.0).abs() < 1e-6, "{o:?}");
		// Full cubes are untouched — the point source stays the center.
		assert_eq!(light_origin(BlockType::Stone as u8), [0.5, 0.5, 0.5]);
	}

	/// Both halves of the `R8Uint` window's class budget, and the headroom
	/// left in each — the numbers, not just a pass, because this is the
	/// wall §7.2's grid walks toward one material at a time (each shaped
	/// material spends a dozen-odd covers) and the failure mode is a
	/// LazyLock panic at startup rather than anything a reader can price.
	#[test]
	fn a_carried_light_hangs_inside_the_body() {
		use crate::player::{EYE_TO_CROWN, EYE_TO_FEET};
		// Yaw-independent: the origin is a function of position alone, so
		// a player turning on the spot is not a moving emitter. There is
		// no facing argument to pass — that IS the guarantee — so the
		// test states the consequence: only y moves.
		let eye = glam::Vec3::new(12.25, 70.5, -3.75);
		let o = carried_origin(eye);
		assert_eq!((o.x, o.z), (eye.x, eye.z));
		// Strictly inside the capsule the player already occupies, and
		// with clearance either side — that is what makes the light's
		// cell provably non-solid. Brushing a wall must never bury it and
		// put the room out.
		assert!(o.y + 0.5 < eye.y + EYE_TO_CROWN, "{o:?} near the crown");
		assert!(o.y - 0.5 > eye.y - EYE_TO_FEET, "{o:?} near the feet");
	}

	#[test]
	fn shape_classes_fit_the_byte() {
		let n = shape_classes().len();
		let room = 256 - SHAPE_CLASS_BASE as usize;
		assert!(n > 0);
		assert!(
			n <= room,
			"{n} shape classes in {room} slots — lower SHAPE_CLASS_BASE or widen the window"
		);
		// The other half: a full cube's class IS its id, so every UNSHAPED
		// row has to stay below the split. Tools never reach the window,
		// but their ids sit in the same space and cost nothing to cover.
		let highest = BlockType::ALL
			.iter()
			.filter(|b| b.props().shape.is_none())
			.map(|&b| b as usize)
			.max()
			.expect("air at least");
		assert!(
			highest < SHAPE_CLASS_BASE as usize,
			"full-cube id {highest} collides with the shape classes"
		);
		// Headroom, spelled out so a catalog wave can be priced before it
		// is written rather than after it panics.
		assert!(
			room - n >= 16,
			"only {} shape slots left — one more shaped material and it overflows",
			room - n
		);
	}

	#[test]
	fn shaped_cells_get_synthetic_classes_per_state() {
		// A floor slab and a ceiling slab occlude differently, so they
		// must classify differently — the old id-only rule collapsed
		// them (and everything else) into one opaque cube.
		let floor = gi_class(make_cell(BlockType::WoodSlab, slab_state(false, false)));
		let ceil = gi_class(make_cell(BlockType::WoodSlab, slab_state(false, true)));
		assert!(floor >= SHAPE_CLASS_BASE);
		assert!(ceil >= SHAPE_CLASS_BASE);
		assert_ne!(floor, ceil);
		// Stairs rotate: each facing is its own cover.
		let e = gi_class(make_cell(BlockType::WoodStairs, Facing::PosX as u8));
		let w = gi_class(make_cell(BlockType::WoodStairs, Facing::NegX as u8));
		assert_ne!(e, w);
	}

	#[test]
	fn an_open_door_lets_light_through() {
		// Open doors have no collision, so they must occlude nothing —
		// the doorway is the whole point of opening one.
		for f in Facing::ALL {
			for upper in [false, true] {
				let open = make_cell(BlockType::LumberDoor, door_state(f, true, upper));
				assert_eq!(gi_class(open), 0);
				let closed = make_cell(BlockType::LumberDoor, door_state(f, false, upper));
				assert!(gi_class(closed) >= SHAPE_CLASS_BASE);
			}
		}
	}

	#[test]
	fn the_cover_contains_every_collision_box() {
		// Box 0 is collision box 0 exactly; box 1 must contain each of
		// the rest. Over-blocking is allowed (a stair's L rounds up to
		// its bounding pair), under-blocking never (light through a
		// solid tread).
		for b in BlockType::ALL {
			if b.props().shape.is_none() {
				continue;
			}
			for state in 0..16u8 {
				let cell = make_cell(b, state);
				let class = gi_class(cell);
				let boxes = shapes::collision_boxes(cell);
				if boxes.is_empty() {
					assert_eq!(class, 0);
					continue;
				}
				let sc = &shape_classes()[(class - SHAPE_CLASS_BASE) as usize];
				assert_eq!(sc.base, b);
				assert_eq!(sc.boxes[0], Some(boxes[0]));
				for cb in &boxes[1..] {
					let u = sc.boxes[1].expect("rest of the boxes need a cover");
					for a in 0..3 {
						assert!(u.min[a] <= cb.min[a] && u.max[a] >= cb.max[a]);
					}
				}
			}
		}
	}

	#[test]
	fn box_pack_matches_the_shader_decode() {
		// gi_box_hit unpacks the three min coordinates and then the three
		// max, a nibble each. 0 must remain the no-box sentinel.
		let b = SubBox::new([0, 8, 3], [12, 12, 7]);
		let p = pack_box(b);
		assert_ne!(p, 0);
		let dec = |v: u32, s: u32| (v >> s) & 0xF;
		assert_eq!([dec(p, 0), dec(p, 4), dec(p, 8)], [0, 8, 3], "min lanes");
		assert_eq!(
			[dec(p, 12), dec(p, 16), dec(p, 20)],
			[12, 12, 7],
			"max lanes"
		);
		// Every real cover box packs nonzero (max is never all-zero).
		for c in shape_classes() {
			for bx in c.boxes.into_iter().flatten() {
				assert_ne!(pack_box(bx), 0);
			}
		}
	}

	#[test]
	fn air_chunk_classifies_to_zeros() {
		let gi = classify_chunk(&ChunkData::default());
		assert!(gi.bytes.iter().all(|&b| b == 0));
		assert_eq!(gi.coarse, [0; 8]);
		assert!(gi.emitters.is_empty());
	}

	#[test]
	fn bytes_are_texel_ordered_not_chunk_ordered() {
		// One stone cell off every axis: its byte must sit at the
		// write_texture layout index (z-major, then y, x fastest), which
		// differs from ChunkData's y-major order.
		let mut data = ChunkData::default();
		data.set(3, 5, 7, BlockType::Stone);
		let gi = classify_chunk(&data);
		assert_eq!(gi.bytes[(7 * 16 + 5) * 16 + 3], BlockType::Stone as u8);
		assert_eq!(gi.bytes.iter().filter(|&&b| b != 0).count(), 1);
	}

	#[test]
	fn coarse_bits_split_blockers_from_emitters() {
		let mut data = ChunkData::default();
		// Stone in the lowest coarse cell, a torch in the far corner one.
		data.set(2, 2, 2, BlockType::Stone);
		data.set(12, 12, 12, BlockType::Torch);
		let gi = classify_chunk(&data);
		assert_eq!(gi.coarse[0], 0b01, "stone: blocks, no emission");
		// The torch blocks (partial cells still pin their brick) and
		// emits; its class is synthetic now, carrying the post boxes.
		assert_eq!(gi.coarse[7], 0b11);
		let torch_class = gi_class(BlockType::Torch as u16);
		assert!(torch_class >= SHAPE_CLASS_BASE);
		assert_eq!(gi.emitters, vec![(12, 12, 12, torch_class)]);
		// Water attenuates but neither blocks nor emits.
		let mut wet = ChunkData::default();
		wet.set(0, 0, 0, BlockType::Water);
		let w = classify_chunk(&wet);
		assert_eq!(w.coarse, [0; 8]);
		assert_eq!(opacity(BlockType::Water), GiOpacity::Translucent);
		assert_ne!(w.bytes[0], 0, "water still classifies — rays attenuate");
	}
}
