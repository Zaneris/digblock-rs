//! GI-class classification for the RTGI voxel window (GI-DESIGN.md).
//!
//! The window is an R8Uint 3D texture, one texel per block, that the
//! probe rays march. Classification is a pure function of the cell so
//! every consumer (arrival uploads, edit re-uploads, tests) agrees by
//! construction. For full cubes the class byte IS the block id (ids fit
//! u8 with room to spare — pinned below); everything a ray needs per
//! class — opacity, albedo, emission — lives in the render side's
//! 256-entry material LUT, so the window never re-uploads when a
//! material's look is tuned.
//!
//! Shaped cells get SYNTHETIC classes from [`SHAPE_CLASS_BASE`] up, one
//! per distinct (block, occlusion geometry) pair, because the v1 rule —
//! "Shaped blocks rays whole" — is exactly why a 1/3 slab cast a
//! full-block shadow onto the slab beside it and a torch post shaded
//! like a pillar. Each synthetic class carries a TWO-BOX COVER of the
//! shape's collision boxes (in twelfths): box 0 is the first collision
//! box exactly, box 1 the AABB union of the rest. For slabs and torches
//! that cover is the exact geometry; a stair's L gains only its top
//! rear quarter; a door leaf keeps its thinness. The cover can only
//! ever OVER-block — never under — and the worst case is the old full
//! cell, so every shape strictly improves. The DDA tests the boxes
//! against the ray's in-cell span (Laine & Karras' ESVO "contours");
//! a state whose collision is EMPTY (an open door) classifies as air,
//! which is what lets light through a doorway.

use std::sync::LazyLock;

use crate::block::{BlockType, RenderClass};
use crate::chunk::{ChunkData, CHUNK_SIZE, CHUNK_VOLUME};
use crate::shapes::{self, SubBox};

/// What a ray does when it enters a cell of this class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GiOpacity {
	Air,
	Translucent,
	Opaque,
}

/// One classified chunk, ready for the GPU: texel bytes in
/// write_texture order, the /8 coarse occupancy bytes, and the cells
/// that emit (for the light list).
pub struct GiChunk {
	/// `[z][y][x]` — depth slices of `bytes_per_row = 16`,
	/// `rows_per_image = 16` (NOT ChunkData's y-major order).
	pub bytes: Box<[u8; CHUNK_VOLUME]>,
	/// 2×2×2 coarse cells of 8³ blocks each, `[z][y][x]`;
	/// bit 0 = any ray-blocking cell, bit 1 = any emitting cell.
	pub coarse: [u8; 8],
	/// Emitting cells as (local x, y, z, class).
	pub emitters: Vec<(u8, u8, u8, u8)>,
}

/// First class byte the shaped-variant classes occupy. Everything below
/// is a plain block id (or air); everything from here up indexes
/// [`shape_classes`]. The two ranges can never collide —
/// `shape_classes_fit_the_byte` pins both ends.
pub const SHAPE_CLASS_BASE: u8 = 200;

/// One synthetic class: the block whose material the LUT entry borrows,
/// and the ≤2-box occlusion cover in twelfths.
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
static SHAPE_CLASSES: LazyLock<(Vec<ShapeClass>, [u8; 1024])> = LazyLock::new(|| {
	let mut classes: Vec<ShapeClass> = Vec::new();
	let mut index = [0u8; 1024];
	for b in BlockType::ALL {
		if b.props().shape.is_none() {
			continue;
		}
		assert!((b as usize) < 64, "shape index packs the id into 6 bits");
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
/// render side builds LUT entries 200.. from this.
pub fn shape_classes() -> &'static [ShapeClass] {
	&SHAPE_CLASSES.0
}

/// The emissive band of a shaped emitter, as a fraction of its cell
/// measured DOWN FROM THE CELL TOP.
///
/// texgen's `torch_side` paints its top `SIZE * 5 / 12` rows as fire and
/// leaves the rest as wood, and shaped UVs are cut from the CELL's face
/// (`shapes::uv_cell`: `v = G - 1 - y`), so the band is cell-relative,
/// not box-relative — the part of it above the shape's own top simply
/// isn't drawn. For the torch (a post 10/12 tall) that leaves a visible
/// flame from 7/12 to 10/12.
const EMISSIVE_BAND: f32 = 5.0 / 12.0;

/// Where inside its cell an emitting class actually radiates from, in
/// block units (0..1 per axis).
///
/// A full cube emits from its center. A shaped emitter does NOT: a torch
/// is a slim post whose fire is the top third of its height, so a center
/// origin puts the point source in the wooden handle, roughly a fifth of
/// a block below the flame the eye reads as the source. Under
/// `LIGHT_NEAR` that costs no brightness — irradiance is flat in there —
/// but the hard point shadows it casts project from the wrong height.
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
/// [`crate::player::HEIGHT`] body, so this lands at feet + 1.0 —
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
/// you can see are about that far apart. Against `LIGHT_RANGE` = 24 that
/// is nothing.
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
/// which is the one place the state nibble matters (a 1/3 floor slab
/// and a 2/3 ceiling slab occlude differently).
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

/// Pack one sub-box into the LUT's box word: min in bits 0/4/8, max in
/// 12/16/20, four bits per twelfth-coordinate (0..=12 fits). 0 is the
/// "no box" sentinel — a real box always has a nonzero max. The WGSL
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
	use crate::shapes::{door_state, slab_state, Facing};

	#[test]
	fn every_block_id_fits_the_class_byte() {
		// Full-cube classes are the id; the LUT has 256 entries with the
		// synthetic range on top. A block catalog crossing
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
		// The post is 2/12 square and 10/12 tall; the cell's top 5/12
		// emissive band clipped to it is 7/12..10/12, centered at 8.5/12.
		// Horizontally the centered post agrees with the cell center.
		assert!((o[0] - 0.5).abs() < 1e-6, "{o:?}");
		assert!((o[2] - 0.5).abs() < 1e-6, "{o:?}");
		assert!((o[1] - 8.5 / 12.0).abs() < 1e-6, "{o:?}");
		// Full cubes are untouched — the point source stays the center.
		assert_eq!(light_origin(BlockType::Stone as u8), [0.5, 0.5, 0.5]);
	}

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
		assert!(n > 0);
		assert!(SHAPE_CLASS_BASE as usize + n <= 256, "{n} classes overflow");
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
		// gi_box_hit unpacks min from bits 0/4/8 and max from 12/16/20,
		// four bits each. 0 must remain the no-box sentinel.
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
		// One stone cell at (x=3, y=5, z=7): the byte must sit at the
		// write_texture layout index (z*16 + y)*16 + x, which differs
		// from ChunkData's (y*16 + z)*16 + x.
		let mut data = ChunkData::default();
		data.set(3, 5, 7, BlockType::Stone);
		let gi = classify_chunk(&data);
		assert_eq!(gi.bytes[(7 * 16 + 5) * 16 + 3], BlockType::Stone as u8);
		assert_eq!(gi.bytes.iter().filter(|&&b| b != 0).count(), 1);
	}

	#[test]
	fn coarse_bits_split_blockers_from_emitters() {
		let mut data = ChunkData::default();
		// Stone in the low 8³ region, a torch in the high corner region.
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
