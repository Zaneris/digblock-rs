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
//!   once per chunk, because a hash per neighbouring grid cell over a
//!   whole padded shell is a millisecond of a worker's frame and a list
//!   walk is nothing.
//!
//! **The unedited rule is the anti-dupe, and it is load-bearing.** Ore
//! manifests only where the cell is still pristine Stone. Without that,
//! placing crafted Stone back into a spent vein coordinate would turn one
//! block's Earth into a vein's worth of Metal, and Metal has no other
//! source. Both sides can answer it: the server from the journal's
//! per-cell map, the client from the same map streamed to it.
//! `JournalIndex::record` only ever inserts, so "has this cell ever been
//! written" is monotone and cannot un-spend a vein.

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

/// The four tiers, deepest-first, with the greatest depth each may appear
/// at and its share among the tiers available there.
///
/// Bands OVERLAP on purpose. Making each band exclusive would mean a
/// player who tunnels deep with only the shallow tiers revealed finds
/// nothing at all — every deposit down there would be Aetherock, which
/// they cannot see. Instead every tier stays available all the way down
/// and the deep ones are simply rarer, so digging deeper raises what you
/// MIGHT find without taking away what you can.
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

/// Salt for the Aether-trace stream, distinct from the deposit and rim
/// streams so a rich cell correlates neither with where a pocket sits nor
/// with the shape of its edge.
const TRACE_SALT: u64 = 0xae74_e7ac_e51f_7b0d;

/// The share of a tracing tier's cells that carry Aether, in 256ths — the
/// same currency as [`DEPOSIT_IN_256`], and deliberately a RATE rather
/// than another line in the tier's yield: the smallest whole unit an
/// essence split can express is already far more Aether than a middle-tier
/// seam should be worth, so the only way to price this below a unit per
/// draw is to price it as a chance of one.
const TRACE_IN_256: u64 = 13;

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

/// Does this vein cell carry a trace of Aether, on top of whatever its
/// tier pays (`alchemy::field_yield`)?
///
/// Pure in the cell and the world seed, exactly like [`rim_open`] and the
/// deposit stream above it — so the server, the engine's offline arm and
/// a rejoin a year later all agree about which seams were rich, with
/// nothing stored, nothing on the wire and nothing to keep in step.
/// Rolling it per draw instead would put the answer in whichever machine
/// happened to ask, which for a value the server credits is no answer at
/// all.
///
/// **Duskstone alone traces**, and that is the whole design of it. Aether
/// is otherwise an endgame currency: the deepest tier is the only vein
/// that reliably carries it and the foliage lines that trickle it sit near
/// the top of the Alchemy ladder, while the Resonant Pick asks for some at
/// the middle of it. [`crate::unlock::reveal`] puts a reliable supply at
/// the level the pick's own swing widens; this is the luck beside that
/// ladder — the chance that a player who has been working the middle tier
/// all along walks up to the bench already holding what the pick wants,
/// rather than a supply anyone can plan around.
pub fn traces_aether(ore: BlockType, x: i32, y: i32, z: i32, seed: f64) -> bool {
	ore == BlockType::Duskstone && mix(x, y, z, seed.to_bits(), TRACE_SALT) & 0xFF < TRACE_IN_256
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
	/// A revealed vein with the siphon in hand: the cell becomes the
	/// tier's [`spent_rock`] and the vein's ESSENCE lands in the player's
	/// stocks. Mining ore never excavates, so the mountainside keeps its
	/// shape.
	///
	/// Essence rather than an ore block, which is the whole of what the
	/// tool is for: a siphon draws a seam out where it stands, so a vein
	/// never becomes something you carry to a bench. What you can carry
	/// home is the spent rock afterwards. The yield is the ore's ordinary
	/// `alchemy::extract_yields`, so the Alchemy level scales a field draw
	/// exactly as it scales the bench.
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

/// The block a dig is TIMED and paid XP as. A vein lives in a cell that
/// still holds Stone — the reveal is a mesh stamp, not a world write — so
/// without this an extraction would take a stone's own hold time and pay
/// a stone's XP however deep the seam was, and the ore rows' hardnesses
/// would be dead numbers on blocks nobody ever digs.
///
/// It no longer decides what the dig CREDITS: a field extraction pays
/// essence, not ore (see [`DigMode::Extract`]).
#[inline]
pub fn dig_block(mode: DigMode, cell_block: BlockType) -> BlockType {
	match mode {
		DigMode::Extract(ore) => ore,
		_ => cell_block,
	}
}

/// The rock a tapped vein leaves standing — the tier's own spent face.
///
/// One per tier rather than one grey stone for all four: a worked seam
/// should still read as the seam you worked, and the block you break out
/// of it afterwards is a trophy of that tier and no other. It is also the
/// ONLY thing a vein ever puts in a player's hands, since the ore itself
/// is never an item ([`BlockType::is_slottable`]).
///
/// Total on the ores, so a caller that has a [`DigMode::Extract`] cannot
/// be handed one it has no rock for; anything else is not a vein and
/// answers `None`.
#[inline]
pub fn spent_rock(ore: BlockType) -> Option<BlockType> {
	Some(match ore {
		BlockType::Emberore => BlockType::DepletedEmberore,
		BlockType::Duskstone => BlockType::DepletedDuskstone,
		BlockType::Glowvein => BlockType::DepletedGlowvein,
		BlockType::Aetherock => BlockType::DepletedAetherock,
		_ => return None,
	})
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
	///
	/// These numbers moved once, deliberately, and the old set is worth
	/// recording because it was pinning a bug rather than guarding
	/// against one: nearly every cell in it sat in the first course above
	/// the world floor, i.e. the first thing this golden ever described
	/// was a vein packed against the undiggable floor. `deposit_in` guarded
	/// only the deposit's CENTRE, so a blob reached [`MAX_RADIUS`] further down
	/// into cells no dig can ever open. Everything here is now clear of
	/// `BEDROCK_Y + MAX_RADIUS`, which
	/// `no_vein_reaches_the_undiggable_floor` enforces in general.
	#[test]
	fn ore_positions_are_pinned() {
		let found: Vec<_> = ore_cells(42.0).into_iter().take(6).collect();
		use BlockType as B;
		assert_eq!(
			found,
			vec![
				(46, 3, 6, B::Emberore),
				(47, 3, 6, B::Emberore),
				(48, 3, 6, B::Emberore),
				(46, 4, 5, B::Emberore),
				(47, 4, 5, B::Emberore),
				(48, 4, 5, B::Emberore),
			]
		);
		// The floor rule, restated against the golden itself, so a future
		// re-pin cannot quietly walk these back down into the floor.
		for &(_, y, _, _) in &found {
			assert!(y > crate::block::BEDROCK_Y + MAX_RADIUS, "pinned at y {y}");
		}
	}

	/// No ore is ever placed where no dig can reach it.
	///
	/// Asked of the CELLS rather than the deposits, because that is the
	/// property that matters and it is the one the centre-only guard
	/// missed: `deposit_in` refused a centre at or below the floor, but
	/// a blob reaches [`MAX_RADIUS`] further down, so a deposit seeded just
	/// above it put ore IN it. There is no bedrock block — `column_block`
	/// fills the floor with ordinary Stone — so the reveal saw a pristine
	/// Stone cell and stamped it, and `dig_hardness` then refused every
	/// hold on the seam forever.
	///
	/// Swept over several seeds because the failure is a hash away: one
	/// seed's grid can easily have no deposit low enough to expose it.
	#[test]
	fn no_vein_reaches_the_undiggable_floor() {
		use crate::block::BEDROCK_Y;
		let mut low = 0u32;
		for &seed in &[7.0f64, -3.5, 1234.5, 0.0, 98765.25] {
			// Every y a blob could touch at or below the floor, plus the
			// first diggable layer as the positive control.
			for y in (BEDROCK_Y - MAX_RADIUS)..=(BEDROCK_Y + 1) {
				for z in -60..60 {
					for x in -60..60 {
						let found = ore_at(x, y, z, seed);
						if y <= BEDROCK_Y {
							assert!(
								found.is_none(),
								"{found:?} at {x},{y},{z} (seed {seed}) — the floor \
								 is undiggable, so this seam could never be mined"
							);
						} else if found.is_some() {
							low += 1;
						}
					}
				}
			}
		}
		// Anti-vacuity: the sweep has to actually reach ore-bearing
		// depth, or "nothing in the floor" is true because there is
		// nothing anywhere.
		assert!(
			low > 0,
			"no ore found at y = {} either — the sweep proves nothing",
			BEDROCK_Y + 1
		);
	}

	/// Depth bands: every tier stays reachable all the way down (the
	/// overlap that keeps a low-level miner in business deep down), but
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
						.find(|&(_, &(o, _, _))| o == d.ore)
						.expect("a listed tier");
					assert!(d.y <= max_y, "{:?} at y {}", d.ore, d.y);
					// The BLOB, not the centre: a deposit reaches
					// MAX_RADIUS below itself, and the cell it would
					// reach into is an ordinary Stone the floor rule
					// makes undiggable forever.
					assert!(
						d.y - MAX_RADIUS > crate::block::BEDROCK_Y,
						"{:?} centred at y {} reaches the floor",
						d.ore,
						d.y
					);
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

	/// Every tier has a spent rock, and nothing else does.
	///
	/// Totality is what makes the two `expect`s at the extraction sites
	/// honest — the client's prediction and the server's write both ask
	/// this and both would panic on a `None`, and they must agree about
	/// the cell or the two ends draw different rock at the same
	/// coordinate. The reverse half matters just as much: a spent rock
	/// that answered to `spent_rock` in its own right would be a vein you
	/// could tap twice.
	#[test]
	fn every_tier_has_a_spent_rock_and_nothing_else_does() {
		let mut spent = Vec::new();
		for &ore in &ORES {
			let rock = spent_rock(ore).unwrap_or_else(|| panic!("{ore:?} taps out into nothing"));
			assert_ne!(rock, ore, "{ore:?} must not tap out into itself");
			assert!(
				rock.dig_drop() == Some(rock),
				"{rock:?} has to drop ITSELF — it is the only thing a vein \
				 ever puts in a player's hands"
			);
			assert_eq!(rock.props().light, 0, "{rock:?} still casts");
			assert!(!rock.props().emissive, "{rock:?} still glows");
			spent.push(rock);
		}
		let mut ids: Vec<u16> = spent.iter().map(|&b| b as u16).collect();
		ids.sort_unstable();
		ids.dedup();
		assert_eq!(
			ids.len(),
			spent.len(),
			"two tiers share a spent rock: {spent:?}"
		);
		for &b in &BlockType::ALL {
			assert_eq!(
				spent_rock(b).is_some(),
				ORES.contains(&b),
				"{b:?} disagrees with the ore roster"
			);
		}
	}

	/// A vein is world geometry and never an item.
	///
	/// The siphon pays essence at the rock face, so nothing anywhere
	/// credits an ore block — and `is_slottable` is what makes that
	/// structural rather than a promise: `reconcile_layout` sweeps a
	/// non-slottable type out of bar and grid whatever the count says, so
	/// it is also the migration for saves written while a siphon still
	/// handed ore over.
	#[test]
	fn raw_ore_is_never_an_item() {
		for &ore in &ORES {
			assert!(ore.is_raw_ore(), "{ore:?} is not in the block predicate");
			assert!(!ore.is_slottable(), "{ore:?} can still be held");
			assert!(
				!ore.is_tool(),
				"{ore:?} must be excluded as ore, not as a tool"
			);
		}
		// And the spent rocks are, or there is nothing to carry home.
		for &ore in &ORES {
			let rock = spent_rock(ore).expect("a tier");
			assert!(rock.is_slottable(), "{rock:?} cannot be picked up");
			assert!(!rock.is_raw_ore(), "{rock:?} reads as a live vein");
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
