//! The alchemy economy: extraction yields, bonus lines, transmutation
//! costs, unlock levels, and the bench pace — every number the server
//! and offline play must agree on (SKILLS-DESIGN §3–§4, §7). All
//! integer math, no floats: yields and costs are cross-side constants.
//!
//! Two invariants hold the economy shut, both pinned by tests here:
//!
//! - **No matter creation**: for every craftable block at every level,
//!   the craft cost strictly exceeds the extraction yield in every
//!   essence — extract→craft→extract is a sink, always.
//! - **Bonus lines only on uncraftable blocks.** A bonus line on a
//!   craftable block is an essence PUMP: craft it from its cost
//!   essences, extract it back, pocket the bonus — at 99 that turns
//!   2 Earth into 1 Metal, or worse, manufactures Aether, which nothing
//!   may. So trace minerals and the Aether lines live on MINED rock
//!   (Rubble, Gravel) and foliage, which have no recipes.

use crate::block::{BlockType, Essence};
use crate::shapes::craft_recipe;

/// Base extraction yields at 100% efficiency (level 99), ≤ 27 total —
/// the shortfall is structure lost to the process. Empty = not
/// extractable at the bench.
pub fn extract_base(b: BlockType) -> &'static [(Essence, u32)] {
	use BlockType as B;
	use Essence as E;
	match b {
		B::Stone => &[(E::Earth, 27)],
		B::Rubble => &[(E::Earth, 24)],
		B::Sandstone => &[(E::Earth, 21)],
		B::Gravel => &[(E::Earth, 15)],
		B::Sand => &[(E::Earth, 12)],
		B::Dirt => &[(E::Earth, 15), (E::Flora, 3)],
		B::Grass => &[(E::Earth, 18), (E::Flora, 9)],
		B::DryGrass => &[(E::Earth, 18), (E::Flora, 6)],
		B::Mud => &[(E::Earth, 12), (E::Flora, 3), (E::Aqua, 3)],
		B::Wood => &[(E::Flora, 27)],
		B::Leaves | B::PineLeaves => &[(E::Flora, 3)],
		B::Snow => &[(E::Aqua, 9)],
		// The bucket charge: extracting it empties the tool.
		B::Water => &[(E::Aqua, 18)],
		B::WoodStairs => &[(E::Flora, 18)],
		B::StoneStairs => &[(E::Earth, 18)],
		B::WoodDoor => &[(E::Flora, 10)],
		B::WoodSlab => &[(E::Flora, 9)],
		B::StoneSlab => &[(E::Earth, 9)],
		_ => &[],
	}
}

/// Flat bonus lines, NOT scaled by efficiency — the unlock is the whole
/// point. Only ever on blocks without a craft recipe (see module doc;
/// pinned by `bonus_lines_never_sit_on_craftable_blocks`).
pub fn bonus_lines(b: BlockType, level: u32) -> &'static [(Essence, u32)] {
	use BlockType as B;
	use Essence as E;
	match b {
		// "Morning dew" — foliage starts carrying a little water.
		B::Leaves if level >= 25 => &[(E::Aqua, 1)],
		// "Resin" — the design doc's heartwood line, moved off craftable
		// Wood (the pump rule above): pine forests are the pre-ore
		// Aether trickle instead.
		B::PineLeaves if level >= 75 => &[(E::Aqua, 1), (E::Aether, 1)],
		B::PineLeaves if level >= 25 => &[(E::Aqua, 1)],
		// "Trace minerals" — mined rock only; clean Stone/Sandstone are
		// craftable and would pump Earth into Metal.
		B::Rubble if level >= 90 => &[(E::Metal, 1), (E::Aether, 1)],
		B::Rubble if level >= 40 => &[(E::Metal, 1)],
		B::Gravel if level >= 60 => &[(E::Metal, 2)],
		_ => &[],
	}
}

/// Level-scaled yield for one base line: 40% at level 1 → 100% at 99,
/// floored, min 1 (a listed line never vanishes entirely).
/// `efficiency(L) = 40% + 60%·(L−1)/98`, as integer math.
pub fn scaled_yield(base: u32, level: u32) -> u32 {
	let level = level.clamp(1, 99);
	(base * (3920 + 60 * (level - 1)) / 9800).max(1)
}

/// Everything one extracted block yields at `level`: scaled bases plus
/// flat bonus lines, merged per essence.
pub fn extract_yields(b: BlockType, level: u32) -> Vec<(Essence, u32)> {
	let mut out: Vec<(Essence, u32)> = extract_base(b)
		.iter()
		.map(|&(e, base)| (e, scaled_yield(base, level)))
		.collect();
	for &(e, n) in bonus_lines(b, level) {
		match out.iter_mut().find(|(oe, _)| *oe == e) {
			Some((_, on)) => *on += n,
			None => out.push((e, n)),
		}
	}
	out
}

/// Alchemy level required to extract this block (1 = always). The
/// bucket charge waits for 15 — the Aqua economy's opening move.
pub fn extract_unlock(b: BlockType) -> u32 {
	match b {
		BlockType::Water => 15,
		_ => 1,
	}
}

/// Sum of a block's base yields — the fixed value extraction XP keys on
/// (`ceil(total / 3)`, never the level-scaled outcome).
pub fn extract_total_base(b: BlockType) -> u32 {
	extract_base(b).iter().map(|&(_, n)| n).sum()
}

/// Transmutation waste multiplier in permille: 1600 (1.60×) at level 1
/// easing to 1050 (1.05×) at 99, with the level-85 unlock clamping the
/// tail to 1100 early. Monotonically non-increasing (pinned).
pub fn waste_permille(level: u32) -> u32 {
	let level = level.clamp(1, 99);
	// round(550·(L−1)/98)
	let lin = 1600 - (550 * (level - 1) + 49) / 98;
	if level >= 85 {
		lin.min(1100)
	} else {
		lin
	}
}

/// One cost line at `level`: `ceil(ideal · waste)`.
pub fn craft_cost(ideal: u32, level: u32) -> u32 {
	(ideal * waste_permille(level)).div_ceil(1000)
}

/// Ideal (pre-waste) craft cost lines for anything craftable: shaped
/// items at their volume-rule cost in their family's essence, raw
/// blocks at the full 27-volume split in their extraction ratio
/// (`ceil(27 · base_e / base_total)` per line — with the ≤ 27 bases,
/// that alone yields no-matter-creation). `None` = not craftable.
pub fn craft_ideal(b: BlockType) -> Option<Vec<(Essence, u32)>> {
	use BlockType as B;
	use Essence as E;
	// Shaped items keep the pinned volume rule via `craft_recipe`; the
	// family essence is Flora for wood, Earth for stone.
	if let Some(r) = craft_recipe(b) {
		return Some(vec![(r.material, r.cost)]);
	}
	let lines: &[(E, u32)] = match b {
		B::Stone | B::Sand | B::Sandstone => &[(E::Earth, 27)],
		B::Wood => &[(E::Flora, 27)],
		// 27-volume in the extraction ratio: Dirt 15/3 of 18 → 23 + 5.
		B::Dirt => &[(E::Earth, 23), (E::Flora, 5)],
		B::Grass => &[(E::Earth, 18), (E::Flora, 9)],
		B::Mud => &[(E::Earth, 18), (E::Flora, 5), (E::Aqua, 5)],
		B::Snow => &[(E::Aqua, 27)],
		// The bucket charge: lossy against its 18-Aqua extraction — no
		// loop. Refused while the bucket is already full.
		B::Water => &[(E::Aqua, 27)],
		_ => return None,
	};
	Some(lines.to_vec())
}

/// Alchemy level that unlocks crafting `b`; `None` = never craftable.
pub fn craft_unlock(b: BlockType) -> Option<u32> {
	use BlockType as B;
	Some(match b {
		B::WoodSlab | B::StoneSlab => 1,
		B::WoodStairs | B::StoneStairs => 5,
		B::WoodDoor => 10,
		B::Stone | B::Wood | B::Dirt | B::Sand => 20,
		B::Sandstone | B::Snow | B::Grass | B::Mud => 30,
		B::Water => 50,
		_ => return None,
	})
}

/// Everything craftable, in the transmute tab's display order: shaped
/// items first (the original catalog), then raw blocks by unlock.
pub const CRAFT_ORDER: [BlockType; 14] = [
	BlockType::WoodSlab,
	BlockType::StoneSlab,
	BlockType::WoodStairs,
	BlockType::StoneStairs,
	BlockType::WoodDoor,
	BlockType::Stone,
	BlockType::Wood,
	BlockType::Dirt,
	BlockType::Sand,
	BlockType::Sandstone,
	BlockType::Snow,
	BlockType::Grass,
	BlockType::Mud,
	BlockType::Water,
];

/// Bench extraction pace in 60 Hz ticks per block: 0.5 s at level 1,
/// 0.35 s from 25, 0.25 s from 75. Client-paced for presentation,
/// server-enforced via the extraction token bucket.
pub fn extract_pace_ticks(level: u32) -> u32 {
	if level >= 75 {
		15
	} else if level >= 25 {
		21
	} else {
		30
	}
}

/// Extraction token-bucket burst (in blocks) — absorbs network bunching
/// exactly as `EDIT_BURST` does for edits.
pub const EXTRACT_BURST: f32 = 2.0;

#[cfg(test)]
mod tests {
	use super::*;
	use crate::shapes::SUBS_PER_BLOCK;

	#[test]
	fn efficiency_and_waste_hit_their_pinned_endpoints() {
		// §3.2: level-1 grass = 7 Earth + 3 Flora; 99 = the full 18 + 9.
		assert_eq!(scaled_yield(18, 1), 7);
		assert_eq!(scaled_yield(9, 1), 3);
		assert_eq!(scaled_yield(18, 99), 18);
		assert_eq!(scaled_yield(9, 99), 9);
		assert_eq!(scaled_yield(27, 99), 27, "100% at the cap, never more");
		// §4.2: 1.60× at 1 → 1.05× at 99; §4.3: ≤1.10× from 85.
		assert_eq!(waste_permille(1), 1600);
		assert_eq!(waste_permille(99), 1050);
		assert!(waste_permille(85) <= 1100);
		for l in 2..=99 {
			assert!(
				waste_permille(l) <= waste_permille(l - 1),
				"waste never rises ({l})"
			);
			for base in [3, 9, 18, 27] {
				assert!(
					scaled_yield(base, l) >= scaled_yield(base, l - 1),
					"yield never falls ({base}@{l})"
				);
			}
		}
	}

	/// The economy's master pin: for every craftable block at every
	/// level, the craft cost strictly exceeds the extraction yield in
	/// EVERY essence, bonus lines included — extract→craft→extract is a
	/// sink at level 1 and at level 99 and everywhere between.
	#[test]
	fn no_level_ever_creates_matter() {
		for &b in &BlockType::ALL {
			let Some(ideal) = craft_ideal(b) else {
				continue;
			};
			for level in 1..=99 {
				let yields = extract_yields(b, level);
				for &(e, got) in &yields {
					let paid: u32 = ideal
						.iter()
						.filter(|(ie, _)| *ie == e)
						.map(|&(_, n)| craft_cost(n, level))
						.sum();
					assert!(
						paid > got,
						"{b:?} at {level}: {e:?} yields {got} vs cost {paid}"
					);
				}
			}
		}
	}

	/// A bonus line on a craftable block is an essence pump (craft from
	/// the cost essences, extract back, pocket the bonus) — at 99 it
	/// would turn 2 Earth into 1 Metal or manufacture Aether outright.
	/// Structural rule: bonus lines only on recipe-less blocks.
	#[test]
	fn bonus_lines_never_sit_on_craftable_blocks() {
		for &b in &BlockType::ALL {
			for level in 1..=99 {
				if !bonus_lines(b, level).is_empty() {
					assert!(
						craft_ideal(b).is_none(),
						"{b:?} has a bonus line at {level} but is craftable"
					);
				}
			}
		}
	}

	/// §7.1's loop numbers: mined rock is Rubble, and refining it into
	/// clean Stone costs ~5 rubble at level 1 and ~1.2 at 99 — always a
	/// sink, and the recycle of clean Stone is one too.
	#[test]
	fn mined_rock_is_always_rubble() {
		use BlockType as B;
		for rock in [B::Stone, B::Sandstone] {
			assert_eq!(rock.dig_drop(), Some(B::Rubble), "{rock:?}");
		}
		assert_eq!(B::Rubble.dig_drop(), Some(B::Rubble), "rubble round-trips");
		assert!(craft_ideal(B::Rubble).is_none(), "rubble is never crafted");
		// Level 1: 9 Earth out of a rubble, 44 in for a clean stone.
		assert_eq!(scaled_yield(24, 1), 9);
		assert_eq!(craft_cost(27, 1), 44);
		// Level 99: 24 out, 29 in — still a sink.
		assert_eq!(scaled_yield(24, 99), 24);
		assert_eq!(craft_cost(27, 99), 29);
		// The clean-stone recycle stays shut too: 27 back vs 29 to craft.
		assert_eq!(extract_total_base(B::Stone), 27);
	}

	#[test]
	fn unlocks_and_pace_follow_the_level_table() {
		use BlockType as B;
		assert_eq!(craft_unlock(B::WoodSlab), Some(1));
		assert_eq!(craft_unlock(B::WoodStairs), Some(5));
		assert_eq!(craft_unlock(B::WoodDoor), Some(10));
		assert_eq!(craft_unlock(B::Stone), Some(20));
		assert_eq!(craft_unlock(B::Sandstone), Some(30));
		assert_eq!(craft_unlock(B::Water), Some(50));
		assert_eq!(craft_unlock(B::Rubble), None);
		assert_eq!(extract_unlock(B::Water), 15);
		assert_eq!(extract_pace_ticks(1), 30);
		assert_eq!(extract_pace_ticks(25), 21);
		assert_eq!(extract_pace_ticks(75), 15);
		// Every CRAFT_ORDER row really is craftable, and nothing
		// craftable is missing from it.
		for &b in &CRAFT_ORDER {
			assert!(craft_ideal(b).is_some(), "{b:?}");
			assert!(craft_unlock(b).is_some(), "{b:?}");
		}
		for &b in &BlockType::ALL {
			assert_eq!(
				craft_ideal(b).is_some(),
				CRAFT_ORDER.contains(&b),
				"{b:?} craftable iff listed"
			);
		}
	}

	/// Base yields never exceed a whole block's 27 sub-units, and every
	/// extractable block pays at least something at level 1.
	#[test]
	fn bases_stay_under_a_block() {
		for &b in &BlockType::ALL {
			let total = extract_total_base(b);
			assert!(total <= SUBS_PER_BLOCK, "{b:?} base {total}");
			if total > 0 {
				let l1: u32 = extract_yields(b, 1).iter().map(|&(_, n)| n).sum();
				assert!(l1 >= 1, "{b:?} pays nothing at level 1");
			}
		}
	}
}
