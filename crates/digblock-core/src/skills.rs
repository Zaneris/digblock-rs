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

	/// Sentence-case display for feed lines ("Mining is now level N").
	pub fn display(self) -> &'static str {
		match self {
			Skill::Mining => "Mining",
			Skill::Alchemy => "Alchemy",
		}
	}
}

pub const MAX_LEVEL: u32 = 99;

/// XP to advance FROM level L (index L-1) to L+1: a slow exponential, a
/// fixed leading coefficient against a fixed per-level growth ratio, so the
/// price doubles over a fixed span of levels (SKILLS-DESIGN.md §2 argues
/// the shape). THIS TABLE IS THE CURVE — it is not a cache of a formula
/// stated elsewhere. Pinned as integers rather than computed with `powf` at
/// runtime: the values are a golden constant both sides must agree on
/// forever, and libm rounding is not a foundation for that. Generated once
/// at full precision, and `xp_curve_table_is_pinned` holds it.
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
/// cap (and below the first level, which no player is ever at).
pub fn xp_to_next(level: u32) -> Option<u64> {
	if !(1..MAX_LEVEL).contains(&level) {
		return None;
	}
	Some(XP_TO_NEXT[(level - 1) as usize])
}

/// Cumulative XP at which `level` is first reached (nothing is owed at
/// the first level or below).
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

/// The levels at which a dig gets faster: a fixed stride up the ladder,
/// plus the cap.
///
/// A dig speeds up a couple of dozen times in a life rather than at every
/// level. Under the old per-level curve most levels moved a hold by a
/// single tick or by nothing at all, so the reward read as noise; a
/// fraction of the levels carrying proportionally more is the same total
/// speedup made legible.
/// The grid is deliberately independent of the curve's shape — this
/// table says WHEN a player gets faster, [`DIG_TAPER`] says by how much.
pub const DIG_STEPS: [u32; 21] = [
	1, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90, 95, 99,
];

/// How much of the journey from a family's novice speed to its mastery
/// speed is complete at each of [`DIG_STEPS`], in permille. A pinned integer
/// table, never a `powf` — same reason as the XP curve: it is a golden
/// both ends must agree on to the unit, and a float would let two
/// platforms round a hold apart.
///
/// **The increments strictly decrease, and that IS the design.** Each
/// jump is a constant fraction smaller than the one before it, so the
/// very first step is the biggest speedup a player will ever feel, and
/// every later one lands a little softer. Both halves of that matter: the
/// early game is where a slow dig is most discouraging, and diminishing
/// returns are what stop a late level from being a rounding error. The
/// relative gain falls too, not just the absolute one, so the curve eases
/// off in felt terms and not merely in ticks.
///
/// One table serves every family: a family declares only where its range
/// starts and ends, and the SHAPE of the journey between them is shared.
/// `Fixed` has a zero-width range, so it stays flat for free.
const DIG_TAPER: [u32; 21] = [
	0, 139, 259, 364, 455, 535, 604, 664, 716, 761, 801, 835, 865, 891, 914, 934, 951, 966, 979,
	990, 1000,
];

/// Which entry of [`DIG_STEPS`]/[`DIG_TAPER`] a level spends: every level
/// digs at the speed of the last step it reached, and the cap is its own
/// final step.
fn dig_step_index(level: u32) -> usize {
	let level = level.clamp(1, 99);
	if level >= 99 {
		DIG_STEPS.len() - 1
	} else {
		(level / 5) as usize
	}
}

/// The level whose speed `level` actually digs at — the last step it has
/// reached. Exposed because the UI has to be able to say when the next
/// jump lands: a reward a player cannot see coming is the thing the grid
/// exists to fix. `dig_speed_step(l) == l` is exactly "this level makes
/// me faster".
pub fn dig_speed_step(level: u32) -> u32 {
	DIG_STEPS[dig_step_index(level)]
}

/// §5.1's per-family dig-speed multiplier in permille: two endpoints per
/// family — its novice speed and its mastery speed — with [`DIG_TAPER`]
/// deciding how the journey between them is spent. A family therefore
/// says only how hard its material resists a novice and how fast mastery
/// gets; the shape of the climb is the same for all of them. Fixed is
/// flat: foliage is already near-instant, the torch is a fixture, and the
/// water scoop's duration is pinned to the viewmodel swing.
///
/// The mastery endpoint IS the speed floor — no second clamp. There is no
/// mid anchor: one mid-ladder level used to be pinned so that it
/// reproduced the shipped hardness table exactly, through SIM v12–v17, and
/// SIM v18 dropped it. So `hardness_ticks` is a BASE the curve scales at
/// every level, and outside `Fixed` no level spends it unscaled — don't
/// reintroduce an assertion that some level returns the raw hardness.
pub fn dig_speed_permille(family: crate::block::DigFamily, level: u32) -> u32 {
	use crate::block::DigFamily;
	let (start, end) = match family {
		DigFamily::Fixed => (1000, 1000),
		DigFamily::Soft => (1875, 417),
		DigFamily::Wood => (2025, 500),
		DigFamily::Rock => (2550, 417),
	};
	start - (DIG_TAPER[dig_step_index(level)] * (start - end) + 500) / 1000
}

/// THE hold-to-dig duration both sides compute: the hardness scaled by the
/// family multiplier for the digger's Mining level, rounded and floored to
/// at least one tick. A level-up landing mid-hold skews client/server by a few
/// ticks at most, which `dig_grace` absorbs — no new tolerance
/// machinery. Hardness stays a separate parameter because callers hold
/// the y-gated `dig_hardness(y)` result (the bedrock rule lives there).
pub fn dig_required(block: BlockType, hardness: u16, level: u32) -> u32 {
	((u32::from(hardness) * dig_speed_permille(block.dig_family(), level) + 500) / 1000).max(1)
}

/// How long a released hold takes to bleed all the way back to
/// untouched. One second is the MAXIMUM, not a flat cost: the rate is
/// one whole bar per second, so a block a tick from breaking takes the
/// full second and a barely-started one is gone almost at once. Setting
/// it as a fraction of the bar rather than in ticks is what makes the
/// crack retreat at the same visible speed on leaves and on stone.
pub const DIG_DECAY_TICKS: u32 = 60;

/// The Resonant Pick's hold multiplier in permille (SKILLS-DESIGN §5.6),
/// applied on top of the family curve when the pick is the ACTIVE tool.
///
/// **Level-only, never derived from how many cells actually qualify.** A
/// world-derived multiplier would let client and server disagree by the
/// whole multiplier whenever their worlds differ by one cell, which is far
/// outside what `dig_grace` absorbs — the honest outcomes are a free
/// completion or a `DigIncomplete` on a dig the player finished.
///
/// The consequence is deliberate: a LONE block costs the multiplier too.
/// That is why the numbers are what they are — the spread penalty is set
/// against an offsetting tool bonus that very nearly cancels it, so
/// swinging at one block costs about what a bare hand does and the pick is
/// always worth holding, while a full row clears in the time of one.
pub fn pick_permille(level: u32) -> u32 {
	match crate::unlock::pick_cells(level) {
		9 => 1800,
		3 => 1050,
		_ => 1000,
	}
}

/// [`dig_required`] with the ACTIVE TOOL taken into account — the shape
/// every dig-timing site should use, since the pick is the one tool that
/// changes how long a hold takes.
pub fn dig_required_with(
	block: BlockType,
	hardness: u16,
	level: u32,
	active_tool: BlockType,
) -> u32 {
	let base = dig_required(block, hardness, level);
	if active_tool != BlockType::ResonantPick {
		return base;
	}
	(base * pick_permille(level)).div_ceil(1000).max(1)
}

/// What is left of `progress` banked ticks out of `required` after
/// `idle` ticks of not digging.
///
/// THE one decay rule: the client predicts with it, the server's
/// completion gate charges it, and offline play runs the same call, so
/// no two of them can drift. Integer and exact — it takes the ELAPSED
/// idle count rather than accumulating a per-tick subtraction, because a
/// fractional per-tick rate would be under one for every block in the game,
/// so it would round to zero and decay nothing at all.
///
/// Truncating down means the bar always outlives its arithmetic by up
/// to a tick, and reaches exactly zero at [`DIG_DECAY_TICKS`] for every
/// hardness.
pub fn dig_decayed(progress: u32, required: u32, idle: u32) -> u32 {
	let idle = idle.min(DIG_DECAY_TICKS);
	progress.saturating_sub(required.saturating_mul(idle) / DIG_DECAY_TICKS)
}

// Progression gates — the Mining strength wall and the Alchemy craft
// gate — live in `crate::unlock` with every other threshold, next to the
// one question each of their consumers asks. This module owns the curve,
// the totals and the award tables; not who may do what.

// ---- award tables ----------------------------------------------------
//
// XP keys on fixed per-action bases, never on level-scaled outcomes —
// per-action XP is fixed for life. Shared verbatim by the server's
// chokepoints and the offline engine.

/// Mining XP for a completed dig of `block`, scaled down from its hardness.
/// Zero for the undiggable (which can't complete a dig anyway).
pub fn dig_xp(block: BlockType) -> u64 {
	match block.hardness_ticks() {
		Some(h) => u64::from(h).div_ceil(6),
		None => 0,
	}
}

/// Alchemy XP per block extracted at the bench, scaled down from the
/// block's BASE yield — fixed for life, never the level-scaled
/// outcome (§4.1).
pub fn convert_xp(block: BlockType) -> u64 {
	u64::from(crate::alchemy::extract_total_base(block)).div_ceil(3)
}

/// The DUAL award for one field extraction at the rock face (§5.4):
/// `(Mining, Alchemy)`. `None` for anything that is not an ore.
///
/// Field extraction is mechanically a mine and thematically a crude
/// alchemical separation, so it pays both — and the Alchemy cut is
/// deliberately the small half, with the substantial Alchemy XP waiting
/// at the bench (`convert_xp`). Each ore therefore carries a fixed total
/// split across TWO moments, and neither skill can be levelled entirely
/// from inside the other.
///
/// Tabled rather than derived from hardness: this is a headline reward
/// and the curve through it is a decision, not a consequence.
pub fn ore_field_xp(ore: BlockType) -> Option<(u64, u64)> {
	Some(match ore {
		BlockType::Emberore => (40, 8),
		BlockType::Duskstone => (70, 14),
		BlockType::Glowvein => (110, 22),
		BlockType::Aetherock => (170, 34),
		_ => return None,
	})
}

/// Alchemy XP per crafted batch, scaled down from the ideal cost summed
/// across the recipe's cost lines (pre-waste — fixed for life). Zero for
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

	/// The golden cumulative pin on [`XP_TO_NEXT`] (SKILLS-DESIGN.md §2 for
	/// the shape it buys). If curve tuning is ever wanted, this pin moves
	/// WITH it, deliberately.
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

	/// §5.1's per-family endpoints, held exactly, plus goldens along the
	/// shared taper — the old mid-ladder anchor is now derived (SIM v18 drops
	/// the mid-curve parity with the shipped hardness table), so it is
	/// pinned here as an OUTPUT to catch a taper edit nobody meant.
	#[test]
	fn dig_speed_anchors_are_pinned() {
		use crate::block::DigFamily as F;
		// The two design inputs per family: its novice and mastery endpoints.
		for (family, start, end) in [
			(F::Fixed, 1000, 1000),
			(F::Soft, 1875, 417),
			(F::Wood, 2025, 500),
			(F::Rock, 2550, 417),
		] {
			assert_eq!(dig_speed_permille(family, 1), start, "{family:?} at 1");
			assert_eq!(dig_speed_permille(family, 99), end, "{family:?} at 99");
		}
		// Derived goldens along the way, one early / mid / late.
		for (family, at15, at30, at60) in [
			(F::Fixed, 1000, 1000, 1000),
			(F::Soft, 1344, 994, 614),
			(F::Wood, 1470, 1104, 706),
			(F::Rock, 1774, 1262, 705),
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
		// The §5.1 table's spot values, in sim ticks.
		use BlockType as B;
		assert_eq!(dig_required(B::Stone, 90, 1), 230, "stone 3.83s at level 1");
		assert_eq!(dig_required(B::Stone, 90, 30), 114, "stone 1.90s at 30");
		assert_eq!(dig_required(B::Stone, 90, 99), 38, "stone 0.63s at 99");
		assert_eq!(dig_required(B::Wood, 60, 1), 122, "wood 2.03s at level 1");
		assert_eq!(dig_required(B::Wood, 60, 30), 66, "wood 1.10s at 30");
		assert_eq!(dig_required(B::Wood, 60, 99), 30, "wood 0.50s at 99");
		assert_eq!(dig_required(B::Dirt, 30, 1), 56, "dirt 0.93s at level 1");
		assert_eq!(dig_required(B::Dirt, 30, 30), 30, "dirt 0.50s at 30");
		// Fixed never scales: leaves and the scoop hold all the way up.
		for level in [1, 30, 99] {
			assert_eq!(dig_required(B::Leaves, 10, level), 10, "leaves flat");
			assert_eq!(dig_required(B::Water, 20, level), 20, "scoop flat");
		}
	}

	/// The grid and the taper together: a dig gets faster a fixed number of
	/// times, the FIRST jump is the biggest of the game, and every jump
	/// after it is strictly smaller than the one before.
	#[test]
	fn every_dig_speedup_is_smaller_than_the_one_before() {
		use crate::block::DigFamily as F;
		// The grid: the earliest levels hold at the first step, then the
		// stride, then the cap on its own.
		for level in 1..=99 {
			let expect = match level {
				1..=4 => 1,
				99 => 99,
				_ => level / 5 * 5,
			};
			assert_eq!(dig_speed_step(level), expect, "step of {level}");
		}
		assert_eq!(dig_speed_step(0), 1, "clamps up");
		assert_eq!(dig_speed_step(1000), 99, "clamps down");
		assert_eq!(DIG_STEPS.len(), DIG_TAPER.len(), "one taper entry per step");
		// The taper spends the whole journey and eases off the whole way.
		assert_eq!(DIG_TAPER[0], 0, "the first step is the level-1 speed");
		assert_eq!(DIG_TAPER[20], 1000, "the last step is the level-99 speed");
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
			assert_eq!(speeds.len(), if flat { 1 } else { 21 }, "{family:?} speeds");
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
		// first jump dwarfs the last.
		use BlockType as B;
		assert_eq!(dig_required(B::Stone, 90, 4), 230, "still the level-1 hold");
		assert_eq!(
			dig_required(B::Stone, 90, 5),
			203,
			"the biggest jump: 27 ticks"
		);
		assert_eq!(dig_required(B::Stone, 90, 9), 203, "held to the next step");
		assert_eq!(dig_required(B::Stone, 90, 95), 39, "the 95 step");
		assert_eq!(dig_required(B::Stone, 90, 98), 39, "held at the 95 step");
		assert_eq!(dig_required(B::Stone, 90, 99), 38, "the last jump: 1 tick");
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
		let need = dig_required(BlockType::Stone, 90, 1); // 153
		assert_eq!(dig_decayed(need / 4, need, DIG_DECAY_TICKS / 4), 0);
		// And a single banked tick on a long hold is gone on the first.
		assert_eq!(dig_decayed(1, need, 1), 0);
	}

	/// §5.2's award table, derived from the SHIPPED hardness values — XP is
	/// fixed per action for life.
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

	/// §4.1's table, keyed on BASE yields/costs — fixed for life regardless
	/// of level-scaled outcomes.
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
		// Crafts pay a fixed fraction of their IDEAL cost, rounded up.
		assert_eq!(craft_xp(B::WoodStairs), 6);
		assert_eq!(craft_xp(B::Dirt), 10);
		assert_eq!(craft_xp(B::Air), 0, "no recipe, no XP");
	}
}
