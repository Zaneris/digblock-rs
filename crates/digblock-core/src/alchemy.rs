//! The alchemy economy: extraction yields, bonus lines, transmutation
//! costs, and the bench pace — every number the server and offline play
//! must agree on (SKILLS-DESIGN §3–§4, §7). All integer math, no floats:
//! yields and costs are cross-side constants. The LEVELS that open each
//! recipe are not here: every threshold in the game lives in
//! [`crate::unlock`], beside the gates that enforce it.
//!
//! Four invariants hold the economy shut, all pinned by tests here:
//!
//! - **Nothing the world hands you may be transmuted** (§7.5). The
//!   catalog is what a fresh world does NOT give away, gated on
//!   [`BlockType::is_bench_only`] — so no recipe competes with a shovel,
//!   and the bench's own output is the only thing extraction can be asked
//!   to un-make. Clean rock passes: the world is made of stone and still
//!   hands you Rubble.
//! - **Nobody who can make a block can dig one back into XP.** Extraction
//!   XP is paid for reducing what the WORLD made
//!   ([`crate::skills::convert_xp`] answers zero for anything bench-only),
//!   and `BlockProps::crumbles` stops a made block laundering itself into
//!   a world-given one to collect it.
//! - **No matter creation**: for every craftable block at every level,
//!   the craft cost strictly exceeds the extraction yield in every
//!   essence — extract→craft→extract is a sink, always.
//! - **Bonus lines only on uncraftable blocks.** A bonus line on a
//!   craftable block is an essence PUMP: craft it from its cost
//!   essences, extract it back, pocket the bonus — at the cap that turns
//!   a couple of Earth into Metal, or worse, manufactures Aether, which nothing
//!   may. So trace minerals and the Aether lines live on MINED rock
//!   (Rubble, Gravel) and foliage, which have no recipes.
//!
//! Neither table is written out block by block any more. A structural
//! block is a MATERIAL in a SHAPE (SKILLS-DESIGN §7.2, [`crate::material`]),
//! so both sides prorate the material's whole-cube split by the shape's
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

/// Base extraction yields at full efficiency (the level cap), never more
/// than a whole block's worth — the shortfall is structure lost to the
/// process. Empty =
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
/// point.
///
/// The rule is stronger than "no recipe", and the weaker form is what
/// let a pump ship: a line may not sit on a recipe-less block that is
/// the DROP of a craftable one either, because the drop is reachable
/// from the recipe in one dig. Rubble carried "trace minerals" on
/// exactly that footing — `Stone` and `Sandstone` are craftable and both
/// `drops: Rubble` (the Rubble rule), so craft → place → dig → extract
/// closed the loop and turned Earth into Metal and Aether at a trivial
/// price. Both halves are pinned by
/// `bonus_lines_never_sit_on_craftable_blocks`, which now follows
/// `dig_drop`.
///
/// The two trickles the Rubble lines provided survive on hosts nothing
/// craftable drops into: Metal on `Gravel` and Aether on `PineLeaves`,
/// each at its own level.
pub fn bonus_lines(b: BlockType, level: u32) -> &'static [(Essence, u32)] {
	use BlockType as B;
	use Essence as E;
	match b {
		// "Morning dew" — foliage starts carrying a little water.
		B::Leaves if level >= 25 => &[(E::Aqua, 1)],
		// "Resin" — the design doc's heartwood line, moved off craftable
		// Wood (the pump rule above): pine forests are the pre-ore
		// Aether trickle instead.
		B::PineLeaves if level >= 45 => &[(E::Aqua, 1), (E::Aether, 1)],
		B::PineLeaves if level >= 25 => &[(E::Aqua, 1)],
		// "Trace minerals" — loose rock that no recipe reaches. Gravel is
		// worldgen-only and is nothing's drop; Rubble is NOT eligible,
		// however recipe-less it looks, because craftable rock drops it
		// (that pump shipped once — the doc comment above is its grave).
		B::Gravel if level >= 35 => &[(E::Metal, 2)],
		// "The spent stone's whisper" — the top of the ladder's Aether
		// trickle sits on the two DEEP tiers' tapped rock, the thing a
		// late-game miner's hands are already full of, so the endgame's
		// best bonus loop is deep seam work rather than a return to
		// farming a first-rung block. Pump-clean by construction: spent
		// rock is only ever written by a siphon draw, drops itself, and
		// nothing craftable drops into it.
		B::DepletedGlowvein | B::DepletedAetherock if level >= 55 => &[(E::Aether, 1)],
		_ => &[],
	}
}

/// Level-scaled yield for one base line: a fraction of the base at the
/// first level rising to all of it at the cap, floored, and never below
/// one (a listed line never vanishes entirely). Integer math throughout.
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

/// What one FIELD extraction pays into a player's stocks (SKILLS-DESIGN
/// §5.3) — a seam drawn out where it stands, with no ore in between.
///
/// The siphon is a bench you carry, so this is deliberately the ordinary
/// [`extract_yields`] of the ore and not a second table: the Alchemy level
/// scales a draw at the rock face exactly as it scales one at the bench,
/// and the ore rows' yields stay the single statement of what a tier is
/// worth. Nothing else could reach them — no ore ever becomes an item, so
/// the bench never sees one.
///
/// Mining's capstone ([`crate::unlock::ore_mastery`]) multiplies the whole
/// split rather than adding a line to it, which is what keeps "one extra
/// vein's worth" true of Aetherock's two essences as well as of the tiers
/// that pay in one.
///
/// The one thing a table cannot say is the TRACE, which is why this takes
/// a cell at all: [`crate::worldgen::ore::traces_aether`] is a chance per
/// draw, and the finest an essence split can be cut is a whole unit. It
/// rides the capstone multiplier with everything else, because a trace is
/// part of what that vein was worth and the capstone is worded as another
/// vein's worth. Being derived from the cell rather than rolled is what
/// lets it live in here at all — a random line would make this function
/// two different answers on two machines.
///
/// The ONE answer the server's dig credit and the engine's offline arm
/// both take, so a solo world and a server cannot pay differently.
pub fn field_yield(
	ore: BlockType,
	alchemy: u32,
	mining: u32,
	at: (i32, i32, i32),
	seed: f64,
) -> Vec<(Essence, u32)> {
	let draws = 1 + crate::unlock::ore_mastery(mining);
	let mut out: Vec<(Essence, u32)> = extract_yields(ore, alchemy)
		.into_iter()
		.map(|(e, per)| (e, per * draws))
		.collect();
	if crate::worldgen::ore::traces_aether(ore, at.0, at.1, at.2, seed) {
		let n = TRACE_AETHER * draws;
		// Merged rather than pushed, the same way a bonus line merges: no
		// tracing tier pays Aether of its own today, and the day one does
		// a second line would read as two separate finds.
		match out.iter_mut().find(|(e, _)| *e == Essence::Aether) {
			Some((_, on)) => *on += n,
			None => out.push((Essence::Aether, n)),
		}
	}
	out
}

/// What one trace-bearing vein cell adds to a draw.
///
/// Tuning belongs to the RATE ([`crate::worldgen::ore::traces_aether`]),
/// not to this: a trace is meant to be worth less per draw than any whole
/// unit, and the only way to say that is how often it lands.
const TRACE_AETHER: u32 = 1;

/// Sum of a block's base yields — the fixed value a block's WORTH at the
/// bench keys on (never the level-scaled outcome).
///
/// Totals the split without BUILDING it. `extract_base` allocates, and
/// this is not a cold path the way that one is: [`extract_worth`] reaches
/// here, `extract_pace_ticks` reaches that, and the server's token bucket
/// asks for the pace every tick for every connected player.
pub fn extract_total_base(b: BlockType) -> u32 {
	let Some((m, s)) = b.grid_cell() else {
		return functional_extract(b).iter().map(|&(_, n)| n).sum();
	};
	crate::material::yield_total(m.extract_units(), s.units())
}

/// What ONE extraction of `b` is worth at the bench — a fixed fraction of
/// its base yield, never the level-scaled outcome.
///
/// Split out from [`crate::skills::convert_xp`], which used to BE this
/// function. Worth and pay are two questions now: the bench spends its
/// time in proportion to what a block is worth ([`extract_pace_ticks`])
/// whoever made it, while the XP is paid only for what the WORLD made. A
/// lumber block is exactly as much substance to take apart as the wood it
/// was planed from and must come apart just as slowly — keying the pace on
/// the XP would have dropped every craftable to the pace floor the day
/// they stopped paying.
pub fn extract_worth(b: BlockType) -> u32 {
	extract_total_base(b).div_ceil(3)
}

/// Transmutation waste multiplier in permille: steep at the first level,
/// easing toward parity at the cap, with a late unlock clamping the tail
/// early. Monotonically non-increasing (pinned).
pub fn waste_permille(level: u32) -> u32 {
	let level = level.clamp(1, 99);
	let lin = 1600 - (550 * (level - 1) + 49) / 98;
	// The clamp is a felt STEP just past where the catalog tops out — the
	// linear curve catches back up a handful of levels later and carries
	// the tail from there, so the post-content levels keep their small
	// per-level gains.
	if level >= 56 { lin.min(1250) } else { lin }
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
		// The first vessel, priced to be the opening loop's first goal: a
		// couple of extractions' worth of the two essences the surface
		// hands a brand-new player. NO AQUA, ever — the bucket is how a
		// player first GETS Aqua, so an Aqua line here is the siphon's
		// Metal deadlock all over again.
		B::Bucket => &[(E::Flora, 6), (E::Earth, 3)],
		B::Torch => &[(E::Flora, 4), (E::Earth, 2)],
		// NO METAL, ever: ore needs the siphon and Metal needs ore, so a
		// Metal-priced siphon deadlocks the economy on the first world
		// where nobody has one. Flora and Earth are what a fresh miner
		// already has too much of.
		B::EssenceSiphon => &[(E::Flora, 30), (E::Earth, 15)],
		// The game's biggest sink, and the only thing that makes Metal a
		// goal rather than a number. The Aether line is what
		// keeps Aetherock worth reaching for before the lamps arrive.
		B::ResonantPick => &[(E::Metal, 240), (E::Earth, 60), (E::Aether, 12)],
		B::Lantern => &[(E::Metal, 8), (E::Earth, 6)],
		B::Aetherlamp => &[(E::Metal, 12), (E::Aether, 3)],
		B::MastersSigil => &[(E::Aether, 99)],
		// Furniture is timber and joinery — near its volume, but chosen,
		// not measured: a chair is mostly air and still costs a chair.
		B::Chair => &[(E::Flora, 6)],
		B::Table => &[(E::Flora, 9)],
		// The bed is the catalog's first genuinely cross-family price:
		// a timber frame AND the bedding on it. Aqua is the essence with the
		// fewest sources — a scooped bucket charge, snow, and the foliage
		// bonus lines — which is what makes carrying water home worth doing.
		B::Bed => &[(E::Flora, 21), (E::Aqua, 6)],
		_ => return None,
	})
}

/// Ideal (pre-waste) craft cost lines for anything craftable. `None` =
/// not craftable.
///
/// Gated on [`BlockType::is_bench_only`] first, the twin of the same gate
/// in [`crate::unlock::craft`] — nothing the world hands you has a price,
/// because it has no recipe. A material keeps its whole-cube split either
/// way: that split is the PRICE BASIS every shape prorates from, so Wood
/// still costs what wood costs even though the felled log itself is
/// something you go and chop.
///
/// Derived for every structural block: the material's whole-cube split
/// ([`crate::material::Material::craft_units`]) prorated UP to the
/// shape's volume. Rounding the cost side up where [`extract_base`]
/// rounds down is what makes "no matter creation" hold for every shape
/// of every material without a proof per pair.
pub fn craft_ideal(b: BlockType) -> Option<Vec<(Essence, u32)>> {
	if !b.is_bench_only() {
		return None;
	}
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

/// The bench's pace for a WHOLE block — one whose every sub-unit comes
/// out, which is the reference every other block is priced against. It
/// quickens at two Alchemy milestones.
pub fn base_pace_ticks(level: u32) -> u32 {
	if level >= 45 {
		15
	} else if level >= 25 {
		21
	} else {
		30
	}
}

/// The reference block's worth ([`extract_worth`] of a full whole-block
/// base), i.e. what [`base_pace_ticks`] is the pace OF.
const REF_WORTH: u32 = 9;

/// Floor on either pace, in ticks. The cheapest things the bench handles
/// — a leaf coming apart, a slab going together — are worth a fraction of
/// the reference block, and a strictly proportional pace would run them
/// many times a second, which is a stream of wire ops rather than a bench.
const MIN_PACE_TICKS: u32 = 6;

/// Bench extraction pace in sim ticks for ONE block of `b` at `level`: the
/// level's whole-block pace scaled by what the block is WORTH, so the
/// bench spends its time in proportion to what it yields. A whole stone
/// takes the full interval, a slab third a third of it, and leaves fall to
/// the floor above.
///
/// Keying on [`extract_worth`] rather than on a second table is
/// deliberate: worth is already a fixed fraction of the base total, so
/// pace and yield can never disagree about which blocks are the
/// substantial ones. It is deliberately NOT keyed on the XP, which the
/// same fraction used to serve — what the bench makes now pays no XP, and
/// a pace that followed would have a lumber block coming apart five times
/// faster than the wood it was planed from. Client-paced for presentation,
/// server-enforced via the extraction token bucket.
pub fn extract_pace_ticks(b: BlockType, level: u32) -> u32 {
	pace_ticks(extract_worth(b), level)
}

/// What ONE craft of `item` is worth at the bench — the same fixed
/// fraction of its ideal (pre-waste) cost that [`extract_worth`] is of a
/// block's yield. Zero for anything with no recipe.
///
/// Pre-waste on purpose, which is what makes it a property of the ITEM
/// rather than of the player: two masons at different levels pay different
/// prices for the same stair and spend the same time making it, exactly as
/// they extract the same stone at the same pace whatever it yields them.
pub fn craft_worth(item: BlockType) -> u32 {
	craft_ideal(item).map_or(0, |lines| {
		lines.iter().map(|&(_, n)| n).sum::<u32>().div_ceil(3)
	})
}

/// Bench TRANSMUTATION pace in sim ticks for ONE `item` at `level` — the
/// mirror of [`extract_pace_ticks`], off the same curve and the same
/// reference block, so the bench runs at one rhythm whichever way the
/// matter is going.
///
/// A craft used to be instant, and it was the last place in the economy
/// where XP arrived in a click: the transmute tab's CTRL multiplier paid a
/// hundred crafts' worth in one frame. Pacing it is not a nerf to the
/// award (crafting still pays what it always paid) — it is what makes the
/// award take the time the work takes, and it puts the two halves of the
/// bench on the same clock. The catalog's biggest price is now also its
/// longest hold, which is the reading a forge should have.
pub fn craft_pace_ticks(item: BlockType, level: u32) -> u32 {
	pace_ticks(craft_worth(item), level)
}

/// The shared curve behind both paces: the level's whole-block interval
/// scaled by what one op is WORTH, floored.
fn pace_ticks(worth: u32, level: u32) -> u32 {
	let ticks = (base_pace_ticks(level) * worth + REF_WORTH / 2) / REF_WORTH;
	ticks.max(MIN_PACE_TICKS)
}

/// Bench token-bucket burst, in REFERENCE blocks — absorbs network
/// bunching exactly as `EDIT_BURST` does for edits. The bucket itself
/// banks TICKS (a pace is per-op), so its capacity is this many
/// whole-block intervals: [`bench_burst_ticks`].
pub const BENCH_BURST: f32 = 2.0;

/// The most expensive SINGLE bench op a player at `level` is allowed to
/// ask for, in ticks — an extraction or a craft, whichever is dearer.
///
/// Gated on what that level can actually do, not on the whole catalog: a
/// fresh player banks what a fresh player's ops cost, and the Resonant
/// Pick's long forge only widens the allowance from the level that can
/// forge one. Memoized per level, because the server's refill asks for the
/// bucket's capacity every tick for every connected client and the answer
/// is a property of the tables rather than of the caller.
fn costliest_op_ticks(level: u32) -> u32 {
	static MAX: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
	let table = MAX.get_or_init(|| {
		(0..=crate::skills::MAX_LEVEL)
			.map(|l| {
				BlockType::ALL
					.into_iter()
					.map(|b| {
						let extract = b.is_extractable().then(|| extract_pace_ticks(b, l));
						let craft = crate::unlock::can_craft(b, l).then(|| craft_pace_ticks(b, l));
						extract.into_iter().chain(craft).max().unwrap_or(0)
					})
					.max()
					.unwrap_or(0)
			})
			.collect()
	});
	table[level.min(crate::skills::MAX_LEVEL) as usize]
}

/// The bench bucket's capacity in ticks at `level`: two whole blocks'
/// worth, or ONE costliest op if that is more.
///
/// ONE bucket for both halves of the bench, because there is one bench and
/// one pair of hands: extracting and transmuting draw on the same clock,
/// so a client cannot run both drips at once for double the throughput.
/// The screen could not anyway — the two live on different tabs — which is
/// exactly why making it structural costs nothing.
///
/// The second half is not a nicety. The bucket banks ticks and an op
/// spends its own pace, which scales with what that op is WORTH — while
/// the burst was sized off the reference block alone. The two deepest ores
/// are worth several times that, so their pace exceeded a FULL bucket at
/// every level and `process_alchemy` refused them forever: no debit, no
/// yield, no XP, just the standing rebase. Aether has exactly one source,
/// so that silently walled off the Master's Sigil, the Aetherlamp and the
/// Resonant Pick — online only, since offline is unpaced, which is the
/// worst way to find out. `bases_stay_under_a_block` used to make it
/// impossible (a base no bigger than a whole block implies a worth of at
/// most `REF_WORTH`, which implies a pace of at most one reference
/// interval); ore is the deliberate exception to that bound and the pick's
/// forge is a second one, so the guarantee has to be stated here instead,
/// and `the_bucket_can_always_afford_the_costliest_op` pins it.
///
/// Sustained throughput is untouched: the bucket still refills one tick
/// per tick, so the drip is exactly as slow as the pace says. Only the
/// bunching allowance grows, and only to what a single op actually costs
/// at that level.
pub fn bench_burst_ticks(level: u32) -> f32 {
	let reference = BENCH_BURST * base_pace_ticks(level) as f32;
	reference.max(costliest_op_ticks(level) as f32)
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
		// §4.2: 1.60× at 1 → 1.05× at 99, with the clamp's dip arriving
		// just past where the catalog tops out.
		assert_eq!(waste_permille(1), 1600);
		assert_eq!(waste_permille(99), 1050);
		assert!(waste_permille(56) <= 1250);
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

	/// **Nothing the world hands you may be transmuted.** For every block
	/// the world places, whatever a dig on one of them puts in a player's
	/// hands has no recipe at any price — the bench makes what a fresh
	/// world does not give away, and a recipe for dirt is the bench
	/// competing with a shovel.
	///
	/// Checked against the hand-priced TABLE as well as against
	/// `craft_ideal`, and that is the half with teeth. `craft_ideal` gates
	/// on `is_bench_only` before it reads anything, so a stale
	/// `functional_ideal` row for something the world gives would be
	/// silently unreachable rather than wrong — which is exactly how a
	/// retired price sits in a table for a year. The bucket charge was one:
	/// water is scooped, so the recipe went with the rule.
	///
	/// Clean rock passes and is meant to: the world places Stone and
	/// Sandstone but hands out Rubble, so neither is a block a dig can
	/// give, and §7.1's economy is untouched.
	#[test]
	fn nothing_the_world_hands_you_can_be_transmuted() {
		for &n in &BlockType::ALL {
			if n.is_crafted_only() {
				continue;
			}
			let Some(given) = n.dig_drop() else {
				continue;
			};
			assert!(
				!given.is_bench_only(),
				"{n:?} hands out {given:?}, which claims the bench is its only source"
			);
			assert!(
				craft_ideal(given).is_none(),
				"digging {n:?} hands out {given:?}, which the bench can also make"
			);
			assert!(
				functional_ideal(given).is_none(),
				"{given:?} is handed out by digging {n:?} and still carries a hand-set \
				 price — a row the gate hides rather than a price anyone can pay"
			);
		}
	}

	/// **The loop-closer.** Nobody who can MAKE a block can dig one back
	/// into something that pays extraction XP.
	///
	/// That is the whole reason clean rock may stay craftable while nothing
	/// else the world is made of does. Crafting pays, extracting what the
	/// world made pays, and the two would compose into a standing converter
	/// from essence into levels if a transmuted block could be laundered
	/// back into a world-given one: craft a stone, place it, break it into
	/// Rubble, take the Rubble apart, collect. `BlockProps::crumbles` is
	/// what shuts it — a stone somebody made comes back as a stone, worth
	/// nothing at the bench.
	///
	/// Asked at every level that can craft the block, because that is
	/// exactly the population the loop needs: a digger below the recipe's
	/// own level still crumbles it and still gets the XP, and is welcome to
	/// — they are not undoing their own work, and they cannot have made the
	/// block they are breaking.
	#[test]
	fn no_one_who_can_make_a_block_can_dig_it_into_xp() {
		for &b in &BlockType::ALL {
			if craft_ideal(b).is_none() {
				continue;
			}
			for level in 1..=crate::skills::MAX_LEVEL {
				if !crate::unlock::can_craft(b, level) {
					continue;
				}
				let paid = b
					.dig_credit(true, level)
					.map_or(0, crate::skills::convert_xp);
				assert_eq!(
					paid, 0,
					"{b:?} is craftable at {level} and digging one back pays {paid} XP"
				);
			}
		}
	}

	/// **Extraction XP is paid for what the world made**, and only for it:
	/// nothing off the bench pays anything back, and everything the world
	/// hands you that is worth taking apart does.
	///
	/// The second half is what stops the first from being satisfied by
	/// paying nobody. Spent rock is the deliberate gap between them — the
	/// siphon already took what was in it, so it is worth nothing to
	/// extract and pays nothing, without ever having been near a bench.
	#[test]
	fn extraction_xp_is_paid_for_what_the_world_made() {
		for &b in &BlockType::ALL {
			if b.is_bench_only() {
				assert_eq!(
					crate::skills::convert_xp(b),
					0,
					"{b:?} comes off the bench and must pay nothing to take apart"
				);
			} else if extract_worth(b) > 0 {
				assert_eq!(
					crate::skills::convert_xp(b),
					u64::from(extract_worth(b)),
					"{b:?} is the world's own and pays what it is worth"
				);
			}
		}
		// The pace is NOT what goes: a made block is as much substance to
		// take apart as the one it was worked from, and both spend the same
		// time at the bench.
		assert_eq!(
			extract_pace_ticks(BlockType::Lumber, 1),
			extract_pace_ticks(BlockType::Wood, 1),
			"planed timber comes apart no faster than the log"
		);
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

	/// The same rule one dig further out, which is where it actually
	/// leaked. "Recipe-less" is not enough: a block reachable from a
	/// recipe by DIGGING what it makes is just as pumpable, because the
	/// craft and the extract are then two halves of one loop.
	///
	/// Rubble shipped a bonus on exactly that footing. It has no recipe
	/// — but Stone and Sandstone do, and both `drops: Rubble` (the
	/// Rubble rule), so craft → place → dig → extract ran at a profit:
	/// 30 Earth in and 22 Earth + 1 Metal + 1 Aether out at Alchemy 90,
	/// unbounded, against a design where Metal comes only from ore and
	/// Aether only from Aetherock.
	///
	/// Neither of the two pins above could see it. This one asks whether
	/// the bonus is reachable, not whether its host has a recipe, and it
	/// is deliberately checked against `dig_drop` rather than a hand
	/// list — a new craftable whose drop happens to carry a line fails
	/// here on the day it is written.
	#[test]
	fn no_craftable_block_drops_into_a_bonus_line() {
		for &b in &BlockType::ALL {
			if craft_ideal(b).is_none() {
				continue;
			}
			let Some(drop) = b.dig_drop() else {
				continue;
			};
			for level in 1..=99 {
				let lines = bonus_lines(drop, level);
				assert!(
					lines.is_empty(),
					"craftable {b:?} drops {drop:?}, which bonuses {lines:?} at \
					 Alchemy {level} — that closes a craft-and-extract loop"
				);
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
			// block, extraction base, craft ideal. An empty recipe column is
			// a block the world HANDS you — loose ground, the felled log,
			// foliage, mined rock, the scooped charge — and the yield column
			// is untouched by that: you still take one apart, you just
			// cannot make one. Clean rock keeps both, because a quarried one
			// arrives as Rubble and never as itself.
			(B::Stone, &[(E::Earth, 27)], &[(E::Earth, 27)]),
			(B::Rubble, &[(E::Earth, 24)], &[]),
			(B::Sandstone, &[(E::Earth, 21)], &[(E::Earth, 27)]),
			(B::Gravel, &[(E::Earth, 15)], &[]),
			(B::Sand, &[(E::Earth, 12)], &[]),
			(B::Dirt, &[(E::Earth, 15), (E::Flora, 3)], &[]),
			(B::Grass, &[(E::Earth, 18), (E::Flora, 9)], &[]),
			(B::DryGrass, &[(E::Earth, 18), (E::Flora, 6)], &[]),
			(B::Mud, &[(E::Earth, 12), (E::Flora, 3), (E::Aqua, 3)], &[]),
			(B::Wood, &[(E::Flora, 27)], &[]),
			(B::Leaves, &[(E::Flora, 1)], &[]),
			(B::PineLeaves, &[(E::Flora, 1)], &[]),
			(B::Snow, &[(E::Aqua, 9)], &[]),
			(B::Water, &[(E::Aqua, 18)], &[]),
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
			// WORTH, not XP: what the bench makes pays no XP at all and must
			// still come apart in proportion to its substance.
			for &o in &BlockType::ALL {
				if o.is_extractable() && extract_worth(b) > extract_worth(o) {
					assert!(
						extract_pace_ticks(b, 1) >= extract_pace_ticks(o, 1),
						"{b:?} vs {o:?}"
					);
				}
			}
		}
		// The bucket's capacity is two whole blocks' worth, or one
		// costliest op available at that level where that is more — see
		// `the_bucket_can_always_afford_the_costliest_op`.
		assert_eq!(bench_burst_ticks(1), 110.0);
		assert_eq!(
			bench_burst_ticks(1),
			extract_pace_ticks(BlockType::Aetherock, 1) as f32,
			"at the first level the costliest op is still the deepest ore"
		);
		// And past the pick's own unlock it is the forge, which is the
		// longest single hold in the catalog either way.
		let pick = crate::unlock::craft(BlockType::ResonantPick).expect("a price");
		assert_eq!(
			bench_burst_ticks(99),
			craft_pace_ticks(BlockType::ResonantPick, 99) as f32
		);
		assert!(
			bench_burst_ticks(pick) > bench_burst_ticks(pick - 1),
			"the allowance widens at the level that can forge one, not before"
		);
	}

	/// A full bucket must be able to pay for the costliest single op a
	/// player at that level may ask for — either half of the bench — or
	/// that op cannot be done AT ALL online: the server refuses on
	/// `bench_tokens < cost` and the tokens never reach it, so the op is
	/// dropped in silence: no debit, no yield, no XP, just the standing
	/// rebase. Offline is unpaced, so single-player looks perfect while
	/// the dedicated server is a wall.
	///
	/// This used to follow from `bases_stay_under_a_block`: a base of at
	/// most a whole block means a worth of at most `REF_WORTH`, so the
	/// pace never exceeded one reference interval and two of them always
	/// covered it. Ore is exempt from that bound — a vein is a
	/// concentrate — which removed the proof without anyone noticing, and
	/// left Glowvein and Aetherock permanently un-extractable. Crafting is
	/// the second exemption: a price is not bounded by a block's volume at
	/// all, and the pick's is an order of magnitude past it.
	#[test]
	fn the_bucket_can_always_afford_the_costliest_op() {
		for &b in &BlockType::ALL {
			for level in 1..=crate::skills::MAX_LEVEL {
				let burst = bench_burst_ticks(level);
				if b.is_extractable() {
					let cost = extract_pace_ticks(b, level) as f32;
					assert!(
						cost <= burst,
						"{b:?} at {level}: one Convert costs {cost} ticks against a \
						 {burst}-tick bucket — refused forever"
					);
				}
				if crate::unlock::can_craft(b, level) {
					let cost = craft_pace_ticks(b, level) as f32;
					assert!(
						cost <= burst,
						"{b:?} at {level}: one Craft costs {cost} ticks against a \
						 {burst}-tick bucket — refused forever"
					);
				}
			}
		}
		// And the ops that broke it are genuinely the expensive end — a
		// catalog where the reference block IS the costliest would make
		// this test vacuous.
		assert!(extract_worth(BlockType::Aetherock) > REF_WORTH);
		assert!(craft_worth(BlockType::ResonantPick) > REF_WORTH);
	}

	/// `extract_total_base` sums the split without building it, so the
	/// two have to agree for every block — a fast path that quietly
	/// disagreed with the yields it totals would move extraction XP, the
	/// bench pace and the token bucket all at once.
	#[test]
	fn the_total_matches_the_split_it_totals() {
		for &b in &BlockType::ALL {
			let split: u32 = extract_base(b).into_iter().map(|(_, n)| n).sum();
			assert_eq!(extract_total_base(b), split, "{b:?}");
		}
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
	/// A field draw pays the ore's own bench yield, and the capstone
	/// multiplies the whole split rather than one line of it.
	///
	/// The first half is why there is no second table: a siphon is a bench
	/// you carry, so the Alchemy level has to scale a draw at the rock
	/// face exactly as it scales one at the bench — and since no ore ever
	/// becomes an item, this is the ONLY thing those yields are for.
	///
	/// The second half is Aetherock's, and it is the reason `ore_mastery`
	/// counts draws instead of units: it is the one tier that pays in two
	/// essences, so a capstone that added a flat bonus would have to pick
	/// which of them to add it to.
	///
	/// Asked of an untraced cell throughout, because the trace is the one
	/// thing a draw pays that the bench's table does not know about —
	/// `a_traced_seam_pays_one_more_aether_than_the_same_seam_without_one`
	/// is where that half is pinned.
	#[test]
	fn a_field_draw_pays_the_ore_and_the_capstone_multiplies_all_of_it() {
		let (plain, _) = trace_cells(TEST_SEED);
		for &ore in &crate::worldgen::ore::ORES {
			for alchemy in [1, 40, crate::skills::MAX_LEVEL] {
				let bench = extract_yields(ore, alchemy);
				assert!(!bench.is_empty(), "{ore:?} is worth nothing to draw");
				assert_eq!(
					field_yield(ore, alchemy, 1, plain, TEST_SEED),
					bench,
					"{ore:?} at Alchemy {alchemy}: a draw is the bench's own yield"
				);
				let mastered =
					field_yield(ore, alchemy, crate::skills::MAX_LEVEL, plain, TEST_SEED);
				let draws = 1 + crate::unlock::ore_mastery(crate::skills::MAX_LEVEL);
				assert!(draws > 1, "the capstone hands out nothing");
				for (i, &(e, n)) in mastered.iter().enumerate() {
					assert_eq!((e, n), (bench[i].0, bench[i].1 * draws), "{ore:?} line {i}");
				}
			}
		}
		// Aetherock is the tier that makes the multiply matter — if it ever
		// stops paying two essences, this test stops proving anything.
		assert!(
			extract_yields(BlockType::Aetherock, 1).len() >= 2,
			"the two-essence tier is what the capstone rule is for"
		);
	}

	/// Any world; the trace is a function of the cell and the seed, and
	/// every seed has both kinds of cell in it.
	const TEST_SEED: f64 = 42.0;

	/// A vein cell with no Aether trace and one with, in that order.
	///
	/// WALKED rather than named, so a retune of the rate moves the test
	/// with it — a hardcoded pair of coordinates would silently land on
	/// the wrong side of the hash and start asserting the opposite of what
	/// it says.
	fn trace_cells(seed: f64) -> ((i32, i32, i32), (i32, i32, i32)) {
		let (mut plain, mut rich) = (None, None);
		for y in 0..4096 {
			let at = (1, y, 2);
			let slot = if crate::worldgen::ore::traces_aether(
				BlockType::Duskstone,
				at.0,
				at.1,
				at.2,
				seed,
			) {
				&mut rich
			} else {
				&mut plain
			};
			slot.get_or_insert(at);
			if plain.is_some() && rich.is_some() {
				break;
			}
		}
		(
			plain.expect("an untraced seam"),
			rich.expect("a traced seam"),
		)
	}

	/// The trace: the same tier, the same levels, one unit of Aether more
	/// where the seam carries it — and NOTHING else different, which is
	/// what makes it a bonus on a draw rather than a second yield table.
	///
	/// The middle tier is the only one that traces, and that is the point
	/// of it: `every_functional_price_has_a_source_at_its_own_level`
	/// proves the Resonant Pick is buildable by anyone who has earned it,
	/// off a tier that reveals at the level the pick's own swing widens.
	/// The trace is the luck beside that ladder — the chance of walking up
	/// to the bench already holding what the pick wants — so it is
	/// deliberately not something the pin is allowed to count.
	#[test]
	fn a_traced_seam_pays_one_more_aether_than_the_same_seam_without_one() {
		let (plain, rich) = trace_cells(TEST_SEED);
		let ore = BlockType::Duskstone;
		for alchemy in [1, 40, crate::skills::MAX_LEVEL] {
			let without = field_yield(ore, alchemy, 1, plain, TEST_SEED);
			let with = field_yield(ore, alchemy, 1, rich, TEST_SEED);
			let aether = |lines: &[(Essence, u32)]| {
				lines
					.iter()
					.find(|&&(e, _)| e == Essence::Aether)
					.map_or(0, |&(_, n)| n)
			};
			assert_eq!(
				aether(&with) - aether(&without),
				TRACE_AETHER,
				"Alchemy {alchemy}: a traced seam is worth exactly one trace more"
			);
			for &(e, n) in &without {
				if e == Essence::Aether {
					continue;
				}
				assert_eq!(
					with.iter().find(|&&(we, _)| we == e).map(|&(_, wn)| wn),
					Some(n),
					"Alchemy {alchemy}: the trace moved {e:?}, which it must not"
				);
			}
		}
		// Only the middle tier traces. If another ever does, the merge arm
		// in `field_yield` starts mattering and this is the reminder.
		let (plain, rich) = (plain, rich);
		for &ore in &crate::worldgen::ore::ORES {
			if ore == BlockType::Duskstone {
				continue;
			}
			assert_eq!(
				field_yield(ore, 40, 1, rich, TEST_SEED),
				field_yield(ore, 40, 1, plain, TEST_SEED),
				"{ore:?} traces, and only the middle tier is meant to"
			);
		}
	}

	/// **Every functional item is buildable by a player who has just
	/// earned it.** For each essence its recipe names, some block that
	/// player can already dig and take apart yields that essence.
	///
	/// This is the pin that was missing when the Resonant Pick shipped.
	/// `functional_ideal` and `unlock::functional` were already pinned as
	/// twins, so an item could not have a price without a level — but
	/// nothing asked whether the price was PAYABLE at that level, and the
	/// pick's Aether line was not: the only reliable Aether in the game
	/// sat behind the deepest ore tier and a foliage bonus near the top of
	/// the Alchemy ladder, both a long climb past the pick's own gate.
	///
	/// What counts as a source is deliberately narrow, and each exclusion
	/// is load-bearing:
	///
	/// - **The block has to be one the world HANDS you.** Anything off the
	///   bench can never be a net source — `no_level_ever_creates_matter`
	///   guarantees making one costs more than taking it apart returns —
	///   so Crystal, which yields more Aether than anything else per cube,
	///   is a SINK and correctly contributes nothing here. Clean rock joins
	///   it for the same reason and not by name: the world places Stone but
	///   hands out Rubble, and Rubble is the source that counts.
	/// - **The strength wall applies**: the Mining level has to clear it
	///   (and, for ore, the tier's own reveal). A source you cannot reach
	///   is not a source.
	/// - **Only what a tier pays RELIABLY.** The Duskstone trace is a
	///   chance, so it is not asked about — a price payable only if the
	///   hash smiles is not payable.
	///
	/// Levelling both skills together is the reading of "has earned it":
	/// the ladders are independent, but an item's price is a claim about a
	/// player at that point in the game, not about one skill in isolation.
	#[test]
	fn every_functional_price_has_a_source_at_its_own_level() {
		for &item in &BlockType::ALL {
			let Some(level) = crate::unlock::functional(item) else {
				continue;
			};
			let price = functional_ideal(item).expect("the twin pin guarantees one");
			for &(essence, _) in price {
				let source = BlockType::ALL.iter().copied().find(|&b| {
					if b.is_bench_only() || crate::unlock::mine(b) > level {
						return false;
					}
					let ore = crate::worldgen::ore::ORES.contains(&b);
					if ore && crate::unlock::reveal(b).is_some_and(|r| r > level) {
						return false;
					}
					extract_yields(b, level)
						.iter()
						.any(|&(e, n)| e == essence && n > 0)
				});
				assert!(
					source.is_some(),
					"{item:?} costs {essence:?} and unlocks at {level}, but nothing a \
					 player at that level can dig yields {essence:?} — the item is \
					 unlocked and unbuildable"
				);
			}
		}
	}

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
