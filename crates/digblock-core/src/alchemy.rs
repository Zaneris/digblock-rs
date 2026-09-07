//! The alchemy economy: node-draw yields, transmutation costs and the bench
//! pace — every number the server and offline play must agree on. All integer
//! math. The LEVELS that open each recipe live in [`crate::unlock`].
//!
//! Three rules this module is pinned against: nothing the world hands you may
//! be transmuted, gated on [`crate::block::BlockType::is_bench_only`]; no
//! craft creates essence, since [`crate::worldgen::nodes`] holds the only
//! faucet and this module only ever debits; and the essence tool costs no
//! essence, ever, or the currency deadlocks on the first world where nobody
//! owns a siphon. Structural costs DERIVE from the refinement chains: a cube
//! is its parent cube plus a catalyst, a shape is its own cube plus a pinch,
//! raw matter shapes for free. The functional roster is hand-priced, because
//! function has no volume.

use crate::block::{BlockType, Essence, ShapeFamily};

/// The most cost lines — feedstock and essence together — any one craft
/// carries. A screen drawing a price as chips sizes its icon budget from
/// this rather than from a guess; `no_craft_has_more_cost_lines_than_the_budget`
/// keeps it honest against the whole catalog.
pub const MAX_COST_LINES: usize = 3;

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

/// What one FIELD draw pays into a player's stocks — a NODE drawn out where it stands, vein or
/// heartwood or spring alike, with no item in between. The ONE answer the server's dig credit and
/// the engine's offline arm both take. Deliberately the ordinary [`node_yields`] and not a second
/// table, with Mining's capstone MULTIPLYING the whole split. The TRACE is why this takes a cell:
/// [`crate::worldgen::ore::traces_aether`] is DERIVED from it, a rolled line differing per machine.
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
	if level >= crate::unlock::WASTE_DIP_AT {
		lin.min(1250)
	} else {
		lin
	}
}

/// One cost line at `level`: `ceil(ideal · waste)`.
pub fn craft_cost(ideal: u32, level: u32) -> u32 {
	(ideal * waste_permille(level)).div_ceil(1000)
}

/// A waystone passage's price, hand-tabled: the fare rises with the port's remoteness WITHIN an
/// essence, and the essence climbs the ladder with the port, so the spine spends every faucet the
/// other ladders open. `None` is a kind with no port, exactly [`crate::unlock::waystone`]'s set —
/// pinned twins, a fare without a port being as wrong as a port without a fare, and payable at its
/// own level. Return Home has no row AT THE TYPE, so the recovery spell costs no essence, ever.
pub fn cast_price(kind: crate::worldgen::SurfaceKind) -> Option<(Essence, u32)> {
	use crate::worldgen::SurfaceKind as K;
	Some(match kind {
		K::Plains => (Essence::Vitae, 8),
		K::Forest => (Essence::Vitae, 10),
		K::Savanna => (Essence::Terra, 12),
		K::Taiga => (Essence::Unda, 16),
		K::Beach => (Essence::Unda, 20),
		K::Mountain => (Essence::Ferrum, 24),
		K::Snow => (Essence::Aether, 1),
		K::Ocean | K::River | K::Lake | K::Marsh | K::GravelShore | K::Scree => return None,
	})
}

/// The self-light's fare: Vitae, the day-one essence, so Arcana's own
/// bootstrap spell is payable from the first heartwood draws. The fare
/// doubles with the burn at `WISP_BRIGHT_AT` — a brighter, longer light
/// priced as the bootstrap one would be a free upgrade, not a sink.
pub fn wisp_price(level: u32) -> (Essence, u32) {
	const BASE: u32 = 4;
	(
		Essence::Vitae,
		if level >= crate::unlock::WISP_BRIGHT_AT {
			BASE * 2
		} else {
			BASE
		},
	)
}

/// Conjured light's fare, easing at the late rung — the caster's Arcana
/// decides, exactly as the port fares ride the port table.
pub fn kindle_price(level: u32) -> (Essence, u32) {
	(
		Essence::Terra,
		if level >= crate::unlock::KINDLE_CHEAP_AT {
			4
		} else {
			6
		},
	)
}

/// The Sphere's fare: Unda, the water essence, for the spell that drives
/// water out — shared with the cold and shore ports ([`cast_price`]), so
/// the springstone node feeds both halves of the ladder rather than one
/// spell. Doubles with each rank, as the sphere's shell roughly does
/// (`unlock::sphere_radius`), so a wider sphere is never a free upgrade.
/// The essence is only half the bill: the shell itself is paid in the
/// caster's held blocks (`sphere::blocks_needed`), and the seal lights
/// the wisp at [`wisp_price`] on top, since most matter makes a dark
/// room.
pub fn sphere_price(level: u32) -> (Essence, u32) {
	const BASE: u32 = 12;
	let rank = match level {
		l if l >= crate::unlock::SPHERE_VAST_AT => 2,
		l if l >= crate::unlock::SPHERE_WIDE_AT => 1,
		_ => 0,
	};
	(Essence::Unda, BASE << rank)
}

/// The Causeway's fare: Unda, the Sphere's essence — the two cell-writing spells share a price
/// line, and the springstone faucet feeds both. Climbs faster than
/// [`crate::unlock::causeway_secs`] lengthens the road, which is what makes a rung sell
/// uninterrupted distance rather than a discount on it.
pub fn causeway_price(level: u32) -> (Essence, u32) {
	const BASE: u32 = 12;
	let fare = match level {
		l if l >= crate::unlock::CAUSEWAY_VAST_AT => BASE * 9,
		l if l >= crate::unlock::CAUSEWAY_LONG_AT => BASE * 3,
		_ => BASE,
	};
	(Essence::Unda, fare)
}

/// Functional items' craft costs — priced BY HAND (§7.3), never by volume, because a torch's cost
/// is a decision rather than a measurement. Cross-family costs live here on purpose: they are what
/// make the essences feel like an economy instead of parallel currencies. The LEVELS these open at
/// are not here — every threshold lives in [`crate::unlock`], and `unlock::functional` is this
/// list's pinned twin. An item may ALSO cost feedstock blocks ([`functional_feedstock`]).
pub fn functional_ideal(b: BlockType) -> Option<&'static [(Essence, u32)]> {
	use BlockType as B;
	use Essence as E;
	Some(match b {
		// The two vessels cost NO essence, ever — see
		// `functional_feedstock`, which is the whole of their price. The
		// empty splits stay in this table because the twin pin reads it:
		// a level and a price are one decision.
		B::Bucket | B::EssenceSiphon => &[],
		B::Torch => &[(E::Vitae, 2), (E::Terra, 3)],
		// The game's biggest sink, and the only thing that makes Ferrum a
		// goal rather than a number: about three duskstone finds and three
		// glowvein finds at the level it opens. The Aether line is what
		// keeps Aetherock worth reaching for before the lamps arrive.
		B::ResonantPick => &[(E::Ferrum, 480), (E::Terra, 120), (E::Aether, 24)],
		B::Lantern => &[(E::Ferrum, 12), (E::Terra, 8)],
		B::Aetherlamp => &[(E::Ferrum, 18), (E::Aether, 5)],
		// Priced above a run of crystal, so it stays the last line.
		B::MastersSigil => &[(E::Aether, 360)],
		// Furniture is a timber feedstock and a living catalyst — chosen,
		// not measured: a chair is mostly air and still costs a chair.
		B::Chair => &[(E::Vitae, 3)],
		B::Table => &[(E::Vitae, 4)],
		// The bed is the catalog's first genuinely cross-family price:
		// a timber frame AND the bedding on it. Unda is the reagent with
		// the fewest sources — the springstone node is the whole of it —
		// which is what makes diving for one worth doing.
		B::Bed => &[(E::Vitae, 6), (E::Unda, 8)],
		_ => return None,
	})
}

/// The BLOCKS a functional item consumes beside its essence lines — matter in, not currency in.
/// Today it carries the two vessels, whose whole price it is. The siphon may never cost essence:
/// every essence in the game is a siphon draw, so an essence-priced siphon deadlocks the currency
/// (`the_essence_tool_costs_no_essence`). The bucket's RUNG is its whole gate, opening past the
/// siphon. Tree matter for both — "dig your way to the trees, fell a few, make your tools" IS
/// the loop; the siphon asks for the canopy as well as the trunk so one felled tree covers it —
/// ANY canopy ([`feedstock_accepts`]), since a spawn among conifers has only needles to give.
pub fn functional_feedstock(b: BlockType) -> &'static [(BlockType, u32)] {
	match b {
		BlockType::Bucket => &[(BlockType::Wood, 6)],
		BlockType::EssenceSiphon => &[(BlockType::Wood, 2), (BlockType::Leaves, 5)],
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

/// Does `b` pay toward a feedstock line that names `f`? The named block always does; beyond it,
/// matter is matter: a line that asks for the canopy takes any canopy, because a spawn among
/// conifers has only needles to offer and the siphon is the one recipe a fresh player MUST be
/// able to make. Every consumer of a line — the craft's check and debit, the bench's chips, the
/// siphon nudge — counts through [`feedstock_held`], so an alternate is worth exactly what the
/// named block is everywhere, and the sweep is over the catalog rather than a hand-kept pair.
pub fn feedstock_accepts(f: BlockType, b: BlockType) -> bool {
	b == f || (f.is_foliage() && b.is_foliage())
}

/// How much of a feedstock line a pack can pay: everything [`feedstock_accepts`] admits, summed.
/// `count` is the pack's own answer, so the bench's copied counts and the live inventory ask
/// one question.
pub fn feedstock_held(f: BlockType, count: impl Fn(BlockType) -> u32) -> u32 {
	BlockType::ALL
		.iter()
		.filter(|&&b| feedstock_accepts(f, b))
		.map(|&b| count(b))
		.sum()
}

/// Feedstock lines for anything craftable — the block half of a price. A CUBE consumes one cube
/// of its parent ([`crate::material::Material::refined_from`]); a SHAPE is cut from its own
/// material's cube, as many as the form takes ([`ShapeFamily::batch`]); functional items carry
/// their hand-set lines. The cube a shape names may be the world's (a dirt slab, a rubble stair)
/// or the bench's (a stone stair), which is the whole shape of the chain.
pub fn craft_feedstock(b: BlockType) -> Vec<(BlockType, u32)> {
	if !b.is_bench_only() {
		return Vec::new();
	}
	if functional_ideal(b).is_some() {
		return functional_feedstock(b).to_vec();
	}
	let Some((m, s)) = b.grid_cell() else {
		return Vec::new();
	};
	match s {
		ShapeFamily::Block => m.refined_from().map(|p| vec![(p, 1)]).unwrap_or_default(),
		_ => own_cube(m)
			.map(|cube| vec![(cube, s.batch().0)])
			.unwrap_or_default(),
	}
}

/// The cube a shape of `m` is cut from, if a player can ever hold one:
/// the world hands it out, or the bench makes it.
fn own_cube(m: crate::material::Material) -> Option<BlockType> {
	let cube = BlockType::from_grid(m, ShapeFamily::Block)?;
	(!cube.is_bench_only() || m.refine_units().is_some()).then_some(cube)
}

/// Ideal (pre-waste) craft cost lines for anything craftable. `None` = not craftable, while EMPTY
/// is a craftable item whose whole price is feedstock ([`craft_feedstock`]) — `Some(vec![])` and
/// `None` mean different things, which is why callers gate on this and not on the lines. Gated on
/// [`BlockType::is_bench_only`] first, the twin of the same gate in [`crate::unlock::craft`], and
/// derived for every structural block from its chain: a cube costs its material's refinement, a
/// shape costs its material's pinch ([`crate::material::Material::shape_units`], nothing for raw
/// matter), which is what makes the raw shapes the vessels' kind of recipe — feedstock alone.
pub fn craft_ideal(b: BlockType) -> Option<Vec<(Essence, u32)>> {
	if !b.is_bench_only() {
		return None;
	}
	if let Some(lines) = functional_ideal(b) {
		return Some(lines.to_vec());
	}
	let (m, s) = b.grid_cell()?;
	Some(match s {
		ShapeFamily::Block => m.refine_units()?.to_vec(),
		_ => {
			own_cube(m)?;
			m.shape_units()
		}
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
	if level >= crate::unlock::BENCH_FASTEST_AT {
		60
	} else if level >= crate::unlock::BENCH_FASTER_AT {
		84
	} else {
		120
	}
}

/// The reference worth [`base_pace_ticks`] is the pace OF — historically
/// a whole plain block's worth at the bench, kept as the scale's anchor.
const REF_WORTH: u32 = 9;

/// What a cube the WORLD handed out embodies, in units — the same figure a
/// plain block's substance was always worth at the bench, so a price paid
/// in matter takes the time the matter takes. A cube the bench made
/// embodies its own craft instead ([`embodied_units`]).
const RAW_WORTH: u32 = 9;

/// Floor on the pace, in ticks. The cheapest crafts are worth a fraction
/// of the reference, and a strictly proportional pace would run them many
/// times a second, which is a stream of wire ops rather than a bench.
const MIN_PACE_TICKS: u32 = 6;

/// What ONE craft of `item` EMBODIES, in units: its ideal essence plus what its feedstock
/// embodied in turn — a world-handed cube counts [`RAW_WORTH`], a bench-made one carries its own
/// craft's units per piece, so a polished slab is worth the rubble, the three catalysts and the
/// cut that went into it. Zero for anything with no recipe. Memoized, since the bench bucket asks
/// every tick; the chain pin bounds the recursion.
fn embodied_units(item: BlockType) -> u32 {
	static TABLE: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
	fn embodied(b: BlockType, depth: u32) -> u32 {
		let Some(lines) = craft_ideal(b) else {
			return 0;
		};
		assert!(
			depth < 8,
			"{b:?}: a feedstock chain that never reaches the world"
		);
		let essence: u32 = lines.iter().map(|&(_, n)| n).sum();
		let matter: u32 = craft_feedstock(b)
			.iter()
			.map(|&(f, n)| {
				let per_piece = if f.is_bench_only() && craft_ideal(f).is_some() {
					embodied(f, depth + 1).div_ceil(crate::shapes::craft_batch(f))
				} else {
					RAW_WORTH
				};
				n * per_piece
			})
			.sum();
		essence + matter
	}
	let table = TABLE.get_or_init(|| BlockType::ALL.into_iter().map(|b| embodied(b, 0)).collect());
	table[item as usize]
}

/// What ONE craft of `item` is worth at the bench — a fixed fraction of what it embodies
/// ([`embodied_units`]). Zero for anything with no recipe. Pre-waste on purpose, which is what
/// makes it a property of the ITEM rather than of the player: two masons at different levels pay
/// different prices for the same stair and spend the same time making it.
pub fn craft_worth(item: BlockType) -> u32 {
	embodied_units(item).div_ceil(3)
}

/// Bench TRANSMUTATION pace in sim ticks for ONE `item` at `level`. An instant craft is the last
/// place in the economy where XP could arrive in a click — the transmute tab's CTRL multiplier
/// paying a hundred crafts' worth in one frame — so pacing is not a nerf to the award but what
/// makes the award take the time the work takes. The catalog's biggest price is its longest hold.
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

/// The most expensive SINGLE craft a player at `level` is allowed to ask for, in ticks. Gated on
/// what that level can actually do, not on the whole catalog: a fresh player banks what a fresh
/// player's ops cost, and the Resonant Pick's long forge only widens the allowance from the level
/// that can forge one. Memoized per level, because the server's refill asks for the bucket's
/// capacity every tick for every connected client and the answer is a property of the tables.
fn costliest_op_ticks(level: u32) -> u32 {
	static MAX: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
	let table = MAX.get_or_init(|| {
		(0..=crate::skills::MAX_LEVEL)
			.map(|l| {
				BlockType::ALL
					.into_iter()
					.filter(|&b| crate::unlock::recipe_open(b, l))
					.map(|b| craft_pace_ticks(b, l))
					.max()
					.unwrap_or(0)
			})
			.collect()
	});
	table[level.min(crate::skills::MAX_LEVEL) as usize]
}

/// The bench bucket's capacity in ticks at `level`: two reference ops' worth, or ONE costliest
/// craft if that is more. The second half is not a nicety — the bucket banks ticks and an op
/// spends its own pace, which scales with what that op is WORTH, while the burst is sized off the
/// reference alone, so a pace exceeding a FULL bucket had `process_alchemy` refuse it forever,
/// online only. `the_bucket_can_always_afford_the_costliest_op` pins it; refill is untouched.
pub fn bench_burst_ticks(level: u32) -> f32 {
	let reference = BENCH_BURST * base_pace_ticks(level) as f32;
	reference.max(costliest_op_ticks(level) as f32)
}

#[cfg(test)]
mod tests;
