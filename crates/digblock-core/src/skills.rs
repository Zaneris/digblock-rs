//! Skills & XP: the shared curve, per-skill totals, and the award
//! tables. Everything here is computed identically by the server (the
//! only authority in multiplayer — awards happen at its two chokepoints
//! and reach the client as new TOTALS) and by offline play (where the
//! engine calls the same functions directly). No RNG anywhere: awards
//! are pure functions of what was done.
//!
//! Storage is XP only (`u64` per skill); level is always derived from
//! the curve. Persistence and the wire carry `(u8, u64)` pairs keyed by
//! [`Skill`]'s `repr(u8)` index, append-only, so unknown skills from a
//! newer build decode harmlessly.

use crate::block::BlockType;

/// A skill, `repr(u8)`, APPEND-ONLY — the index is a wire and
/// persistence format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Skill {
	Mining = 0,
	Alchemy = 1,
}

impl Skill {
	pub const COUNT: usize = 2;
	pub const ALL: [Skill; Self::COUNT] = [Skill::Mining, Skill::Alchemy];

	pub fn try_from_index(i: u8) -> Option<Self> {
		match i {
			0 => Some(Skill::Mining),
			1 => Some(Skill::Alchemy),
			_ => None,
		}
	}

	/// The HUD/screen name (the UI is all-caps everywhere it labels).
	pub fn name(self) -> &'static str {
		match self {
			Skill::Mining => "MINING",
			Skill::Alchemy => "ALCHEMY",
		}
	}

	/// Sentence-case display for feed lines ("Mining is now level 48").
	pub fn display(self) -> &'static str {
		match self {
			Skill::Mining => "Mining",
			Skill::Alchemy => "Alchemy",
		}
	}
}

pub const MAX_LEVEL: u32 = 99;

/// XP to advance FROM level L (index L-1) to L+1: `floor(83 · 2^(L/10))`
/// — doubles every 10 levels. Pinned as integers rather than computed
/// with `powf` at runtime: the values are a golden constant both sides
/// must agree on forever, and libm rounding is not a foundation for
/// that. Generated once at full precision; the pins in the tests below
/// hold the table to the design table in SKILLS-DESIGN.md §2.
const XP_TO_NEXT: [u64; (MAX_LEVEL - 1) as usize] = [
	88, 95, 102, 109, 117, 125, 134, //
	144, 154, 166, 177, 190, 204, 219, //
	234, 251, 269, 289, 309, 332, 355, //
	381, 408, 438, 469, 503, 539, 578, //
	619, 664, 711, 762, 817, 876, 939, //
	1006, 1078, 1156, 1239, 1328, 1423, 1525, //
	1634, 1752, 1878, 2012, 2157, 2312, 2478, //
	2656, 2846, 3050, 3269, 3504, 3756, 4025, //
	4314, 4624, 4956, 5312, 5693, 6101, 6539, //
	7009, 7512, 8051, 8629, 9248, 9912, 10624, //
	11386, 12203, 13079, 14018, 15024, 16102, 17258, //
	18497, 19825, 21248, 22773, 24407, 26159, 28036, //
	30049, 32205, 34517, 36994, 39650, 42496, 45546, //
	48815, 52318, 56073, 60098, 64411, 69034, 73989,
];

/// XP required to advance from `level` to `level + 1`; `None` at the
/// cap (and for the impossible level 0).
pub fn xp_to_next(level: u32) -> Option<u64> {
	if !(1..MAX_LEVEL).contains(&level) {
		return None;
	}
	Some(XP_TO_NEXT[(level - 1) as usize])
}

/// Cumulative XP at which `level` is first reached (level ≤ 1 → 0).
pub fn total_xp_for(level: u32) -> u64 {
	let level = level.min(MAX_LEVEL);
	XP_TO_NEXT
		.iter()
		.take(level.saturating_sub(1) as usize)
		.sum()
}

/// The level a total implies. Always derived, never stored.
pub fn level_for_xp(xp: u64) -> u32 {
	let mut level = 1;
	let mut cum = 0u64;
	for &step in &XP_TO_NEXT {
		cum += step;
		if xp < cum {
			break;
		}
		level += 1;
	}
	level
}

/// Per-skill XP totals. The whole of a player's skill state — levels
/// are derived on demand.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct SkillSet {
	xp: [u64; Skill::COUNT],
}

impl SkillSet {
	#[inline]
	pub fn xp(&self, s: Skill) -> u64 {
		self.xp[s as usize]
	}

	#[inline]
	pub fn level(&self, s: Skill) -> u32 {
		level_for_xp(self.xp(s))
	}

	/// Credit XP (saturating) and return the new total — the value an
	/// `XpGain` frame carries.
	pub fn add_xp(&mut self, s: Skill, amount: u64) -> u64 {
		let c = &mut self.xp[s as usize];
		*c = c.saturating_add(amount);
		*c
	}

	/// Adopt an authoritative total. Totals only ever grow, so a stale
	/// or reordered frame can never rewind a newer one.
	pub fn set_total(&mut self, s: Skill, total: u64) {
		let c = &mut self.xp[s as usize];
		*c = (*c).max(total);
	}

	/// Strict decode of `(skill index, total)` pairs: unknown skills
	/// from a newer build drop harmlessly, duplicates last-write-win.
	pub fn from_wire(pairs: &[(u8, u64)]) -> Self {
		let mut set = Self::default();
		for &(i, total) in pairs {
			if let Some(s) = Skill::try_from_index(i) {
				set.xp[s as usize] = total;
			}
		}
		set
	}

	/// Nonzero totals as wire/persistence pairs (absent = zero).
	pub fn to_wire(&self) -> Vec<(u8, u64)> {
		Skill::ALL
			.iter()
			.map(|&s| (s as u8, self.xp(s)))
			.filter(|&(_, xp)| xp > 0)
			.collect()
	}
}

// ---- award tables ----------------------------------------------------
//
// XP keys on fixed per-action bases, never on level-scaled outcomes —
// per-action XP is fixed for life. Shared verbatim by the server's
// chokepoints and the offline engine.

/// Mining XP for a completed dig of `block`: `ceil(hardness_ticks / 6)`.
/// Zero for the undiggable (which can't complete a dig anyway).
pub fn dig_xp(block: BlockType) -> u64 {
	match block.hardness_ticks() {
		Some(h) => u64::from(h).div_ceil(6),
		None => 0,
	}
}

/// Alchemy XP per block extracted at the bench:
/// `ceil(yield / 3)` of the conversion's sub-material yield.
pub fn convert_xp_per_block() -> u64 {
	u64::from(crate::shapes::SUBS_PER_BLOCK).div_ceil(3)
}

/// Alchemy XP per crafted batch: `ceil(cost / 3)` of the recipe's
/// sub-material cost. Zero for items with no recipe (which can't be
/// crafted anyway).
pub fn craft_xp(item: BlockType) -> u64 {
	match crate::shapes::craft_recipe(item) {
		Some(r) => u64::from(r.cost).div_ceil(3),
		None => 0,
	}
}

/// Total Alchemy XP for one validated alchemy op (call only after
/// `apply_alchemy` succeeded — the op's counts are then known-valid).
pub fn alchemy_xp(op: &crate::inventory::AlchemyOp) -> u64 {
	use crate::inventory::AlchemyOp;
	match *op {
		AlchemyOp::Convert { count, .. } => convert_xp_per_block() * u64::from(count),
		AlchemyOp::Craft { item, count } => craft_xp(item) * u64::from(count),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The golden cumulative table from SKILLS-DESIGN.md §2. If curve
	/// tuning is ever wanted, this pin moves WITH it, deliberately.
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
		assert_eq!(level_for_xp(0), 1);
		assert_eq!(level_for_xp(87), 1, "one short of level 2");
		assert_eq!(level_for_xp(88), 2);
		assert_eq!(level_for_xp(total_xp_for(50)), 50);
		assert_eq!(level_for_xp(total_xp_for(50) - 1), 49);
		assert_eq!(level_for_xp(total_xp_for(99)), 99);
		assert_eq!(level_for_xp(u64::MAX), 99, "the cap holds");
		// The curve is strictly increasing — no level is free and no
		// level is skipped.
		for level in 1..MAX_LEVEL {
			assert!(xp_to_next(level).unwrap() >= 88);
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

	/// §5.2's award table: `ceil(hardness_ticks / 6)` of the SHIPPED
	/// hardness values — XP is fixed per action for life.
	#[test]
	fn dig_xp_follows_the_hardness_table() {
		use BlockType as B;
		for (block, xp) in [
			(B::Leaves, 2u64),
			(B::Snow, 3),
			(B::Sand, 5),
			(B::Dirt, 5),
			(B::Grass, 5),
			(B::Gravel, 8),
			(B::Wood, 10),
			(B::Sandstone, 12),
			(B::Stone, 15),
			(B::WoodSlab, 4), // a slab third pays per completed hold
		] {
			assert_eq!(dig_xp(block), xp, "{block:?}");
		}
		assert_eq!(dig_xp(B::Air), 0, "undiggable pays nothing");
	}

	#[test]
	fn alchemy_xp_derives_from_the_current_tables() {
		use crate::inventory::AlchemyOp;
		use BlockType as B;
		// 27 subs per block → 9 XP per extraction.
		assert_eq!(convert_xp_per_block(), 9);
		assert_eq!(
			alchemy_xp(&AlchemyOp::Convert {
				block: B::Stone,
				count: 3
			}),
			27
		);
		// Crafts pay a third of their cost, rounded up.
		let stairs = crate::shapes::craft_recipe(B::WoodStairs).unwrap();
		assert_eq!(craft_xp(B::WoodStairs), u64::from(stairs.cost).div_ceil(3));
		assert_eq!(craft_xp(B::Air), 0, "no recipe, no XP");
	}
}
