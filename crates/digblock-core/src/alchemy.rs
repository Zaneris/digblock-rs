//! The alchemy economy: node-draw yields, transmutation costs, and the
//! bench pace — every number the server and offline play must agree on
//! (SKILLS-DESIGN §3–§4, §7, reshaped by the essence redesign). All
//! integer math, no floats: yields and costs are cross-side constants.
//! The LEVELS that open each recipe are not here: every threshold in the
//! game lives in [`crate::unlock`], beside the gates that enforce it.
//!
//! **Essence is found, not ground.** Bench extraction is gone: matter
//! never becomes essence at a bench, and the siphon's field draw
//! ([`field_yield`], over [`crate::worldgen::nodes`]) is the only faucet
//! the currency has. The invariants that used to police the
//! extract→craft loop are structural now — there is no extract — and the
//! ones that remain are pinned by tests here:
//!
//! - **Nothing the world hands you may be transmuted** (§7.5). The
//!   catalog is what a fresh world does NOT give away, gated on
//!   [`BlockType::is_bench_only`] — so no recipe competes with a shovel.
//!   Clean rock passes: the world is made of stone and still hands you
//!   Rubble.
//! - **No craft creates essence.** A craft only ever debits the essence
//!   pool; nothing anywhere credits it but a draw. With the faucet and
//!   the drain in different modules, "no matter creation" needs no
//!   per-level proof any more.
//! - **The essence tool costs no essence, ever.** Every essence is a
//!   siphon draw, so a siphon priced in essence would deadlock the whole
//!   currency on the first world where nobody has one — the old
//!   no-Metal-in-the-siphon rule, generalized. Its price (and the
//!   bucket's, for the same day-one reason) is FEEDSTOCK alone: blocks,
//!   which a fresh player can go and chop.
//!
//! Costs still derive. A structural block is a MATERIAL in a SHAPE
//! (SKILLS-DESIGN §7.2, [`crate::material`]), so its price prorates the
//! material's whole-cube split by the shape's volume. What remains
//! hand-written is the FUNCTIONAL roster: torches, lamps and tools, which
//! §7.3 prices by hand precisely because they are not matter.

use crate::block::{BlockType, Essence};
use crate::material::cost_split;

/// Level-scaled yield for one draw line: a fraction of the base at the
/// first level rising to all of it at the cap, floored, and never below
/// one (a listed line never vanishes entirely). Integer math throughout.
pub fn scaled_yield(base: u32, level: u32) -> u32 {
	let level = level.clamp(1, 99);
	(base * (3920 + 60 * (level - 1)) / 9800).max(1)
}

/// Everything one node cell is worth to a draw at `level`: the kind's
/// yield basis ([`crate::material::Material::node_units`]) at the
/// level's efficiency. Empty for anything that is not a live node.
pub fn node_yields(node: BlockType, level: u32) -> Vec<(Essence, u32)> {
	node.material().map_or_else(Vec::new, |m| {
		m.node_units()
			.iter()
			.map(|&(e, base)| (e, scaled_yield(base, level)))
			.collect()
	})
}

/// What one FIELD draw pays into a player's stocks (SKILLS-DESIGN §5.3,
/// generalized by the essence redesign) — a NODE drawn out where it
/// stands, vein or heartwood or spring alike, with no item in between.
///
/// The siphon is the economy's one faucet, so this is deliberately the
/// ordinary [`node_yields`] and not a second table: the Alchemy level
/// scales a draw the same way everywhere, and the node rows' yields stay
/// the single statement of what a kind is worth.
///
/// Mining's capstone ([`crate::unlock::ore_mastery`]) multiplies the whole
/// split rather than adding a line to it, which is what keeps "one extra
/// vein's worth" true of Aetherock's two essences as well as of the kinds
/// that pay in one. It multiplies SURFACE draws too, deliberately: the
/// capstone is mastery of extraction itself, and a second table for where
/// the extraction happened is exactly the drift this function exists to
/// prevent.
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
	node: BlockType,
	alchemy: u32,
	mining: u32,
	at: (i32, i32, i32),
	seed: f64,
) -> Vec<(Essence, u32)> {
	let draws = 1 + crate::unlock::ore_mastery(mining);
	let mut out: Vec<(Essence, u32)> = node_yields(node, alchemy)
		.into_iter()
		.map(|(e, per)| (e, per * draws))
		.collect();
	if crate::worldgen::ore::traces_aether(node, at.0, at.1, at.2, seed) {
		let n = TRACE_AETHER * draws;
		// Merged rather than pushed, the same way two lines of one split
		// merge: no tracing tier pays Aether of its own today, and the day
		// one does a second line would read as two separate finds.
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
/// the other way round). An item may ALSO cost feedstock blocks
/// ([`functional_feedstock`]); the two vessels cost ONLY that.
pub fn functional_ideal(b: BlockType) -> Option<&'static [(Essence, u32)]> {
	use BlockType as B;
	use Essence as E;
	Some(match b {
		// The two vessels cost NO essence, ever — see
		// `functional_feedstock`, which is the whole of their price. The
		// empty splits stay in this table because the twin pin reads it:
		// a level and a price are one decision.
		B::Bucket | B::EssenceSiphon => &[],
		B::Torch => &[(E::Flora, 2), (E::Earth, 2)],
		// The game's biggest sink, and the only thing that makes Metal a
		// goal rather than a number. The Aether line is what
		// keeps Aetherock worth reaching for before the lamps arrive.
		B::ResonantPick => &[(E::Metal, 240), (E::Earth, 60), (E::Aether, 12)],
		B::Lantern => &[(E::Metal, 8), (E::Earth, 6)],
		B::Aetherlamp => &[(E::Metal, 12), (E::Aether, 3)],
		B::MastersSigil => &[(E::Aether, 99)],
		// Furniture is a timber feedstock and a living catalyst — chosen,
		// not measured: a chair is mostly air and still costs a chair.
		B::Chair => &[(E::Flora, 2)],
		B::Table => &[(E::Flora, 2)],
		// The bed is the catalog's first genuinely cross-family price:
		// a timber frame AND the bedding on it. Aqua is the essence with
		// the fewest sources — the springstone node is the whole of it —
		// which is what makes diving for one worth doing.
		B::Bed => &[(E::Flora, 4), (E::Aqua, 6)],
		_ => return None,
	})
}

/// The BLOCKS a functional item consumes beside its essence lines —
/// matter in, not currency in. Today it carries the two vessels, whose
/// whole price it is:
///
/// - **The siphon may never cost essence.** Every essence in the game is
///   a siphon draw now, so an essence-priced siphon deadlocks the whole
///   currency — the old no-Metal rule generalized, and pinned
///   (`the_essence_tool_costs_no_essence`).
/// - **The bucket is the same deadlock one step out**: it sits at the
///   level everyone starts at, before the first draw, so its price has
///   to be something a fresh player can go and chop.
///
/// Wood for both, because timber is the first thing the strength ladder
/// opens beyond soil — "dig your way to the trees, fell a few, make your
/// tools" IS the opening loop.
pub fn functional_feedstock(b: BlockType) -> &'static [(BlockType, u32)] {
	match b {
		BlockType::Bucket => &[(BlockType::Wood, 6)],
		BlockType::EssenceSiphon => &[(BlockType::Wood, 8)],
		// A torch is a whittled stick with a treated head.
		BlockType::Torch => &[(BlockType::Wood, 1)],
		// Furniture is joinery: planed timber, worked. `pickup` gates the
		// LUMBER below the furniture's own levels, so the feedstock is
		// always makable first.
		BlockType::Chair => &[(BlockType::Lumber, 1)],
		BlockType::Table => &[(BlockType::Lumber, 2)],
		BlockType::Bed => &[(BlockType::Lumber, 2)],
		// The lamps are a lit pane in a housing: glass, plus the metal
		// and light priced in their essence lines.
		BlockType::Lantern => &[(BlockType::Glass, 1)],
		BlockType::Aetherlamp => &[(BlockType::Glass, 1)],
		_ => &[],
	}
}

/// Feedstock lines for anything craftable — the block half of a price.
/// A grid item consumes ONE cube of its material's feedstock (see
/// [`crate::material::Material::feedstock`] and
/// [`crate::shapes::craft_batch`], which says how many pieces that cube
/// cuts into); functional items carry their hand-set lines.
pub fn craft_feedstock(b: BlockType) -> Vec<(BlockType, u32)> {
	if !b.is_bench_only() {
		return Vec::new();
	}
	if functional_ideal(b).is_some() {
		return functional_feedstock(b).to_vec();
	}
	b.grid_cell()
		.and_then(|(m, _)| m.feedstock())
		.map(|f| vec![(f, 1)])
		.unwrap_or_default()
}

/// Ideal (pre-waste) craft cost lines for anything craftable. `None` =
/// not craftable. May be EMPTY for a craftable item whose whole price is
/// feedstock ([`craft_feedstock`]) — `Some(vec![])` and `None` mean
/// different things, which is why callers gate on this and not on the
/// lines.
///
/// Gated on [`BlockType::is_bench_only`] first, the twin of the same gate
/// in [`crate::unlock::craft`] — nothing the world hands you has a price,
/// because it has no recipe.
///
/// Derived for every structural block: the material's whole-cube split
/// ([`crate::material::Material::craft_units`]) prorated UP to the
/// shape's volume.
pub fn craft_ideal(b: BlockType) -> Option<Vec<(Essence, u32)>> {
	if !b.is_bench_only() {
		return None;
	}
	if let Some(lines) = functional_ideal(b) {
		return Some(lines.to_vec());
	}
	let (m, s) = b.grid_cell()?;
	let units = m.craft_units()?;
	Some(match m.feedstock() {
		// A feedstock recipe consumes a whole cube of matter per craft
		// whatever shape it cuts (`craft_batch` says how many pieces), so
		// its catalyst is the cube's own, never prorated by the shape.
		Some(_) => units.to_vec(),
		// Essence-only substances keep the volume rule: the shape's share
		// of a cube's worth of the currency.
		None => cost_split(units, s.units()),
	})
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

/// The bench's pace for a reference-worth op — the base interval every
/// craft's own worth scales. It quickens at two Alchemy milestones.
pub fn base_pace_ticks(level: u32) -> u32 {
	if level >= 45 {
		60
	} else if level >= 25 {
		84
	} else {
		120
	}
}

/// The reference worth [`base_pace_ticks`] is the pace OF — historically
/// a whole plain block's worth at the bench, kept as the scale's anchor.
const REF_WORTH: u32 = 9;

/// What one feedstock BLOCK contributes to an item's worth — the same
/// figure a plain block's substance was always worth at the bench, so a
/// price paid in matter takes the time the matter takes.
const FEEDSTOCK_WORTH: u32 = 9;

/// Floor on the pace, in ticks. The cheapest crafts are worth a fraction
/// of the reference, and a strictly proportional pace would run them many
/// times a second, which is a stream of wire ops rather than a bench.
const MIN_PACE_TICKS: u32 = 6;

/// What ONE craft of `item` is worth at the bench — a fixed fraction of
/// its ideal (pre-waste) cost, essence and feedstock together. Zero for
/// anything with no recipe.
///
/// Pre-waste on purpose, which is what makes it a property of the ITEM
/// rather than of the player: two masons at different levels pay
/// different prices for the same stair and spend the same time making it.
pub fn craft_worth(item: BlockType) -> u32 {
	let Some(lines) = craft_ideal(item) else {
		return 0;
	};
	let essence: u32 = lines.iter().map(|&(_, n)| n).sum();
	let matter: u32 = craft_feedstock(item)
		.iter()
		.map(|&(_, n)| n * FEEDSTOCK_WORTH)
		.sum();
	(essence + matter).div_ceil(3)
}

/// Bench TRANSMUTATION pace in sim ticks for ONE `item` at `level`.
///
/// A craft used to be instant, and it was the last place in the economy
/// where XP arrived in a click: the transmute tab's CTRL multiplier paid a
/// hundred crafts' worth in one frame. Pacing it is not a nerf to the
/// award (crafting still pays what it always paid) — it is what makes the
/// award take the time the work takes. The catalog's biggest price is
/// also its longest hold, which is the reading a forge should have.
pub fn craft_pace_ticks(item: BlockType, level: u32) -> u32 {
	pace_ticks(craft_worth(item), level)
}

/// The pace curve: the level's reference interval scaled by what one op
/// is WORTH, floored.
fn pace_ticks(worth: u32, level: u32) -> u32 {
	let ticks = (base_pace_ticks(level) * worth + REF_WORTH / 2) / REF_WORTH;
	ticks.max(MIN_PACE_TICKS)
}

/// Bench token-bucket burst, in REFERENCE ops — absorbs network bunching
/// exactly as `EDIT_BURST` does for edits. The bucket itself banks TICKS
/// (a pace is per-op), so its capacity is this many reference intervals:
/// [`bench_burst_ticks`].
pub const BENCH_BURST: f32 = 2.0;

/// The most expensive SINGLE craft a player at `level` is allowed to ask
/// for, in ticks.
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
					.filter(|&b| crate::unlock::can_craft(b, l))
					.map(|b| craft_pace_ticks(b, l))
					.max()
					.unwrap_or(0)
			})
			.collect()
	});
	table[level.min(crate::skills::MAX_LEVEL) as usize]
}

/// The bench bucket's capacity in ticks at `level`: two reference ops'
/// worth, or ONE costliest craft if that is more.
///
/// The second half is not a nicety. The bucket banks ticks and an op
/// spends its own pace, which scales with what that op is WORTH — while
/// the burst is sized off the reference alone. The pick's forge is worth
/// an order of magnitude more, so its pace exceeded a FULL bucket and
/// `process_alchemy` refused it forever: no debit, no yield, no XP, just
/// the standing rebase — online only, since offline is unpaced, which is
/// the worst way to find out. `the_bucket_can_always_afford_the_costliest_op`
/// pins the guarantee.
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

	#[test]
	fn efficiency_and_waste_hit_their_pinned_endpoints() {
		// §3.2's curve, now scaling node draws: a fraction of the base at
		// the first level, all of it at the cap, never more.
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

	/// **No craft creates essence** — the redesign's master invariant, and
	/// it is structural: the essence pool has exactly one faucet
	/// ([`field_yield`], reachable only through a siphon draw on a live
	/// node) and the bench only ever debits it. This test states the
	/// structure rather than trusting it: nothing craftable pays a node
	/// yield, so no recipe's output can be turned back into the currency
	/// that bought it.
	#[test]
	fn no_craft_creates_essence() {
		for &b in &BlockType::ALL {
			if craft_ideal(b).is_none() {
				continue;
			}
			assert!(
				node_yields(b, crate::skills::MAX_LEVEL).is_empty(),
				"{b:?} is craftable and pays a draw"
			);
			// Nor does anything a craftable block can be dug back into.
			if let Some(drop) = b.dig_drop() {
				assert!(
					node_yields(drop, crate::skills::MAX_LEVEL).is_empty(),
					"{b:?} drops {drop:?}, which pays a draw"
				);
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
	/// retired price sits in a table for a year.
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

	/// **The essence tool costs no essence, ever** — and neither does the
	/// vessel that precedes it. Every essence in the game is a siphon
	/// draw, so an essence-priced siphon is a currency that cannot be
	/// bootstrapped; the bucket sits at the level everyone starts at, on
	/// the same footing. Both are priced in FEEDSTOCK a fresh player can
	/// chop, and their essence splits are pinned EMPTY.
	#[test]
	fn the_essence_tool_costs_no_essence() {
		for b in [BlockType::EssenceSiphon, BlockType::Bucket] {
			assert_eq!(
				functional_ideal(b),
				Some(&[][..]),
				"{b:?} must carry an empty essence split"
			);
			assert!(
				!craft_feedstock(b).is_empty(),
				"{b:?} must cost SOMETHING — feedstock is its whole price"
			);
			for (f, n) in craft_feedstock(b) {
				assert!(n > 0, "{b:?}: a zero-count feedstock line");
				assert!(
					!f.is_bench_only(),
					"{b:?} feeds on {f:?}, which only the bench makes — circular"
				);
				assert_eq!(
					crate::unlock::pickup(f),
					1,
					"{b:?} feeds on {f:?}, which a fresh player cannot pick up"
				);
			}
		}
		// Every feedstock line anywhere names a block a player can hold,
		// and a bench-made feedstock (furniture's planed timber, the
		// lamps' glass) must itself be craftable — a feedstock with no
		// recipe and no world source is a deadlock one table deeper.
		for &b in &BlockType::ALL {
			for (f, _) in craft_feedstock(b) {
				assert!(f.is_slottable(), "{b:?} feeds on unholdable {f:?}");
				assert!(
					!f.is_bench_only() || crate::unlock::craft(f).is_some(),
					"{b:?} feeds on unobtainable {f:?}"
				);
			}
		}
	}

	/// §7.1's loop half that survives the redesign: mined rock is Rubble,
	/// and clean rock is bench work — the crumble is what keeps the mason's
	/// unlock meaning something.
	#[test]
	fn mined_rock_is_always_rubble() {
		use BlockType as B;
		for rock in [B::Stone, B::Sandstone] {
			assert_eq!(rock.dig_drop(), Some(B::Rubble), "{rock:?}");
		}
		assert_eq!(B::Rubble.dig_drop(), Some(B::Rubble), "rubble round-trips");
		assert!(craft_ideal(B::Rubble).is_none(), "rubble is never crafted");
	}

	/// The bench's pace steps, and the catalog: the LEVELS that open each
	/// recipe live in `crate::unlock`, which pins them beside the gates
	/// they feed — here we only hold the catalog to the recipe table.
	#[test]
	fn pace_steps_and_the_catalog_are_pinned() {
		assert_eq!(base_pace_ticks(1), 120);
		assert_eq!(base_pace_ticks(25), 84);
		assert_eq!(base_pace_ticks(45), 60);
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

	/// The grid's craft prices, block by block — the feedstock-and-catalyst
	/// derivation, golden-pinned: a cube of the material's feedstock plus
	/// its small catalyst for every shape (the batch varies, the price per
	/// CRAFT does not), and a cube's worth of pure essence for the
	/// substances nothing in the world is made of.
	#[test]
	fn the_grid_reproduces_the_shipped_recipes() {
		use BlockType as B;
		use Essence as E;
		type Row = (
			BlockType,
			&'static [(BlockType, u32)],
			&'static [(Essence, u32)],
			u32,
		);
		// block, feedstock, catalyst, batch.
		let cases: &[Row] = &[
			(B::WoodStairs, &[(B::Wood, 1)], &[(E::Flora, 3)], 1),
			(B::StoneStairs, &[(B::Rubble, 1)], &[(E::Earth, 3)], 1),
			(B::LumberDoor, &[(B::Wood, 1)], &[(E::Flora, 3)], 2),
			(B::WoodSlab, &[(B::Wood, 1)], &[(E::Flora, 3)], 3),
			(B::StoneSlab, &[(B::Rubble, 1)], &[(E::Earth, 3)], 3),
			(B::Lumber, &[(B::Wood, 1)], &[(E::Flora, 3)], 1),
			(B::Stone, &[(B::Rubble, 1)], &[(E::Earth, 3)], 1),
			(B::Sandstone, &[(B::Sand, 1)], &[(E::Earth, 3)], 1),
			(B::Glass, &[(B::Sand, 1)], &[(E::Earth, 3)], 1),
			(B::StoneDoor, &[(B::Rubble, 1)], &[(E::Earth, 3)], 2),
			(B::DarkLumber, &[(B::Wood, 1)], &[(E::Flora, 3)], 1),
			(B::RedLumberStairs, &[(B::Wood, 1)], &[(E::Flora, 3)], 1),
			// The essence-only substances: no feedstock, the volume rule.
			(B::Metal, &[], &[(E::Metal, 27)], 1),
			(B::MetalSlab, &[], &[(E::Metal, 9)], 1),
			(B::Crystal, &[], &[(E::Aether, 18), (E::Metal, 9)], 1),
			// And the vessels, feedstock alone.
			(B::Bucket, &[(B::Wood, 6)], &[], 1),
			(B::EssenceSiphon, &[(B::Wood, 8)], &[], 1),
			// The torch: a stick with a treated head.
			(
				B::Torch,
				&[(B::Wood, 1)],
				&[(E::Flora, 2), (E::Earth, 2)],
				1,
			),
		];
		for &(b, feed, ideal, batch) in cases {
			assert_eq!(craft_feedstock(b), feed.to_vec(), "{b:?} feedstock");
			assert_eq!(craft_ideal(b), Some(ideal.to_vec()), "{b:?} catalyst");
			assert_eq!(crate::shapes::craft_batch(b), batch, "{b:?} batch");
		}
		// Nothing gains an economy by accident: a block crafts only
		// through a material it declared or a hand-priced entry, both of
		// which are their own decisions elsewhere.
		for &b in &BlockType::ALL {
			if !b.is_grid_item() {
				assert_eq!(
					craft_ideal(b).is_some(),
					functional_ideal(b).is_some(),
					"{b:?} crafts without a material or a hand-set price"
				);
			}
			if craft_ideal(b).is_none() {
				assert!(craft_feedstock(b).is_empty(), "{b:?} feeds with no recipe");
			}
		}
	}

	/// The bench spends its time in proportion to what a craft is worth —
	/// essence and feedstock alike — and pace never rises with level.
	#[test]
	fn pace_tracks_what_a_craft_is_worth() {
		use BlockType as B;
		// The two vessels take real time despite costing no essence: their
		// feedstock carries the worth, so "make your first vessel" is a
		// moment rather than a click.
		assert!(
			craft_worth(B::Bucket) > 0,
			"the bucket's timber is worth time"
		);
		assert!(craft_worth(B::EssenceSiphon) > craft_worth(B::Bucket));
		// The pick's forge stays the longest hold in the catalog.
		for &b in &BlockType::ALL {
			if craft_ideal(b).is_some() && b != B::ResonantPick {
				assert!(
					craft_pace_ticks(b, 99) <= craft_pace_ticks(B::ResonantPick, 99),
					"{b:?} out-holds the forge"
				);
			}
		}
		for &b in &BlockType::ALL {
			if craft_ideal(b).is_none() {
				assert_eq!(craft_worth(b), 0, "{b:?} has no recipe and no worth");
				continue;
			}
			for level in 2..=99 {
				assert!(
					craft_pace_ticks(b, level) <= craft_pace_ticks(b, level - 1),
					"{b:?} never slows at {level}"
				);
				assert!(craft_pace_ticks(b, level) >= MIN_PACE_TICKS, "{b:?}");
			}
			// A worth-more craft is never quicker than a worth-less one.
			for &o in &BlockType::ALL {
				if craft_ideal(o).is_some() && craft_worth(b) > craft_worth(o) {
					assert!(
						craft_pace_ticks(b, 1) >= craft_pace_ticks(o, 1),
						"{b:?} vs {o:?}"
					);
				}
			}
		}
	}

	/// A full bucket must be able to pay for the costliest single craft a
	/// player at that level may ask for, or that craft cannot be done AT
	/// ALL online: the server refuses on `bench_tokens < cost` and the
	/// tokens never reach it — no debit, no yield, no XP, just the
	/// standing rebase. Offline is unpaced, so single-player looks perfect
	/// while the dedicated server is a wall.
	#[test]
	fn the_bucket_can_always_afford_the_costliest_op() {
		for &b in &BlockType::ALL {
			for level in 1..=crate::skills::MAX_LEVEL {
				if crate::unlock::can_craft(b, level) {
					let cost = craft_pace_ticks(b, level) as f32;
					let burst = bench_burst_ticks(level);
					assert!(
						cost <= burst,
						"{b:?} at {level}: one Craft costs {cost} ticks against a \
						 {burst}-tick bucket — refused forever"
					);
				}
			}
		}
		// And the op that stresses it is genuinely the expensive end — a
		// catalog where the reference op IS the costliest would make this
		// test vacuous.
		assert!(craft_worth(BlockType::ResonantPick) > REF_WORTH);
	}

	/// A field draw pays the node's own table, and the capstone multiplies
	/// the whole split rather than one line of it.
	#[test]
	fn a_field_draw_pays_the_node_and_the_capstone_multiplies_all_of_it() {
		let (plain, _) = trace_cells(TEST_SEED);
		for &node in &crate::worldgen::nodes::NODES {
			for alchemy in [1, 40, crate::skills::MAX_LEVEL] {
				let table = node_yields(node, alchemy);
				assert!(!table.is_empty(), "{node:?} is worth nothing to draw");
				assert_eq!(
					field_yield(node, alchemy, 1, plain, TEST_SEED),
					table,
					"{node:?} at Alchemy {alchemy}: a draw is the yield table's own answer"
				);
				let mastered =
					field_yield(node, alchemy, crate::skills::MAX_LEVEL, plain, TEST_SEED);
				let draws = 1 + crate::unlock::ore_mastery(crate::skills::MAX_LEVEL);
				assert!(draws > 1, "the capstone hands out nothing");
				for (i, &(e, n)) in mastered.iter().enumerate() {
					assert_eq!(
						(e, n),
						(table[i].0, table[i].1 * draws),
						"{node:?} line {i}"
					);
				}
			}
		}
		// Aetherock is the kind that makes the multiply matter — if it
		// ever stops paying two essences, this test stops proving anything.
		assert!(
			node_yields(BlockType::Aetherock, 1).len() >= 2,
			"the two-essence tier is what the capstone rule is for"
		);
		// And nothing OUTSIDE the node roster pays a draw at all.
		for &b in &BlockType::ALL {
			if !crate::worldgen::nodes::NODES.contains(&b) {
				assert!(
					node_yields(b, crate::skills::MAX_LEVEL).is_empty(),
					"{b:?} pays a draw without being a node"
				);
			}
		}
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
		for &node in &crate::worldgen::nodes::NODES {
			if node == BlockType::Duskstone {
				continue;
			}
			assert_eq!(
				field_yield(node, 40, 1, rich, TEST_SEED),
				field_yield(node, 40, 1, plain, TEST_SEED),
				"{node:?} traces, and only the middle tier is meant to"
			);
		}
	}

	/// **Every priced item is buildable by a player who has just earned
	/// it.** For each essence its recipe names, some NODE that player can
	/// already see and draw pays that essence; for each feedstock line,
	/// the block is one the world hands out below the item's own level.
	///
	/// This is the pin that was missing when the Resonant Pick shipped:
	/// the twins guaranteed a price had a level, and nothing asked whether
	/// the price was PAYABLE at it. With extraction gone the question is
	/// sharper — a node is the only faucet, so an essence with no
	/// reachable node is a price nobody can pay.
	///
	/// What counts as reachable, and why each rule is load-bearing:
	///
	/// - **The node's reveal must be met on its own axis** at the item's
	///   level, read as a player levelling both skills together — a find
	///   you cannot SEE is not a source. Mining walls on the host are NOT
	///   checked: soil is ungated, so Mining is self-serve by digging, and
	///   an item's Alchemy level says nothing about how much rock its
	///   owner has broken.
	/// - **Only what a kind pays RELIABLY.** The Duskstone trace is a
	///   chance, and `node_yields` deliberately excludes it — a price
	///   payable when the hash smiles is not payable.
	/// - **Feedstock must be world-handed and ungated by Alchemy**: a
	///   craft-only feedstock below its own recipe's level would nest the
	///   same deadlock one table deeper.
	#[test]
	fn every_functional_price_has_a_source_at_its_own_level() {
		for &item in &BlockType::ALL {
			let Some(level) = crate::unlock::functional(item) else {
				continue;
			};
			let price = functional_ideal(item).expect("the twin pin guarantees one");
			for &(essence, _) in price {
				let source = crate::worldgen::nodes::NODES.iter().copied().find(|&n| {
					if crate::unlock::reveal(n).is_some_and(|r| r > level)
						|| crate::unlock::alchemy_reveal(n).is_some_and(|r| r > level)
					{
						return false;
					}
					node_yields(n, level)
						.iter()
						.any(|&(e, y)| e == essence && y > 0)
				});
				assert!(
					source.is_some(),
					"{item:?} costs {essence:?} and unlocks at {level}, but no node a \
					 player at that level can see pays {essence:?} — the item is \
					 unlocked and unbuildable"
				);
			}
			for (f, _) in craft_feedstock(item) {
				assert!(
					crate::unlock::pickup(f) <= level,
					"{item:?} feeds on {f:?}, gated past the item itself"
				);
			}
		}
	}
}
