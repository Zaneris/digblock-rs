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

// ---- dig speed -------------------------------------------------------

/// §5.1's per-family dig-speed multiplier in permille. Every family
/// passes EXACTLY through 1000 at level 30 — today's hardness table is
/// the shared mid-curve reference — so a family only shapes the approach
/// (how hard the material resists a novice) and the ceiling (how fast
/// mastery gets). Fixed is flat: foliage is already near-instant, the
/// torch is a fixture, and the water scoop's duration is pinned to the
/// viewmodel swing. Two exact-anchored round-half-up segments per
/// family; the level-99 endpoint IS the speed floor — no second clamp.
pub fn dig_speed_permille(family: crate::block::DigFamily, level: u32) -> u32 {
	use crate::block::DigFamily;
	let level = level.clamp(1, 99);
	let (start, end) = match family {
		DigFamily::Fixed => (1000, 1000),
		DigFamily::Soft => (1250, 417),
		DigFamily::Wood => (1350, 500),
		DigFamily::Rock => (1700, 417),
	};
	if level <= 30 {
		start - (2 * (start - 1000) * (level - 1) + 29) / 58
	} else {
		1000 - (2 * (1000 - end) * (level - 30) + 69) / 138
	}
}

/// THE hold-to-dig duration both sides compute: `max(1,
/// round(hardness · family multiplier))` ticks at the digger's Mining
/// level. Level 30 reproduces the legacy table bit-exactly for EVERY
/// block; a level-up landing mid-hold skews client/server by a few
/// ticks at most, which `dig_grace` absorbs — no new tolerance
/// machinery. Hardness stays a separate parameter because callers hold
/// the y-gated `dig_hardness(y)` result (the bedrock rule lives there).
pub fn dig_required(block: BlockType, hardness: u16, level: u32) -> u32 {
	((u32::from(hardness) * dig_speed_permille(block.dig_family(), level) + 500) / 1000).max(1)
}

/// §5.1's rock gate: the Mining level below which this block refuses to
/// be dug at all (1 = ungated). Deliberately a strength wall — the doc's
/// original "no strength wall" stance was reversed by design: rock
/// opens a few levels in, after soft ground and loose gravel have paid
/// for them. The client predicts this in `diggable()` (it knows its own
/// level); the server refuses at DigStart with
/// `RejectReason::SkillGated` as the backstop, exactly the `Protected`
/// pattern.
pub fn dig_level_gate(block: BlockType) -> u32 {
	match block {
		BlockType::Sandstone => 3,
		BlockType::Stone | BlockType::StoneStairs | BlockType::StoneSlab | BlockType::Rubble => 5,
		_ => 1, // Gravel stays the entry rock; everything else ungated
	}
}

/// The feed line a gated dig speaks — beside `protect::REFUSE_*_MSG` in
/// spirit: one wording shared by prediction and the server verdict.
pub const REFUSE_GATED_MSG: &str = "this rock defies your mining level";

// ---- unlocks ---------------------------------------------------------

/// Does reaching `level` in `skill` open something that was shut at
/// `level - 1`? The level-up fanfare asks this to pick its voice (a
/// bigger sound for a level that hands you something), and the answer is
/// DERIVED from the gate tables themselves rather than a second list —
/// retuning a `craft_unlock` or a `dig_level_gate` moves the fanfare with
/// it, and cannot drift.
///
/// Level 1 is never an unlock: it is where everyone starts, and no one
/// levels up INTO it.
pub fn level_unlocks_something(skill: Skill, level: u32) -> bool {
	if level <= 1 || level > MAX_LEVEL {
		return false;
	}
	BlockType::ALL.iter().any(|&b| match skill {
		// A rock whose strength wall stands exactly here.
		Skill::Mining => dig_level_gate(b) == level,
		// A new recipe, a newly extractable block, or a bonus line that
		// wasn't paying at the level below.
		Skill::Alchemy => {
			crate::alchemy::craft_unlock(b) == Some(level)
				|| crate::alchemy::extract_unlock(b) == level
				|| crate::alchemy::bonus_lines(b, level)
					!= crate::alchemy::bonus_lines(b, level - 1)
		}
	})
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

/// Alchemy XP per block extracted at the bench: `ceil(base_total / 3)`
/// of the block's BASE yield — fixed for life, never the level-scaled
/// outcome (§4.1).
pub fn convert_xp(block: BlockType) -> u64 {
	u64::from(crate::alchemy::extract_total_base(block)).div_ceil(3)
}

/// Alchemy XP per crafted batch: `ceil(ideal_cost / 3)` summed across
/// the recipe's cost lines (pre-waste — fixed for life). Zero for
/// uncraftable items.
pub fn craft_xp(item: BlockType) -> u64 {
	match crate::alchemy::craft_ideal(item) {
		Some(lines) => u64::from(lines.iter().map(|&(_, n)| n).sum::<u32>()).div_ceil(3),
		None => 0,
	}
}

/// Total Alchemy XP for one validated alchemy op (call only after
/// `apply_alchemy` succeeded — the op's counts are then known-valid).
pub fn alchemy_xp(op: &crate::inventory::AlchemyOp) -> u64 {
	use crate::inventory::AlchemyOp;
	match *op {
		AlchemyOp::Convert { block, count } => convert_xp(block) * u64::from(count),
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

	/// §5.1's per-family anchors, held exactly: level 30 IS the legacy
	/// hardness table for EVERY family (the old table lives on as the
	/// shared mid-curve golden), each family's 1/99 endpoints are pinned,
	/// Soft reproduces the original SIM-v11 curve bit-exactly, Fixed
	/// never scales — and `required` never rises with level anywhere.
	#[test]
	fn dig_speed_anchors_are_pinned() {
		use crate::block::DigFamily as F;
		for (family, start, end) in [
			(F::Fixed, 1000, 1000),
			(F::Soft, 1250, 417),
			(F::Wood, 1350, 500),
			(F::Rock, 1700, 417),
		] {
			assert_eq!(dig_speed_permille(family, 1), start, "{family:?} at 1");
			assert_eq!(
				dig_speed_permille(family, 30),
				1000,
				"{family:?} parity with today at 30"
			);
			assert_eq!(dig_speed_permille(family, 99), end, "{family:?} at 99");
		}
		// Soft IS the shipped v11 curve, value for value.
		for level in 1..=99 {
			let legacy = if level <= 30 {
				1250 - (500 * (level - 1) + 29) / 58
			} else {
				1000 - (1166 * (level - 30) + 69) / 138
			};
			assert_eq!(dig_speed_permille(F::Soft, level), legacy, "level {level}");
		}
		for &b in &BlockType::ALL {
			let Some(h) = b.hardness_ticks() else {
				continue;
			};
			assert_eq!(
				dig_required(b, h, 30),
				u32::from(h),
				"{b:?} at 30 = the legacy table exactly"
			);
			for level in 2..=99 {
				assert!(
					dig_required(b, h, level) <= dig_required(b, h, level - 1),
					"{b:?} never slows with a level ({level})"
				);
			}
		}
		// The §5.1 table's spot values (60 Hz ticks).
		use BlockType as B;
		assert_eq!(dig_required(B::Stone, 90, 1), 153, "stone 2.55s at level 1");
		assert_eq!(dig_required(B::Stone, 90, 99), 38, "stone 0.63s at 99");
		assert_eq!(dig_required(B::Wood, 60, 1), 81, "wood 1.35s at level 1");
		assert_eq!(dig_required(B::Wood, 60, 99), 30, "wood 0.50s at 99");
		assert_eq!(dig_required(B::Dirt, 30, 1), 38, "dirt 0.63s at level 1");
		// Fixed never scales: leaves and the scoop at 1 / 30 / 99.
		for level in [1, 30, 99] {
			assert_eq!(dig_required(B::Leaves, 10, level), 10, "leaves flat");
			assert_eq!(dig_required(B::Water, 20, level), 20, "scoop flat");
		}
	}

	/// The rock gate (§5.1, reversed-by-design "strength wall"): rock
	/// refuses below its Mining level; gravel is the ungated entry rock.
	#[test]
	fn rock_is_gated_until_its_level() {
		use BlockType as B;
		assert_eq!(dig_level_gate(B::Sandstone), 3);
		for b in [B::Stone, B::StoneStairs, B::StoneSlab, B::Rubble] {
			assert_eq!(dig_level_gate(b), 5, "{b:?}");
		}
		for b in [B::Gravel, B::Dirt, B::Wood, B::Leaves, B::Water, B::Air] {
			assert_eq!(dig_level_gate(b), 1, "{b:?} ungated");
		}
	}

	/// The fanfare's question, derived from the gate tables: the levels
	/// that hand you something are exactly the gate levels, and every
	/// other level is an ordinary one.
	#[test]
	fn unlock_levels_are_the_gate_levels() {
		// Mining: the two rock walls, and nothing else.
		let mining: Vec<u32> = (1..=MAX_LEVEL)
			.filter(|&l| level_unlocks_something(Skill::Mining, l))
			.collect();
		assert_eq!(mining, vec![3, 5], "sandstone, then stone");
		// Alchemy: craft unlocks (5/8/10/20/30/50), the bucket charge's
		// extraction (15), and the bonus lines (25/40/60/75/90).
		let alchemy: Vec<u32> = (1..=MAX_LEVEL)
			.filter(|&l| level_unlocks_something(Skill::Alchemy, l))
			.collect();
		assert_eq!(alchemy, vec![5, 8, 10, 15, 20, 25, 30, 40, 50, 60, 75, 90]);
		// Level 1 is where everyone starts — you never level up into it,
		// and the slabs sitting at unlock 1 must not say otherwise.
		for s in Skill::ALL {
			assert!(!level_unlocks_something(s, 1), "{s:?}");
			assert!(!level_unlocks_something(s, 0));
			assert!(!level_unlocks_something(s, MAX_LEVEL + 1));
		}
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

	/// §4.1's table, keyed on BASE yields/costs: Stone/Wood/Grass 9,
	/// Dirt 6, Snow 3, the bucket charge 6, Rubble 8 — fixed for life
	/// regardless of level-scaled outcomes.
	#[test]
	fn alchemy_xp_derives_from_the_base_tables() {
		use crate::inventory::AlchemyOp;
		use BlockType as B;
		for (block, xp) in [
			(B::Stone, 9u64),
			(B::Wood, 9),
			(B::Grass, 9),
			(B::Rubble, 8),
			(B::Dirt, 6),
			(B::Snow, 3),
			(B::Water, 6),
			(B::Leaves, 1),
		] {
			assert_eq!(convert_xp(block), xp, "{block:?}");
		}
		assert_eq!(
			alchemy_xp(&AlchemyOp::Convert {
				block: B::Stone,
				count: 3
			}),
			27
		);
		// Crafts pay a third of their IDEAL cost, rounded up: stairs
		// 18 → 6, a raw Dirt block (23 + 5 = 28) → 10.
		assert_eq!(craft_xp(B::WoodStairs), 6);
		assert_eq!(craft_xp(B::Dirt), 10);
		assert_eq!(craft_xp(B::Air), 0, "no recipe, no XP");
	}
}
