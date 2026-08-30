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

/// Golden per-seed positions, in the mould of `spawn_positions_are_pinned`: a test that only asks
/// whether ore MOVED cannot catch ore that moved somewhere bad, so this pins where it actually is.
/// Everything here is clear of `BEDROCK_Y + MAX_RADIUS`, which
/// `no_vein_reaches_the_undiggable_floor` enforces in general.
#[test]
fn ore_positions_are_pinned() {
	let found: Vec<_> = ore_cells(42.0).into_iter().take(6).collect();
	use BlockType as B;
	// Re-pinned at the rebanding (floors + the Leystone tier): the same
	// deposit stream, but a pocket at y 3–4 now rolls the DEEPEST tier,
	// which is the whole point — the bottom of the world is never
	// common ore again.
	assert_eq!(
		found,
		vec![
			(46, 3, 6, B::Aetherock),
			(47, 3, 6, B::Aetherock),
			(48, 3, 6, B::Aetherock),
			(46, 4, 5, B::Aetherock),
			(47, 4, 5, B::Aetherock),
			(48, 4, 5, B::Aetherock),
		]
	);
	// The floor rule, restated against the golden itself, so a future
	// re-pin cannot quietly walk these back down into the floor.
	for &(_, y, _, _) in &found {
		assert!(y > crate::block::BEDROCK_Y + MAX_RADIUS, "pinned at y {y}");
	}
}

/// No ore is ever placed where no dig can reach it, asked of the CELLS rather than the deposits
/// because a centre-only guard misses it: `deposit_in` refuses a centre at or below the floor, but
/// a blob reaches [`MAX_RADIUS`] further down. There is no bedrock block — `column_block` fills
/// the floor with ordinary Stone — so the reveal would stamp a pristine cell and `dig_hardness`
/// refuse every hold on the seam forever. Swept over several seeds: the failure is a hash away.
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

/// Depth bands, both edges: no tier appears above its ceiling OR below
/// its floor — depth predicts tier, which is the whole point of the
/// rebanding: a find at the bottom of the world is never common ore.
#[test]
fn tiers_respect_their_depth_bands() {
	let seed = 7.0;
	let mut seen = [0u32; TIERS.len()];
	for gy in 0..24 {
		for gz in -40..40 {
			for gx in -40..40 {
				let Some(d) = deposit_in(gx, gy, gz, seed) else {
					continue;
				};
				let (i, &(_, lo, hi, _)) = TIERS
					.iter()
					.enumerate()
					.find(|&(_, &(o, _, _, _))| o == d.ore)
					.expect("a listed tier");
				assert!(
					(lo..=hi).contains(&d.y),
					"{:?} at y {} outside its band",
					d.ore,
					d.y
				);
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
	for (i, &(ore, _, _, _)) in TIERS.iter().enumerate() {
		assert!(seen[i] > 0, "{ore:?} never appears");
	}
	// The deepest tier stays the rarest find in the world overall.
	assert!(
		seen[TIERS.len() - 1] < seen[0],
		"Aetherock out-numbers the surface tier: {seen:?}"
	);
	// And the bands genuinely exclude: the bottom rows of the world
	// hold no shallow tier at all.
	for gz in -40..40 {
		for gx in -40..40 {
			if let Some(d) = deposit_in(gx, 0, gz, seed) {
				assert!(
					matches!(d.ore, BlockType::Glowvein | BlockType::Aetherock),
					"{:?} at the world floor",
					d.ore
				);
			}
		}
	}
}

/// The anti-dupe, both halves. A vein is spent by the write that took
/// it, and cannot be regrown by putting stone back. (The unified
/// question lives in [`super::super::nodes`]; this exercises it over
/// the DEPOSIT stream specifically.)
#[test]
fn ore_never_manifests_in_an_edited_cell() {
	use crate::worldgen::nodes::{Levels, revealed};
	let seed = 42.0;
	let stone = BlockType::Stone as u16;
	let (x, y, z, _) = ore_cells(seed)[0];
	let max = Levels {
		mining: crate::skills::MAX_LEVEL,
		alchemy: crate::skills::MAX_LEVEL,
		siphon: true,
	};
	assert!(
		revealed(
			stone,
			(x, y, z),
			crate::worldgen::Seed::current(seed),
			max,
			false
		)
		.is_some(),
		"pristine"
	);
	// Somebody put stone back where the vein was.
	assert_eq!(
		revealed(
			stone,
			(x, y, z),
			crate::worldgen::Seed::current(seed),
			max,
			true
		),
		None,
		"edited"
	);
	// And the tapped face itself never re-reads as ore.
	let spent = BlockType::DepletedStone as u16;
	assert_eq!(
		revealed(
			spent,
			(x, y, z),
			crate::worldgen::Seed::current(seed),
			max,
			false
		),
		None
	);
	assert_eq!(
		revealed(
			spent,
			(x, y, z),
			crate::worldgen::Seed::current(seed),
			max,
			true
		),
		None
	);
	// Nor does anything else standing in that coordinate (a live face
	// stored outright is the regrown node, pinned in `nodes::tests`).
	for b in [BlockType::Air, BlockType::Rubble] {
		assert_eq!(
			revealed(
				b as u16,
				(x, y, z),
				crate::worldgen::Seed::current(seed),
				max,
				false
			),
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
			crate::worldgen::Seed::current(seed),
			max,
			false
		),
		None
	);
}

/// You dig what you can see: below a tier's reveal level the cell is
/// stone to that player, and stays stone through the dig. The Mining
/// tiers gate on Mining alone, and the surface tier riding this same
/// stream gates on the siphon alone.
#[test]
fn a_tier_is_invisible_below_its_level() {
	use crate::worldgen::nodes::{Levels, revealed};
	let seed = 42.0;
	let stone = BlockType::Stone as u16;
	// Found through the DEPOSITS rather than by sweeping cells: a
	// deposit's centre is always inside its own blob, and Aetherock is
	// rare enough that a cell sweep small enough to run in a test can
	// honestly miss it, which is how this test first "passed" by
	// finding nothing at all.
	let all = deposits_near((-400, 1, -400), (400, 200, 400), seed);
	for &ore in &ORES {
		let need = crate::unlock::reveal(ore).expect("an ore");
		let d = all
			.iter()
			.find(|d| d.ore == ore)
			.unwrap_or_else(|| panic!("no {ore:?} anywhere in the sampled world"));
		let (x, y, z) = (d.x, d.y, d.z);
		assert_eq!(
			ore_at(x, y, z, seed),
			Some(ore),
			"the centre is in the blob"
		);
		let at = |mining, siphon| {
			revealed(
				stone,
				(x, y, z),
				crate::worldgen::Seed::current(seed),
				Levels {
					mining,
					alchemy: crate::skills::MAX_LEVEL,
					siphon,
				},
				false,
			)
		};
		assert_eq!(at(need - 1, true), None, "{ore:?}");
		assert_eq!(at(need, false), Some(ore), "{ore:?}");
	}
	// Leystone: the same stream, the OTHER gate — the siphon bit, with
	// both skills maxed to prove no level opens it.
	let ley = all
		.iter()
		.find(|d| d.ore == BlockType::Leystone)
		.expect("no leystone in the sampled world");
	let (x, y, z) = (ley.x, ley.y, ley.z);
	assert!(crate::unlock::siphon_reveal(BlockType::Leystone));
	let at = |mining, siphon| {
		revealed(
			stone,
			(x, y, z),
			crate::worldgen::Seed::current(seed),
			Levels {
				mining,
				alchemy: crate::skills::MAX_LEVEL,
				siphon,
			},
			false,
		)
	};
	assert_eq!(at(crate::skills::MAX_LEVEL, false), None, "leystone");
	assert_eq!(at(1, true), Some(BlockType::Leystone), "leystone");
}

/// **Every tier in the stream is reachable by the gate that decides whether the stamp runs at
/// all.** A player who has just met a tier's reveal must get past [`lowest_reveal`] /
/// [`siphon_reveals_deposit`], or the mesh stamp skips the whole deposit pass while
/// `nodes::revealed` reveals the tier anyway — a find you can see and cannot extract. [`ORES`] is
/// the Mining roster and [`TIERS`] the stream, so a gate naming a tier by hand misses the next one.
#[test]
fn the_stamps_gate_admits_every_tier_at_its_own_reveal() {
	for &(tier, ..) in &TIERS {
		let mining = crate::unlock::reveal(tier);
		let siphon = crate::unlock::siphon_reveal(tier);
		assert!(
			mining.is_some() != siphon,
			"{tier:?} must answer to exactly one reveal gate"
		);
		// Nothing on the gate this tier does NOT use: the stamp has to
		// open on the tier's own gate alone.
		let opens = mining.is_some_and(|need| need >= lowest_reveal())
			|| (siphon && siphon_reveals_deposit());
		assert!(
			opens,
			"{tier:?} reveals at a gate the stamp's own early-out refuses"
		);
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
	// And it does not reach past its own range — at the level where the
	// range is widest, since a narrower rank reaching further would
	// only make this looser.
	let wide = crate::unlock::prospect_range(need - 1).expect("the sense is on by then");
	let far = (at.0 + wide * 4, at.1, at.2);
	let heard = unseen_nearby(far, seed, need - 1);
	assert!(
		heard.is_none_or(|d| {
			let dx = i64::from(d.x - far.0);
			let dy = i64::from(d.y - far.1);
			let dz = i64::from(d.z - far.2);
			dx * dx + dy * dy + dz * dz <= i64::from(wide).pow(2)
		}),
		"heard something out of range"
	);
	// The sense itself is ranked: silent before its first rank, and its
	// reach only ever widens with the level.
	assert!(
		unseen_nearby(at, seed, 1).is_none(),
		"heard before the sense arrives"
	);
	let mut last = 0;
	for l in 1..=crate::skills::MAX_LEVEL {
		let r = crate::unlock::prospect_range(l).unwrap_or(0);
		assert!(r >= last, "the sense narrowed at {l}");
		last = r;
	}
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
