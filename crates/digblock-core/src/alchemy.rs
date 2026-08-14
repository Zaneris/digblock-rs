//! The alchemy economy: extraction yields, bonus lines, transmutation
//! costs, and the bench pace — every number the server and offline play
//! must agree on (SKILLS-DESIGN §3–§4, §7). All integer math, no floats:
//! yields and costs are cross-side constants. The LEVELS that open each
//! recipe are not here: every threshold in the game lives in
//! [`crate::unlock`], beside the gates that enforce it.
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
//!
//! Neither table is written out block by block any more. A structural
//! block is a MATERIAL in a SHAPE (SKILLS-DESIGN §7.2, [`crate::material`]),
//! so both sides prorate the material's 27-unit split by the shape's
//! volume — and the two invariants above become properties of the axis
//! rather than facts a test happens to check. What remains hand-written
//! here is the FUNCTIONAL roster: torches, the bucket charge, lamps and
//! tools, which §7.3 prices by hand precisely because they are not matter.

use crate::block::{BlockType, Essence};
use crate::material::{cost_split, yield_split};

/// Functional items' extraction yields — the ones with no material, so
/// nothing to prorate. Everything else derives; see [`extract_base`].
fn functional_extract(b: BlockType) -> &'static [(Essence, u32)] {
	match b {
		// The bucket charge: extracting it empties the tool.
		BlockType::Water => &[(Essence::Aqua, 18)],
		// A torch is a fixture, not stock — you place it or you carry it.
		_ => &[],
	}
}

/// Base extraction yields at 100% efficiency (level 99), ≤ 27 total for a
/// whole block — the shortfall is structure lost to the process. Empty =
/// not extractable at the bench.
///
/// Derived: the material's base ([`crate::material::Material::extract_units`])
/// prorated DOWN to the shape's volume, so a stair gives back two thirds
/// of its substance and a slab a third, in whatever essences that
/// substance is made of. Allocates a short Vec; every caller is cold (op
/// validation and a screen, never a per-cell loop).
pub fn extract_base(b: BlockType) -> Vec<(Essence, u32)> {
	let Some((m, s)) = b.grid_cell() else {
		return functional_extract(b).to_vec();
	};
	let base = m.extract_units();
	if base.is_empty() {
		return Vec::new();
	}
	yield_split(base, s.units())
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
		.into_iter()
		.map(|(e, base)| (e, scaled_yield(base, level)))
		.collect();
	for &(e, n) in bonus_lines(b, level) {
		match out.iter_mut().find(|(oe, _)| *oe == e) {
			Some((_, on)) => *on += n,
			None => out.push((e, n)),
		}
	}
	out
}

/// Sum of a block's base yields — the fixed value extraction XP keys on
/// (`ceil(total / 3)`, never the level-scaled outcome).
pub fn extract_total_base(b: BlockType) -> u32 {
	extract_base(b).into_iter().map(|(_, n)| n).sum()
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

/// Functional items' craft costs — priced BY HAND (§7.3), never by
/// volume, because a torch's cost is a decision rather than a
/// measurement. Cross-family costs live here on purpose: they are what
/// make the essences feel like an economy instead of parallel currencies.
///
/// The LEVELS these open at are not here — every threshold in the game
/// lives in [`crate::unlock`], and `unlock::functional` is this list's
/// twin (pinned to it, so an item cannot have a price without a level or
/// the other way round).
pub fn functional_ideal(b: BlockType) -> Option<&'static [(Essence, u32)]> {
	use BlockType as B;
	use Essence as E;
	Some(match b {
		B::Torch => &[(E::Flora, 4), (E::Earth, 2)],
		// NO METAL, ever: ore needs the siphon and Metal needs ore, so a
		// Metal-priced siphon deadlocks the economy on the first world
		// where nobody has one. Flora and Earth are what a fresh miner
		// already has too much of.
		B::EssenceSiphon => &[(E::Flora, 30), (E::Earth, 15)],
		// Furniture is timber and joinery — near its volume, but chosen,
		// not measured: a chair is mostly air and still costs a chair.
		B::Chair => &[(E::Flora, 6)],
		B::Table => &[(E::Flora, 9)],
		// The bed is the catalog's first genuinely cross-family price:
		// a timber frame AND the bedding on it. Aqua has exactly one
		// source (the bucket charge, extraction unlocked at 15), so this
		// is what makes carrying water home worth doing.
		B::Bed => &[(E::Flora, 21), (E::Aqua, 6)],
		// The bucket charge: lossy against its 18-Aqua extraction — no
		// loop. Refused while the bucket is already full.
		B::Water => &[(E::Aqua, 27)],
		_ => return None,
	})
}

/// Ideal (pre-waste) craft cost lines for anything craftable. `None` =
/// not craftable.
///
/// Derived for every structural block: the material's 27-unit split
/// ([`crate::material::Material::craft_units`]) prorated UP to the
/// shape's volume. Rounding the cost side up where [`extract_base`]
/// rounds down is what makes "no matter creation" hold for every shape
/// of every material without a proof per pair.
pub fn craft_ideal(b: BlockType) -> Option<Vec<(Essence, u32)>> {
	if let Some(lines) = functional_ideal(b) {
		return Some(lines.to_vec());
	}
	let (m, s) = b.grid_cell()?;
	Some(cost_split(m.craft_units()?, s.units()))
}

/// Everything craftable, in the transmute tab's display order: by shape
/// family, members in material (i.e. unlock) order, with the hand-priced
/// functional items last. Derived from the grid, so a new material ×
/// shape pair appears without anyone maintaining a list.
pub fn craft_order() -> Vec<BlockType> {
	use crate::block::ShapeFamily;
	let mut out = Vec::new();
	for s in ShapeFamily::ALL {
		for m in crate::material::Material::ALL {
			match BlockType::from_grid(m, s) {
				Some(b) if craft_ideal(b).is_some() => out.push(b),
				_ => {}
			}
		}
	}
	out.extend(
		BlockType::ALL
			.into_iter()
			.filter(|b| !b.is_grid_item() && functional_ideal(*b).is_some()),
	);
	out
}

/// The bench's pace for a WHOLE block — one whose 27 sub-units all come
/// out, which is the 9-XP reference every other block is priced against:
/// 0.5 s at level 1, 0.35 s from 25, 0.25 s from 75.
pub fn base_pace_ticks(level: u32) -> u32 {
	if level >= 75 {
		15
	} else if level >= 25 {
		21
	} else {
		30
	}
}

/// The reference block's extraction XP (`convert_xp` of a full 27-unit
/// base), i.e. what [`base_pace_ticks`] is the pace OF.
const REF_XP: u32 = 9;

/// Floor on the pace, in ticks. Leaves pay a single XP, and a strictly
/// proportional pace would run them at 3 ticks — twenty a second, which
/// is a stream of wire ops rather than a bench.
const MIN_PACE_TICKS: u32 = 6;

/// Bench extraction pace in 60 Hz ticks for ONE block of `b` at `level`:
/// the level's whole-block pace scaled by what the block is WORTH, so
/// the bench spends its time in proportion to what it yields. A stone
/// (9 XP) takes the full interval, a slab third (3 XP) a third of it,
/// and leaves fall to the floor above.
///
/// Keying on `convert_xp` rather than on a second table is deliberate:
/// extraction XP is already `ceil(base_total / 3)`, i.e. the block's
/// worth at the bench, so pace and pay can never disagree about which
/// blocks are the substantial ones. Client-paced for presentation,
/// server-enforced via the extraction token bucket.
pub fn extract_pace_ticks(b: BlockType, level: u32) -> u32 {
	let xp = crate::skills::convert_xp(b) as u32;
	let ticks = (base_pace_ticks(level) * xp + REF_XP / 2) / REF_XP;
	ticks.max(MIN_PACE_TICKS)
}

/// Extraction token-bucket burst, in REFERENCE blocks — absorbs network
/// bunching exactly as `EDIT_BURST` does for edits. The bucket itself
/// banks TICKS (the pace is per-block now), so its capacity is this many
/// whole-block intervals: [`extract_burst_ticks`].
pub const EXTRACT_BURST: f32 = 2.0;

/// The extraction bucket's capacity in ticks at `level` — two whole
/// blocks' worth, exactly as before the pace became per-block.
pub fn extract_burst_ticks(level: u32) -> f32 {
	EXTRACT_BURST * base_pace_ticks(level) as f32
}

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

	/// The bench's pace steps, and the catalog: the LEVELS that open each
	/// recipe live in `crate::unlock`, which pins them beside the gates
	/// they feed — here we only hold the catalog to the recipe table.
	#[test]
	fn pace_steps_and_the_catalog_are_pinned() {
		assert_eq!(base_pace_ticks(1), 30);
		assert_eq!(base_pace_ticks(25), 21);
		assert_eq!(base_pace_ticks(75), 15);
		// The display order is derived, so it can neither list something
		// uncraftable nor miss something craftable — but it must still be
		// a permutation, with nothing doubled by a grid cell claimed twice.
		let order = craft_order();
		for &b in &order {
			assert!(craft_ideal(b).is_some(), "{b:?}");
		}
		for &b in &BlockType::ALL {
			assert_eq!(
				craft_ideal(b).is_some(),
				order.contains(&b),
				"{b:?} craftable iff listed"
			);
		}
		let mut seen = order.clone();
		seen.sort_by_key(|b| *b as u16);
		seen.dedup();
		assert_eq!(seen.len(), order.len(), "no block listed twice");
		// Shape-major, material-minor: every slab together, then every
		// stair — which is exactly how the transmute tab groups them.
		let mut runs: Vec<_> = order
			.iter()
			.map(|b| b.grid_cell().map(|(_, s)| s))
			.collect();
		runs.dedup();
		for (i, f) in runs.iter().enumerate() {
			assert!(
				!runs[..i].contains(f),
				"{f:?} members are not contiguous in the display order"
			);
		}
	}

	/// The launch roster's numbers, block by block — this is a REFACTOR
	/// of four hand-written tables into one derivation, so every recipe
	/// and yield that shipped must still be the same number.
	#[test]
	fn the_grid_reproduces_the_shipped_tables() {
		use BlockType as B;
		use Essence as E;
		/// One shipped row: the block, what it extracts to, what it costs.
		type Row = (
			BlockType,
			&'static [(Essence, u32)],
			&'static [(Essence, u32)],
		);
		let cases: &[Row] = &[
			// block, extraction base, craft ideal
			(B::Stone, &[(E::Earth, 27)], &[(E::Earth, 27)]),
			(B::Rubble, &[(E::Earth, 24)], &[]),
			(B::Sandstone, &[(E::Earth, 21)], &[(E::Earth, 27)]),
			(B::Gravel, &[(E::Earth, 15)], &[]),
			(B::Sand, &[(E::Earth, 12)], &[(E::Earth, 27)]),
			(
				B::Dirt,
				&[(E::Earth, 15), (E::Flora, 3)],
				&[(E::Earth, 23), (E::Flora, 5)],
			),
			(
				B::Grass,
				&[(E::Earth, 18), (E::Flora, 9)],
				&[(E::Earth, 18), (E::Flora, 9)],
			),
			(B::DryGrass, &[(E::Earth, 18), (E::Flora, 6)], &[]),
			(
				B::Mud,
				&[(E::Earth, 12), (E::Flora, 3), (E::Aqua, 3)],
				&[(E::Earth, 18), (E::Flora, 5), (E::Aqua, 5)],
			),
			(B::Wood, &[(E::Flora, 27)], &[(E::Flora, 27)]),
			(B::Leaves, &[(E::Flora, 1)], &[]),
			(B::PineLeaves, &[(E::Flora, 1)], &[]),
			(B::Snow, &[(E::Aqua, 9)], &[(E::Aqua, 27)]),
			(B::Water, &[(E::Aqua, 18)], &[(E::Aqua, 27)]),
			(B::WoodStairs, &[(E::Flora, 18)], &[(E::Flora, 18)]),
			(B::StoneStairs, &[(E::Earth, 18)], &[(E::Earth, 18)]),
			(B::LumberDoor, &[(E::Flora, 10)], &[(E::Flora, 10)]),
			(B::WoodSlab, &[(E::Flora, 9)], &[(E::Flora, 9)]),
			(B::StoneSlab, &[(E::Earth, 9)], &[(E::Earth, 9)]),
			(B::Lumber, &[(E::Flora, 27)], &[(E::Flora, 27)]),
			(B::LumberStairs, &[(E::Flora, 18)], &[(E::Flora, 18)]),
			(B::LumberSlab, &[(E::Flora, 9)], &[(E::Flora, 9)]),
			(B::Torch, &[], &[(E::Flora, 4), (E::Earth, 2)]),
		];
		// What the grid then GREW, at the same prices — a new material ×
		// shape pair is a block row, and these are the numbers that fall
		// out of it with nobody choosing them.
		let grown: &[Row] = &[
			(B::StoneDoor, &[(E::Earth, 10)], &[(E::Earth, 10)]),
			(B::SandstoneStairs, &[(E::Earth, 14)], &[(E::Earth, 18)]),
			(B::SandstoneSlab, &[(E::Earth, 7)], &[(E::Earth, 9)]),
			(B::SandstoneDoor, &[(E::Earth, 7)], &[(E::Earth, 10)]),
			(B::DarkLumber, &[(E::Flora, 27)], &[(E::Flora, 27)]),
			(B::DarkLumberStairs, &[(E::Flora, 18)], &[(E::Flora, 18)]),
			(B::DarkLumberSlab, &[(E::Flora, 9)], &[(E::Flora, 9)]),
			(B::DarkLumberDoor, &[(E::Flora, 10)], &[(E::Flora, 10)]),
			(B::RedLumber, &[(E::Flora, 27)], &[(E::Flora, 27)]),
			(B::RedLumberStairs, &[(E::Flora, 18)], &[(E::Flora, 18)]),
			(B::RedLumberSlab, &[(E::Flora, 9)], &[(E::Flora, 9)]),
			(B::RedLumberDoor, &[(E::Flora, 10)], &[(E::Flora, 10)]),
		];
		for &(b, base, ideal) in cases.iter().chain(grown) {
			assert_eq!(extract_base(b), base.to_vec(), "{b:?} extraction");
			let want = (!ideal.is_empty()).then(|| ideal.to_vec());
			assert_eq!(craft_ideal(b), want, "{b:?} recipe");
		}
		// Nothing gains an economy by accident: a block extracts or crafts
		// only through a material it declared or a hand-priced entry, both
		// of which are their own decisions elsewhere. (This replaces a
		// closed roster, which would have to be re-listed with every
		// catalog wave and would stop meaning anything by the third.)
		for &b in &BlockType::ALL {
			if !b.is_grid_item() {
				assert_eq!(
					!extract_base(b).is_empty(),
					!functional_extract(b).is_empty(),
					"{b:?} extracts without a material or a hand-set yield"
				);
				assert_eq!(
					craft_ideal(b).is_some(),
					functional_ideal(b).is_some(),
					"{b:?} crafts without a material or a hand-set price"
				);
			}
		}
	}

	/// The bench spends its time in proportion to what a block is worth:
	/// the pace is `convert_xp` against the 9-XP whole block, floored so
	/// the cheapest foliage doesn't become a wire-op stream.
	#[test]
	fn pace_tracks_what_a_block_is_worth() {
		use BlockType as B;
		// Level 1 (a 30-tick whole block), across the value range.
		for (b, ticks) in [
			(B::Stone, 30u32),  // the 9-XP reference itself
			(B::Grass, 30),     //
			(B::Rubble, 27),    // 8
			(B::Sandstone, 23), // 7
			(B::Dirt, 20),      // 6
			(B::Water, 20),     // the bucket charge
			(B::Gravel, 17),    // 5
			(B::Sand, 13),      // 4
			(B::Snow, 10),      // 3
			(B::WoodSlab, 10),  // a slab third
			(B::Leaves, 6),     // 1 XP — the floor, not 3 ticks
		] {
			assert_eq!(extract_pace_ticks(b, 1), ticks, "{b:?}");
		}
		// The level curve still applies on top, and pace never rises with
		// level for any block.
		assert_eq!(extract_pace_ticks(B::Stone, 25), 21);
		assert_eq!(extract_pace_ticks(B::Stone, 75), 15);
		for &b in &BlockType::ALL {
			if !b.is_extractable() {
				continue;
			}
			for level in 2..=99 {
				assert!(
					extract_pace_ticks(b, level) <= extract_pace_ticks(b, level - 1),
					"{b:?} never slows at {level}"
				);
				assert!(extract_pace_ticks(b, level) >= MIN_PACE_TICKS, "{b:?}");
			}
			// A worth-more block is never quicker than a worth-less one.
			for &o in &BlockType::ALL {
				if o.is_extractable() && crate::skills::convert_xp(b) > crate::skills::convert_xp(o)
				{
					assert!(
						extract_pace_ticks(b, 1) >= extract_pace_ticks(o, 1),
						"{b:?} vs {o:?}"
					);
				}
			}
		}
		// The bucket's capacity is still two whole blocks' worth.
		assert_eq!(extract_burst_ticks(1), 60.0);
		assert_eq!(extract_burst_ticks(99), 30.0);
	}

	/// Base yields never exceed a whole block's 27 sub-units, and every
	/// extractable block pays at least something at level 1.
	///
	/// **Ore is the deliberate exception**, and the only one the rule can
	/// safely have: a vein is a CONCENTRATE, not a substance you shape, so
	/// it pays 18–99 units. That is not matter creation because ore has no
	/// recipe to make it from — there is nothing to run the loop with. The
	/// cap exists to stop a craftable substance out-yielding its own cost,
	/// and `no_level_ever_creates_matter` is the test that actually says so.
	#[test]
	fn bases_stay_under_a_block() {
		for &b in &BlockType::ALL {
			let total = extract_total_base(b);
			if crate::worldgen::ore::ORES.contains(&b) {
				assert!(total > 0, "{b:?} must be worth taking to the bench");
				assert!(craft_ideal(b).is_none(), "{b:?} must have no recipe");
				continue;
			}
			assert!(total <= SUBS_PER_BLOCK, "{b:?} base {total}");
			if total > 0 {
				let l1: u32 = extract_yields(b, 1).iter().map(|&(_, n)| n).sum();
				assert!(l1 >= 1, "{b:?} pays nothing at level 1");
			}
		}
	}
}
