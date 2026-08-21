use super::*;

#[test]
fn discriminants_index_props() {
	for (i, &b) in BlockType::ALL.iter().enumerate() {
		assert_eq!(b as usize, i, "{b:?} discriminant matches ALL order");
		assert_eq!(BlockType::from_id(b as u16), b, "from_id roundtrip");
		assert_eq!(BlockType::try_from_id(b as u16), Some(b));
	}
	assert_eq!(
		BlockType::from_id(1000),
		BlockType::Air,
		"out of range → Air"
	);
	assert_eq!(BlockType::try_from_id(1000), None, "strict decode refuses");
}

#[test]
fn table_sanity() {
	for &b in &BlockType::ALL {
		let p = b.props();
		// Class-None is Air and the item-only tools, nothing else —
		// and nothing colliding is None.
		assert_eq!(
			p.class == RenderClass::None,
			b == BlockType::Air || b.is_tool()
		);
		if p.collides {
			assert_ne!(p.class, RenderClass::None);
		}
		// Shape tag and render class travel together, and every shaped
		// block collides (physics resolves its sub-boxes/ramp).
		assert_eq!(p.shape.is_some(), p.class == RenderClass::Shaped, "{b:?}");
		if p.shape.is_some() {
			assert!(p.collides, "{b:?} shaped blocks collide");
		}
		// Glass-the-class and Translucent-the-bucket travel together in
		// one direction each: every Glass row is bucketed translucent,
		// and every translucent row is Glass or a shaped variant — and
		// never a fluid, whose see-through-ness is water's own pass.
		if p.class == RenderClass::Glass {
			assert_eq!(p.bucket, MeshBucket::Translucent, "{b:?}");
		}
		if p.bucket == MeshBucket::Translucent {
			assert!(
				matches!(p.class, RenderClass::Glass | RenderClass::Shaped),
				"{b:?}"
			);
			assert!(p.fluid.is_none(), "{b:?} translucent rows are not fluids");
		}
	}
}

#[test]
fn dig_table_sanity() {
	for &b in &BlockType::ALL {
		let p = b.props();
		// Every solid block can be dug; only the source in the water
		// family can (the water tool's scoop) — flows/falls cannot.
		if p.collides {
			assert!(b.hardness_ticks().is_some(), "{b:?} is diggable");
		}
		if b.is_water() {
			assert_eq!(
				b.hardness_ticks().is_some(),
				b.is_fluid_source(),
				"{b:?} collectible iff source"
			);
		}
		assert!(BlockType::Air.hardness_ticks().is_none());
		// Every diggable block credits something valid and diggable
		// itself (so the credit is placeable and re-collectible).
		match b.dig_drop() {
			Some(d) => {
				assert!(b.hardness_ticks().is_some(), "{b:?}");
				assert!(
					d.hardness_ticks().is_some(),
					"{b:?} drops re-diggable {d:?}"
				);
				assert_ne!(d, BlockType::Air, "{b:?} never drops Air");
			}
			None => assert!(b.hardness_ticks().is_none(), "{b:?}"),
		}
		// A row says either "always this" or "this, unless somebody
		// made it" — never both, or the unconditional one silently wins.
		assert!(
			b.props().drops.is_none() || b.props().crumbles.is_none(),
			"{b:?} sets both drops and crumbles"
		);
	}
}

/// Only the world's own rock crumbles, and it crumbles for everyone who
/// could not have made one.
///
/// The two halves are one rule read from either end: a pristine cell is
/// the mountain's whatever your level, and an edited one is somebody's
/// work — returned whole to a mason, and broken by anyone who has not
/// learned to cut it. `dig_drop` is the pristine answer and must stay
/// level-blind, because every economy pin means "what can a fresh world
/// put in a player's hands".
#[test]
fn only_the_worlds_own_rock_crumbles() {
	use crate::skills::MAX_LEVEL;
	for &b in &BlockType::ALL {
		let Some(into) = b.props().crumbles else {
			// Everything else answers the same however the cell got
			// there — the conditional arm is opt-in, one row at a time.
			for level in [1, MAX_LEVEL] {
				assert_eq!(b.dig_credit(false, level), b.dig_drop(), "{b:?}");
				assert_eq!(b.dig_credit(true, level), b.dig_drop(), "{b:?}");
			}
			continue;
		};
		assert_ne!(into, b, "{b:?} crumbles into itself, which says nothing");
		let made = crate::unlock::craft(b).expect("a crumbling block is bench work");
		for level in 1..=MAX_LEVEL {
			assert_eq!(
				b.dig_credit(false, level),
				Some(into),
				"{b:?} in the mountain crumbles at every level, {level} included"
			);
			let want = if level >= made { b } else { into };
			assert_eq!(
				b.dig_credit(true, level),
				Some(want),
				"a placed {b:?} dug at {level}"
			);
		}
		assert_eq!(b.dig_drop(), Some(into), "{b:?} — the pristine answer");
	}
}

/// A block's dig family is a decision, never a builder default — but it
/// is a decision per SUBSTANCE, not per piece. So the roster names one
/// row for each material and each block that has no material at all,
/// and a sweep carries that answer to every shape cut from it. Adding a
/// stair costs nothing here; adding a MATERIAL, or a node host, is a
/// line — which is the half that was ever really a choice.
///
/// The completeness check at the end is what keeps the roster honest:
/// a new diggable row that neither carries a listed material nor names
/// itself above fails here rather than silently inheriting.
#[test]
fn dig_families_are_pinned() {
	use BlockType as B;
	use DigFamily::*;
	// One row per material, keyed on the material rather than on any one
	// of its blocks.
	let by_material = [
		(Material::Dirt, Soft),
		(Material::Grass, Soft),
		(Material::DryGrass, Soft),
		(Material::Sand, Soft),
		(Material::Mud, Soft),
		(Material::Snow, Soft),
		(Material::Wood, Wood),
		(Material::Lumber, Wood),
		(Material::DarkLumber, Wood),
		(Material::RedLumber, Wood),
		(Material::Stone, Rock),
		(Material::Sandstone, Rock),
		(Material::Gravel, Rock),
		(Material::Rubble, Rock),
		(Material::StoneBrick, Rock),
		(Material::PolishedStone, Rock),
		(Material::SandstoneBrick, Rock),
		(Material::Glass, Soft),
		(Material::RedGlass, Soft),
		(Material::GreenGlass, Soft),
		(Material::BlueGlass, Soft),
		(Material::AmberGlass, Soft),
		(Material::Metal, Rock),
		(Material::Crystal, Rock),
	];
	// And the rows that carry no material: fixtures, fluids, foliage, and
	// the node hosts — a node digs as its HOST digs, because the node
	// changes what a hold PAYS, never what swings at it.
	let standalone = [
		(B::Water, Fixed), // scoop time = the viewmodel swing
		(B::Leaves, Fixed),
		(B::PineLeaves, Fixed),
		(B::Torch, Fixed),
		(B::Lantern, Rock),
		(B::Aetherlamp, Rock),
		// Furniture is hand-authored and carries no material either.
		(B::Table, Wood),
		(B::Chair, Wood),
		(B::Bed, Wood),
		(B::Emberore, Rock),
		(B::Duskstone, Rock),
		(B::Glowvein, Rock),
		(B::Aetherock, Rock),
		(B::DepletedStone, Rock),
		(B::DepletedEmberore, Rock),
		(B::DepletedDuskstone, Rock),
		(B::DepletedGlowvein, Rock),
		(B::DepletedAetherock, Rock),
		(B::Heartwood, Wood),
		(B::DrainedHeartwood, Wood),
		(B::Leystone, Rock),
		(B::SpentLeystone, Rock),
		(B::Springstone, Soft),
		(B::SpentSpringstone, Soft),
	];

	let family_of_material =
		|m: Material| by_material.iter().find(|(mm, _)| *mm == m).map(|(_, f)| *f);
	for (m, f) in by_material {
		assert!(
			Material::ALL.contains(&m),
			"{m:?} is listed but no longer a material"
		);
		let _ = f;
	}
	for (b, f) in standalone {
		assert_eq!(b.dig_family(), f, "{b:?}");
	}

	// Every shape of a listed material swings as its material does. The
	// named rows above are skipped: a few of them DO carry a material
	// (foliage is `Leaf`), they just are not cut into shapes and their
	// family is a fixture rather than a substance.
	let mut swept = 0;
	for &b in &BlockType::ALL {
		if b.hardness_ticks().is_none() || standalone.iter().any(|(s, _)| *s == b) {
			continue;
		}
		let m = b
			.material()
			.unwrap_or_else(|| panic!("{b:?} is diggable, material-less and unnamed above"));
		let want = family_of_material(m)
			.unwrap_or_else(|| panic!("{m:?} is a material with no family listed above"));
		assert_eq!(b.dig_family(), want, "{b:?} is {m:?}");
		swept += 1;
	}

	// The two halves together must cover every diggable row — a new one
	// belongs in a list above, or under a material that is.
	let diggable = BlockType::ALL
		.iter()
		.filter(|b| b.hardness_ticks().is_some())
		.count();
	assert_eq!(
		swept + standalone.len(),
		diggable,
		"{swept} swept + {} named against {diggable} diggable rows",
		standalone.len()
	);
}

/// The material assignment is an explicit roster for the same reason
/// the dig families are: it decides a block's unlock level, its craft
/// cost, its extraction yield and its earned wall all at once, so a new
/// row picking one up by accident would move four numbers silently.
/// The blocks WITHOUT a material are the other half of the decision —
/// functional items, priced by hand (§7.3).
#[test]
fn material_roster_is_pinned() {
	use BlockType as B;
	use Material as M;
	let expect = [
		(B::Dirt, M::Dirt),
		(B::Grass, M::Grass),
		(B::DryGrass, M::DryGrass),
		(B::Sand, M::Sand),
		(B::Mud, M::Mud),
		(B::Snow, M::Snow),
		(B::Stone, M::Stone),
		(B::StoneStairs, M::Stone),
		(B::StoneSlab, M::Stone),
		(B::StoneDoor, M::Stone),
		(B::Sandstone, M::Sandstone),
		(B::SandstoneStairs, M::Sandstone),
		(B::SandstoneSlab, M::Sandstone),
		(B::SandstoneDoor, M::Sandstone),
		(B::Gravel, M::Gravel),
		(B::Rubble, M::Rubble),
		(B::Emberore, M::Emberore),
		(B::Duskstone, M::Duskstone),
		(B::Glowvein, M::Glowvein),
		(B::Aetherock, M::Aetherock),
		(B::DepletedStone, M::DepletedStone),
		(B::DepletedEmberore, M::DepletedEmberore),
		(B::DepletedDuskstone, M::DepletedDuskstone),
		(B::DepletedGlowvein, M::DepletedGlowvein),
		(B::DepletedAetherock, M::DepletedAetherock),
		(B::Heartwood, M::Heartwood),
		(B::DrainedHeartwood, M::DrainedHeartwood),
		(B::Leystone, M::Leystone),
		(B::SpentLeystone, M::SpentLeystone),
		(B::Springstone, M::Springstone),
		(B::SpentSpringstone, M::SpentSpringstone),
		(B::StoneBrick, M::StoneBrick),
		(B::StoneBrickStairs, M::StoneBrick),
		(B::StoneBrickSlab, M::StoneBrick),
		(B::StoneBrickDoor, M::StoneBrick),
		(B::PolishedStone, M::PolishedStone),
		(B::PolishedStoneStairs, M::PolishedStone),
		(B::PolishedStoneSlab, M::PolishedStone),
		(B::SandstoneBrick, M::SandstoneBrick),
		(B::SandstoneBrickStairs, M::SandstoneBrick),
		(B::SandstoneBrickSlab, M::SandstoneBrick),
		(B::Glass, M::Glass),
		(B::RedGlass, M::RedGlass),
		(B::GreenGlass, M::GreenGlass),
		(B::BlueGlass, M::BlueGlass),
		(B::AmberGlass, M::AmberGlass),
		(B::Metal, M::Metal),
		(B::MetalStairs, M::Metal),
		(B::MetalSlab, M::Metal),
		(B::MetalDoor, M::Metal),
		(B::Crystal, M::Crystal),
		(B::CrystalSlab, M::Crystal),
		(B::Wood, M::Wood),
		(B::WoodStairs, M::Wood),
		(B::WoodSlab, M::Wood),
		(B::Lumber, M::Lumber),
		(B::LumberStairs, M::Lumber),
		(B::LumberSlab, M::Lumber),
		(B::LumberDoor, M::Lumber),
		(B::DarkLumber, M::DarkLumber),
		(B::DarkLumberStairs, M::DarkLumber),
		(B::DarkLumberSlab, M::DarkLumber),
		(B::DarkLumberDoor, M::DarkLumber),
		(B::RedLumber, M::RedLumber),
		(B::RedLumberStairs, M::RedLumber),
		(B::RedLumberSlab, M::RedLumber),
		(B::RedLumberDoor, M::RedLumber),
		(B::Leaves, M::Leaf),
		(B::PineLeaves, M::PineLeaf),
	];
	for (b, m) in expect {
		assert_eq!(b.material(), Some(m), "{b:?}");
	}
	for &b in &BlockType::ALL {
		let listed = expect.iter().any(|&(e, _)| e == b);
		assert_eq!(b.material().is_some(), listed, "{b:?}");
	}
	// And the complement, spelled out: a new row joining the
	// material-LESS set is a decision too, because it opts the block
	// out of the grid and into being hand-priced.
	let functional: Vec<BlockType> = BlockType::ALL
		.into_iter()
		.filter(|b| b.material().is_none())
		.collect();
	assert_eq!(
		functional,
		vec![
			B::Air,
			B::Water,
			B::WaterFlow1,
			B::WaterFlow2,
			B::WaterFlow3,
			B::WaterFlow4,
			B::WaterFlow5,
			B::WaterFlow6,
			B::WaterFlow7,
			B::WaterFall,
			B::Torch,
			B::Bucket,
			B::Table,
			B::Chair,
			B::Bed,
			B::EssenceSiphon,
			B::ResonantPick,
			B::Lantern,
			B::Aetherlamp,
			B::MastersSigil,
			// Conjured light: temporary by the wisplight sweep, so it is
			// neither substance nor priced item.
			B::Wisplight,
		]
	);
}

/// Grid membership: a substance in a form. The torch is the standing
/// counter-example — a shaped block that is a THING, not a form any
/// material could be worked into, so it has no family and is priced by
/// hand instead.
#[test]
fn the_grid_is_material_times_shape() {
	use BlockType as B;
	assert_eq!(
		B::Stone.grid_cell(),
		Some((Material::Stone, ShapeFamily::Block))
	);
	assert_eq!(
		B::LumberDoor.grid_cell(),
		Some((Material::Lumber, ShapeFamily::Door))
	);
	assert_eq!(B::Torch.shape_family(), None, "a torch is not a form");
	assert!(!B::Torch.is_grid_item());
	assert!(!B::Bucket.is_grid_item());
	assert!(!B::Water.is_grid_item());
	// `from_grid` is the inverse of `grid_cell` wherever a pair is
	// authored, and each cell is authored at most once.
	for &b in &BlockType::ALL {
		if let Some((m, s)) = b.grid_cell() {
			assert_eq!(BlockType::from_grid(m, s), Some(b), "{b:?}");
		}
	}
	for &m in &Material::ALL {
		for &s in &ShapeFamily::ALL {
			let n = BlockType::ALL
				.iter()
				.filter(|b| b.grid_cell() == Some((m, s)))
				.count();
			assert!(n <= 1, "{m:?} × {s:?} authored {n} times");
		}
	}
}

/// Light is a property of the SUBSTANCE, not of the piece it was cut
/// into: a crystal slab is the same crystal as the cube it came off, so it
/// wears the same ink and casts the same level.
///
/// This is the grid's one un-derived pair — `emissive` and `light` stayed
/// per-ROW builders, and a per-row property on a grid axis drifts the
/// moment a material grows a second shape. It did: `Crystal` glowed and
/// `CrystalSlab` did not, so growing a slab stack snapped the identical
/// volume of the identical stuff to fullbright in one frame. The two are
/// pinned SEPARATELY because they are genuinely independent; what neither
/// may do is differ between a cube and its own slab. FUNCTIONAL rows carry
/// no material at all, so they are outside this by construction.
#[test]
fn every_shape_of_a_material_agrees_on_light() {
	let mut compared = 0;
	for &m in &Material::ALL {
		let mut rows = BlockType::ALL
			.into_iter()
			.filter(|b| b.material() == Some(m));
		let first = rows
			.next()
			.unwrap_or_else(|| panic!("{m:?} has no row at all"));
		for b in rows {
			assert_eq!(
				b.props().emissive,
				first.props().emissive,
				"{b:?} and {first:?} are both {m:?} but disagree on the ink"
			);
			assert_eq!(
				b.props().light,
				first.props().light,
				"{b:?} and {first:?} are both {m:?} but cast different light"
			);
			compared += 1;
		}
	}
	// A walk that compares nothing passes forever. Every row carrying a
	// material is either its material's first shape or one comparison,
	// so this pins that the loop actually reached all of them.
	let rows = BlockType::ALL
		.into_iter()
		.filter(|b| b.material().is_some())
		.count();
	assert_eq!(
		compared + Material::COUNT,
		rows,
		"{compared} comparisons over {rows} rows in {} materials",
		Material::COUNT
	);
	use BlockType as B;
	// The set that actually exercises the rule, named so re-authoring
	// the crystal is a deliberate edit rather than a silent one.
	assert!(B::Crystal.props().emissive && B::CrystalSlab.props().emissive);
	assert_eq!(
		B::Crystal.props().light,
		0,
		"ink only — a wall lights nothing"
	);
	assert_eq!(B::CrystalSlab.props().light, 0);
	// And the counter-examples, on the other side of the material line.
	for b in [B::Torch, B::Lantern, B::Aetherlamp] {
		assert_eq!(b.material(), None, "{b:?} is functional, so hand-authored");
		assert!(b.props().emissive && b.props().light > 0, "{b:?} lights");
	}
}

/// A slab has to have something to complete into — that is why the
/// `Lumber` block exists at all. Derived from the material, so a new
/// timber's slab cannot ship without its cube.
#[test]
fn every_slab_completes_into_its_material() {
	for &b in &BlockType::ALL {
		if !b.is_slab() {
			assert_eq!(b.slab_base(), None, "{b:?} is not a slab");
			continue;
		}
		let base = b.slab_base().unwrap_or_else(|| panic!("{b:?} has no base"));
		assert_eq!(base.material(), b.material(), "{b:?}");
		assert_eq!(base.shape_family(), Some(ShapeFamily::Block), "{b:?}");
	}
	use BlockType as B;
	assert_eq!(B::WoodSlab.slab_base(), Some(B::Wood));
	assert_eq!(B::StoneSlab.slab_base(), Some(B::Stone));
	assert_eq!(B::LumberSlab.slab_base(), Some(B::Lumber));
}

#[test]
fn bedrock_floor_is_undiggable() {
	for &b in &BlockType::ALL {
		assert_eq!(b.dig_hardness(BEDROCK_Y), None, "{b:?} undiggable at floor");
		assert_eq!(
			b.dig_hardness(BEDROCK_Y + 1),
			b.hardness_ticks(),
			"{b:?} normal hardness above the floor"
		);
	}
}

/// Every water row agrees across the fields that describe it. The
/// amount is what the render class, the mesh bucket and the collision
/// flag all follow, so a row that holds water and collides — or one
/// that is opaque — is a table error the mesher draws wrong. The
/// bucket/class equivalence is asserted rather than assumed — and glass
/// deliberately kept it true by taking its own class (`Glass`) and its
/// own bucket (`Translucent`), so Translucent-the-class still names
/// exactly the water family.
#[test]
fn the_water_rows_agree_across_every_field_that_describes_them() {
	let fullest = BlockType::ALL
		.iter()
		.filter_map(|b| b.water_amount())
		.max()
		.expect("the table has water in it");
	for &b in &BlockType::ALL {
		if let Some(a) = b.water_amount() {
			assert!(a > 0, "{b:?} holds water but none of it");
			assert_eq!(b.props().class, RenderClass::Translucent, "{b:?}");
			assert!(!b.props().collides, "{b:?} never collides");
		}
		assert_eq!(b.is_falling(), b == BlockType::WaterFall, "{b:?}");
		assert_eq!(b.is_fluid_source(), b == BlockType::Water, "{b:?}");
		assert_eq!(
			b.props().bucket == MeshBucket::Water,
			b.props().class == RenderClass::Translucent,
			"{b:?}"
		);
	}
	assert_eq!(
		BlockType::Water.water_amount(),
		Some(fullest),
		"the source is the fullest water there is"
	);
	assert_eq!(
		BlockType::WaterFall.water_amount(),
		Some(fullest),
		"and so is a fall"
	);
	// `water_flow` is the only constructor that picks a row BY amount,
	// so every flowing row must be the one its own amount chooses.
	let mut flows = 0;
	for &b in &BlockType::ALL {
		let Some(f) = b.props().fluid else { continue };
		if f.source || f.falling {
			continue;
		}
		flows += 1;
		assert_eq!(BlockType::water_flow(f.amount), b, "{b:?}");
	}
	assert!(
		flows > 1,
		"a table with no flowing water proves nothing here"
	);
}

/// An identifier is not a label. `Essence::ident` is what an operator
/// types at `/give`, so it stays ASCII, lower case and unique — a label
/// may be translated one day and would take the command with it.
#[test]
fn an_essence_identifier_is_ascii_and_its_own() {
	for (i, &e) in Essence::ALL.iter().enumerate() {
		let id = e.ident();
		assert!(
			id.chars().all(|c| c.is_ascii_lowercase()),
			"{e:?} identifier {id:?} is not typeable ASCII"
		);
		assert!(
			!Essence::ALL[..i].iter().any(|o| o.ident() == id),
			"{e:?} shares its identifier with an earlier essence"
		);
		assert!(!e.name().is_empty(), "{e:?} has no label");
	}
}
