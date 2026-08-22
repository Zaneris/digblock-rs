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
	Arcana = 2,
}

impl Skill {
	pub const COUNT: usize = 3;
	pub const ALL: [Skill; Self::COUNT] = [Skill::Mining, Skill::Alchemy, Skill::Arcana];

	pub fn try_from_index(i: u8) -> Option<Self> {
		match i {
			0 => Some(Skill::Mining),
			1 => Some(Skill::Alchemy),
			2 => Some(Skill::Arcana),
			_ => None,
		}
	}

	/// What the skill is called. ONE case, stored: a screen that shouts
	/// its labels uppercases where it draws, which is what keeps the
	/// name a single string rather than one per context.
	pub fn name(self) -> &'static str {
		match self {
			Skill::Mining => "Mining",
			Skill::Alchemy => "Alchemy",
			Skill::Arcana => "Arcana",
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

	/// Wind one skill back to nothing — the reader's tool for a career
	/// earned under rules a retune withdrew. Deliberately not on the
	/// wire or in any award path: the only honest reason to move a total
	/// DOWN is a migration that says so at a version.
	pub fn clear(&mut self, s: Skill) {
		self.xp[s as usize] = 0;
	}

	/// Take the HIGHER total per skill, for the legacy-character migration.
	///
	/// The max, never the sum. Both sides are the same player's XP, and a
	/// sum would pay them twice for one career — join a server on the new
	/// account, grind, then claim the old character, and the totals add.
	/// The max says the honest thing instead: you are as far along as your
	/// furthest self, and the ladder is unchanged by having walked it on
	/// two keys.
	///
	/// This is [`Self::set_total`]'s rule, and deliberately so: totals only
	/// ever grow, in a merge exactly as on the wire.
	pub fn merge_from(&mut self, other: &Self) {
		for s in Skill::ALL {
			self.set_total(s, other.xp(s));
		}
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

/// How much of the journey from a family's novice speed to its mastery speed is
/// complete at each of [`DIG_STEPS`], in permille. A pinned integer table, never
/// a `powf` — same reason as the XP curve: it is a golden both ends must agree
/// on to the unit, and a float would let two platforms round a hold apart.
///
/// **The increments strictly decrease, and that IS the design.** Each jump is a
/// constant fraction smaller than the last, so the first step is the biggest
/// speedup a player will ever feel. The early game is where a slow dig is most
/// discouraging, and diminishing returns stop a late level from being a
/// rounding error. The RELATIVE gain falls too, so the curve eases off in felt
/// terms and not merely in ticks.
///
/// One table serves every family, so the shape is authored once. `Fixed` has a
/// zero-width range and stays flat for free.
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

/// §5.1's per-family dig-speed multiplier in permille: two endpoints per family
/// — its novice speed and its mastery speed — with [`DIG_TAPER`] deciding how
/// the journey between them is spent. A family says only how hard its material
/// resists a novice and how fast mastery gets; the shape of the climb is shared.
/// Fixed is flat: foliage is already near-instant, the torch is a fixture, and
/// the water scoop's duration is pinned to the viewmodel swing.
///
/// The mastery endpoint IS the speed floor — no second clamp — and there is no
/// mid anchor. So `hardness_ticks` is a BASE the curve scales at every level,
/// and outside `Fixed` no level spends it unscaled: don't reintroduce an
/// assertion that some level returns the raw hardness.
///
/// The RATIO between the two ends — what a whole life of levelling is worth as
/// a multiple — is deliberately modest: mastery was once fast enough that the
/// ladder's top was where the material stopped mattering.
pub fn dig_speed_permille(family: crate::block::DigFamily, level: u32) -> u32 {
	use crate::block::DigFamily;
	let (start, end) = match family {
		DigFamily::Fixed => (1000, 1000),
		DigFamily::Soft => (2063, 542),
		DigFamily::Wood => (2228, 650),
		DigFamily::Rock => (2805, 542),
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
/// world-derived multiplier would let the two ends disagree by the whole
/// multiplier whenever their worlds differ by one cell, far outside what
/// `dig_grace` absorbs.
///
/// The consequence is deliberate: a LONE block costs the multiplier too. The
/// spread penalty is set against an offsetting tool bonus that very nearly
/// cancels it, so swinging at one block costs about what a bare hand does and
/// the pick is always worth holding, while a full row clears in the time of
/// one.
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

/// What is left of `progress` banked ticks out of `required` after `idle` ticks
/// of not digging.
///
/// THE one decay rule: the client predicts with it, the server's completion
/// gate charges it, and offline play runs the same call, so no two can drift.
/// Integer and exact — it takes the ELAPSED idle count rather than accumulating
/// a per-tick subtraction, because a fractional per-tick rate would be under
/// one for every block in the game and so would round to zero and decay
/// nothing. Truncating down means the bar outlives its arithmetic by up to a
/// tick and reaches exactly zero at [`DIG_DECAY_TICKS`] for every hardness.
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
/// Zero for the undiggable (which can't complete a dig anyway) — and zero,
/// BY NAME, for foliage and the torch: a trivial break is housekeeping,
/// not mining, and the scale-down alone cannot say so, because its
/// rounding hands even the softest block a whole point. The FLAT award —
/// where the dig happened is [`depth_xp_permille`]'s business, and
/// [`dig_award`] is what puts the two together (its cap is what keeps a
/// zero-worth block's swing cells at zero too).
pub fn dig_xp(block: BlockType) -> u64 {
	if block.is_foliage() || block == BlockType::Torch || block.is_conjured() {
		return 0;
	}
	match block.hardness_ticks() {
		Some(h) => u64::from(h).div_ceil(24),
		None => 0,
	}
}

/// Where the depth reward opens: the waterline. At and above it a dig pays
/// the flat table exactly, so nothing about surface mining moves.
pub const DEPTH_XP_FROM: i32 = crate::worldgen::SEA_LEVEL;

/// Where it tops out: the deepest cell a dig can happen in at all. Bedrock
/// itself is undiggable, so anchoring the far end one cell above it is what
/// makes [`DEPTH_XP_MAX_PERMILLE`] a figure somebody can actually be handed,
/// rather than a limit the world approaches and never reaches.
pub const DEPTH_XP_TO: i32 = crate::block::BEDROCK_Y + 1;

/// One whole, in permille — the unit [`depth_xp_permille`] is expressed
/// against and [`dig_award`] rounds to.
const PERMILLE: u32 = 1000;

/// The depth reward at its deepest, in permille of the flat award.
///
/// Bounded from both sides, and neither bound is arithmetic. Above: well
/// under the multiple that would make depth compulsory — this is a reason
/// to go down, not a tax on staying up. Below: enough to be worth
/// noticing. What used to bind it from underneath was the rounding, and no
/// longer does: [`dig_award`] rounds to NEAREST, so any ramp above
/// [`PERMILLE`] moves every award in the game by construction
/// (`the_depth_reward_moves_every_award`) — however cheap the block, and
/// however small the award scale is retuned to be.
pub const DEPTH_XP_MAX_PERMILLE: u32 = 1500;

/// What a cell the Resonant Pick's swing reached — every cell but the one under
/// the crosshair — pays, in permille of its own flat award (§5.6).
///
/// A wide swing is a reach bonus, not an XP multiplier. The pick already pays
/// for itself in **material**: one hold hands back a whole plane, at a
/// level-only multiple of a single-cell hold. Paying full XP per cell on top
/// would make the tool the only sensible way to earn Mining at all, and each of
/// its two reach milestones a step change in levelling speed rather than in
/// what the tool DOES.
///
/// Floored at one whole point per cell by [`dig_award`]: an extra cell that
/// pays literally nothing reads as a bug rather than as a discount.
pub const PICK_EXTRA_PERMILLE: u32 = 200;

/// §5.2's depth reward: the multiplier on a dig's Mining XP for the cell it
/// happened in, in permille. Flat at and above [`DEPTH_XP_FROM`], then rising
/// LINEARLY with depth to [`DEPTH_XP_MAX_PERMILLE`] at [`DEPTH_XP_TO`].
///
/// **Linear per block rather than quantized into depth layers**, the opposite
/// of what [`DIG_STEPS`] does to the dig curve. That curve had to be quantized
/// because ticks are fine-grained enough that a per-level change moved a hold
/// by one or by nothing. XP has no such problem: the awards are small integers
/// and [`dig_award`]'s rounding quantizes them for free, leaving each block
/// type its own ladder of whole-point steps as you descend
/// (`the_depth_reward_climbs_one_point_at_a_time`). Layering on top would buy
/// cliffs — depths where one more block down is worth a permanent step and
/// every block after it is worth nothing.
pub fn depth_xp_permille(y: i32) -> u32 {
	let depth = (DEPTH_XP_FROM - y.clamp(DEPTH_XP_TO, DEPTH_XP_FROM)) as u32;
	let span = (DEPTH_XP_FROM - DEPTH_XP_TO) as u32;
	PERMILLE + (DEPTH_XP_MAX_PERMILLE - PERMILLE) * depth / span
}

/// THE award for one completed dig, `(Mining, Alchemy)` — the whole answer in
/// one call, so no award site can read the tables and forget the depth reward.
/// The server's dig-credit chokepoint and the engine's offline arm call this
/// and nothing else.
///
/// `prev` is the journaled truth about the cell that was replaced (never the
/// raycast hit), `broken` how many cells the one hold cleared, and `y` the
/// ANCHOR cell's — a wide pick swing spans several rows, and the anchor is the
/// one cell both ends provably agree on. The whole swing is scaled once rather
/// than cell by cell: one action, one rounding.
///
/// **Only the aimed cell pays in full**; every other cell pays
/// [`PICK_EXTRA_PERMILLE`] of one. A door pair is still ONE block and never
/// reaches that arm, since `broken` counts blocks rather than cells.
///
/// A field draw pays its per-kind figure in BOTH skills instead, and the depth
/// reward deliberately does not touch it: the ore tiers are depth-banded
/// already, so scaling them by depth would count the same axis twice.
pub fn dig_award(
	prev: BlockType,
	extracting: Option<BlockType>,
	broken: u32,
	y: i32,
) -> (u64, u64) {
	if let Some(split) = extracting.and_then(field_xp) {
		return split;
	}
	let unit = u64::from(PERMILLE);
	let base = dig_xp(prev);
	// A cell the swing reached, in permille of its own award: the fraction,
	// floored at one whole point so the cheapest blocks still pay something,
	// and capped at the award itself — which is what keeps a block worth
	// nothing worth nothing, and stops the floor out-paying the cell it is
	// a fraction of.
	let each = (base * u64::from(PICK_EXTRA_PERMILLE))
		.max(unit)
		.min(base * unit);
	// Everything stays in permille to here, so the swing is ONE rounding —
	// it is one action, and a fraction rounded per cell and then multiplied
	// out would drift further from the fraction the further the pick
	// reaches, which is the opposite of what a widening swing should do.
	let flat = match broken {
		0 => 0,
		n => base * unit + u64::from(n - 1) * each,
	};
	// To NEAREST rather than truncated. The awards are small integers, so
	// truncation quietly under-delivers both the ramp and the fraction, and
	// it under-delivers them worst on the cheapest digs — the ones with the
	// least room to show it. Rounding is also what makes the ramp's
	// guarantee structural: every award moves with depth for any multiplier
	// above unity, so neither a new cheap row nor a retune of the award
	// scale can silently strand the bottom of the hardness order on a flat
	// number.
	let scale = unit * unit;
	let mining = (flat * u64::from(depth_xp_permille(y)) + scale / 2) / scale;
	(mining, 0)
}

/// The DUAL award for one field draw — a siphon taking a node where it stands:
/// `(Mining, Alchemy)`. `None` for anything that is not a node.
///
/// A draw is mechanically a dig and thematically an alchemical separation, so
/// it pays both, and WHICH skill takes the headline follows which skill found
/// the node: the ore tiers pay Mining first with an Alchemy cut, the surface
/// nodes invert it, because the surface hunt is Alchemy's and a forager who
/// never opens a mine still moves the skill that revealed the find. There is no
/// second moment behind either — a node is never an item, so no draw is
/// "finished at the bench" later.
///
/// Tabled rather than derived from hardness: these are headline rewards and the
/// curve through them is a decision. Which is why the table does NOT follow a
/// retune of [`dig_xp`] — a retune moves what a find is worth RELATIVE to plain
/// digging, deliberately: a lever, not a drift.
pub fn field_xp(node: BlockType) -> Option<(u64, u64)> {
	Some(match node {
		// The Alchemy cuts sit high against their Mining headlines on
		// purpose: with bench extraction gone, a draw is one of exactly
		// two ways the skill earns at all, so each has to carry what the
		// retired drip used to spread over a hoard.
		BlockType::Emberore => (40, 30),
		BlockType::Duskstone => (70, 55),
		BlockType::Glowvein => (110, 85),
		BlockType::Aetherock => (170, 130),
		// The surface ladder, Alchemy-primary, rising with the reveal
		// order — heartwood is the day-one node, and a handful of hearts
		// should reach the leystone reveal within the first session.
		BlockType::Heartwood => (8, 40),
		BlockType::Leystone => (10, 55),
		BlockType::Springstone => (12, 75),
		_ => return None,
	})
}

/// Alchemy XP per crafted item: the craft's full worth at the bench
/// ([`crate::alchemy::craft_worth`], a fixed fraction of the ideal pre-waste
/// cost, feedstock included) — fixed for life. Zero for uncraftable items.
///
/// §13.4's award-side HALVING is deliberately gone. It existed because the
/// bench was an unbounded XP machine, and the lever un-pulls with the machine:
/// a craft is now bounded by essence that had to be found and matter that had
/// to be quarried, so it may pay what it is worth. Award and pace read the same
/// underlying worth, so a thing worth more to make takes the time the work
/// takes.
///
/// This is the bench's ONLY award: Alchemy is earned by drawing nodes in the
/// field ([`field_xp`]) and by making things here, and by nothing that can be
/// repeated against an unbounded supply.
pub fn craft_xp(item: BlockType) -> u64 {
	u64::from(crate::alchemy::craft_worth(item))
}

/// Arcana XP for a Return Home cast, derived from the DISTANCE the jump
/// covered rather than a flat figure — the depth ramp's spirit: the
/// world scales the award, both ends compute it from positions the
/// server already holds, and a zero-length jump pays zero, which is the
/// anti-idle rule stated as arithmetic. Capped, so no journey is worth
/// making only to be teleported back out of.
pub fn home_xp(dist_blocks: f32) -> u64 {
	((dist_blocks.max(0.0) / HOME_XP_STRIDE) as u64).min(HOME_XP_CAP)
}

/// Blocks per point of the home award.
pub const HOME_XP_STRIDE: f32 = 8.0;

/// The home award's ceiling, per jump.
pub const HOME_XP_CAP: u64 = 60;

/// Arcana XP for a PRICED cast, derived from what it drains — the
/// `craft_xp` rule: the award rides the price, so a fare retune moves
/// its XP with it and no spell pays out of proportion to its drain.
/// Zero-cost casts pay zero structurally.
///
/// The rate and the fares are one decision, not two: raising the fares
/// alone would pay MORE per cast for costing more, which is the opposite
/// of a sink.
pub fn cast_xp(essence_price: u32) -> u64 {
	u64::from(essence_price) * CAST_XP_PER_ESSENCE
}

/// Points per essence unit a cast drains.
pub const CAST_XP_PER_ESSENCE: u64 = 7;

/// Total Alchemy XP for one validated alchemy op (call only after
/// `apply_alchemy` succeeded — the op's counts are then known-valid).
pub fn alchemy_xp(op: &crate::inventory::AlchemyOp) -> u64 {
	use crate::inventory::AlchemyOp;
	match *op {
		AlchemyOp::Craft { item, count } => craft_xp(item) * u64::from(count),
	}
}

#[cfg(test)]
mod tests;
