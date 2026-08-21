use super::*;
use crate::skills::total_xp_for;

/// The craft table, spot by spot — every number a player feels.
#[test]
fn craft_levels_are_pinned() {
	use BlockType as B;
	// The starter timber, in every form you have learned so far — but
	// not the felled log itself, which grows on a tree.
	assert_eq!(craft(B::WoodSlab), Some(1));
	assert_eq!(craft(B::WoodStairs), Some(5), "the stairs SHAPE opens");
	assert_eq!(craft(B::Torch), Some(10), "light waits for the Earth node");
	assert_eq!(
		craft(B::EssenceSiphon),
		Some(1),
		"the essence tool is the first craft beat"
	);
	// §7.2's grid rule: the whole planed-timber set opens together at
	// its MATERIAL's level, whatever each shape's own level is — and
	// the door SHAPE lands at the same level, so the first door fanfare
	// and the first craftable door are one event.
	for b in [B::Lumber, B::LumberSlab, B::LumberStairs, B::LumberDoor] {
		assert_eq!(craft(b), Some(12), "{b:?}");
	}
	// And the mason's unlock does the same for stone: the slab and the
	// stairs no longer arrive nineteen levels before the block they are
	// cut from, which is the inversion the grid exists to fix.
	for b in [B::Stone, B::StoneSlab, B::StoneStairs] {
		assert_eq!(craft(b), Some(15), "{b:?}");
	}
	for b in [B::Sandstone, B::SandstoneSlab, B::Glass] {
		assert_eq!(craft(b), Some(21), "{b:?}");
	}
	// Nothing the world hands you has a level, because it has no
	// recipe: the loose ground, the felled log, and the water you scoop
	// are all things you go and pick up. Clean rock is the one
	// substance the world PLACES and never GIVES — it crumbles — so it
	// keeps the mason's unlock and §7.1's economy with it.
	for b in [
		B::Wood,
		B::Dirt,
		B::Sand,
		B::Snow,
		B::Grass,
		B::Mud,
		B::Water,
		B::Rubble,
		B::Gravel,
	] {
		assert_eq!(craft(b), None, "{b:?} is the world's, not the bench's");
	}
	// The first vessel: available from the start, gated by its price
	// alone — level 1 is where everyone begins, so `opens` never
	// celebrates it and the fanfare list below is unmoved.
	assert_eq!(craft(B::Bucket), Some(1));
	// A level and a recipe are one decision: neither exists alone.
	for &b in &BlockType::ALL {
		assert_eq!(
			craft(b).is_some(),
			crate::alchemy::craft_ideal(b).is_some(),
			"{b:?}: an unlock level and a recipe come as a pair"
		);
	}
}

/// Every ore is dug at exactly the level that lets you SEE it.
///
/// The two numbers have to move together or they contradict each
/// other: `reveal` decides when a vein appears, and `mine` decides
/// when the block may be broken. `mine_material` reads `reveal`
/// rather than repeating it, and this is the pin on that — it fails
/// if an arm is dropped and the material falls back to the table's
/// ungated `_`, which is exactly how every ore came to be minable at
/// Mining 1.
#[test]
fn every_ore_is_gated_at_its_own_reveal() {
	for &ore in &crate::worldgen::ore::ORES {
		let seen = reveal(ore).expect("an ore has a reveal level");
		assert_eq!(
			mine(ore),
			seen,
			"{ore:?} reveals at {seen} but is minable at {}",
			mine(ore)
		);
		assert!(
			mine(ore) > 1,
			"{ore:?} is ungated — a placed ore block is ordinary chunk \
				 data that any level can see and break"
		);
	}
}

/// The other half of the strength wall: no material may reach the
/// EARNED table's ungated fallback by accident.
///
/// `mine_material`'s `_ => 1` means "this substance is only as hard as
/// its family", which is right for soil, foliage, timber and plain
/// rock and wrong for anything that has to be paid for — but the two
/// are indistinguishable at the match arm, so a new substance nobody
/// remembers to gate simply inherits its family's wall and no more.
/// Spelling the set out makes joining it a decision, the way
/// `material_roster_is_pinned` makes having a material at all one.
#[test]
fn the_materials_with_no_gate_beyond_their_family_are_spelled_out() {
	use Material as M;
	let family_only: Vec<Material> = Material::ALL
		.into_iter()
		.filter(|&m| mine_material(m) == 1)
		.collect();
	assert_eq!(
		family_only,
		vec![
			M::Wood,
			M::Dirt,
			M::Sand,
			M::Lumber,
			M::Stone,
			M::DarkLumber,
			M::StoneBrick,
			M::Sandstone,
			M::Snow,
			M::Grass,
			M::Mud,
			M::Glass,
			M::RedGlass,
			M::GreenGlass,
			M::BlueGlass,
			M::AmberGlass,
			M::RedLumber,
			M::PolishedStone,
			M::SandstoneBrick,
			M::Rubble,
			M::Gravel,
			M::DryGrass,
			M::Leaf,
			M::PineLeaf,
			M::DepletedStone,
			// Spent rock, deep tiers included. What a gate would
			// protect is the ore, and the siphon has already taken
			// it — somebody else's tapped Aetherock face is a rock
			// with nothing in it, so it breaks like one.
			M::DepletedEmberore,
			M::DepletedDuskstone,
			M::DepletedGlowvein,
			M::DepletedAetherock,
			// The surface nodes and their spent faces: a live surface
			// node can never be placed (never an item), so the reveal
			// and the family wall are the only gates it needs — and a
			// spent face is a find already taken.
			M::Heartwood,
			M::DrainedHeartwood,
			M::Leystone,
			M::SpentLeystone,
			M::Springstone,
			M::SpentSpringstone,
		],
		"a material joined or left the family-only set — is that intended?"
	);
}

/// §7.2's grid rule, enforced instead of described: EVERY authored
/// material × shape pair unlocks at exactly `max(shape, material)` —
/// no curated stragglers, and no pair that quietly predates the
/// substance it is cut from.
#[test]
fn grid_gates_compose() {
	for &b in &BlockType::ALL {
		if !b.is_bench_only() {
			// The world's own: no level, because no recipe. The grid
			// still prices the SUBSTANCE — a dirt slab authored tomorrow
			// would open at dirt's level — it simply has no cell here.
			assert_eq!(craft(b), None, "{b:?}");
			continue;
		}
		let Some((m, s)) = b.grid_cell() else {
			// Everything outside the grid is hand-gated, and every
			// hand-gated item has a hand-written price to match.
			assert_eq!(
				craft(b),
				functional(b),
				"{b:?} is not a grid item, so only `functional` may gate it"
			);
			assert_eq!(
				functional(b).is_some(),
				crate::alchemy::functional_ideal(b).is_some(),
				"{b:?}: a hand-priced item needs a hand-set level"
			);
			continue;
		};
		assert!(
			functional(b).is_none(),
			"{b:?} is a grid item and must not also be hand-gated"
		);
		assert_eq!(
			craft(b),
			material(m).map(|l| l.max(shape(s))),
			"{b:?} = max({s:?}, {m:?})"
		);
		// A shape you have already learned never delays a material:
		// the level a substance arrives at is the level its whole
		// learned set arrives at.
		if let Some(ml) = material(m)
			&& ml >= shape(s)
		{
			assert_eq!(craft(b), Some(ml), "{b:?} arrives with {m:?}");
		}
	}
	// The composition is the point: reaching a material hands you every
	// shape you already know, in one level-up.
	use crate::block::ShapeFamily;
	for &m in &Material::ALL {
		let Some(ml) = material(m) else { continue };
		let together: Vec<u32> = ShapeFamily::ALL
			.into_iter()
			.filter(|&s| shape(s) <= ml)
			.filter_map(|s| BlockType::from_grid(m, s))
			.filter_map(craft)
			.collect();
		assert!(
			together.iter().all(|&l| l == ml),
			"{m:?} does not arrive all at once: {together:?}"
		);
	}
}

/// The strength wall (§5.1, the reversed-by-design stance): the ladder
/// is the DIG FAMILY, so it is soil → timber → rock and every block of
/// a family opens together, shapes included. Substances that must also
/// be earned take the later of the two gates.
#[test]
fn the_strength_wall_is_the_dig_family() {
	use BlockType as B;
	// Soil and foliage from the first swing.
	for b in [B::Dirt, B::Grass, B::Sand, B::Snow, B::Mud, B::Leaves] {
		assert_eq!(mine(b), 1, "{b:?} is Soft/Fixed");
	}
	// Non-diggables and the scoop never gate on strength.
	for b in [B::Water, B::Air] {
		assert_eq!(mine(b), 1, "{b:?} ungated");
	}
	// Timber next, every shape of it — grown and planed alike.
	for b in [
		B::Wood,
		B::WoodStairs,
		B::WoodSlab,
		B::Lumber,
		B::LumberDoor,
	] {
		assert_eq!(mine(b), 3, "{b:?} is Wood family");
	}
	// Rock last, gravel and sandstone included: gravel used to be the
	// ungated entry rock and timber has taken that job.
	for b in [
		B::Gravel,
		B::Sandstone,
		B::Stone,
		B::StoneStairs,
		B::StoneSlab,
		B::Rubble,
		B::DepletedStone,
	] {
		assert_eq!(mine(b), 10, "{b:?} is Rock family");
	}
	// And the earned substances sit past their family's wall, never
	// before it — the max, not the material table alone. Their walls
	// equal their materials' own craft levels: one number, both hands,
	// so neither gate can celebrate a block the other still refuses.
	use crate::material::Material as M;
	assert_eq!(Some(mine(B::Metal)), material(M::Metal));
	assert_eq!(Some(mine(B::Crystal)), material(M::Crystal));
	for b in BlockType::ALL {
		assert!(
			mine(b) >= mine_family(b.dig_family()),
			"{b:?} dug before its family's strength wall"
		);
	}
}

/// The craft gate: a craft-only block refuses to be dug below the
/// Alchemy level that makes it, and everything the world grows stays
/// ungated however craftable it happens to be.
#[test]
fn craft_only_blocks_are_gated_until_they_can_be_made() {
	use BlockType as B;
	// The gate IS the recipe's level, item by item — and the recipe's
	// level now follows the material, so a stone shape asks for the
	// mason's unlock rather than for the shape's own.
	assert_eq!(pickup(B::WoodSlab), 1, "a level-1 recipe");
	assert_eq!(pickup(B::WoodStairs), 5);
	assert_eq!(pickup(B::Torch), 10);
	for b in [B::Lumber, B::LumberSlab, B::LumberStairs, B::LumberDoor] {
		assert_eq!(pickup(b), 12, "{b:?} — the timber set");
	}
	for b in [B::StoneSlab, B::StoneStairs] {
		assert_eq!(pickup(b), 15, "{b:?} — clean stone in any form");
	}
	// Craftable is NOT the question — being craft-ONLY is, and clean
	// Stone is where the two come apart. It has a recipe AND the world
	// is made of it, which is the whole shape of §7.1: a quarried one
	// crumbles to Rubble, so the gate has nothing to protect and the
	// dig stays open to anyone strong enough to swing at rock.
	assert!(craft(B::Stone).is_some(), "clean stone is craftable");
	for b in [B::Stone, B::Wood, B::Dirt, B::Sand, B::Grass, B::Water] {
		assert_eq!(pickup(b), 1, "{b:?} is natural — never gated");
	}
	// And the gate reads the DROP: pristine Stone is craft-only-adjacent
	// but hands out Rubble, which nothing can craft at all.
	assert_eq!(B::Stone.dig_drop(), Some(B::Rubble));
	assert_eq!(pickup(B::Rubble), 1);
	// Every gated block really is craft-only, and every craft-only block
	// really is gated at its own recipe's level — the two tables cannot
	// drift apart.
	for &b in &BlockType::ALL {
		match b.dig_drop().filter(|d| d.is_crafted_only()) {
			Some(d) => assert_eq!(Some(pickup(b)), craft(d), "{b:?} drops craft-only {d:?}"),
			None => assert_eq!(pickup(b), 1, "{b:?} is not craft-only"),
		}
	}
}

/// A craft-only block is one the bench alone produces, so it must have
/// a recipe, and digging one must hand back the same item (a `drops`
/// override would gate on one item and pay out another).
#[test]
fn crafted_blocks_have_recipes_and_drop_themselves() {
	for &b in &BlockType::ALL {
		if !b.is_crafted_only() {
			continue;
		}
		assert!(craft(b).is_some(), "{b:?} has no recipe");
		assert_eq!(b.dig_drop(), Some(b), "{b:?} drops something else");
	}
	// The set itself, spelled out — a new row joining it is a decision.
	let crafted: Vec<BlockType> = BlockType::ALL
		.into_iter()
		.filter(|b| b.is_crafted_only())
		.collect();
	use BlockType as B;
	assert_eq!(
		crafted,
		vec![
			B::WoodStairs,
			B::StoneStairs,
			B::LumberDoor,
			B::WoodSlab,
			B::StoneSlab,
			B::Torch,
			B::Lumber,
			B::LumberStairs,
			B::LumberSlab,
			B::StoneDoor,
			B::SandstoneStairs,
			B::SandstoneSlab,
			B::SandstoneDoor,
			B::DarkLumber,
			B::DarkLumberStairs,
			B::DarkLumberSlab,
			B::DarkLumberDoor,
			B::RedLumber,
			B::RedLumberStairs,
			B::RedLumberSlab,
			B::RedLumberDoor,
			B::Table,
			B::Chair,
			B::Bed,
			B::StoneBrick,
			B::StoneBrickStairs,
			B::StoneBrickSlab,
			B::StoneBrickDoor,
			B::PolishedStone,
			B::PolishedStoneStairs,
			B::PolishedStoneSlab,
			B::SandstoneBrick,
			B::SandstoneBrickStairs,
			B::SandstoneBrickSlab,
			B::Glass,
			B::Metal,
			B::MetalStairs,
			B::MetalSlab,
			B::MetalDoor,
			B::Crystal,
			B::CrystalSlab,
			B::Lantern,
			B::Aetherlamp,
			B::RedGlass,
			B::GreenGlass,
			B::BlueGlass,
			B::AmberGlass,
		]
	);
}

/// One question answers every dig gate, for every consumer: it reports
/// the FIRST unmet requirement (strength before recipe, the order the
/// server refuses in), the level that would satisfy it, and the
/// sentence to say — and it says nothing once both are met.
#[test]
fn one_question_answers_every_dig_gate() {
	use BlockType as B;
	let at = |mining: u32, alchemy: u32| {
		let mut s = SkillSet::default();
		s.set_total(Skill::Mining, total_xp_for(mining));
		s.set_total(Skill::Alchemy, total_xp_for(alchemy));
		s
	};
	// A fresh player: soil yes, rock no, other people's torches no. The
	// LEVELS are read from the tables rather than spelled again — they are
	// pinned by `the_strength_wall_is_the_dig_family` and
	// `craft_levels_are_pinned`, and what is under test here is that one
	// question reports them, not what they are.
	let fresh = at(1, 1);
	assert_eq!(dig_blocked(B::Dirt, &fresh), None);
	assert_eq!(
		dig_blocked(B::Wood, &fresh).map(|r| (r.skill, r.level, r.kind)),
		Some((Skill::Mining, mine(B::Wood), Kind::Strength)),
		"timber is the second rung"
	);
	for b in [B::Gravel, B::Stone] {
		assert_eq!(
			dig_blocked(b, &fresh).map(|r| (r.skill, r.level, r.kind)),
			Some((Skill::Mining, mine(B::Stone), Kind::Strength)),
			"{b:?} is the third rung"
		);
	}
	assert_eq!(
		dig_blocked(B::Torch, &fresh).map(|r| (r.skill, r.level, r.kind)),
		Some((Skill::Alchemy, pickup(B::Torch), Kind::Recipe))
	);
	// A stone stair asks BOTH; strength is reported first, and clearing
	// it exposes the recipe underneath rather than opening the dig.
	assert_eq!(
		dig_blocked(B::StoneStairs, &fresh).map(|r| r.kind),
		Some(Kind::Strength)
	);
	let (rock, recipe) = (mine(B::StoneStairs), pickup(B::StoneStairs));
	assert_eq!(
		dig_blocked(B::StoneStairs, &at(rock, 1)).map(|r| (r.level, r.kind)),
		Some((recipe, Kind::Recipe))
	);
	assert_eq!(
		dig_blocked(B::StoneStairs, &at(rock, recipe)),
		None,
		"both met"
	);
	// Every requirement any block has is one `dig_blocked` reports:
	// satisfy the others and fall one level short of this one.
	for &b in &BlockType::ALL {
		for r in dig_requirements(b) {
			assert!(r.level >= 1, "{b:?} — 1 is 'no requirement'");
			if r.level == 1 {
				continue;
			}
			let mut s = SkillSet::default();
			for other in dig_requirements(b) {
				let lvl = if other.skill == r.skill {
					r.level - 1
				} else {
					MAX_LEVEL
				};
				s.set_total(other.skill, total_xp_for(lvl));
			}
			assert_eq!(dig_blocked(b, &s), Some(r), "{b:?} one short of {r:?}");
		}
		// Maxed out, nothing is ever refused.
		let mut maxed = SkillSet::default();
		for s in Skill::ALL {
			maxed.set_total(s, total_xp_for(MAX_LEVEL));
		}
		assert_eq!(dig_blocked(b, &maxed), None, "{b:?}");
	}
	// A verdict that names only a GATE — all the wire carries — recovers
	// the very requirement prediction raised, which is what lets one
	// sentence serve both. What that sentence IS belongs to the client's
	// `voice`, and is pinned there.
	assert_eq!(
		requirement(B::Wood, Kind::Strength),
		dig_blocked(B::Wood, &fresh)
	);
	assert_eq!(
		requirement(B::Wood, Kind::Recipe),
		None,
		"wood has no recipe gate"
	);
}

/// The fanfare's question, derived from the tables: the levels that
/// hand you something are exactly the threshold levels, and every
/// other level is an ordinary one.
#[test]
fn unlock_levels_are_the_threshold_levels() {
	// Mining: the two strength rungs above the starting one, then the
	// reveals — the first reveal sits one rung past rock on purpose, so a
	// quarry is something you work before it starts glittering.
	let mining: Vec<u32> = (1..=MAX_LEVEL)
		.filter(|&l| opens(Skill::Mining, l))
		.collect();
	assert_eq!(
		mining,
		vec![3, 10, 15, 18, 23, 33, 40, 43, 50, 60, 85],
		"timber and rock, the reveals, metal and crystal, the pick, the \
			 prospector's two ranks, mastery — the content tops at 60, and \
			 the pick's plane at 85 is the tail's one deliberate exception"
	);
	// Alchemy: the shape and material unlocks, the hand-priced items,
	// and the surface-node reveals.
	let alchemy: Vec<u32> = (1..=MAX_LEVEL)
		.filter(|&l| opens(Skill::Alchemy, l))
		.collect();
	assert_eq!(
		alchemy,
		vec![
			5, 10, 12, 14, 15, 18, 21, 24, 27, 28, 30, 33, 36, 40, 50, 99
		],
		"the catalog tops at 50; 99 is the sigil alone — the tail is \
			 expansion headroom. 5 and 12 carry the leystone and springstone \
			 reveals beside their recipes; heartwood and the siphon sit at \
			 the first level, which is never an unlock, and the old bonus \
			 lines left with bench extraction"
	);
	// Arcana: the seven waystones interleaved with the travel rungs —
	// ports are the spine the way reveals are Mining's. Return Home and
	// Wisp sit at the first level, which is never an unlock.
	let arcana: Vec<u32> = (1..=MAX_LEVEL)
		.filter(|&l| opens(Skill::Arcana, l))
		.collect();
	assert_eq!(
		arcana,
		vec![
			6,
			KINDLE_AT,
			14,
			RETURN_SWIFT_AT,
			22,
			CHANNEL_QUICK_AT,
			34,
			WISP_BRIGHT_AT,
			42,
			50,
			KINDLE_CHEAP_AT,
			RETURN_SWIFTER_AT,
			60
		],
		"seven ports rising by remoteness, the two swift-return rungs and \
			 the channel quickening between them — the far cold port crowns \
			 the content top"
	);
	// The first level is where everyone starts — you never level up into
	// it, and the slabs sitting at that unlock must not say otherwise.
	for s in Skill::ALL {
		assert!(!opens(s, 1), "{s:?}");
		assert!(!opens(s, 0));
		assert!(!opens(s, MAX_LEVEL + 1));
	}
}

/// The three port tables are twins over ONE set of kinds — a fare
/// without a port is as wrong as a port without a fare or a milestone —
/// and remoteness orders fares and milestones alike: a farther port
/// costs more and pays more, with no inversion to game. The fare is also
/// PAYABLE when its first port opens: every fare is drawn in an essence
/// whose node reveals earlier on its own ladder (the payability pin's
/// question, asked cross-skill the way the siphon handshake is).
#[test]
fn the_port_tables_are_twins_and_the_fare_is_payable() {
	use crate::worldgen::SurfaceKind;
	use BlockType as B;
	let mut ports: Vec<(u32, u32, u64)> = Vec::new();
	for k in SurfaceKind::ALL {
		let port = waystone(k);
		let fare = crate::alchemy::cast_price(k);
		let stone = crate::skills::waystone_milestone(k);
		assert_eq!(port.is_some(), fare.is_some(), "{k:?} fare/port twins");
		assert_eq!(port.is_some(), stone.is_some(), "{k:?} milestone twin");
		if let (Some(l), Some((_, amt)), Some(ms)) = (port, fare, stone) {
			ports.push((l, amt, ms));
		}
	}
	assert!(
		!ports.is_empty(),
		"no ports at all — the sweep proved nothing"
	);
	ports.sort_unstable();
	for w in ports.windows(2) {
		assert!(w[0].0 < w[1].0, "two ports on one level");
		assert!(w[0].1 < w[1].1, "a farther port must cost more");
		assert!(w[0].2 < w[1].2, "a farther port must pay more");
	}
	let first_port = ports[0].0;
	assert!(
		alchemy_reveal(B::Leystone).expect("leystone reveals") <= first_port,
		"the travel essence must be findable before the first port opens"
	);
}

/// The travel rungs only ever help: neither the cooldown nor the channel
/// may lengthen with a level, and each really steps somewhere — a sweep
/// that saw no step proved nothing.
#[test]
fn the_travel_rungs_never_lengthen_with_level() {
	let (mut cooldown_steps, mut channel_steps, mut examined) = (0u32, 0u32, 0u32);
	for l in 2..=MAX_LEVEL {
		examined += 1;
		assert!(
			return_cooldown_secs(l) <= return_cooldown_secs(l - 1),
			"cooldown lengthened at {l}"
		);
		assert!(
			channel_ticks(l) <= channel_ticks(l - 1),
			"channel lengthened at {l}"
		);
		cooldown_steps += u32::from(return_cooldown_secs(l) != return_cooldown_secs(l - 1));
		channel_steps += u32::from(channel_ticks(l) != channel_ticks(l - 1));
	}
	assert_eq!(examined, MAX_LEVEL - 1);
	assert!(cooldown_steps > 0, "the cooldown never stepped");
	assert!(channel_steps > 0, "the channel never stepped");
}
