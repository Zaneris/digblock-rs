use super::*;

#[test]
fn rotation_is_exact_and_cyclic() {
	// 4 × 90° = identity, and each step preserves volume.
	let v0 = raster_volume(&LEGACY_STAIR_BOXES);
	for k in 0..=4u8 {
		let r = rotate_boxes(&LEGACY_STAIR_BOXES, k);
		assert_eq!(raster_volume(&r), v0, "k={k} preserves volume");
	}
	let full = rotate_boxes(&LEGACY_STAIR_BOXES, 4);
	assert_eq!(&full[..], &LEGACY_STAIR_BOXES[..], "4 rotations = identity");
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
			// A third of the grid, or two thirds: the name is the height.
			let h = GRID / 3 * if two_thirds { 2 } else { 1 };
			let v = shape_of(BlockType::WoodSlab, slab_state(two_thirds, top)).unwrap();
			assert_eq!(v.collision.len(), 1);
			let want = if top {
				SubBox::new([0, GRID - h, 0], [GRID, GRID, GRID])
			} else {
				SubBox::new([0, 0, 0], [GRID, h, GRID])
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
	// Validates the .shape FILE: an editor edit breaking the mirror breaks
	// the equal-volume/equal-cost invariant, and fails here first.
	let d = def::of(ShapeKind::Slab, ShapeStyle::Plain);
	for two_thirds in [false, true] {
		let bottom = d.state_boxes(slab_state(two_thirds, false))[0].sub;
		let top = d.state_boxes(slab_state(two_thirds, true))[0].sub;
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
			assert!(!closed.interaction.is_empty(), "upper={upper}");
			assert_eq!(closed.interaction[0], closed.collision[0]);
			assert!(closed.occludes_full.iter().all(|&f| !f), "upper={upper}");
			assert!(open.collision.is_empty());
			assert_eq!(open.interaction.len(), closed.interaction.len());
			assert_ne!(
				open.interaction, closed.interaction,
				"open leaf is the swung pose"
			);
		}
	}
	// The upper closed panel has see-through lites: strictly fewer
	// filled sub-cells than the uncut lower leaf, while both halves
	// share one uncut collision leaf.
	let d = def::of(ShapeKind::Door, ShapeStyle::Plain);
	let cell = |cy: i32, pick: fn(def::Roles) -> bool| -> Vec<SubBox> {
		slice_cell(&d.boxes, (0, cy, 0))
			.iter()
			.filter(|b| pick(b.roles))
			.map(|b| b.sub)
			.collect()
	};
	let lower = raster_volume(&cell(0, |r| r.visual));
	let upper = raster_volume(&cell(1, |r| r.visual));
	// The uncut leaf is what physics meets, in either half.
	assert_eq!(
		raster_volume(&cell(0, |r| r.collision)),
		raster_volume(&cell(1, |r| r.collision))
	);
	assert!(upper < lower, "lites carved: {upper} < {lower}");
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
	// Ported families price from their .shape FILE, so an editor edit that
	// moves a box moves the recipe cost and fails here rather than silently.
	// A family prices from its kind's PLAIN file: an alternate look is the
	// same form in a different dress, and costs the same.
	let def_volume = |kind, state: u8| {
		let boxes: Vec<SubBox> = def::of(kind, ShapeStyle::Plain)
			.state_boxes(state)
			.iter()
			.map(|b| b.sub)
			.collect();
		raster_volume(&boxes)
	};
	let volume = |s: ShapeFamily| match s {
		ShapeFamily::Block => SUBS_PER_BLOCK * SUB_UNIT_VOLUME,
		ShapeFamily::Stairs => def_volume(ShapeKind::Stairs, 0),
		// A door prices BOTH cells of its footprint, every box (uncut
		// leaves — the visual-only lites don't discount the collision one).
		ShapeFamily::Door => {
			let d = def::of(ShapeKind::Door, ShapeStyle::Plain);
			let all = |cy: i32| -> Vec<SubBox> {
				slice_cell(&d.boxes, (0, cy, 0))
					.iter()
					.map(|b| b.sub)
					.collect()
			};
			raster_volume(&all(0)) + raster_volume(&all(1))
		}
		ShapeFamily::Slab => def_volume(ShapeKind::Slab, 0),
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
	// Matter conservation, per form: the cubes a craft takes hold at
	// least the volume of the pieces it cuts, and no more than a piece's
	// worth goes to waste — the stairs used to throw a third of every cube
	// away, and now take two cubes for three.
	for s in ShapeFamily::ALL {
		let (cubes, pieces) = s.batch();
		assert!(cubes >= 1 && pieces >= 1, "{s:?}");
		assert!(
			pieces * s.units() <= cubes * SUBS_PER_BLOCK,
			"{s:?}: {cubes} cubes cannot cut {pieces} pieces of {} units",
			s.units()
		);
		assert!(
			cubes * SUBS_PER_BLOCK - pieces * s.units() < s.units(),
			"{s:?} wastes a whole piece's worth per craft"
		);
	}
	for &b in &BlockType::ALL {
		let batch = craft_batch(b);
		match b.grid_cell() {
			Some((_, s)) => assert_eq!(batch, s.batch().1, "{b:?}"),
			None => assert_eq!(batch, 1, "{b:?} — hand-priced items buy singly"),
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

// --- .shape port pins ---------------------------------------------------
//
// The retired hardcoded geometry, kept verbatim as LEGACY_* pins: each
// ported family must bake BYTE-IDENTICAL to what these consts built, which
// is what lets the port ship without a SIM_VERSION bump (collision feeds
// Player::tick) and with every golden untouched. A pin dies only with the
// first INTENTIONAL edit to that shape — which then follows the golden
// discipline instead (re-pin, and bump SIM_VERSION if collision moved).

const LEGACY_TORCH_BOXES: [SubBox; 1] = [SubBox::new([5, 0, 5], [7, 10, 7])];

const LEGACY_STAIR_BOXES: [SubBox; 3] = [
	SubBox::new([0, 0, 0], [12, 4, 12]),
	SubBox::new([4, 4, 0], [12, 8, 12]),
	SubBox::new([8, 8, 0], [12, 12, 12]),
];

const LEGACY_SLAB_BOXES: [SubBox; 4] = [
	SubBox::new([0, 0, 0], [12, 4, 12]),
	SubBox::new([0, 0, 0], [12, 8, 12]),
	SubBox::new([0, 8, 0], [12, 12, 12]),
	SubBox::new([0, 4, 0], [12, 12, 12]),
];

const LEGACY_TABLE_BOXES: [SubBox; 5] = [
	SubBox::new([0, 10, 0], [12, 12, 12]),
	SubBox::new([1, 0, 1], [3, 10, 3]),
	SubBox::new([9, 0, 1], [11, 10, 3]),
	SubBox::new([1, 0, 9], [3, 10, 11]),
	SubBox::new([9, 0, 9], [11, 10, 11]),
];

const LEGACY_CHAIR_BOXES: [SubBox; 6] = [
	SubBox::new([2, 6, 2], [10, 8, 10]),
	SubBox::new([8, 8, 2], [10, 12, 10]),
	SubBox::new([2, 0, 2], [4, 6, 4]),
	SubBox::new([8, 0, 2], [10, 6, 4]),
	SubBox::new([2, 0, 8], [4, 6, 10]),
	SubBox::new([8, 0, 8], [10, 6, 10]),
];

const LEGACY_BED_BOXES: [SubBox; 6] = [
	SubBox::new([0, 3, 1], [12, 6, 11]),
	SubBox::new([8, 6, 2], [11, 8, 10]),
	SubBox::new([0, 0, 1], [2, 3, 3]),
	SubBox::new([0, 0, 9], [2, 3, 11]),
	SubBox::new([10, 0, 1], [12, 3, 3]),
	SubBox::new([10, 0, 9], [12, 3, 11]),
];

/// The old `furniture()` assembly, verbatim: all roles share the rotated
/// boxes, no swung pose, no ramp.
fn legacy_furniture(canonical: &[SubBox], k: u8) -> ShapeVariant {
	let boxes = rotate_boxes(canonical, k);
	let baked = bake(&boxes);
	ShapeVariant {
		collision: boxes.clone(),
		light: boxes.clone(),
		interaction: boxes,
		quads: baked.quads,
		face_masks: baked.face_masks,
		occludes_full: baked.occludes_full,
		bounds: baked.bounds,
		ramp: None,
	}
}

/// The old torch assembly, verbatim: walked through (no collision) while
/// light and the interact ray still meet it.
fn legacy_torch() -> ShapeVariant {
	let boxes = LEGACY_TORCH_BOXES.to_vec();
	let baked = bake(&boxes);
	ShapeVariant {
		collision: Vec::new(),
		light: boxes.clone(),
		interaction: boxes,
		quads: baked.quads,
		face_masks: baked.face_masks,
		occludes_full: baked.occludes_full,
		bounds: baked.bounds,
		ramp: None,
	}
}

/// Exact equality across every field, ORDER INCLUDED for the role lists —
/// collision order reaches the physics sweep, so even a reordering port
/// is a port that must explain itself.
fn assert_variant_identical(new: &ShapeVariant, legacy: &ShapeVariant, ctx: &str) {
	assert_eq!(new.collision, legacy.collision, "{ctx}: collision");
	assert_eq!(new.light, legacy.light, "{ctx}: light");
	assert_eq!(new.interaction, legacy.interaction, "{ctx}: interaction");
	assert_eq!(new.quads, legacy.quads, "{ctx}: quads");
	assert_eq!(new.face_masks, legacy.face_masks, "{ctx}: face_masks");
	assert_eq!(new.occludes_full, legacy.occludes_full, "{ctx}: occludes");
	assert_eq!(new.bounds, legacy.bounds, "{ctx}: bounds");
	assert_eq!(new.ramp, legacy.ramp, "{ctx}: ramp");
}

#[test]
fn ported_families_stay_byte_identical_to_the_legacy_consts() {
	assert_variant_identical(&TORCHES[0], &legacy_torch(), "torch");
	assert_variant_identical(
		&TABLES[0],
		&legacy_furniture(&LEGACY_TABLE_BOXES, 0),
		"table",
	);
	for f in Facing::ALL {
		let k = f as u8;
		assert_variant_identical(
			&CHAIRS[k as usize],
			&legacy_furniture(&LEGACY_CHAIR_BOXES, k),
			&format!("chair k={k}"),
		);
		assert_variant_identical(
			&BEDS[k as usize],
			&legacy_furniture(&LEGACY_BED_BOXES, k),
			&format!("bed k={k}"),
		);
		// The old STAIRS assembly, verbatim: furniture plus the ramp tag.
		let mut stair = legacy_furniture(&LEGACY_STAIR_BOXES, k);
		stair.ramp = Some(f);
		assert_variant_identical(
			&STAIRS[ShapeStyle::Plain as usize][k as usize],
			&stair,
			&format!("stairs k={k}"),
		);
	}
	// The old SLABS assembly, verbatim: one unrotated box per state.
	for state in 0..4usize {
		let slab = legacy_furniture(&LEGACY_SLAB_BOXES[state..=state], 0);
		assert_variant_identical(&SLABS[state], &slab, &format!("slab state {state}"));
	}
}

#[test]
fn every_shape_asset_parses_and_bakes() {
	// Forces the loader's cross-checks AND every ported family's bake, so
	// a malformed committed file fails `just check` long before a client
	// or server boots it.
	for &(name, src) in def::ASSETS {
		let d = def::parse(src).unwrap_or_else(|e| panic!("{name}: {e}"));
		assert_eq!(d.name, name);
		let kind = def::kind_by_name(name).unwrap_or_else(|| panic!("{name} is a kind"));
		// No orphan file: every asset is some (kind, look) pair's.
		assert!(
			ShapeStyle::ALL
				.iter()
				.any(|&s| def::asset_name(kind, s) == name),
			"{name} is no look of {kind:?}"
		);
	}
	// Every styled row names a file the roster has (`of` panics otherwise),
	// and only the stairs have per-look tables — a styled row of any other
	// kind would bake plain and never say so.
	for &b in &BlockType::ALL {
		let p = b.props();
		if let Some(kind) = p.shape {
			let _ = def::of(kind, p.shape_style);
			assert!(
				p.shape_style == ShapeStyle::Plain || kind == ShapeKind::Stairs,
				"{b:?}: only the stairs bake per look"
			);
		}
	}
	// The table index is the discriminant.
	for (i, s) in ShapeStyle::ALL.iter().enumerate() {
		assert_eq!(*s as usize, i);
	}
	prewarm();
}

#[test]
fn shape_files_round_trip_through_emit() {
	// parse(emit(parse(x))) is a fixpoint (comments are the one loss).
	for &(name, src) in def::ASSETS {
		let d = def::parse(src).unwrap();
		let text = def::emit(&d);
		let d2 = def::parse(&text).unwrap_or_else(|e| panic!("{name} re-parse: {e}"));
		assert_eq!(d, d2, "{name}");
		assert_eq!(def::emit(&d2), text, "{name}: emit is a fixpoint");
	}
}

#[test]
fn the_shape_grammar_rejects_malformed_input() {
	let ok = "version 1\nshape chair\ngrid 12\nstates facing\nbox all 0 0 0 12 12 12\n";
	assert!(def::parse(ok).is_ok());
	for (broken, why) in [
		(
			"shape chair\ngrid 12\nstates none\nbox all 0 0 0 1 1 1\n",
			"version first",
		),
		(
			"version 2\nshape x\ngrid 12\nstates none\nbox all 0 0 0 1 1 1\n",
			"unknown version",
		),
		(
			"version 1\nshape x\ngrid 16\nstates none\nbox all 0 0 0 1 1 1\n",
			"wrong grid",
		),
		(
			"version 1\nshape x\ngrid 12\nbox all 0 0 0 1 1 1\n",
			"box before states",
		),
		(
			"version 1\ngrid 12\nstates none\nbox all 0 0 0 1 1 1\n",
			"missing shape",
		),
		("version 1\nshape x\ngrid 12\nstates none\n", "no boxes"),
		(
			"version 1\nshape x\ngrid 12\nstates none\nbox all 0 0 0 13 1 1\n",
			"out of range",
		),
		(
			"version 1\nshape x\ngrid 12\nstates none\nbox all 4 0 0 4 1 1\n",
			"min == max",
		),
		(
			"version 1\nshape x\ngrid 12\nstates none\nbox glow 0 0 0 1 1 1\n",
			"unknown role",
		),
		(
			"version 1\nshape x\ngrid 12\nstates none\nbox all 0 0 0 1 1 1 side=NOPE\n",
			"bad texture",
		),
		(
			"version 1\nshape x\ngrid 12\nstates explicit 2\nstate 0\nbox all 0 0 0 1 1 1\n",
			"missing state section",
		),
		(
			"version 1\nshape x\ngrid 12\nstates explicit 1\nstate 0\nstate 1\n",
			"extra state section",
		),
		(
			"version 1\nshape x\ngrid 12\nfootprint 2 2 2\nstates none\nbox all 0 0 0 1 1 1\n",
			"too many parts",
		),
		(
			"version 1\nshape x\ngrid 12\nstates door\nbox all 0 0 0 12 12 12\n",
			"swing envelope",
		),
		(
			"version 1\nshape x\ngrid 12\nstates none\nramp\nbox all 0 0 0 1 1 1\nwibble\n",
			"unknown directive",
		),
	] {
		assert!(def::parse(broken).is_err(), "accepted: {why}");
	}
}

#[test]
fn the_shape_grammar_reads_roles_footprints_and_textures() {
	use crate::block::tex;
	let src = "version 1\nshape x\ngrid 12\nfootprint 1 2 1\nstates door\n\
		box visual+interaction 0 9 9 4 11 11 all=METAL\n\
		box collision+light 1 12 0 3 24 12 top=STONE side=LUMBER_DARK\n";
	let d = def::parse(src).unwrap();
	assert_eq!(d.footprint, [1, 2, 1]);
	assert_eq!(d.states, def::StateModel::Door);
	let b0 = &d.boxes[0];
	assert!(b0.roles.visual && b0.roles.interaction);
	assert!(!b0.roles.collision && !b0.roles.light);
	assert_eq!(b0.tex, [Some(tex::METAL); 3]);
	let b1 = &d.boxes[1];
	assert_eq!(b1.sub, SubBox::new([1, 12, 0], [3, 24, 12]));
	assert_eq!(b1.tex, [Some(tex::STONE), Some(tex::LUMBER_DARK), None]);
	// Textures survive emit by NAME.
	let text = def::emit(&d);
	assert!(text.contains("all=METAL"), "{text}");
	assert!(text.contains("top=STONE"), "{text}");
	assert_eq!(def::parse(&text).unwrap(), d);
}

#[test]
fn texture_overrides_split_quads_and_the_later_box_wins() {
	use crate::block::tex;
	// Two flush half-slabs, sides overridden differently: no seam exists in
	// the union, but the side faces may not merge across the texture split.
	let stone = [Some(tex::STONE); 3];
	let metal = [Some(tex::METAL); 3];
	let split = bake_tex(&[
		(SubBox::new([0, 0, 0], [12, 4, 6]), stone),
		(SubBox::new([0, 0, 6], [12, 4, 12]), metal),
	]);
	let plain = bake(&[SubBox::new([0, 0, 0], [12, 4, 12])]);
	assert_eq!(
		split.face_masks, plain.face_masks,
		"coverage ignores textures"
	);
	assert!(
		split.quads.len() > plain.quads.len(),
		"the split shows in the quads"
	);
	for q in &split.quads {
		let want = match q.dir {
			// The z=6 plane splits the West/East/Top/Bottom faces; the two
			// z boundary faces each belong wholly to one box.
			Dir::South => stone[0],
			Dir::North => metal[0],
			_ => {
				assert!(q.tex.is_some(), "{q:?} untagged");
				continue;
			}
		};
		assert_eq!(q.tex, want, "{q:?}");
	}
	// Overlap: a later box repaints the sub-cells it covers.
	let over = bake_tex(&[
		(SubBox::new([0, 0, 0], [12, 4, 12]), stone),
		(SubBox::new([0, 0, 6], [12, 4, 12]), metal),
	]);
	assert_eq!(over.quads.len(), split.quads.len());
	for (a, b) in over.quads.iter().zip(&split.quads) {
		assert_eq!(a, b, "overlap paints the same result as flush boxes");
	}
	// And with NO overrides the tagged path is the plain path, bit for bit.
	for q in &plain.quads {
		assert_eq!(q.tex, None);
	}
}

// --- multi-cell machinery ----------------------------------------------

/// The generic footprint machinery must agree with the door's legacy pair
/// formula for all 16 states — `door_partner` is now a wrapper over it,
/// and the server's derived writes ride the same derivation.
#[test]
fn object_cells_reproduces_the_door_pair_for_all_16_states() {
	use crate::chunk::make_cell;
	let cell = (7, 40, -3);
	for state in 0..16u8 {
		let (_, _, upper) = door_parts(state);
		let legacy = (cell.0, cell.1 + if upper { -1 } else { 1 }, cell.2);
		assert_eq!(door_partner(cell, state), legacy, "state {state}");
		let value = make_cell(BlockType::LumberDoor, state);
		let cells = object_cells(cell, value);
		assert_eq!(cells.len(), 2, "state {state}");
		// [0] is the anchor (lower, part 0) wearing part 0's state; [1] the
		// upper wearing the upper bit — the exact value the server writes.
		let lower_cell = if upper { legacy } else { cell };
		assert_eq!(cells[0].0, lower_cell, "state {state}");
		assert_eq!(cells[1].0, (lower_cell.0, lower_cell.1 + 1, lower_cell.2));
		assert_eq!(cells[0].1, make_cell(BlockType::LumberDoor, state & 0b0111));
		assert_eq!(cells[1].1, make_cell(BlockType::LumberDoor, state | 0b1000));
		assert_eq!(part_index(cells[0].1), 0);
		assert_eq!(part_index(cells[1].1), 1);
		// The wire's placement grammar: only the closed anchor is canonical.
		assert_eq!(place_canonical(value), state & 0b1100 == 0, "state {state}");
	}
	assert!(is_multi_cell(BlockType::LumberDoor));
	assert!(!is_multi_cell(BlockType::Chair));
	assert!(!is_multi_cell(BlockType::Stone));
	// Single-cell objects answer themselves.
	let chair = make_cell(BlockType::Chair, 2);
	assert_eq!(object_cells(cell, chair), vec![(cell, chair)]);
	assert!(place_canonical(chair));
}

/// A rotated multi-cell footprint turns its part offsets with the facing,
/// and every part agrees on the object's cells whichever part you ask.
#[test]
fn part_offsets_turn_with_the_facing_and_agree_from_any_part() {
	let l = StateLayout {
		facing: true,
		open_bit: None,
		part_shift: 2,
		footprint: [2, 1, 1],
	};
	// Canonical +x: part 1 sits one cell toward +x; each k turns it CCW.
	assert_eq!(part_offset(&l, 1, 0), (1, 0, 0));
	assert_eq!(part_offset(&l, 1, 1), (0, 0, 1));
	assert_eq!(part_offset(&l, 1, 2), (-1, 0, 0));
	assert_eq!(part_offset(&l, 1, 3), (0, 0, -1));
	// And a door's derivation is stable from either half.
	use crate::chunk::make_cell;
	for state in 0..16u8 {
		let value = make_cell(BlockType::LumberDoor, state);
		let cells = object_cells((2, 5, 2), value);
		for &(c, v) in &cells {
			assert_eq!(object_cells(c, v), cells, "state {state} from {c:?}");
		}
	}
}

const LEGACY_LEAF: SubBox = SubBox::new([1, 0, 0], [3, 12, 12]);
const LEGACY_HANDLE: SubBox = SubBox::new([0, 9, 9], [4, 11, 11]);
const LEGACY_HINGE_LOWER: SubBox = SubBox::new([3, 1, 0], [4, 3, 1]);
const LEGACY_HINGE_UPPER: SubBox = SubBox::new([3, 9, 0], [4, 11, 1]);
const LEGACY_DOOR_UPPER_BOXES: [SubBox; 6] = [
	SubBox::new([1, 0, 0], [3, 2, 12]),
	SubBox::new([1, 10, 0], [3, 12, 12]),
	SubBox::new([1, 2, 0], [3, 10, 2]),
	SubBox::new([1, 2, 10], [3, 10, 12]),
	SubBox::new([1, 2, 5], [3, 10, 7]),
	SubBox::new([1, 5, 2], [3, 7, 10]),
];

/// The old DOORS assembly, verbatim: per-half authored lists, swing before
/// the facing rotation, collision as the uncut leaf, open = empty
/// collision/light and a swung interaction.
fn legacy_door(state: u8) -> ShapeVariant {
	let (facing, open, upper) = door_parts(state);
	let k = facing as u8;
	let mut panel: Vec<SubBox> = if upper {
		let mut p = LEGACY_DOOR_UPPER_BOXES.to_vec();
		p.push(LEGACY_HINGE_UPPER);
		p
	} else {
		vec![LEGACY_LEAF, LEGACY_HANDLE, LEGACY_HINGE_LOWER]
	};
	let mut slab = vec![LEGACY_LEAF];
	let mut interact = if upper {
		vec![LEGACY_LEAF]
	} else {
		vec![LEGACY_LEAF, LEGACY_HANDLE]
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
		light: if open { Vec::new() } else { slab.clone() },
		collision: if open { Vec::new() } else { slab },
		interaction: interact,
		quads: baked.quads,
		face_masks: baked.face_masks,
		occludes_full: baked.occludes_full,
		bounds: baked.bounds,
		ramp: None,
	}
}

#[test]
fn the_ported_door_stays_byte_identical_across_all_16_states() {
	for state in 0..16u8 {
		assert_variant_identical(
			&DOORS[state as usize],
			&legacy_door(state),
			&format!("door state {state}"),
		);
	}
}

#[test]
fn preview_overrides_hijack_shape_of_and_clear() {
	// (Stone, 5) is a pair nothing else asks `shape_of` about — the other
	// tests run in parallel against the same statics, so this test must
	// not shadow a variant any of them read.
	let b = BlockType::Stone;
	assert!(shape_of(b, 5).is_none(), "unshaped without an override");
	let list = [def::DefBox {
		sub: SubBox::new([0, 0, 0], [12, 6, 12]),
		roles: def::Roles::ALL,
		tex: [None; 3],
	}];
	let v = preview_variant(&list, (0, 0, 0), 0);
	assert_eq!(v.collision, vec![SubBox::new([0, 0, 0], [12, 6, 12])]);
	assert!(!v.quads.is_empty());
	set_preview_override(b, 5, v);
	let got = shape_of(b, 5).expect("override answers");
	assert_eq!(got.collision.len(), 1);
	// A replaced override swaps in place; a slice from another cell of a
	// footprint is empty geometry but still a variant.
	set_preview_override(b, 5, preview_variant(&list, (1, 0, 0), 0));
	assert!(
		shape_of(b, 5).unwrap().quads.is_empty(),
		"sliced past the box"
	);
	clear_preview_overrides();
	assert!(shape_of(b, 5).is_none(), "cleared");
}

// --- twelfth-step ramps -------------------------------------------------

/// Stairs of one-twelfth steps ascending +x, `steps` treads, across z in
/// [z0, z1): tread i spans x in [i, 12) at height i.
fn twelfth_stairs(steps: u8, z0: u8, z1: u8) -> Vec<SubBox> {
	(0..steps)
		.map(|i| SubBox::new([i, i, z0], [GRID, i + 1, z1]))
		.collect()
}

fn ramps(b: &Baked) -> Vec<&ShapeQuad> {
	b.quads.iter().filter(|q| q.is_ramp()).collect()
}

fn tris(b: &Baked) -> usize {
	b.quads.iter().filter(|q| q.is_tri()).count()
}

#[test]
fn a_run_of_twelfth_steps_bakes_to_one_ramp_and_two_side_triangles() {
	let b = bake(&twelfth_stairs(12, 0, GRID));
	// Ramp, two side triangles, and per side the y=0 row and the top
	// column; then the top tread, the bottom, the back, the first riser.
	assert_eq!(b.quads.len(), 11, "{:#?}", b.quads);
	assert_eq!(tris(&b), 2);
	let r = ramps(&b);
	assert_eq!(r.len(), 1);
	let r = r[0];
	assert_eq!(r.dir, Dir::Top);
	assert!(!r.boundary);
	// The plane rises one per twelfth from the first tread's low edge
	// (x=0, y=1) to the last riser's top (x=11, y=12), across all z.
	for c in r.corners {
		assert_eq!(
			u32::from(c[1]),
			u32::from(c[0]) + 1,
			"corner {c:?} off the plane"
		);
		assert!(c[2] == 0 || c[2] == GRID);
		assert!(c[0] == 0 || c[0] == 11);
	}
	// Nothing flat remains where the ramp is: every Top quad left is the
	// last tread at y = 12.
	for q in b.quads.iter().filter(|q| q.dir == Dir::Top && !q.is_ramp()) {
		assert!(q.corners.iter().all(|c| c[1] == GRID), "{q:?}");
	}
	// Both side triangles are boundary quads with a valid cull rect.
	for q in b.quads.iter().filter(|q| q.is_tri()) {
		assert!(q.boundary);
		let [a0, b0, a1, b1] = q.rect;
		assert!(a0 < a1 && a1 <= GRID && b0 < b1 && b1 <= GRID, "{q:?}");
	}
	// The masks are still the raster's: the +x back and the bottom are full.
	assert!(b.face_masks[Dir::West as usize].is_full());
	assert!(b.face_masks[Dir::Bottom as usize].is_full());
}

#[test]
fn a_single_twelfth_step_stays_boxy() {
	let b = bake(&twelfth_stairs(2, 0, GRID));
	assert!(ramps(&b).is_empty());
	assert_eq!(tris(&b), 0);
	assert!(b.quads.iter().all(|q| !q.is_ramp()));
}

#[test]
fn two_risers_are_enough_to_join() {
	let b = bake(&twelfth_stairs(3, 0, GRID));
	assert_eq!(ramps(&b).len(), 1);
	assert_eq!(tris(&b), 2);
	let r = ramps(&b)[0];
	let xs: Vec<u8> = r.corners.iter().map(|c| c[0]).collect();
	assert!(xs.iter().all(|&x| x == 0 || x == 2), "{xs:?}");
}

#[test]
fn a_ramp_joins_in_every_horizontal_direction() {
	for k in 0..4u8 {
		let boxes = rotate_boxes(&twelfth_stairs(12, 0, GRID), k);
		let b = bake(&boxes);
		assert_eq!(ramps(&b).len(), 1, "k={k}");
		assert_eq!(tris(&b), 2, "k={k}");
		assert_eq!(b.quads.len(), 11, "k={k}");
	}
}

#[test]
fn a_wall_beside_the_ramp_hides_that_side_triangle() {
	let mut boxes = twelfth_stairs(12, 0, 6);
	boxes.push(SubBox::new([0, 0, 6], [GRID, GRID, GRID]));
	let b = bake(&boxes);
	assert_eq!(ramps(&b).len(), 1);
	assert_eq!(tris(&b), 1, "{:#?}", b.quads);
	let t = b.quads.iter().find(|q| q.is_tri()).unwrap();
	assert_eq!(t.dir, Dir::South);
}

#[test]
fn a_hollow_support_falls_back_to_one_filler_triangle_per_step() {
	// A diagonal string of single cells: a tread run on top and a soffit
	// run underneath both join, but with no solid wedge behind either,
	// only the first two steps merge with their one support cell into a
	// triangle; every step past that gets its own filler, per side.
	let boxes: Vec<SubBox> = (0..12u8)
		.map(|i| SubBox::new([i, i, 0], [i + 1, i + 1, GRID]))
		.collect();
	let b = bake(&boxes);
	let mut dirs: Vec<Dir> = ramps(&b).iter().map(|q| q.dir).collect();
	dirs.sort_by_key(|d| *d as u8);
	assert_eq!(dirs, [Dir::Bottom, Dir::Top]);
	assert_eq!(tris(&b), 2 * 2 * (1 + 9));
}

#[test]
fn ramp_pieces_wind_outward_under_the_shared_index_pattern() {
	assert_winds_outward(&bake(&twelfth_stairs(12, 0, GRID)));
}

/// `twelfth_stairs(12)` mirrored in y: column x is solid from y = 11 - x up.
fn upside_down_stairs() -> Vec<SubBox> {
	(0..12u8)
		.map(|i| SubBox::new([i, 11 - i, 0], [GRID, GRID - i, GRID]))
		.collect()
}

#[test]
fn a_soffit_run_joins_as_a_ramp_facing_down() {
	// The twelfth stairs upside down: a stringer whose underside climbs -x.
	let b = bake(&upside_down_stairs());
	assert_eq!(b.quads.len(), 11, "{:#?}", b.quads);
	assert_eq!(tris(&b), 2);
	let r = ramps(&b);
	assert_eq!(r.len(), 1);
	assert_eq!(r[0].dir, Dir::Bottom);
	// The plane follows the underside, y = 11 - x for x in [0, 11].
	for c in r[0].corners {
		assert_eq!(
			u32::from(c[0]) + u32::from(c[1]),
			11,
			"corner {c:?} off the plane"
		);
		assert!(c[0] == 0 || c[0] == 11);
	}
	for q in b
		.quads
		.iter()
		.filter(|q| q.dir == Dir::Bottom && !q.is_ramp())
	{
		assert!(q.corners.iter().all(|c| c[1] == 0), "{q:?}");
	}
	assert!(b.face_masks[Dir::Top as usize].is_full());
	assert!(b.face_masks[Dir::West as usize].is_full());
	assert_winds_outward(&b);
}

/// A wall whose plan view is the twelfth stairs: column x is solid for
/// z < x + 1, full height. Its sawtooth faces +z and -x.
fn sawtooth_wall() -> Vec<SubBox> {
	(0..12u8)
		.map(|k| SubBox::new([k, 0, 0], [k + 1, GRID, k + 1]))
		.collect()
}

#[test]
fn a_wall_run_joins_into_a_vertical_ramp() {
	let b = bake(&sawtooth_wall());
	assert_eq!(b.quads.len(), 11, "{:#?}", b.quads);
	let r = ramps(&b);
	assert_eq!(r.len(), 1);
	assert_eq!(r[0].dir, Dir::North);
	// The plane z = x + 1 for x in [0, 11], full height.
	for c in r[0].corners {
		assert_eq!(
			u32::from(c[2]),
			u32::from(c[0]) + 1,
			"corner {c:?} off the plane"
		);
		assert!(c[1] == 0 || c[1] == GRID);
	}
	// The side triangles lie on the top and the bottom.
	let mut dirs: Vec<Dir> = b
		.quads
		.iter()
		.filter(|q| q.is_tri())
		.map(|q| q.dir)
		.collect();
	dirs.sort_by_key(|d| *d as u8);
	assert_eq!(dirs, [Dir::Bottom, Dir::Top]);
	assert_winds_outward(&b);
}

#[test]
fn a_wall_run_joins_facing_the_other_way_too() {
	// Mirrored in z: the sawtooth faces -z and -x, the ramp faces South.
	let boxes: Vec<SubBox> = (0..12u8)
		.map(|k| SubBox::new([k, 0, 11 - k], [k + 1, GRID, GRID]))
		.collect();
	let b = bake(&boxes);
	assert_eq!(b.quads.len(), 11, "{:#?}", b.quads);
	let r = ramps(&b);
	assert_eq!(r.len(), 1);
	assert_eq!(r[0].dir, Dir::South);
	for c in r[0].corners {
		assert_eq!(
			u32::from(c[0]) + u32::from(c[2]),
			11,
			"corner {c:?} off the plane"
		);
	}
	assert_eq!(tris(&b), 2);
	assert_winds_outward(&b);
}

#[test]
fn a_thin_diagonal_wall_gets_a_ramp_on_both_faces() {
	// One cell thick in plan: both sawtooth faces join, and with nothing
	// solid behind either, each side gets one merged triangle over the
	// first two steps and a filler per step after, on top and below.
	let boxes: Vec<SubBox> = (0..12u8)
		.map(|k| SubBox::new([k, 0, k], [k + 1, GRID, k + 1]))
		.collect();
	let b = bake(&boxes);
	let mut dirs: Vec<Dir> = ramps(&b).iter().map(|q| q.dir).collect();
	dirs.sort_by_key(|d| *d as u8);
	assert_eq!(dirs, [Dir::North, Dir::South]);
	assert_eq!(tris(&b), 2 * 2 * (1 + 9));
	assert_winds_outward(&b);
}

/// Two twelfth-thick stringers with flat tops and climbing undersides,
/// three flat treads between them: the editor's stairs test. Each
/// stringer's soffit joins into one ramp facing down. The first stringer
/// is solid over its whole run, one triangle per side; the second thins
/// out past its fourth step from the top, so each side gets one triangle
/// over those four and a small one per remaining step (five).
#[test]
fn stringer_soffits_join_and_flat_treads_stay() {
	let src = "version 1\nshape stairs_test\ngrid 12\nstates facing\nramp\n\
box all  0 0 2  4 4 3\nbox all  0 0 9  3 4 10\nbox all  4 1 2  5 8 3\nbox all  3 1 9  4 4 10\n\
box all  5 2 2  6 8 3\nbox all  4 2 9  5 8 10\nbox all  0 3 0  4 4 2\nbox all  6 3 2  7 8 3\n\
box all  0 3 3  4 4 9\nbox all  5 3 9  6 8 10\nbox all  0 3 10  4 4 12\nbox all  7 4 2  8 8 3\n\
box all  6 4 9  7 8 10\nbox all  8 5 2  9 12 3\nbox all  7 5 9  8 8 10\nbox all  9 6 2  10 12 3\n\
box all  8 6 9  9 12 10\nbox all  4 7 0  8 8 2\nbox all  10 7 2  11 12 3\nbox all  4 7 3  8 8 9\n\
box all  9 7 9  10 12 10\nbox all  4 7 10  8 8 12\nbox all  10 8 9  11 12 10\nbox all  11 9 2  12 12 3\n\
box all  11 9 9  12 12 10\nbox all  8 11 0  12 12 2\nbox all  8 11 3  12 12 9\nbox all  8 11 10  12 12 12\n";
	let d = def::parse(src).unwrap();
	let boxes: Vec<SubBox> = d.boxes.iter().map(|b| b.sub).collect();
	let before = boxes.len();
	let b = bake(&boxes);
	assert_eq!(before, 28);
	let r = ramps(&b);
	assert_eq!(r.len(), 2, "{:#?}", r);
	assert!(r.iter().all(|q| q.dir == Dir::Bottom));
	// Two for the solid stringer, (1 + 5) per side for the thin one.
	assert_eq!(tris(&b), 2 + 2 * (1 + 5), "{:#?}", b.quads);
	assert_winds_outward(&b);
}

/// Every ramp and triangle, triangulated as `quad_indices` does, has an
/// outward normal: along `dir`'s axis sign for the ramp, along `dir` for a
/// side piece.
fn assert_winds_outward(b: &Baked) {
	for q in b.quads.iter().filter(|q| q.is_ramp() || q.is_tri()) {
		let p = |i: usize| q.corners[i].map(f32::from);
		let (ox, oy, oz) = q.dir.offset();
		let want = [ox as f32, oy as f32, oz as f32];
		let mut drawn = 0;
		for tri in [[3, 1, 0], [3, 0, 2]] {
			let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
			let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
			let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
			let n = [
				u[1] * v[2] - u[2] * v[1],
				u[2] * v[0] - u[0] * v[2],
				u[0] * v[1] - u[1] * v[0],
			];
			let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
			if len == 0.0 {
				continue;
			}
			drawn += 1;
			let dot = n[0] * want[0] + n[1] * want[1] + n[2] * want[2];
			assert!(dot > 0.0, "{q:?}: triangle {tri:?} normal {n:?}");
		}
		assert_eq!(drawn, if q.is_tri() { 1 } else { 2 }, "{q:?}");
	}
}

#[test]
fn shipped_shapes_have_no_twelfth_runs() {
	// The legacy pins prove this bit for bit; this names the property.
	for b in [
		BlockType::WoodStairs,
		BlockType::LumberDoor,
		BlockType::WoodSlab,
		BlockType::Torch,
		BlockType::Table,
		BlockType::Chair,
		BlockType::Bed,
	] {
		for state in 0..16u8 {
			if let Some(v) = shape_of(b, state) {
				assert!(
					v.quads.iter().all(|q| !q.is_ramp() && !q.is_tri()),
					"{b:?} {state}"
				);
			}
		}
	}
}
