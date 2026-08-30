use super::*;

/// §3.2's yield curve and §4.2's waste curve: the relationships a player feels — mastery keeps
/// everything, a novice keeps less, neither curve moves the wrong way, a craft never returns MORE
/// than its ideal — plus derived-output goldens along both. The goldens are the deliberate-pin
/// exemption at work: monotone bounds cannot notice a curve changing SLOPE, and these two ARE the
/// player-facing economy, so a retune moves these rows knowingly and a reshape fails them.
#[test]
fn efficiency_and_waste_hit_their_pinned_endpoints() {
	use crate::skills::MAX_LEVEL;
	const BASES: [u32; 4] = [3, 9, 18, 27];

	// Derived goldens: the novice keeps two fifths, the midpoint seven
	// tenths; waste starts, steps down at the dip, holds its plateau,
	// and resumes the linear tail to the mastery floor.
	for (level, yield100, waste) in [
		(1, 40, 1600),
		(2, 40, 1594),
		(50, 70, 1325),
		(55, 73, 1297),
		(56, 73, 1250),
		(63, 77, 1250),
		(64, 78, 1246),
		(MAX_LEVEL, 100, 1050),
	] {
		assert_eq!(scaled_yield(100, level), yield100, "yield at {level}");
		assert_eq!(waste_permille(level), waste, "waste at {level}");
	}

	for base in BASES {
		assert_eq!(
			scaled_yield(base, MAX_LEVEL),
			base,
			"mastery keeps all of a {base} draw, and never more"
		);
		assert!(
			scaled_yield(base, 1) < base,
			"a novice must waste some of a {base} draw"
		);
		assert!(scaled_yield(base, 1) >= 1, "a draw never rounds to nothing");
	}

	// Waste is an overhead: always at least the ideal, never below it,
	// and strictly cheaper at mastery than at the first level.
	assert!(
		waste_permille(MAX_LEVEL) < waste_permille(1),
		"waste never improves"
	);
	for l in 1..=MAX_LEVEL {
		assert!(
			waste_permille(l) >= 1000,
			"a craft cannot pay back more than ideal"
		);
	}
	// The clamp is a felt STEP: the dip level is cheaper than the linear
	// curve alone would have made it.
	assert!(
		waste_permille(crate::unlock::WASTE_DIP_AT)
			< waste_permille(crate::unlock::WASTE_DIP_AT - 1),
		"the dip must actually dip"
	);

	for l in 2..=MAX_LEVEL {
		assert!(
			waste_permille(l) <= waste_permille(l - 1),
			"waste never rises ({l})"
		);
		for base in BASES {
			assert!(
				scaled_yield(base, l) >= scaled_yield(base, l - 1),
				"yield never falls ({base}@{l})"
			);
		}
	}
}

/// **No craft creates essence** — structural, not a policy: the pool has exactly one faucet
/// ([`field_yield`], reachable only through a siphon draw on a live node) and the bench only ever
/// debits it. Stated rather than trusted here — nothing craftable pays a node yield, so no
/// recipe's output can be turned back into the currency that bought it.
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

/// **Nothing the world hands you may be transmuted**: whatever a dig on a world-placed block puts
/// in a player's hands has no recipe at any price. Checked against the hand-priced TABLE as well
/// as `craft_ideal`, and that half has the teeth — `craft_ideal` gates on `is_bench_only` before
/// it reads anything, so a stale `functional_ideal` row would be silently unreachable rather than
/// wrong. Clean rock passes and is meant to: the world places Stone but hands out Rubble.
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

/// **The essence tool costs no essence, ever** — and neither does the vessel that follows it.
/// Every essence in the game is a siphon draw, so an essence-priced siphon is a currency that
/// cannot be bootstrapped; the bucket keeps the feedstock-only price anyway, because its rung is
/// meant to be its whole gate. Both are priced in FEEDSTOCK a fresh player can chop, and their
/// essence splits are pinned EMPTY.
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

/// The bench's pace steps, and the catalog: the LEVELS that open each
/// recipe live in `crate::unlock`, which pins them beside the gates
/// they feed — here we only hold the catalog to the recipe table.
#[test]
fn pace_steps_and_the_catalog_are_pinned() {
	// The pace drops at exactly the two levels that name it, and holds
	// flat everywhere else — the durations themselves are the if-chain's
	// own arms, so asserting them back would only restate the function.
	use crate::skills::MAX_LEVEL;
	use crate::unlock::{BENCH_FASTER_AT, BENCH_FASTEST_AT};
	for l in 2..=MAX_LEVEL {
		let (prev, now) = (base_pace_ticks(l - 1), base_pace_ticks(l));
		if l == BENCH_FASTER_AT || l == BENCH_FASTEST_AT {
			assert!(now < prev, "the bench must speed up at {l}");
		} else {
			assert_eq!(now, prev, "the bench must hold flat at {l}");
		}
	}
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
		(B::WoodStairs, &[(B::Wood, 1)], &[(E::Vitae, 3)], 1),
		(B::StoneStairs, &[(B::Rubble, 1)], &[(E::Terra, 3)], 1),
		(B::LumberDoor, &[(B::Wood, 1)], &[(E::Vitae, 3)], 2),
		(B::WoodSlab, &[(B::Wood, 1)], &[(E::Vitae, 3)], 3),
		(B::StoneSlab, &[(B::Rubble, 1)], &[(E::Terra, 3)], 3),
		(B::Lumber, &[(B::Wood, 1)], &[(E::Vitae, 3)], 1),
		(B::Stone, &[(B::Rubble, 1)], &[(E::Terra, 3)], 1),
		(B::Sandstone, &[(B::Sand, 1)], &[(E::Terra, 3)], 1),
		(B::Glass, &[(B::Sand, 1)], &[(E::Terra, 3)], 1),
		(B::StoneDoor, &[(B::Rubble, 1)], &[(E::Terra, 3)], 2),
		(B::DarkLumber, &[(B::Wood, 1)], &[(E::Vitae, 3)], 1),
		(B::RedLumberStairs, &[(B::Wood, 1)], &[(E::Vitae, 3)], 1),
		// The essence-only substances: no feedstock, the volume rule.
		(B::Metal, &[], &[(E::Ferrum, 27)], 1),
		(B::MetalSlab, &[], &[(E::Ferrum, 9)], 1),
		(B::Crystal, &[], &[(E::Aether, 18), (E::Ferrum, 9)], 1),
		// And the vessels, feedstock alone.
		(B::Bucket, &[(B::Wood, 6)], &[], 1),
		(B::EssenceSiphon, &[(B::Wood, 2), (B::Leaves, 5)], &[], 1),
		// The torch: a stick with a treated head.
		(
			B::Torch,
			&[(B::Wood, 1)],
			&[(E::Vitae, 2), (E::Terra, 2)],
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

/// A full bucket must pay for the costliest single craft a player at that level may ask for, or
/// that craft cannot be done AT ALL online: the server refuses on `bench_tokens < cost` and the
/// tokens never reach it — no debit, no yield, no XP, just the standing rebase. Offline is
/// unpaced, so single-player looks perfect while the dedicated server is a wall.
#[test]
fn the_bucket_can_always_afford_the_costliest_op() {
	for &b in &BlockType::ALL {
		for level in 1..=crate::skills::MAX_LEVEL {
			if crate::unlock::recipe_open(b, level) {
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
			let mastered = field_yield(node, alchemy, crate::skills::MAX_LEVEL, plain, TEST_SEED);
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

/// A vein cell with no Aether trace and one with, in that order. WALKED rather than named, so a
/// retune of the rate moves the test with it — a hardcoded pair of coordinates would silently
/// land on the wrong side of the hash and start asserting the opposite of what it says.
fn trace_cells(seed: f64) -> ((i32, i32, i32), (i32, i32, i32)) {
	let (mut plain, mut rich) = (None, None);
	for y in 0..4096 {
		let at = (1, y, 2);
		let slot =
			if crate::worldgen::ore::traces_aether(BlockType::Duskstone, at.0, at.1, at.2, seed) {
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

/// **Every priced item is buildable by a player who has just earned it**: each essence its recipe
/// names is paid by a NODE that player can see and draw, each feedstock line by a block the world
/// hands out below the item's own level. Three rules carry it — the reveal is checked on the
/// node's OWN axis, since a Mining wall on the host is another skill; `node_yields` excludes the
/// unreliable Duskstone trace; and feedstock must be world-handed and ungated by Alchemy.
#[test]
fn every_price_has_a_source_at_its_own_level() {
	for &item in &BlockType::ALL {
		let (Some(level), Some(price)) = (crate::unlock::craft(item), craft_ideal(item)) else {
			continue;
		};
		for &(essence, _) in &price {
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

/// **Where each essence's faucet actually opens** — the first level at which a player can both SEE
/// a node paying it and legally swing at its host. The cross-skill coupling
/// `every_price_has_a_source_at_its_own_level` will not assert, held as a golden because it is a
/// CONSEQUENCE of three tables — reveal, strength wall, yield split — answering to two skills and
/// knowing nothing of each other; a bad retune fails nowhere, leaving early rows nobody can buy.
#[test]
fn essence_faucets_open_where_they_always_did() {
	let mut opens: Vec<(Essence, u32)> = Vec::new();
	for &e in &Essence::ALL {
		let at = crate::worldgen::nodes::NODES
			.iter()
			.filter(|&&n| {
				node_yields(n, crate::skills::MAX_LEVEL)
					.iter()
					.any(|&(y, amount)| y == e && amount > 0)
			})
			.filter_map(|&n| {
				// The node's own reveal, on whichever axis owns it,
				// against the wall on the host it stands in — a draw is
				// a hold like any other and `dig_blocked` gates it.
				let seen = crate::unlock::reveal(n).or(crate::unlock::alchemy_reveal(n))?;
				Some(seen.max(crate::unlock::mine(n)))
			})
			.min()
			.unwrap_or(u32::MAX);
		opens.push((e, at));
	}
	assert_eq!(
		opens,
		vec![
			(Essence::Vitae, 2),
			(Essence::Terra, 10),
			(Essence::Unda, 12),
			(Essence::Ferrum, 15),
			(Essence::Aether, 33),
		],
		"an essence's faucet moved — check every recipe priced in it \
			 still has a level a player can reach it at"
	);
}

/// `MAX_COST_LINES` is a budget the client derives icon space from, so it
/// must dominate every craft in the catalog — and not by so much that it
/// is meaningless: some craft has to reach it.
#[test]
fn no_craft_has_more_cost_lines_than_the_budget() {
	let mut widest = 0;
	let mut checked = 0;
	for &b in &BlockType::ALL {
		let Some(ideal) = craft_ideal(b) else {
			continue;
		};
		let lines = craft_feedstock(b).len() + ideal.len();
		assert!(lines <= MAX_COST_LINES, "{b:?} has {lines} cost lines");
		widest = widest.max(lines);
		checked += 1;
	}
	assert!(checked > 0, "nothing craftable");
	assert_eq!(widest, MAX_COST_LINES, "the budget has slack to give back");
}
