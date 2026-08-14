//! Ore placement (SKILLS-DESIGN §5.3) — **derived, never stored**.
//!
//! This module writes nothing. `build_column` returns exactly the bytes it
//! returned before ore existed, the journal carries no ore cells, and the
//! worldgen golden hash does not move — which is what lets veins appear
//! retroactively in worlds that already exist, and why shipping ore stales
//! nobody's save.
//!
//! A deposit is a salted position hash over a coarse grid, judged by pure
//! per-cell functions — the [`crate::decoration::structures_near`] pattern
//! in three dimensions. Every chunk within a deposit's reach computes the
//! identical deposit independently, so pockets span chunk borders with no
//! pipeline state and no generation-order dependence.
//!
//! Two consumers, two shapes, and they must agree (pinned):
//!
//! - [`ore_at`] answers ONE cell. The dig path asks it once per action.
//! - [`deposits_near`] gathers a box's worth of deposits so a caller can
//!   test thousands of cells against a short list. The mesh stamp asks it
//!   once per chunk, because 27 hashes per cell over a 18³ shell is a
//!   millisecond of a worker's frame and a list walk is nothing.
//!
//! **The unedited rule is the anti-dupe, and it is load-bearing.** Ore
//! manifests only where the cell is still pristine Stone. Without that,
//! placing crafted Stone back into a spent vein coordinate would turn ~29
//! Earth into 18–90 Metal, and Metal has no other source. Both sides can
//! answer it: the server from the journal's per-cell map, the client from
//! the same map streamed to it. `JournalIndex::record` only ever inserts,
//! so "has this cell ever been written" is monotone and cannot un-spend a
//! vein.

use crate::block::BlockType;

/// Edge of one deposit grid cell, in blocks. A deposit reaches at most
/// [`MAX_RADIUS`] from its centre, so a query only has to look at the
/// 3×3×3 neighbourhood — that is the whole apron argument.
const CELL: i32 = 12;

/// The furthest a deposit's blob reaches from its centre.
pub const MAX_RADIUS: i32 = 2;

/// Salt for the deposit stream, distinct from decoration's families
/// (tree 0, landmark, boulder, bush) so ore and trees never correlate.
const DEPOSIT_SALT: u64 = 0x0e5e_ee45_1c3d_b10b;

/// How many of 256 grid cells hold a deposit at all. The other half of
/// the density knob is the blob radius in [`deposit_in`]; this one sets
/// how SPREAD OUT veins are, which is what a player feels as "how far do
/// I tunnel between finds". Measured at ~49 ore cells per 10,000 with
/// these numbers — `ore_density_stays_in_a_playable_band` reports it.
const DEPOSIT_IN_256: u64 = 200;

/// The four tiers, deepest-first, with the greatest depth each may appear
/// at and its share among the tiers available there.
///
/// Bands OVERLAP on purpose. Making each band exclusive would mean a
/// Mining-10 player who tunnels to y = 20 finds nothing at all — every
/// deposit down there would be Aetherock, which they cannot see. Instead
/// every tier stays available all the way down and the deep ones are
/// simply rarer, so digging deeper raises what you MIGHT find without
/// taking away what you can.
const TIERS: [(BlockType, i32, u64); 4] = [
	(BlockType::Emberore, 140, 8),
	(BlockType::Duskstone, 96, 4),
	(BlockType::Glowvein, 56, 2),
	(BlockType::Aetherock, 24, 1),
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

/// Splitmix64 over a 3D cell + the world seed, salted per stream — the
/// three-axis sibling of `decoration::hash`. Each axis rides its own odd
/// multiplier so a change on any one of them reaches every output bit.
#[inline]
fn mix(cx: i32, cy: i32, cz: i32, seed_bits: u64, salt: u64) -> u64 {
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
	// Centre anywhere in the cell. A byte reduced mod 12 is very slightly
	// biased toward the low four offsets, which is the kind of thing
	// nobody can see in a vein.
	let x = gx * CELL + ((h >> 8) & 0xFF) as i32 % CELL;
	let y = gy * CELL + ((h >> 16) & 0xFF) as i32 % CELL;
	let z = gz * CELL + ((h >> 24) & 0xFF) as i32 % CELL;
	// The world floor is undiggable, and a vein you can see and never
	// reach is worse than no vein.
	if y <= crate::block::BEDROCK_Y {
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

/// Which tier a deposit at height `y` rolls, given a hash stream.
fn tier_at(y: i32, roll: u64) -> Option<BlockType> {
	let total: u64 = TIERS
		.iter()
		.filter(|&&(_, max_y, _)| y <= max_y)
		.map(|&(_, _, w)| w)
		.sum();
	if total == 0 {
		return None;
	}
	let mut pick = roll % total;
	for &(ore, max_y, w) in &TIERS {
		if y > max_y {
			continue;
		}
		if pick < w {
			return Some(ore);
		}
		pick -= w;
	}
	None
}

/// Every deposit whose blob can reach the inclusive block box — the
/// apron gather, for callers testing many cells (the mesh stamp).
///
/// Returned in grid order, which is deterministic and therefore safe to
/// use for tie-breaking: overlapping deposits resolve to the FIRST in this
/// list, and both consumers walk it the same way.
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
				if let Some(d) = deposit_in(gx, gy, gz, seed) {
					if let Some(t) = f(d) {
						return Some(t);
					}
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

/// The Mining level a tier becomes visible at lives in
/// [`crate::unlock::reveal`] with every other threshold; this is only the
/// roster it is asked about.
pub const ORES: [BlockType; 4] = [
	BlockType::Emberore,
	BlockType::Duskstone,
	BlockType::Glowvein,
	BlockType::Aetherock,
];

/// No deposit's centre sits above this, so a chunk that starts above it
/// can skip the whole reveal stamp. (A blob reaches [`MAX_RADIUS`] past
/// its centre, which the callers' box already covers.)
pub const MAX_DEPOSIT_Y: i32 = TIERS[0].1;

/// The lowest Mining level at which ANY tier is visible — below it the
/// stamp has nothing to do at all, which is the cheap early-out for every
/// player who has not reached the first reveal.
pub fn lowest_reveal() -> u32 {
	ORES.iter()
		.filter_map(|&o| crate::unlock::reveal(o))
		.min()
		.expect("four tiers")
}

/// THE visibility question, asked identically by the mesh stamp, the
/// client's dig prediction and the server's validation: what ore does this
/// cell show to a miner of this level?
///
/// `cell` is the world's CURRENT cell value and `edited` says whether that
/// coordinate has ever been written. Both gates matter and neither is
/// redundant: the id test is what makes a spent vein (now DepletedStone)
/// stay spent, and the edit test is what stops someone placing clean Stone
/// back into the coordinate to regrow it.
pub fn revealed(
	cell: u16,
	at: (i32, i32, i32),
	seed: f64,
	mining: u32,
	edited: bool,
) -> Option<BlockType> {
	if edited || crate::chunk::cell_id(cell) != BlockType::Stone as u16 {
		return None;
	}
	let ore = ore_at(at.0, at.1, at.2, seed)?;
	(mining >= crate::unlock::reveal(ore)?).then_some(ore)
}

/// What a dig on one cell actually IS, for this player with this tool in
/// hand (SKILLS-DESIGN §5.3). The one question the client's prediction and
/// the server's validation both ask, so they cannot disagree about whether
/// an action was a dig, a draw, or a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DigMode {
	/// Ordinary: the cell breaks and drops what it drops.
	Break,
	/// A revealed vein with the siphon in hand: the cell becomes
	/// [`BlockType::DepletedStone`] and the ore lands in the inventory.
	/// Mining ore never excavates, so the mountainside keeps its shape.
	Extract(BlockType),
	/// A revealed vein and no siphon. Refused at `DigStart`, not at the
	/// completing edit — if you can SEE the seam, the game will not let
	/// you spend it on a stone dig by accident, and it says so before you
	/// hold the button through a two-second timer for nothing.
	NeedsSiphon,
}

pub fn dig_mode(
	cell: u16,
	at: (i32, i32, i32),
	seed: f64,
	mining: u32,
	edited: bool,
	active_tool: BlockType,
) -> DigMode {
	match revealed(cell, at, seed, mining, edited) {
		// Below the reveal there is nothing here to protect: that player's
		// client and server agree the cell is stone, so it digs as stone
		// and the vein is spent. You dig what you can see.
		None => DigMode::Break,
		Some(ore) if active_tool == BlockType::EssenceSiphon => DigMode::Extract(ore),
		Some(_) => DigMode::NeedsSiphon,
	}
}

/// The nearest deposit within [`crate::unlock::PROSPECT_RANGE`] whose
/// tier this miner cannot yet SEE — the prospector's sense (§5.5).
///
/// Unrevealed on purpose: a sense that also hums at veins already
/// glittering in front of you is noise. This is the one hint the game
/// gives about what is behind the wall, and it says only "something you
/// have not levelled into is near", never what or exactly where.
pub fn unseen_nearby(at: (i32, i32, i32), seed: f64, mining: u32) -> Option<Deposit> {
	let r = crate::unlock::PROSPECT_RANGE;
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

/// The block a dig is TIMED and CREDITED as. A vein lives in a cell that
/// still holds Stone — the reveal is a mesh stamp, not a world write — so
/// without this an extraction would take a stone's 90 ticks and pay a
/// stone's XP however deep the seam was, and the ore rows' hardnesses
/// would be dead numbers on blocks nobody ever digs.
#[inline]
pub fn dig_block(mode: DigMode, cell_block: BlockType) -> BlockType {
	match mode {
		DigMode::Extract(ore) => ore,
		_ => cell_block,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::make_cell;

	/// Every ore cell in a slab of world, in a deterministic walk — the
	/// shared finder behind the pins below. A single z PLANE is thin
	/// enough that a narrow x window can hold no ore at all, which is how
	/// the first draft of these tests came to pass vacuously.
	fn ore_cells(seed: f64) -> Vec<(i32, i32, i32, BlockType)> {
		let mut out = Vec::new();
		for y in 1..=140 {
			for z in 0..8 {
				for x in -64..64 {
					if let Some(o) = ore_at(x, y, z, seed) {
						out.push((x, y, z, o));
					}
				}
			}
		}
		out
	}

	/// The two shapes must answer the same question — the mesh stamp uses
	/// the gathered list and the dig path uses the single-cell call, and a
	/// disagreement is a vein you can see and cannot extract.
	#[test]
	fn the_gather_and_the_single_cell_query_agree() {
		let seed = 42.0;
		let (min, max) = ((-20, 10, -20), (20, 60, 20));
		let list = deposits_near(min, max, seed);
		for y in min.1..=max.1 {
			for z in min.2..=max.2 {
				for x in min.0..=max.0 {
					let from_list = list.iter().find(|d| d.contains(x, y, z)).map(|d| d.ore);
					assert_eq!(from_list, ore_at(x, y, z, seed), "({x},{y},{z})");
				}
			}
		}
	}

	/// Pure in the way the whole module claims to be: same inputs, same
	/// answer, no matter what order anyone asks in.
	#[test]
	fn placement_is_a_pure_function_of_position_and_seed() {
		for &seed in &[0.0, 42.0, -1.5, 1e9] {
			for (x, y, z) in [(0, 40, 0), (137, 12, -991), (-4, 139, 7)] {
				let a = ore_at(x, y, z, seed);
				assert_eq!(a, ore_at(x, y, z, seed));
			}
		}
		// A different seed is a different world.
		let a: Vec<_> = (0..400).filter_map(|i| ore_at(i, 30, 0, 1.0)).collect();
		let b: Vec<_> = (0..400).filter_map(|i| ore_at(i, 30, 0, 2.0)).collect();
		assert_ne!(a, b, "the seed reaches the deposits");
	}

	/// Golden per-seed positions, in the mould of `spawn_positions_are_pinned`:
	/// a test that only asks whether ore MOVED cannot catch ore that moved
	/// somewhere bad, so this pins where it actually is.
	#[test]
	fn ore_positions_are_pinned() {
		let found: Vec<_> = ore_cells(42.0).into_iter().take(6).collect();
		use BlockType as B;
		assert_eq!(
			found,
			vec![
				(-19, 1, 3, B::Emberore),
				(-20, 1, 4, B::Emberore),
				(-19, 1, 4, B::Emberore),
				(-18, 1, 4, B::Emberore),
				(-19, 1, 5, B::Emberore),
				(-19, 2, 4, B::Emberore),
			]
		);
	}

	/// Depth bands: every tier stays reachable all the way down (the
	/// overlap that keeps a low-level miner in business at y = 20), but
	/// none appears above its own ceiling.
	#[test]
	fn tiers_respect_their_depth_bands() {
		let seed = 7.0;
		let mut seen = [0u32; 4];
		for gy in 0..24 {
			for gz in -40..40 {
				for gx in -40..40 {
					let Some(d) = deposit_in(gx, gy, gz, seed) else {
						continue;
					};
					let (i, &(_, max_y, _)) = TIERS
						.iter()
						.enumerate()
						.find(|(_, &(o, _, _))| o == d.ore)
						.expect("a listed tier");
					assert!(d.y <= max_y, "{:?} at y {}", d.ore, d.y);
					assert!(d.y > crate::block::BEDROCK_Y, "never in the floor");
					seen[i] += 1;
				}
			}
		}
		for (i, &(ore, _, _)) in TIERS.iter().enumerate() {
			assert!(seen[i] > 0, "{ore:?} never appears");
		}
		// Rarer with depth, in the order the tier weights declare.
		assert!(
			seen[0] > seen[1] && seen[1] > seen[2] && seen[2] > seen[3],
			"{seen:?}"
		);
	}

	/// The anti-dupe, both halves. A vein is spent by the write that took
	/// it, and cannot be regrown by putting stone back.
	#[test]
	fn ore_never_manifests_in_an_edited_cell() {
		let seed = 42.0;
		let stone = BlockType::Stone as u16;
		let (x, y, z, _) = ore_cells(seed)[0];
		let max = crate::skills::MAX_LEVEL;
		assert!(
			revealed(stone, (x, y, z), seed, max, false).is_some(),
			"pristine"
		);
		// Somebody put stone back where the vein was.
		assert_eq!(revealed(stone, (x, y, z), seed, max, true), None, "edited");
		// And the tapped face itself never re-reads as ore.
		let spent = BlockType::DepletedStone as u16;
		assert_eq!(revealed(spent, (x, y, z), seed, max, false), None);
		assert_eq!(revealed(spent, (x, y, z), seed, max, true), None);
		// Nor does anything else standing in that coordinate.
		for b in [BlockType::Air, BlockType::Rubble, BlockType::Emberore] {
			assert_eq!(
				revealed(b as u16, (x, y, z), seed, max, false),
				None,
				"{b:?}"
			);
		}
		// A shaped cell in the same coordinate is not stone either, state
		// nibble or no state nibble.
		assert_eq!(
			revealed(
				make_cell(BlockType::StoneSlab, 1),
				(x, y, z),
				seed,
				max,
				false
			),
			None
		);
	}

	/// You dig what you can see: below a tier's reveal level the cell is
	/// stone to that player, and stays stone through the dig.
	#[test]
	fn a_tier_is_invisible_below_its_level() {
		let seed = 42.0;
		let stone = BlockType::Stone as u16;
		// Found through the DEPOSITS rather than by sweeping cells: a
		// deposit's centre is always inside its own blob, and Aetherock is
		// one deposit in fifteen in the bottom two grid rows — rare enough
		// that a cell sweep small enough to run in a test can honestly
		// miss it, which is how this test first "passed" by finding
		// nothing at all.
		for &ore in &ORES {
			let need = crate::unlock::reveal(ore).expect("an ore");
			let d = deposits_near((-400, 1, -400), (400, 140, 400), seed)
				.into_iter()
				.find(|d| d.ore == ore)
				.unwrap_or_else(|| panic!("no {ore:?} anywhere in the sampled world"));
			let (x, y, z) = (d.x, d.y, d.z);
			assert_eq!(
				ore_at(x, y, z, seed),
				Some(ore),
				"the centre is in the blob"
			);
			assert_eq!(revealed(stone, (x, y, z), seed, need - 1, false), None);
			assert_eq!(revealed(stone, (x, y, z), seed, need, false), Some(ore));
		}
	}

	/// The prospector's sense hears only what it CANNOT see: a vein
	/// already glittering in front of the player is not a hint, and a
	/// sense that hummed at one would be noise on every screen it
	/// mattered on.
	#[test]
	fn the_prospectors_sense_is_deaf_to_what_it_can_already_see() {
		let seed = 42.0;
		let deep = deposits_near((-400, 1, -400), (400, 56, 400), seed)
			.into_iter()
			.find(|d| d.ore == BlockType::Glowvein)
			.expect("a glowvein");
		let at = (deep.x, deep.y, deep.z);
		let need = crate::unlock::reveal(BlockType::Glowvein).unwrap();
		assert_eq!(
			unseen_nearby(at, seed, need - 1).map(|d| d.ore),
			Some(BlockType::Glowvein),
			"heard below its reveal"
		);
		assert!(
			unseen_nearby(at, seed, crate::skills::MAX_LEVEL).is_none(),
			"silent once every tier is visible"
		);
		// And it does not reach past its own range.
		let far = (at.0 + crate::unlock::PROSPECT_RANGE * 4, at.1, at.2);
		let heard = unseen_nearby(far, seed, need - 1);
		assert!(
			heard.is_none_or(|d| {
				let dx = i64::from(d.x - far.0);
				let dy = i64::from(d.y - far.1);
				let dz = i64::from(d.z - far.2);
				dx * dx + dy * dy + dz * dz <= i64::from(crate::unlock::PROSPECT_RANGE).pow(2)
			}),
			"heard something out of range"
		);
	}

	/// The density knob, reported rather than merely bounded — the rates
	/// players remember get set here, and §11 says to soak-test them.
	#[test]
	fn ore_density_stays_in_a_playable_band() {
		let seed = 42.0;
		let (span, top) = (96i64, 140i64);
		let mut ore = 0i64;
		let list = deposits_near((0, 1, 0), (span as i32, top as i32, span as i32), seed);
		for y in 1..=top {
			for z in 0..span {
				for x in 0..span {
					if list
						.iter()
						.any(|d| d.contains(x as i32, y as i32, z as i32))
					{
						ore += 1;
					}
				}
			}
		}
		let cells = span * span * top;
		let per_10k = ore * 10_000 / cells;
		assert!(
			(20..=90).contains(&per_10k),
			"{per_10k} ore per 10k cells is outside the playable band ({ore} of {cells})"
		);
	}
}
