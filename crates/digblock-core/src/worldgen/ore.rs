//! Ore placement — **derived, never stored**.
//!
//! This module writes nothing: `build_column` returns exactly the bytes it
//! returned before ore existed and the worldgen golden hash does not move,
//! which is what lets veins appear retroactively in worlds that already exist.
//! A deposit is a salted position hash over a coarse grid, the
//! [`crate::decoration::structures_near`] pattern in three dimensions, so
//! pockets span chunk borders with no pipeline state.
//!
//! Two consumers, two shapes, pinned to agree: [`ore_at`] answers ONE cell for
//! the dig path, and [`deposits_near`] gathers a box's worth so the mesh stamp
//! can test a whole padded shell against a short list.
//!
//! **The unedited rule is the anti-dupe** — see [`super::nodes`].

use crate::block::BlockType;

/// Edge of one deposit grid cell, in blocks. A deposit reaches at most
/// [`MAX_RADIUS`] from its centre, so a query only has to look at the
/// immediately neighbouring grid cells — that is the whole apron argument.
const CELL: i32 = 12;

/// The furthest a deposit's blob reaches from its centre.
pub const MAX_RADIUS: i32 = 2;

/// Salt for the deposit stream, distinct from decoration's families (the
/// tree, landmark, boulder and bush streams) so ore and trees never
/// correlate.
const DEPOSIT_SALT: u64 = 0x0e5e_ee45_1c3d_b10b;

/// The share of grid cells that hold a deposit at all, in 256ths. The
/// other half of the density knob is the blob radius in [`deposit_in`];
/// this one sets how SPREAD OUT veins are, which is what a player feels as
/// "how far do I tunnel between finds". `ore_density_stays_in_a_playable_band`
/// measures what falls out of the pair and reports it.
const DEPOSIT_IN_256: u64 = 200;

/// The tiers, shallowest-first: `(block, floor, ceiling, weight)` — a deposit's CENTRE rolls a
/// tier whose band contains its y, weighted among the bands open there. Every band has a FLOOR as
/// well as a ceiling and adjacent bands overlap only at their seams, so **depth predicts tier**;
/// bands that ran to bedrock made the bottom of the world mostly first-tier ore again. Leystone
/// rides this same stream and manifests in pristine Stone as the ores do, but reveals on ALCHEMY.
const TIERS: [(BlockType, i32, i32, u64); 5] = [
	(BlockType::Leystone, 96, 200, 10),
	(BlockType::Emberore, 64, 140, 8),
	(BlockType::Duskstone, 28, 96, 4),
	(BlockType::Glowvein, 8, 56, 2),
	(BlockType::Aetherock, 1, 24, 1),
];

/// One deposit: a centre and the squared radius of its blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Deposit {
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pub ore: BlockType,
	/// Squared block radius — compared against the squared distance, so
	/// the whole test stays integer.
	pub r2: i32,
}

impl Deposit {
	/// Is this cell inside the blob? The rim is jittered per cell so a
	/// pocket reads as broken rock rather than a sphere someone drew.
	#[inline]
	pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
		let (dx, dy, dz) = (x - self.x, y - self.y, z - self.z);
		let d2 = dx * dx + dy * dy + dz * dz;
		if d2 > self.r2 {
			return false;
		}
		// Interior cells are always in; only the outermost shell rolls,
		// which is what keeps a pocket connected.
		d2 < self.r2 || rim_open(x, y, z)
	}
}

/// The per-cell rim roll. A pure function of the CELL and nothing else —
/// which is exactly what makes two overlapping deposits agree about a
/// shared rim cell instead of each drawing its own edge through it.
#[inline]
fn rim_open(x: i32, y: i32, z: i32) -> bool {
	mix(x, y, z, 0, RIM_SALT) & 3 != 0
}

const RIM_SALT: u64 = 0x5eed_1e55_1a17_e5e5;

/// Salt for the Aether-trace stream, distinct from the deposit and rim
/// streams so a rich cell correlates neither with where a pocket sits nor
/// with the shape of its edge.
const TRACE_SALT: u64 = 0xae74_e7ac_e51f_7b0d;

/// The share of a tracing tier's cells that carry Aether, in 256ths — the same currency as
/// [`DEPOSIT_IN_256`], and deliberately a RATE rather than another line in the tier's yield: the
/// smallest whole unit an essence split can express is already far more Aether than a middle-tier
/// seam should be worth, so the only way to price this below a unit per draw is as a chance of one.
const TRACE_IN_256: u64 = 13;

/// Splitmix64 over a 3D cell + the world seed, salted per stream — the three-axis sibling of
/// `decoration::hash`. Each axis rides its own odd multiplier so a change on any one of them
/// reaches every output bit. `pub(crate)` so every per-cell stream draws from the same mixer with
/// a salt of its own — the surface nodes in [`super::nodes`], the grass sweep's sun-ray jitter in
/// `worldjob::grass` — rather than each copying the magic multipliers.
#[inline]
pub(crate) fn mix(cx: i32, cy: i32, cz: i32, seed_bits: u64, salt: u64) -> u64 {
	let mut h = u64::from(cx as u32).wrapping_mul(0x9e37_79b9_7f4a_7c15);
	h ^= u64::from(cy as u32).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
	h ^= u64::from(cz as u32).wrapping_mul(0x1656_67b1_9e37_79f9);
	h ^= seed_bits ^ salt;
	h ^= h >> 30;
	h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
	h ^= h >> 27;
	h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
	h ^ (h >> 31)
}

/// The deposit seeded in one grid cell, if any.
fn deposit_in(gx: i32, gy: i32, gz: i32, seed: f64) -> Option<Deposit> {
	let h = mix(gx, gy, gz, seed.to_bits(), DEPOSIT_SALT);
	if h & 0xFF >= DEPOSIT_IN_256 {
		return None;
	}
	// Centre anywhere in the cell. A byte reduced mod the cell edge is
	// very slightly biased toward the low offsets, which is the kind of
	// thing nobody can see in a vein.
	let x = gx * CELL + ((h >> 8) & 0xFF) as i32 % CELL;
	let y = gy * CELL + ((h >> 16) & 0xFF) as i32 % CELL;
	let z = gz * CELL + ((h >> 24) & 0xFF) as i32 % CELL;
	// The world floor is undiggable, and a vein you can see and never
	// reach is worse than no vein — so the whole BLOB has to clear it,
	// not just the centre. Guarding the centre alone left a deposit
	// seeded just above the floor reaching [`MAX_RADIUS`] below itself
	// into the bedrock course, where `column_block` puts ordinary Stone
	// (there is no bedrock block; the floor is positional). The reveal
	// stamped it, the seam glittered in the bottom layer of the world, and
	// `dig_hardness` then refused every hold on it forever — with
	// `dig_mode` answering NeedsSiphon, which sends a player off to
	// craft a tool for a block that can never break.
	if y - MAX_RADIUS <= crate::block::BEDROCK_Y {
		return None;
	}
	let ore = tier_at(y, h >> 32)?;
	// Bigger pockets are rarer, so a rich find stays a find.
	let r2 = match (h >> 40) & 7 {
		0..=4 => 1,
		5 | 6 => 2,
		_ => MAX_RADIUS * MAX_RADIUS,
	};
	Some(Deposit { x, y, z, ore, r2 })
}

/// Which tier a deposit at height `y` rolls, given a hash stream — among
/// the bands whose floor-to-ceiling span contains `y`.
fn tier_at(y: i32, roll: u64) -> Option<BlockType> {
	let open = |&&(_, lo, hi, _): &&(BlockType, i32, i32, u64)| (lo..=hi).contains(&y);
	let total: u64 = TIERS.iter().filter(open).map(|&(_, _, _, w)| w).sum();
	if total == 0 {
		return None;
	}
	let mut pick = roll % total;
	for t in TIERS.iter().filter(|t| open(t)) {
		let (ore, _, _, w) = *t;
		if pick < w {
			return Some(ore);
		}
		pick -= w;
	}
	None
}

/// Every deposit whose blob can reach the inclusive block box — the apron gather, for callers
/// testing many cells (the mesh stamp). Returned in grid order, which is deterministic and
/// therefore safe to use for tie-breaking: overlapping deposits resolve to the FIRST in this list,
/// and both consumers walk it the same way.
pub fn deposits_near(min: (i32, i32, i32), max: (i32, i32, i32), seed: f64) -> Vec<Deposit> {
	let mut out = Vec::new();
	for_each_deposit(min, max, seed, |d| {
		out.push(d);
		None::<()>
	});
	out
}

/// The one grid walk both entry points share, so "grid order" means the
/// same thing to each of them. Stops early when `f` answers.
#[inline]
fn for_each_deposit<T>(
	min: (i32, i32, i32),
	max: (i32, i32, i32),
	seed: f64,
	mut f: impl FnMut(Deposit) -> Option<T>,
) -> Option<T> {
	let lo = |v: i32| (v - MAX_RADIUS).div_euclid(CELL);
	let hi = |v: i32| (v + MAX_RADIUS).div_euclid(CELL);
	for gy in lo(min.1)..=hi(max.1) {
		for gz in lo(min.2)..=hi(max.2) {
			for gx in lo(min.0)..=hi(max.0) {
				if let Some(d) = deposit_in(gx, gy, gz, seed)
					&& let Some(t) = f(d)
				{
					return Some(t);
				}
			}
		}
	}
	None
}

/// The ore a single cell would hold if it were pristine Stone — the
/// placement half of §5.3, with no visibility rule and no edit rule.
/// Callers want [`revealed`] almost always.
pub fn ore_at(x: i32, y: i32, z: i32, seed: f64) -> Option<BlockType> {
	// Walked rather than gathered: this runs per dig action and, in the
	// tests, hundreds of thousands of times — a Vec per cell would be the
	// whole cost of the function.
	for_each_deposit((x, y, z), (x, y, z), seed, |d| {
		d.contains(x, y, z).then_some(d.ore)
	})
}

/// Does this vein cell carry a trace of Aether, on top of whatever its tier pays
/// (`alchemy::field_yield`)? Pure in the cell and the world seed, exactly like [`rim_open`] — so
/// the server, the engine's offline arm and a rejoin a year later all agree about which seams were
/// rich, with nothing stored and nothing on the wire. **Duskstone alone traces**: it is the luck
/// beside the ladder where the Resonant Pick asks for Aether, not a supply anyone can plan around.
pub fn traces_aether(ore: BlockType, x: i32, y: i32, z: i32, seed: f64) -> bool {
	ore == BlockType::Duskstone && mix(x, y, z, seed.to_bits(), TRACE_SALT) & 0xFF < TRACE_IN_256
}

/// The four MINING tiers — the deposits [`crate::unlock::reveal`] gates.
/// Leystone rides the same stream but is a surface node revealed by
/// Alchemy; the full live roster is [`super::nodes::NODES`].
pub const ORES: [BlockType; 4] = [
	BlockType::Emberore,
	BlockType::Duskstone,
	BlockType::Glowvein,
	BlockType::Aetherock,
];

/// No deposit's centre sits above this, so a chunk that starts above it
/// can skip the whole DEPOSIT half of the reveal stamp. (A blob reaches
/// [`MAX_RADIUS`] past its centre, which the callers' box already covers.)
pub const MAX_DEPOSIT_Y: i32 = {
	let mut max = i32::MIN;
	let mut i = 0;
	while i < TIERS.len() {
		if TIERS[i].2 > max {
			max = TIERS[i].2;
		}
		i += 1;
	}
	max
};

/// The lowest Mining level at which ANY tier is visible — below it the
/// stamp has nothing to do at all, which is the cheap early-out for every
/// player who has not reached the first reveal.
pub fn lowest_reveal() -> u32 {
	ORES.iter()
		.filter_map(|&o| crate::unlock::reveal(o))
		.min()
		.expect("four tiers")
}

/// The same question on the other axis: the lowest ALCHEMY level at which any tier in the deposit
/// stream is visible. Its twin above walks [`ORES`] because Mining's tiers are exactly that list;
/// this one walks [`TIERS`], because the stream also carries surface nodes [`ORES`] excludes.
/// Derived rather than naming the one tier that qualifies today: hardcoded, a second
/// Alchemy-revealed tier would be a find you can see and cannot extract. [`u32::MAX`] if none.
pub fn lowest_alchemy_reveal() -> u32 {
	TIERS
		.iter()
		.filter_map(|&(b, ..)| crate::unlock::alchemy_reveal(b))
		.min()
		.unwrap_or(u32::MAX)
}

/// The nearest deposit within the sense's own reach ([`crate::unlock::prospect_range`], which
/// widens with the Mining level) whose tier this miner cannot yet SEE — the prospector's sense
/// (§5.5). `None` below the sense's first rank. Unrevealed on purpose: a sense that also hums at
/// veins already glittering in front of you is noise. It says only "something you have not
/// levelled into is near", never what or exactly where.
pub fn unseen_nearby(at: (i32, i32, i32), seed: f64, mining: u32) -> Option<Deposit> {
	let r = crate::unlock::prospect_range(mining)?;
	let mut best: Option<(i64, Deposit)> = None;
	for d in deposits_near(
		(at.0 - r, at.1 - r, at.2 - r),
		(at.0 + r, at.1 + r, at.2 + r),
		seed,
	) {
		if crate::unlock::reveal(d.ore).is_none_or(|need| mining >= need) {
			continue;
		}
		let (dx, dy, dz) = (
			i64::from(d.x - at.0),
			i64::from(d.y - at.1),
			i64::from(d.z - at.2),
		);
		let d2 = dx * dx + dy * dy + dz * dz;
		if d2 <= i64::from(r) * i64::from(r) && best.is_none_or(|(b, _)| d2 < b) {
			best = Some((d2, d));
		}
	}
	best.map(|(_, d)| d)
}

#[cfg(test)]
mod tests;
