use super::*;
use crate::skills::{Skill, SkillSet};

/// A set at `level` in every skill, so a gate on any of them reads the one figure.
fn at(level: u32) -> SkillSet {
	let mut s = SkillSet::default();
	for skill in Skill::ALL {
		s.set_total(skill, crate::skills::total_xp_for(level));
	}
	s
}

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

/// The legacy-character migration's inventory rule: two piles one player owns become one, and every
/// cap still holds.
///
/// The tool line is the point. Both sides own a bucket, and a merge that summed counts would hand
/// back two — an item the game has no way to represent.
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
	// Two separate facts, kept apart. First: an essence rides the wire
	// as its own index behind the flag — read off the enum, so a RENAME
	// costs nothing here.
	for (e, n) in [
		(Essence::Vitae, 54),
		(Essence::Terra, 1),
		(Essence::Aether, 2),
	] {
		assert!(
			wire.contains(&(SUB_WIRE_FLAG | e as u16, n)),
			"{e:?} is not on the wire at its own index"
		);
	}
	// Second: those indices are frozen. This one IS a literal on
	// purpose — the numbering is a persistence format, and the two
	// oldest slots carry the retired Material economy's records.
	assert_eq!(
		Essence::Vitae as u16,
		0,
		"Vitae holds legacy sub-wood's slot"
	);
	assert_eq!(
		Essence::Terra as u16,
		1,
		"Terra holds legacy sub-stone's slot"
	);
	assert_eq!(Inventory::from_wire(&wire), inv, "round-trips");

	// THE legacy migration, which is a numbering choice rather than
	// code: a record written under the two-family Material economy
	// (sub-wood at 0, sub-stone at 1) decodes 1:1 into Vitae and
	// Terra because the Essence enum kept those indices.
	let legacy = Inventory::from_wire(&[(SUB_WIRE_FLAG, 54), (SUB_WIRE_FLAG | 1, 27)]);
	assert_eq!(legacy.sub_count(Essence::Vitae), 54, "sub-wood → Vitae");
	assert_eq!(legacy.sub_count(Essence::Terra), 27, "sub-stone → Terra");

	// Unknown families (incl. the hotbar's u16::MAX-area sentinels,
	// which never belong in inventory pairs) drop harmlessly.
	let hostile = [(SUB_WIRE_FLAG | 7, 5u32), (u16::MAX, 9u32)];
	assert_eq!(Inventory::from_wire(&hostile), Inventory::default());
}

/// The crest purge strikes exactly the reused ids and nothing beside
/// them — not the flagged essence pairs, whose low bits may land on the
/// same numbers, and not the ids either side.
#[test]
fn the_crest_purge_strikes_only_the_reused_ids() {
	let ids = BlockType::REUSED_CREST_IDS;
	let pairs = vec![
		(BlockType::Dirt as u16, 4),
		(*ids.start(), 1),
		(*ids.end(), 2),
		(*ids.end() + 1, 3),
		(SUB_WIRE_FLAG | *ids.start(), 9),
	];
	assert_eq!(
		Inventory::without_reused_crests(&pairs),
		vec![
			(BlockType::Dirt as u16, 4),
			(*ids.end() + 1, 3),
			(SUB_WIRE_FLAG | *ids.start(), 9),
		]
	);
	// And the ids really are the raw shapes' now, so the purge is what
	// stands between an old crest and a dirt slab.
	for id in ids {
		let b = BlockType::try_from_id(id).expect("reused");
		assert!(b.is_crafted_only(), "{b:?}");
	}
}

/// The feedstock half of a price: the two vessels cost blocks and no
/// essence, the debit is all-or-nothing beside the essence lines, and
/// a fresh player's first craft is exactly this.
#[test]
fn a_vessel_is_crafted_from_feedstock_alone() {
	let feed = crate::alchemy::craft_feedstock(BlockType::EssenceSiphon);
	let (short, _) = feed[0];
	let mut inv = Inventory::default();
	for &(b, n) in &feed {
		inv.add(b, if b == short { n - 1 } else { n });
	}
	assert!(
		!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::EssenceSiphon,
				count: 1
			},
			&at(1)
		),
		"one log short is short"
	);
	for &(b, n) in &feed {
		assert_eq!(
			inv.count(b),
			if b == short { n - 1 } else { n },
			"no partial debit"
		);
	}
	inv.add(short, 1);
	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::EssenceSiphon,
			count: 1
		},
		&at(1)
	));
	assert_eq!(
		inv.count(BlockType::EssenceSiphon),
		1,
		"the vessel is owned"
	);
	for &(b, _) in &feed {
		assert_eq!(inv.count(b), 0, "the feedstock is spent");
	}
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
		&at(99)
	));
	assert_eq!(inv, before, "no partial debit — the timber included");
	inv.add_sub(Essence::Terra, 5);
	assert!(apply_alchemy(
		&mut inv,
		AlchemyOp::Craft {
			item: BlockType::Torch,
			count: 1
		},
		&at(99)
	));
	assert_eq!(inv.count(BlockType::Torch), 1);
	assert_eq!(inv.count(BlockType::Wood), 0, "the stick is spent");
	let torch = crate::alchemy::craft_ideal(BlockType::Torch).expect("craftable");
	let paid = |e: Essence| -> u32 {
		torch
			.iter()
			.find(|(k, _)| *k == e)
			.map(|&(_, n)| crate::alchemy::craft_cost(n, crate::skills::MAX_LEVEL))
			.expect("the torch has this line")
	};
	assert_eq!(inv.sub_count(Essence::Vitae), 100 - paid(Essence::Vitae));
	assert_eq!(inv.sub_count(Essence::Terra), 6 - paid(Essence::Terra));
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
		&at(crate::skills::MAX_LEVEL)
	));
	assert_eq!(inv, before, "nothing paid, nothing filled");
}

/// Owning a tool is a yes/no (§7.3), so the bench sells you exactly one. Both ways to ask for a
/// second are refused BEFORE any debit: a batch multiplier — the transmute tab aims those at every
/// member, tools included — and a plain craft once it's in the bag. `add` saturates, so an
/// unrefused one would take the whole batch's essence and hand back nothing.
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
			&at(99)
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
		&at(99)
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
			&at(99)
		),
		"a tool already owned has nowhere to go"
	);
	assert_eq!(held(&inv), owned, "and so costs nothing");
	assert_eq!(inv.count(pick), 1);
	assert_eq!(inv.headroom(pick), 0, "the screen greys on this number");
}

/// Two fares in one essence are one question. Asked separately each passes on a purse that covers
/// either alone, so the pair must be summed before anything is debited — and a refused pair leaves
/// the purse untouched, while a paid one is refundable line by line.
#[test]
fn fares_are_paid_together_or_not_at_all() {
	use crate::block::Essence as E;
	let mut inv = Inventory::default();
	inv.add_sub(E::Unda, 5);
	inv.add_sub(E::Vitae, 2);
	let pair = [(E::Unda, 3), (E::Unda, 3)];
	assert!(
		inv.can_pay(&pair[..1]) && inv.can_pay(&pair[1..]),
		"each alone is affordable"
	);
	assert!(!inv.can_pay(&pair), "together they are not");
	assert!(!inv.try_take_fares(&pair));
	assert_eq!(inv.sub_count(E::Unda), 5, "a refusal debits nothing");
	let mixed = [(E::Unda, 3), (E::Vitae, 2)];
	assert!(inv.try_take_fares(&mixed));
	assert_eq!((inv.sub_count(E::Unda), inv.sub_count(E::Vitae)), (2, 0));
	inv.add_fares(&mixed);
	assert_eq!(
		(inv.sub_count(E::Unda), inv.sub_count(E::Vitae)),
		(5, 2),
		"the refund is exact"
	);
	// Short in the second essence alone: nothing of the first leaves either.
	assert!(!inv.try_take_fares(&[(E::Unda, 1), (E::Vitae, 3)]));
	assert_eq!(inv.sub_count(E::Unda), 5);
}
