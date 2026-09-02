use super::*;

/// The legacy-character migration's XP rule: the higher total per skill, never the sum.
///
/// A sum would pay one career twice — play the new account up, then claim the old one, and the
/// ladder shortens for anybody who had both. The max leaves the curve meaning what it meant.
#[test]
fn merging_two_of_a_players_careers_takes_the_further_one() {
	let mut mine = SkillSet::default();
	mine.add_xp(Skill::Mining, 100);
	mine.add_xp(Skill::Alchemy, 900);

	let mut old = SkillSet::default();
	old.add_xp(Skill::Mining, 5_000);
	old.add_xp(Skill::Alchemy, 40);

	mine.merge_from(&old);
	assert_eq!(mine.xp(Skill::Mining), 5_000, "the old career was further");
	assert_eq!(mine.xp(Skill::Alchemy), 900, "the new one was, here");

	// Never the sum, in either direction, and idempotent: a repeated
	// claim of the same character cannot inflate anything.
	let before = mine.clone();
	mine.merge_from(&old);
	assert_eq!(mine, before);
}

/// The golden cumulative pin on [`XP_TO_NEXT`]. If curve tuning is ever
/// wanted, this pin moves WITH it, deliberately.
#[test]
fn xp_curve_table_is_pinned() {
	for (level, to_next, cumulative) in [
		(2u32, 95u64, 88u64),
		(5, 117, 394),
		(10, 166, 1_068),
		(20, 332, 3_376),
		(30, 664, 7_998),
		(40, 1_328, 17_246),
		(50, 2_656, 35_745),
		(60, 5_312, 72_745),
		(70, 10_624, 146_751),
		(80, 21_248, 294_767),
		(90, 42_496, 590_805),
		(92, 48_815, 678_847),
	] {
		assert_eq!(xp_to_next(level), Some(to_next), "to-next at {level}");
		assert_eq!(total_xp_for(level), cumulative, "cumulative at {level}");
	}
	assert_eq!(total_xp_for(99), 1_103_585, "the 99 grand total");
	assert_eq!(xp_to_next(99), None, "the cap has no next");
	assert_eq!(xp_to_next(0), None);
}

#[test]
fn levels_derive_from_totals() {
	// The first step is asked for, not written down: it is the cheapest
	// level in the game by construction, so it is also the floor every
	// later step has to clear.
	let first = xp_to_next(1).expect("level 1 has a next");
	assert_eq!(level_for_xp(0), 1);
	assert_eq!(level_for_xp(first - 1), 1, "one short of level 2");
	assert_eq!(level_for_xp(first), 2);
	assert_eq!(level_for_xp(total_xp_for(50)), 50);
	assert_eq!(level_for_xp(total_xp_for(50) - 1), 49);
	assert_eq!(level_for_xp(total_xp_for(MAX_LEVEL)), MAX_LEVEL);
	assert_eq!(level_for_xp(u64::MAX), MAX_LEVEL, "the cap holds");
	// The curve is strictly increasing — no level is free and no
	// level is skipped.
	for level in 1..MAX_LEVEL {
		assert!(xp_to_next(level).unwrap() >= first);
		if level > 1 {
			assert!(xp_to_next(level).unwrap() > xp_to_next(level - 1).unwrap());
		}
	}
}

#[test]
fn skill_set_accumulates_and_round_trips() {
	let mut set = SkillSet::default();
	assert_eq!(set.level(Skill::Mining), 1);
	let total = set.add_xp(Skill::Mining, 100);
	assert_eq!(total, 100);
	assert_eq!(set.level(Skill::Mining), 2);
	set.add_xp(Skill::Alchemy, 9);

	let wire = set.to_wire();
	assert_eq!(SkillSet::from_wire(&wire), set);

	// Totals only grow: a reordered older frame can't rewind.
	set.set_total(Skill::Mining, 50);
	assert_eq!(set.xp(Skill::Mining), 100);
	set.set_total(Skill::Mining, 150);
	assert_eq!(set.xp(Skill::Mining), 150);

	// Unknown skills from the future drop harmlessly; zeros are
	// absent from the wire.
	let hostile = [(200u8, 5u64), (0, 42)];
	let decoded = SkillSet::from_wire(&hostile);
	assert_eq!(decoded.xp(Skill::Mining), 42);
	assert_eq!(decoded.to_wire(), vec![(0, 42)]);
}

/// Goldens along the shared taper, pinned as OUTPUTS to catch a taper edit nobody meant — a
/// relationship cannot notice that the curve between the endpoints changed SHAPE, the whole risk
/// here. The endpoints are deliberately NOT asserted: `dig_speed_permille` returns the family's
/// `start` and `end` there by construction. What IS asserted is what they buy — every scaling
/// family gets faster, and the flat one does not move.
#[test]
fn dig_speed_anchors_are_pinned() {
	use crate::block::DigFamily as F;
	for family in [F::Fixed, F::Soft, F::Wood, F::Rock] {
		let (novice, master) = (
			dig_speed_permille(family, 1),
			dig_speed_permille(family, MAX_LEVEL),
		);
		if family == F::Fixed {
			assert_eq!(novice, master, "{family:?} must never scale");
		} else {
			assert!(
				master < novice,
				"{family:?} did not speed up: {novice}->{master}"
			);
		}
	}
	// Derived goldens along the way, one early / mid / late.
	for (family, at15, at30, at60) in [
		(F::Fixed, 1000, 1000, 1000),
		(F::Soft, 1509, 1144, 747),
		(F::Wood, 1654, 1275, 863),
		(F::Rock, 1981, 1438, 848),
	] {
		assert_eq!(dig_speed_permille(family, 15), at15, "{family:?} at 15");
		assert_eq!(dig_speed_permille(family, 30), at30, "{family:?} at 30");
		assert_eq!(dig_speed_permille(family, 60), at60, "{family:?} at 60");
	}
	for &b in &BlockType::ALL {
		let Some(h) = b.hardness_ticks() else {
			continue;
		};
		for level in 2..=99 {
			assert!(
				dig_required(b, h, level) <= dig_required(b, h, level - 1),
				"{b:?} never slows with a level ({level})"
			);
		}
	}
	// The §5.1 table's spot values, in sim ticks. The hardness is read
	// off each block rather than written down beside it: a hold length
	// restated here would pin the hardness table twice, and a retune of
	// one row would fail a test about the CURVE.
	use BlockType as B;
	let hardness = |b: BlockType| b.hardness_ticks().expect("diggable");
	for (block, at1, at30, at99) in [(B::Stone, 252, 129, 49), (B::Wood, 134, 77, 39)] {
		let h = hardness(block);
		assert_eq!(dig_required(block, h, 1), at1, "{block:?} at level 1");
		assert_eq!(dig_required(block, h, 30), at30, "{block:?} mid-ladder");
		assert_eq!(
			dig_required(block, h, MAX_LEVEL),
			at99,
			"{block:?} at mastery"
		);
	}
	let dirt = hardness(B::Dirt);
	assert_eq!(dig_required(B::Dirt, dirt, 1), 62, "dirt at level 1");
	assert_eq!(dig_required(B::Dirt, dirt, 30), 34, "dirt mid-ladder");
	// Fixed never scales: leaves and the scoop hold all the way up, so
	// the requirement stays the block's own hardness at every level.
	for block in [B::Leaves, B::Water] {
		let h = hardness(block);
		for level in [1, 30, MAX_LEVEL] {
			assert_eq!(
				dig_required(block, h, level),
				u32::from(h),
				"{block:?} flat at {level}"
			);
		}
	}
}

/// The grid and the taper together: a dig gets faster a fixed number of
/// times, the FIRST jump is the biggest of the game, and every jump
/// after it is strictly smaller than the one before.
#[test]
fn every_dig_speedup_is_smaller_than_the_one_before() {
	use crate::block::DigFamily as F;
	// The step is the last entry of the PINNED table the level has
	// reached — derived from the table rather than restating the
	// stride arithmetic, so this is a table-vs-index cross-check and an
	// indexing off-by-one fails it.
	for level in 1..=MAX_LEVEL {
		let expect = *DIG_STEPS
			.iter()
			.rfind(|&&step| step <= level)
			.expect("level 1 is a step");
		assert_eq!(dig_speed_step(level), expect, "step of {level}");
	}
	assert_eq!(dig_speed_step(0), 1, "clamps up");
	assert_eq!(dig_speed_step(1000), MAX_LEVEL, "clamps down");
	assert_eq!(DIG_STEPS.len(), DIG_TAPER.len(), "one taper entry per step");
	// The taper spends the whole journey and eases off the whole way.
	assert_eq!(DIG_TAPER[0], 0, "the first step is the level-1 speed");
	assert_eq!(
		DIG_TAPER[DIG_TAPER.len() - 1],
		1000,
		"the last step is the mastery speed"
	);
	for i in 2..DIG_TAPER.len() {
		let (prev, now) = (
			DIG_TAPER[i - 1] - DIG_TAPER[i - 2],
			DIG_TAPER[i] - DIG_TAPER[i - 1],
		);
		assert!(now < prev, "taper jump {i} ({now}) must be under {prev}");
	}
	for family in [F::Fixed, F::Soft, F::Wood, F::Rock] {
		let flat = family == F::Fixed;
		// Flat between steps, and it moves on every one of them.
		for level in 2..=99 {
			let (prev, now) = (
				dig_speed_permille(family, level - 1),
				dig_speed_permille(family, level),
			);
			if dig_speed_step(level) == level && !flat {
				assert!(now < prev, "{family:?} must jump at its step {level}");
			} else {
				assert_eq!(now, prev, "{family:?} must hold flat at {level}");
			}
		}
		// Twenty jumps in a life, so twenty-one distinct speeds.
		let speeds: std::collections::BTreeSet<u32> =
			(1..=99).map(|l| dig_speed_permille(family, l)).collect();
		assert_eq!(
			speeds.len(),
			if flat { 1 } else { DIG_TAPER.len() },
			"{family:?} speeds"
		);
		// And each of those jumps is smaller than its predecessor —
		// the whole point, in the multiplier where it is exact (a
		// given block's TICK count can wobble by one on rounding).
		let at: Vec<u32> = DIG_STEPS
			.iter()
			.map(|&l| dig_speed_permille(family, l))
			.collect();
		for i in 2..at.len() {
			let (prev, now) = (at[i - 2] - at[i - 1], at[i - 1] - at[i]);
			assert!(
				now < prev || flat,
				"{family:?} jump at {} too big",
				DIG_STEPS[i]
			);
		}
	}
	// A stone dig gets faster on the stride and nowhere else, and the
	// first jump dwarfs the last. The hold lengths are goldens; the
	// HARDNESS behind them is read off the block, because restating it
	// here would pin the hardness table from a test about the curve.
	use BlockType as B;
	let h = B::Stone.hardness_ticks().expect("stone is diggable");
	for (level, want, why) in [
		(4, 252, "still the level-1 hold"),
		(5, 224, "the biggest jump"),
		(9, 224, "held to the next step"),
		(95, 51, "the 95 step"),
		(98, 51, "held at the 95 step"),
		(MAX_LEVEL, 49, "the last jump"),
	] {
		assert_eq!(dig_required(B::Stone, h, level), want, "{why}");
	}
}

/// The decay is a FRACTION of the bar per tick, not a tick count:
/// every block loses the same share of its own progress in the same
/// wall time, so the crack retreats at one speed everywhere and
/// a full bar is gone in exactly a second whatever the hardness.
#[test]
fn an_abandoned_hold_bleeds_out_in_exactly_one_second() {
	// Every diggable block in the game, at the level extremes.
	for &b in &BlockType::ALL {
		let Some(h) = b.hardness_ticks() else {
			continue;
		};
		for level in [1, 30, 99] {
			let need = dig_required(b, h, level);
			assert_eq!(dig_decayed(need, need, 0), need, "{b:?} untouched at rest");
			assert_eq!(
				dig_decayed(need, need, DIG_DECAY_TICKS),
				0,
				"{b:?} full bar gone at one second"
			);
			// Monotone, and never below zero however long it idles.
			let mut prev = need;
			for idle in 1..=DIG_DECAY_TICKS * 2 {
				let now = dig_decayed(need, need, idle);
				assert!(now <= prev, "{b:?} decay went back up at {idle}");
				prev = now;
			}
			// Half the window costs half the bar (to the truncation).
			let half = dig_decayed(need, need, DIG_DECAY_TICKS / 2);
			assert!(
				half.abs_diff(need / 2) <= 1,
				"{b:?} half a second should cost half a bar, not {half} of {need}"
			);
		}
	}
	// A part-filled bar empties proportionally sooner — that is what
	// makes "one second" a ceiling rather than a flat wait.
	let need = dig_required(BlockType::Stone, 90, 1);
	assert_eq!(dig_decayed(need / 4, need, DIG_DECAY_TICKS / 4), 0);
	// And a single banked tick on a long hold is gone on the first.
	assert_eq!(dig_decayed(1, need, 1), 0);
}

/// §5.2's award: XP is fixed per action for life and rides what a block is WORTH, so one worth
/// more never pays less. Asserted as that ordering plus the exemptions rather than as a table
/// of quotients — `dig_xp` IS the worth divided down, so the quotients only restate the
/// division and every one moves when a row is retuned. Over the worth and NOT the hold, which
/// a `stubborn` row multiplies without earning a point for it.
#[test]
fn dig_xp_follows_what_a_block_is_worth() {
	use BlockType as B;
	let mut paid = 0;
	for &a in &BlockType::ALL {
		if a.hardness_ticks().is_none() || dig_xp(a) == 0 {
			continue;
		}
		let wa = a.xp_ticks();
		paid += 1;
		for &b in &BlockType::ALL {
			if b.hardness_ticks().is_none() {
				continue;
			}
			let wb = b.xp_ticks();
			if dig_xp(b) > 0 && wa <= wb {
				assert!(
					dig_xp(a) <= dig_xp(b),
					"{a:?} ({wa}) outpaid the richer {b:?} ({wb})"
				);
			}
		}
	}
	assert!(
		paid > 5,
		"a catalog that pays for almost nothing proves nothing here"
	);
	// A slab is a third of a block and pays per completed hold, so it is
	// worth less than the block it completes into.
	assert!(
		dig_xp(B::WoodSlab) < dig_xp(B::Wood),
		"a slab third must not pay a whole block's award"
	);
	assert_eq!(dig_xp(B::Air), 0, "undiggable pays nothing");
	// Foliage and the torch pay nothing at all — named in `dig_xp`,
	// because the hardness scale-down alone rounds even the softest
	// block up to a point.
	for b in [B::Leaves, B::PineLeaves, B::Torch] {
		assert_eq!(dig_xp(b), 0, "{b:?} is housekeeping, not mining");
	}
}

/// Every block a player can dig, at both ends of the ramp — the
/// diggable roster walked rather than sampled, because the pins below
/// are about the SMALLEST award in the game and a new cheap row is
/// exactly what would quietly break them.
fn diggables() -> Vec<BlockType> {
	BlockType::ALL
		.iter()
		.copied()
		.filter(|b| b.hardness_ticks().is_some())
		.collect()
}

/// The surface world is untouched: at the waterline and anywhere above
/// it a dig pays its flat table entry exactly, and a swing pays what a
/// swing pays there — depth is the only thing this ramp knows about, so
/// however many cells the pick took, y is not what decided it.
#[test]
fn the_depth_reward_is_flat_above_the_waterline() {
	for y in [DEPTH_XP_FROM, DEPTH_XP_FROM + 1, DEPTH_XP_FROM + 90] {
		assert_eq!(depth_xp_permille(y), PERMILLE, "flat at y {y}");
		for b in diggables() {
			assert_eq!(dig_award(b, None, 1, y), (dig_xp(b), 0), "{b:?} at y {y}");
			assert_eq!(
				dig_award(b, None, 9, y),
				dig_award(b, None, 9, DEPTH_XP_FROM),
				"swing at {y}"
			);
		}
	}
}

/// §5.6: the pick's reach is a bonus in MATERIAL, not in XP. The cell under the crosshair pays in
/// full and every cell behind it a fraction of one — floored at a point, because a fraction of the
/// cheap end of the hardness order is nothing at all, and capped at the award itself, so the floor
/// can never out-pay the cell it is a fraction of. Walked over the whole diggable roster and every
/// reach the tool has, because both bounds bite only at the ends of the hardness order.
#[test]
fn a_wide_swing_pays_the_aimed_cell_in_full_and_the_rest_a_fraction() {
	// At the waterline the ramp is identity, so the split IS the answer.
	let y = DEPTH_XP_FROM;
	for b in diggables() {
		let full = dig_xp(b);
		// A block worth nothing stays worth nothing however wide the
		// swing — the cap in `dig_award` is what holds the floor at
		// zero here, and this is the pin on that.
		if full == 0 {
			for broken in [1u32, 3, 9] {
				assert_eq!(dig_award(b, None, broken, y).0, 0, "{b:?} swing");
			}
			continue;
		}
		assert_eq!(dig_award(b, None, 1, y).0, full, "{b:?} aimed cell");
		let mut prev = full;
		for broken in 2..=9u32 {
			let got = dig_award(b, None, broken, y).0;
			let step = got - prev;
			assert!(step >= 1, "{b:?}: cell {broken} of the swing paid nothing");
			assert!(step <= full, "{b:?}: cell {broken} out-paid the aimed one");
			prev = got;
		}
		// And a plane is well short of paying per cell, for anything the
		// fraction can actually cut into. At the floor of the scale it
		// cannot: one point IS the whole award there.
		if full > 1 {
			assert!(dig_award(b, None, 9, y).0 < full * 9, "{b:?} pays per cell");
		}
	}
	// The golden: a stone plane at the pick's widest reach, against what
	// those same nine blocks pay as nine separate holds.
	let stone = BlockType::Stone;
	assert_eq!(dig_award(stone, None, 9, y).0, 24);
	assert_eq!(dig_xp(stone) * 9, 63);
}

/// The ceiling lands on the deepest cell a dig can REACH, not on
/// bedrock — a maximum nobody can be handed is not a maximum. Below it
/// the clamp holds, so the undiggable floor and everything the world
/// does not have cannot push the ramp past its own end.
#[test]
fn the_depth_reward_tops_out_where_a_dig_still_reaches() {
	assert_eq!(depth_xp_permille(DEPTH_XP_TO), DEPTH_XP_MAX_PERMILLE);
	assert!(
		crate::block::BlockType::Stone
			.dig_hardness(DEPTH_XP_TO)
			.is_some()
	);
	assert!(
		crate::block::BlockType::Stone
			.dig_hardness(crate::block::BEDROCK_Y)
			.is_none()
	);
	for y in [crate::block::BEDROCK_Y, -1, -500] {
		assert_eq!(
			depth_xp_permille(y),
			DEPTH_XP_MAX_PERMILLE,
			"clamped at {y}"
		);
	}
}

/// The reward has to be VISIBLE, and the awards are small integers, so a multiplier that rounded
/// away on the cheapest dig in the game would be no reward at all there. Every diggable block pays
/// strictly more at the bottom than at the waterline — including those at the very floor of the
/// award scale, which a truncating ramp could not have moved by any multiplier under two.
/// `dig_award` rounds to nearest, so this survives a retune of either end.
#[test]
fn the_depth_reward_moves_every_award() {
	assert!(
		diggables().iter().any(|b| dig_xp(*b) == 1),
		"the floor of the award scale is occupied — the case this pin is for"
	);
	for b in diggables() {
		let (top, _) = dig_award(b, None, 1, DEPTH_XP_FROM);
		let (bottom, _) = dig_award(b, None, 1, DEPTH_XP_TO);
		// The zeroed blocks are the one exemption, and depth must NOT
		// move them: a multiplier that conjured a point out of a
		// zero-worth break would be depth creating XP the table says
		// does not exist.
		if dig_xp(b) == 0 {
			assert_eq!((top, bottom), (0, 0), "{b:?} moved off zero");
			continue;
		}
		assert!(
			bottom > top,
			"{b:?} pays {top} at the top and {bottom} at the bottom"
		);
	}
}

/// Descending never pays less, and the ladder has no rung missing: stone walks from its flat award
/// to its deepest ONE POINT AT A TIME. That whole-point stepping is why the ramp is linear instead
/// of layered — the flooring already delivers the felt steps, without a cliff to stand on.
#[test]
fn the_depth_reward_climbs_one_point_at_a_time() {
	let stone = BlockType::Stone;
	let mut seen = Vec::new();
	let mut prev = 0;
	for y in (DEPTH_XP_TO..=DEPTH_XP_FROM).rev() {
		let (award, _) = dig_award(stone, None, 1, y);
		assert!(award >= prev, "never falls with depth (y {y})");
		if award != prev {
			assert!(
				prev == 0 || award == prev + 1,
				"jumped {prev} -> {award} at y {y}"
			);
			seen.push(award);
		}
		prev = award;
	}
	// Every value between the two ends occurs, so no depth band is a
	// dead stretch and none is a cliff.
	let (top, _) = dig_award(stone, None, 1, DEPTH_XP_FROM);
	let (bottom, _) = dig_award(stone, None, 1, DEPTH_XP_TO);
	assert_eq!(seen, (top..=bottom).collect::<Vec<_>>());
}

/// A field draw pays the tabled split, and depth does not touch it:
/// the ore tiers are depth-banded already, so scaling them would count
/// the same axis twice — and a surface node has no depth story at all.
/// The excavation at a vein is the spent face left behind, and THAT
/// scales like any other dig.
#[test]
fn a_field_draw_ignores_the_depth_reward() {
	for node in [BlockType::Aetherock, BlockType::Heartwood] {
		let split = field_xp(node).expect("a node");
		for y in [DEPTH_XP_FROM + 40, DEPTH_XP_FROM, 64, DEPTH_XP_TO] {
			assert_eq!(
				dig_award(BlockType::Stone, Some(node), 1, y),
				split,
				"y {y}"
			);
		}
	}
	// The tapped face is an ordinary dig and does climb.
	let spent = BlockType::DepletedStone;
	assert!(dig_award(spent, None, 1, DEPTH_XP_TO) > dig_award(spent, None, 1, DEPTH_XP_FROM));
	// A non-node never routes through the field table, whatever is held.
	assert_eq!(
		dig_award(BlockType::Stone, Some(BlockType::Dirt), 1, DEPTH_XP_FROM),
		(dig_xp(BlockType::Stone), 0),
	);
	// Every node kind has a split, and the headline skill follows the
	// skill that revealed the find: Mining first at a vein (an even
	// split at the entry tier is allowed), Alchemy first at a surface
	// node.
	for &node in &crate::worldgen::nodes::NODES {
		let (mining, alchemy) = field_xp(node).unwrap_or_else(|| panic!("{node:?} pays nothing"));
		assert!(mining > 0 && alchemy > 0, "{node:?} pays both skills");
		if crate::unlock::reveal(node).is_some() {
			assert!(mining >= alchemy, "{node:?} is Mining's find");
		} else {
			assert!(alchemy > mining, "{node:?} is Alchemy's find");
		}
	}
}

/// §4.1's table, keyed on BASE costs — fixed for life regardless of
/// level-scaled outcomes. With extraction gone, crafting is the whole
/// of the bench's XP.
#[test]
fn alchemy_xp_derives_from_the_base_tables() {
	use crate::inventory::AlchemyOp;
	use BlockType as B;
	// Crafts pay their FULL worth — a fixed fraction of what the craft
	// EMBODIES, feedstock included: two raw logs for the stairs, a log
	// and its catalyst for the plank. The old halving un-pulled with the
	// unbounded bench it existed to pace.
	assert_eq!(craft_xp(B::WoodStairs), 6);
	assert_eq!(craft_xp(B::Lumber), 5);
	assert_eq!(craft_xp(B::Air), 0, "no recipe, no XP");
	assert_eq!(
		alchemy_xp(&AlchemyOp::Craft {
			item: B::Lumber,
			count: 3
		}),
		15
	);
	// And worth compounds down the chain: a stone brick embodies the
	// rubble, two catalysts and nothing else, so it pays more than the
	// stone it was dressed from.
	assert!(craft_xp(B::StoneBrick) > craft_xp(B::Stone));
	// The two feedstock-priced vessels still pay: their timber carries
	// the worth, so the first craft beat is also the first Alchemy XP
	// a fresh player earns at a bench.
	assert!(craft_xp(B::Bucket) > 0);
	assert!(craft_xp(B::EssenceSiphon) > craft_xp(B::Bucket));
}

/// The master derives board levels from a COPY of this module's curve and skill roster
/// (`apps/master/Hiscores/Skills.cs`), and nothing in either build system can check one against
/// the other — so this reads the C# source and does. A drift would rank players by a curve the
/// game does not run, which no request to either side would ever report.
#[test]
fn the_masters_curve_mirrors_ours() {
	let src = std::fs::read_to_string(concat!(
		env!("CARGO_MANIFEST_DIR"),
		"/../../apps/master/Hiscores/Skills.cs"
	))
	.expect("the master's Skills.cs beside this workspace");
	let ints = |from: &str, to: &str| -> Vec<u64> {
		let start = src
			.find(from)
			.unwrap_or_else(|| panic!("{from:?} in Skills.cs"))
			+ from.len();
		let end = start + src[start..].find(to).expect("closing delimiter");
		src[start..end]
			.split(|c: char| !c.is_ascii_digit())
			.filter(|s| !s.is_empty())
			.map(|s| s.parse().unwrap())
			.collect()
	};
	let curve = ints("XpToNext =\n    [", "]");
	let ours: Vec<u64> = (1..MAX_LEVEL).map(|l| xp_to_next(l).unwrap()).collect();
	assert_eq!(curve, ours, "the master's XpToNext is not our XP_TO_NEXT");
	assert!(src.contains(&format!("MaxLevel = {MAX_LEVEL};")));
	for s in Skill::ALL {
		assert!(
			src.contains(&format!("{} = {},", s.name(), s as u8)),
			"the master's Skill enum lacks {} = {}",
			s.name(),
			s as u8
		);
	}
	assert_eq!(
		ints("public enum Skill\n{", "}").len(),
		Skill::COUNT,
		"the master's Skill enum has a row we do not"
	);
}

/// The nameplate byte round-trips the roster, and every mastered skill is one the master's
/// content ladder actually tops out at.
#[test]
fn mastery_bits_round_trip() {
	for bits in 0..(1u8 << Skill::COUNT) {
		let mut skills = SkillSet::default();
		for s in Skill::from_mastery(bits) {
			skills.set_total(s, total_xp_for(crate::unlock::MASTERY_AT));
		}
		assert_eq!(Skill::mastery_bits(&skills), bits);
		for s in Skill::ALL {
			assert_eq!(s.mastered(&skills), bits & (1 << s as u8) != 0);
		}
	}
	// A bit past the roster is dropped rather than decoded as something.
	assert_eq!(Skill::from_mastery(0x80).count(), 0);
}
