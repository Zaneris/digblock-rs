//! GI-class classification for the RTGI voxel window (GI-DESIGN.md).
//!
//! The window is an R8Uint 3D texture, one texel per block, that the probe rays
//! march. Classification is a pure function of the cell, so every consumer
//! agrees by construction. For full cubes the class byte IS the block id;
//! everything a ray needs per class lives in the render side's material LUT, so
//! the window never re-uploads when a look is tuned.
//!
//! Shaped cells get SYNTHETIC classes from [`SHAPE_CLASS_BASE`] up, each
//! carrying a TWO-BOX COVER of the shape's collision boxes: box 0 exactly, then
//! the AABB union of the rest. A cover can only ever OVER-block, never under. A
//! state whose collision is EMPTY — an open door — classifies as air.

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
	/// See-through with a per-class rgb transmittance (glass, crystal):
	/// the marches multiply a throughput instead of stopping, and the
	/// coarse rule still sets the blocker bit so a brick of panes is
	/// marched rather than skipped straight through untinted.
	Tinted,
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

/// First class byte the shaped-variant classes occupy. Everything below is a
/// plain block id (or air); everything from here up indexes [`shape_classes`].
/// The window texture is `R8Uint`, so one byte is the whole budget and this
/// constant is where it is SPLIT.
///
/// **The price to know before adding a material: a dozen-odd classes per shaped
/// material** (a cover per stair facing, per slab anchor and thickness, per
/// closed door facing). `shape_classes_fit_the_byte` pins both ends and prints
/// the remaining headroom rather than merely passing — and holds a margin back,
/// so the free slots are NOT a material's worth of room. The next shaped
/// material has to move this split first; the full-cube half is only used up to
/// the last block id, so every id between that and the base sits idle. A NEW
/// shape family is the expensive case, costing covers in every material at once.
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

/// Where inside its cell an emitting class actually radiates from, in block
/// units (0..1 per axis).
///
/// A full cube emits from its center. A shaped emitter does not: a torch is a
/// slim post whose fire is the top of its height, so a center origin puts the
/// point source down in the wooden handle. Under `LIGHT_NEAR` that costs no
/// brightness — irradiance is flat in there — but the hard point shadows it
/// casts project from the wrong height.
///
/// Written per-axis though only y moves today, so a wall-mounted variant works
/// when the state nibble arrives.
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
/// [`crate::player::HEIGHT`] body, so this hangs around shoulder height
/// — comfortably inside the capsule, which is the whole point. The
/// crown is the near wall and the only one worth watching: shrinking
/// this drop thins the overlap between the light's cell and the body
/// that proves that cell non-solid, and
/// `a_carried_light_hangs_inside_the_body` is where the clearance is
/// pinned. See [`carried_origin`].
pub const CARRY_DROP: f32 = 0.25;

/// Where a light carried by a player whose eye is at `eye` radiates from.
///
/// Deliberately a function of POSITION alone and not of facing. The cost is
/// that the light does not sit where the item is drawn — about half a block
/// off, which against `LIGHT_RANGE` is nothing.
///
/// What it buys, and neither is cosmetic. **Mouse-look never moves the light**,
/// so a player standing still and turning is not a moving emitter: no
/// invalidation fires, the resolve's screen history climbs back to `N_MAX`, and
/// a lit room settles as deeply as one lit by a torch on the floor. And **the
/// cell is guaranteed non-solid** — the player's own capsule occupies it — so
/// nothing can bury the light inside geometry when they brush a wall.
pub fn carried_origin(eye: glam::Vec3) -> glam::Vec3 {
	eye - glam::Vec3::Y * CARRY_DROP
}

/// What a held emitter casts at, for a wielder at this Mining level.
///
/// Every block answers with its own row and stops there — a torch in a hand is
/// the torch on the floor (R28). The Resonant Pick is the one exception, and a
/// deliberate one: the tool that breaks a whole plane resonates hard enough to
/// work by, so its glow steps up with the swing.
///
/// Keyed on [`crate::unlock::pick_cells`] reaching [`crate::unlock::PICK_PLANE`]
/// rather than a level of its own, because two thresholds for one rank drift
/// apart the first time either moves.
///
/// A remote's pick answers at its row's level: their skills are not in the
/// snapshot, and inventing a rank for someone else's tool would put a light in
/// the world that the player holding it cannot see.
pub fn carried_light(b: BlockType, mining: u32) -> CarriedLight {
	if b == BlockType::ResonantPick
		&& crate::unlock::pick_cells(mining) == crate::unlock::PICK_PLANE
	{
		return CarriedLight {
			level: PICK_RESONANT_LIGHT,
			whitened: true,
		};
	}
	CarriedLight {
		level: b.props().light,
		whitened: false,
	}
}

/// What a held emitter casts: how much, and in what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CarriedLight {
	/// The cast level, [`crate::block::BlockProps::light`]'s units.
	pub level: u8,
	/// Draw this light's tint pulled toward white.
	///
	/// The pick's authored blue is what makes its glow READ as resonance
	/// at a glance, and it is also why the tool is a poor lamp: a
	/// saturated blue carries almost no luminance, so a room lit by one
	/// is dim and uniformly blue however high the level goes. At the
	/// resonant rank the tool stops being a signal and becomes a light to
	/// work by — brighter, and less blue for the same reason a hot flame
	/// is less orange. It rides the light-list ENTRY beside the level,
	/// not the class, because it is a property of the wielder.
	pub whitened: bool,
}

/// What the Resonant Pick casts at once it breaks the full plane — see
/// [`carried_light`], the only thing that reads it.
///
/// Set against the WHITENED tint rather than the authored one, which is
/// most of the answer to why it is not higher: whitening more than
/// doubles what the same level delivers, so this lands the resonant pick
/// as the brightest thing a hand can hold short of a torch, and the step
/// up from its own first rank is one a player cannot miss.
const PICK_RESONANT_LIGHT: u8 = 12;

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
		// A translucent shaped cell (a crystal slab, a pane) skips the
		// synthetic partial classes and answers its BASE id: the GI
		// approximates it as a full see-through cell, since a box that
		// attenuates instead of blocking has no home in the box words.
		if b.props().bucket != crate::block::MeshBucket::Translucent {
			let key = ((b as usize) << 4) | crate::chunk::cell_state(cell) as usize;
			return SHAPE_CLASSES.1[key];
		}
	}
	b as u8
}

/// How a full-cube class attenuates rays (LUT-side mirror; also the
/// coarse rule). Shaped cells don't route through this — their
/// synthetic classes are partial by construction — EXCEPT translucent
/// ones, which [`gi_class`] hands back as their base id so they land
/// here like any full cube.
///
/// The see-through test is the BUCKET, not a class: a translucent
/// shaped row keeps `RenderClass::Shaped`, and `Translucent`-the-class
/// still means water (the marches' op-1 path-length counter). Glass is
/// `Tinted`: light passes, filtered by the class's own transmittance.
#[inline]
pub fn opacity(b: BlockType) -> GiOpacity {
	if b.props().bucket == crate::block::MeshBucket::Translucent {
		return GiOpacity::Tinted;
	}
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
				// A SURFACE emitter is not a point source and does not
				// belong in this list: a vein radiates from the faces it
				// opens into, and most of a vein opens into nothing. The
				// mesher — the one pass that is already deciding, face by
				// face, what is exposed — hands the renderer those
				// rectangles instead (GI-DESIGN.md R31). Placing a point
				// here as well would light buried rock AND double every
				// exposed cell.
				let emits = b.props().light > 0 && !b.props().surface_light;
				if emits {
					emitters.push((x as u8, y as u8, z as u8, class));
				}
				// A partial cell sets the blocker bit too: the coarse
				// texture only licenses SKIPPING a brick, and a brick
				// holding a slab must still be marched. Tinted cells set
				// it for the same reason — a skipped brick accumulates no
				// throughput, so a wall of panes would go untinted.
				let blocks_rays = class >= SHAPE_CLASS_BASE
					|| matches!(opacity(b), GiOpacity::Opaque | GiOpacity::Tinted);
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

	/// The see-through predicate is the BUCKET, swept over every block:
	/// a translucent row reads as tinted transmission to the rays, water
	/// alone reads as the water counter, and everything solid blocks. Derived from the same fields the table asserts, so
	/// a new row lands in exactly one arm or fails here by name.
	#[test]
	fn the_bucket_is_the_see_through_predicate() {
		use crate::block::{MeshBucket, RenderClass};
		// Set equality, both directions, rather than a re-spelling of
		// `opacity`'s arms: the bucket names EXACTLY the tinted blocks,
		// and `Translucent`-the-class still names exactly the water the
		// marches count path length through. Written as memberships, a
		// row that gained the bucket without the op — or the reverse —
		// fails here by name, while a retune of the arms' ORDER does not.
		let (mut tinted, mut water) = (0u32, 0u32);
		for b in BlockType::ALL {
			let p = b.props();
			assert_eq!(
				p.bucket == MeshBucket::Translucent,
				opacity(b) == GiOpacity::Tinted,
				"{b:?}: the bucket and the tinted op name one set"
			);
			// Water is the OTHER see-through thing, and it must not have
			// drifted into the tinted op: it is a participating medium
			// with a path length, not a filter product.
			assert_eq!(
				p.class == RenderClass::Translucent && p.bucket != MeshBucket::Translucent,
				opacity(b) == GiOpacity::Translucent,
				"{b:?}: the water op stays the water class"
			);
			tinted += u32::from(opacity(b) == GiOpacity::Tinted);
			water += u32::from(opacity(b) == GiOpacity::Translucent);
		}
		// Neither sweep may pass by examining nothing.
		assert!(tinted > 0, "the bucket has members to sweep");
		assert!(water > 0, "the water class has members to sweep");
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
				// A translucent shaped block skips the synthetic classes
				// on purpose: the GI takes it as a full see-through cell
				// of its base id, boxes and all (see `gi_class`).
				if b.props().bucket == crate::block::MeshBucket::Translucent {
					assert_eq!(class, b as u8, "{b:?}");
					continue;
				}
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
		// A tinted cell PINS its brick — a skipped brick accumulates no
		// throughput, so a wall of panes would go untinted — but it
		// still emits nothing.
		let mut pane = ChunkData::default();
		pane.set(0, 0, 0, BlockType::Glass);
		let g = classify_chunk(&pane);
		assert_eq!(g.coarse[0], 0b01, "glass: pins the brick, no emission");
		assert_eq!(opacity(BlockType::Glass), GiOpacity::Tinted);
	}
}
