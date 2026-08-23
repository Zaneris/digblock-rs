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

/// EVERY shaped block, every valid state — the cap sizes the renderer's
/// shared index buffer, so a family missing from this sweep (the
/// furniture was, once) can overflow it without a test noticing.
#[test]
fn every_facing_bakes_and_stays_under_the_quad_cap() {
	use crate::chunk::make_cell;
	let mut max = 0;
	let mut swept_blocks = 0;
	for b in BlockType::ALL {
		if b.props().shape.is_none() {
			continue;
		}
		swept_blocks += 1;
		let mut variants = 0;
		for state in 0..16u8 {
			if !state_valid(make_cell(b, state)) {
				continue;
			}
			let v = shape_of(b, state)
				.unwrap_or_else(|| panic!("{b:?} state {state} is valid but has no shape"));
			assert!(!v.quads.is_empty(), "{b:?} state {state} bakes nothing");
			variants += 1;
			max = max.max(v.quads.len());
		}
		assert!(variants > 0, "{b:?} has no valid state to bake");
	}
	assert!(
		swept_blocks > 4,
		"the shaped catalog shrank to {swept_blocks}"
	);
	{
		let v = shape_of(BlockType::Torch, 0).unwrap();
		assert_eq!(v.ramp, None);
		assert!(
			v.occludes_full.iter().all(|f| !f),
			"a torch hides no neighbor face"
		);
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

/// A door is one object across two cells, so `object_bounds` answers the same anchor and the same
/// box from either half: the pair's lower cell, and a box two cells tall. The union has to be a
/// real union rather than the lower half stretched — the handle protrudes past the leaf on the
/// lower half and the lites do not, so it is the LOWER half that sets the thin axis and the UPPER
/// half's box does not reach it.
#[test]
fn a_door_bounds_as_one_object() {
	use crate::chunk::make_cell;
	let lower = (3, 40, 7);
	let upper = (lower.0, lower.1 + 1, lower.2);
	for f in Facing::ALL {
		for open in [false, true] {
			let at =
				|c, up| object_bounds(c, make_cell(BlockType::LumberDoor, door_state(f, open, up)));
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
					draw_bounds(make_cell(BlockType::LumberDoor, door_state(f, open, up))).unwrap();
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
		let batch = craft_batch(b);
		match b.grid_cell() {
			// Matter conservation, per pair: a cube in, at most a
			// cube's volume out.
			Some((m, s)) if m.feedstock().is_some() => {
				assert!(batch >= 1, "{b:?}");
				assert!(
					batch * s.units() <= SUBS_PER_BLOCK,
					"{b:?}: one cube cannot cut {batch} pieces of {} units",
					s.units()
				);
			}
			_ => assert_eq!(batch, 1, "{b:?} — essence recipes buy singly"),
		}
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
