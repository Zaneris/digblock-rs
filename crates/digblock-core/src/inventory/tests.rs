
use super::*;

/// A price naming one key twice costs the SUM of its lines. Checking
/// them one at a time against the whole balance is what
/// [`apply_alchemy`] used to do, and it passed on half the price —
/// then debited through a `debug_assert!` that compiles out, so a
/// release build handed over the item and took what it could.
#[test]
fn totals_folds_a_repeated_key() {
	use crate::block::Essence as E;
	assert_eq!(
		totals([(E::Vitae, 2), (E::Terra, 3), (E::Vitae, 5)]),
		vec![(E::Vitae, 7), (E::Terra, 3)],
		"first-seen order, summed"
	);
	assert_eq!(
		totals([(E::Unda, 1)]),
		vec![(E::Unda, 1)],
		"the common case"
	);
	assert!(totals(std::iter::empty::<(E, u32)>()).is_empty());
}

/// The legacy-character migration's inventory rule: two piles one
/// player owns become one, and every cap still holds.
///
/// The tool line is the point. Both sides own a bucket, and a merge
/// that summed counts would hand back two — an item the game has no
/// way to represent, from a path no player ever asked for.
#[test]
fn merging_two_of_a_players_inventories_respects_every_cap() {
	let mut mine = Inventory::default();
	mine.add(BlockType::Dirt, 5);
	mine.add(BlockType::Bucket, 1);
	mine.add_sub(Essence::Terra, 40);

	let mut old = Inventory::default();
	old.add(BlockType::Dirt, 7);
	old.add(BlockType::Stone, 3);
	old.add(BlockType::Bucket, 1);
	old.add_sub(Essence::Terra, 60);
	old.add_sub(Essence::Vitae, 2);

	mine.merge_from(&old);
	assert_eq!(mine.count(BlockType::Dirt), 12, "stacks add");
	assert_eq!(mine.count(BlockType::Stone), 3, "and carry over");
	assert_eq!(
		mine.count(BlockType::Bucket),
		cap(BlockType::Bucket),
		"a tool owned on both sides is still one tool"
	);
	assert_eq!(mine.sub_count(Essence::Terra), 100, "essence adds too");
	assert_eq!(mine.sub_count(Essence::Vitae), 2);

	// Merging with nothing changes nothing, which is the common case:
	// most players have no old character on most servers.
	let before = mine.clone();
	mine.merge_from(&Inventory::default());
	assert_eq!(mine, before);
}

#[test]
fn add_take_and_clamp() {
	let mut inv = Inventory::default();
	assert_eq!(inv.count(BlockType::Dirt), 0);
	assert!(!inv.try_take(BlockType::Dirt, 1), "empty refuses");

	inv.add(BlockType::Dirt, 3);
	assert!(inv.try_take(BlockType::Dirt, 2));
	assert_eq!(inv.count(BlockType::Dirt), 1);
	assert!(!inv.try_take(BlockType::Dirt, 2), "insufficient refuses");
	assert_eq!(inv.count(BlockType::Dirt), 1, "refusal leaves count intact");

	inv.add(BlockType::Stone, u32::MAX);
	assert_eq!(inv.count(BlockType::Stone), MAX_STACK, "saturates");
}

/// The bucket carries a charge, not a stack: one source or none, and
/// a stack recorded before that was true decodes as one full bucket
/// rather than a pile of free sources.
#[test]
fn water_is_a_charge_not_a_stack() {
	let mut inv = Inventory::default();
	assert!(!inv.bucket_full());
	inv.add(BlockType::Water, 1);
	assert!(inv.bucket_full());
	inv.add(BlockType::Water, 40);
	assert_eq!(inv.count(BlockType::Water), BUCKET_CHARGE, "no second unit");
	assert!(inv.try_take(BlockType::Water, 1), "pouring empties it");
	assert!(!inv.bucket_full());

	// A save or identity record from when water WAS an item.
	let legacy = Inventory::from_wire(&[(BlockType::Water as u16, 40)]);
	assert_eq!(legacy.count(BlockType::Water), BUCKET_CHARGE);
}

#[test]
fn wire_round_trip_drops_unknown_ids() {
	let mut inv = Inventory::default();
	inv.add(BlockType::Sand, 7);
	inv.add(BlockType::Water, 2);
	let wire = inv.to_wire();
	assert_eq!(Inventory::from_wire(&wire), inv);

	let hostile = [(9999u16, 5u32), (BlockType::Wood as u16, u32::MAX)];
	let decoded = Inventory::from_wire(&hostile);
	assert_eq!(decoded.count(BlockType::Wood), MAX_STACK, "clamped");
	assert_eq!(decoded.iter_nonzero().count(), 1, "unknown id dropped");
}

#[test]
fn essences_ride_the_wire_as_flagged_ids() {
	let mut inv = Inventory::default();
	inv.add(BlockType::Wood, 3);
	inv.add_sub(Essence::Vitae, 54);
	inv.add_sub(Essence::Terra, 1);
	inv.add_sub(Essence::Aether, 2);
	let wire = inv.to_wire();
	assert!(wire.contains(&(SUB_WIRE_FLAG, 54)), "Vitae flagged at 0");
	assert!(wire.contains(&(SUB_WIRE_FLAG | 1, 1)), "Earth flagged at 1");
	assert!(
		wire.contains(&(SUB_WIRE_FLAG | 4, 2)),
		"Aether flagged at 4"
	);
	assert_eq!(Inventory::from_wire(&wire), inv, "round-trips");

	// THE legacy migration, which is a numbering choice rather than
	// code: a record written under the two-family Material economy
	// (sub-wood at 0, sub-stone at 1) decodes 1:1 into Vitae and
	// Terra because the Essence enum kept those indices.
	let legacy = Inventory::from_wire(&[(SUB_WIRE_FLAG, 54), (SUB_WIRE_FLAG | 1, 27)]);
	assert_eq!(legacy.sub_count(Essence::Vitae), 54, "sub-wood → Vitae");
	assert_eq!(legacy.sub_count(Essence::Terra), 27, "sub-stone → Earth");

	// Unknown families (incl. the hotbar's u16::MAX-area sentinels,
	// which never belong in inventory pairs) drop harmlessly.
	let hostile = [(SUB_WIRE_FLAG | 7, 5u32), (u16::MAX, 9u32)];
	assert_eq!(Inventory::from_wire(&hostile), Inventory::default());
}

#[test]
fn alchemy_craft_scales_with_level() {
	let mut inv = Inventory::default();
	inv.add(BlockType::Wood, 3);
	inv.add_sub(Essence::Vitae, 20);

	// Craft a door batch and two stairs: each craft consumes one
	// timber cube and the catalyst at mastery's smallest waste markup;
	// the door's cube cuts TWO leaves, the stairs' one flight each.
	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::LumberDoor,
			count: 1
		},
		99
	));
	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::WoodStairs,
			count: 2
		},
		99
	));
	assert_eq!(inv.count(BlockType::LumberDoor), 2, "one cube, two leaves");
	assert_eq!(inv.count(BlockType::WoodStairs), 2);
	assert_eq!(inv.count(BlockType::Wood), 0, "three cubes consumed");
	assert_eq!(
		inv.sub_count(Essence::Vitae),
		20 - 3 * 4,
		"ceil(3·1.05) each"
	);

	// The bottom of the curve: the steepest waste, so the same essence
	// buys less — and no timber refuses outright.
	let mut low = Inventory::default();
	low.add(BlockType::Wood, 1);
	low.add_sub(Essence::Vitae, 4);
	assert!(
		!apply_alchemy(
			&mut low,
			AlchemyOp::Craft {
				item: BlockType::WoodSlab,
				count: 1
			},
			1
		),
		"the catalyst costs ceil(3·1.60) = 5 > 4"
	);
	low.add_sub(Essence::Vitae, 1);
	assert!(apply_alchemy(
		&mut low,
		AlchemyOp::Craft {
			item: BlockType::WoodSlab,
			count: 1
		},
		1
	));
	assert_eq!(low.count(BlockType::WoodSlab), 3, "a cube cuts three slabs");
	assert_eq!(low.count(BlockType::Wood), 0);

	// Unaffordable, level-locked, invalid, zero/oversized counts:
	// refused with no mutation.
	let before = inv.clone();
	assert!(!apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::StoneStairs,
			count: 1
		},
		99
	));
	assert!(
		!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Stone,
				count: 1
			},
			14
		),
		"clean Stone is locked below the mason's unlock"
	);
	assert!(!apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::LumberDoor,
			count: 0
		},
		99
	));
	assert!(!apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::LumberDoor,
			count: ALCHEMY_MAX_COUNT + 1
		},
		99
	));
	assert_eq!(inv, before, "failed ops never mutate");
}

/// The feedstock half of a price: the two vessels cost blocks and no
/// essence, the debit is all-or-nothing beside the essence lines, and
/// a fresh player's first craft is exactly this.
#[test]
fn a_vessel_is_crafted_from_feedstock_alone() {
	let need: u32 = crate::alchemy::craft_feedstock(BlockType::Bucket)
		.iter()
		.map(|&(_, n)| n)
		.sum();
	let mut inv = Inventory::default();
	inv.add(BlockType::Wood, need - 1);
	assert!(
		!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Bucket,
				count: 1
			},
			1
		),
		"one log short is short"
	);
	assert_eq!(inv.count(BlockType::Wood), need - 1, "no partial debit");
	inv.add(BlockType::Wood, 1);
	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::Bucket,
			count: 1
		},
		1
	));
	assert_eq!(inv.count(BlockType::Bucket), 1, "the vessel is owned");
	assert_eq!(inv.count(BlockType::Wood), 0, "the timber is spent");
	for &e in &Essence::ALL {
		assert_eq!(inv.sub_count(e), 0, "no essence moved");
	}
}

/// A multi-reagent craft (a torch = Vitae + Terra) debits every line or
/// none: short on the SECOND line must not consume the first.
#[test]
fn multi_essence_crafts_never_half_charge() {
	let mut inv = Inventory::default();
	inv.add(BlockType::Wood, 1);
	inv.add_sub(Essence::Vitae, 100);
	inv.add_sub(Essence::Terra, 1); // deliberately short of the torch's Terra line
	let before = inv.clone();
	assert!(!apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::Torch,
			count: 1
		},
		99
	));
	assert_eq!(inv, before, "no partial debit — the timber included");
	inv.add_sub(Essence::Terra, 5);
	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::Torch,
			count: 1
		},
		99
	));
	assert_eq!(inv.count(BlockType::Torch), 1);
	assert_eq!(inv.count(BlockType::Wood), 0, "the stick is spent");
	assert_eq!(inv.sub_count(Essence::Vitae), 100 - 3, "ceil(2·1.05)");
	assert_eq!(inv.sub_count(Essence::Terra), 6 - 3, "ceil(2·1.05)");
}

/// The bucket charge is never crafted: water is something the world
/// hands you, so no amount of Unda buys a charge at any level — the
/// only way to fill the tool is to go and scoop one. (Its old
/// extraction half left with the bench's extract slot: the springstone
/// node is the Unda economy now.)
#[test]
fn the_bucket_charge_is_never_crafted() {
	let mut inv = Inventory::default();
	inv.add_sub(Essence::Unda, 100_000);
	let before = inv.clone();
	assert!(!apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::Water,
			count: 1
		},
		crate::skills::MAX_LEVEL
	));
	assert_eq!(inv, before, "nothing paid, nothing filled");
}

/// Owning a tool is a yes/no (§7.3), so the bench sells you exactly
/// one. Both ways to ask for a second are refused BEFORE any debit:
/// a batch multiplier — the transmute tab aims those at every member,
/// tools included — and a plain craft once it's in the bag. `add`
/// saturates, so an unrefused one would take the whole batch's essence
/// and hand back nothing.
#[test]
fn a_tool_is_crafted_once_and_never_paid_for_twice() {
	let held =
		|inv: &Inventory| -> Vec<u32> { Essence::ALL.iter().map(|&e| inv.sub_count(e)).collect() };
	let pick = BlockType::ResonantPick;
	let mut inv = Inventory::default();
	for &e in &Essence::ALL {
		inv.add_sub(e, 100_000);
	}

	let before = held(&inv);
	assert!(
		!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: pick,
				count: 10
			},
			99
		),
		"nine of the ten would vanish into the cap"
	);
	assert_eq!(held(&inv), before, "nothing paid");
	assert_eq!(inv.count(pick), 0);

	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: pick,
			count: 1
		},
		99
	));
	assert_eq!(inv.count(pick), 1);
	let owned = held(&inv);
	assert_ne!(owned, before, "the first one costs");

	assert!(
		!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: pick,
				count: 1
			},
			99
		),
		"a tool already owned has nowhere to go"
	);
	assert_eq!(held(&inv), owned, "and so costs nothing");
	assert_eq!(inv.count(pick), 1);
	assert_eq!(inv.headroom(pick), 0, "the screen greys on this number");
}
