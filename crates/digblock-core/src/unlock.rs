//! Every level threshold in the game, and the one question each gate
//! asks. Nothing else may hold an unlock number.
//!
//! Progression used to be spread across the modules that consumed it —
//! craft and extraction levels in `alchemy`, the strength wall in `skills`,
//! and each call site re-deriving "may this player do this?" by hand from
//! two tables and a comparison. Five copies of a comparison is five
//! chances to add a third gate and miss one. So the tables live here
//! together, and so does the question:
//!
//! - **The levels** — [`shape`] and [`material`] (SKILLS-DESIGN §7.2's two
//!   grid axes), [`functional`] for the hand-priced items, plus [`extract`]
//!   and [`mine`]. Each is the sole authority for its own axis, and the
//!   two questions players actually ask are DERIVED from them: [`craft`] is
//!   `max(shape, material)` and [`pickup`] reads `craft` of the drop. That
//!   is what stopped the wood slab opening nineteen levels before wood.
//! - **The questions** — [`can_craft`], [`can_extract`], [`dig_blocked`].
//!   Callers ask these; they do not compare levels themselves. The dig
//!   gates in particular are asked identically by client prediction, the
//!   server's `DigStart` backstop, the bot's target filter and the test
//!   harness, so all four agree by construction rather than by review.
//! - **What a level opens** — [`opens`], DERIVED from the tables above
//!   plus alchemy's own yield tables rather than restated as a list, so
//!   retuning any threshold moves the level-up fanfare and the character
//!   screen's teasers with it.
//!
//! The refusal wording lives here too ([`Kind::message`]): prediction and
//! the server's verdict speak the same sentence because there is only one.

use crate::alchemy;
use crate::block::{BlockType, ShapeFamily};
use crate::material::Material;
use crate::skills::{MAX_LEVEL, Skill, SkillSet};

// ---- the tables ------------------------------------------------------

/// The Alchemy level that opens a SHAPE — can you work any substance into
/// this form at all? (SKILLS-DESIGN §7.2's shape axis.)
///
/// The full cube sits at the very first level deliberately: the
/// progression belongs to the
/// materials, so a substance arrives in every form you have already
/// learned rather than dribbling in one shape at a time. Reaching a shape
/// LATER then hands it to you in every material you already own.
pub fn shape(s: ShapeFamily) -> u32 {
	match s {
		ShapeFamily::Block | ShapeFamily::Slab => 1,
		ShapeFamily::Stairs => 5,
		ShapeFamily::Door => 10,
	}
}

/// The Alchemy level that opens a MATERIAL — can you transmute this
/// substance into block form at all? `None` = never, in any shape, which
/// is what makes it eligible for a §3.3 bonus line.
///
/// Materials with no authored block yet are still listed: the level IS
/// the design, and a catalog wave should be a block row and a PNG.
pub fn material(m: Material) -> Option<u32> {
	use Material as M;
	Some(match m {
		// The starter timber and the loose ground under your feet — what
		// the world hands a fresh player, so the bench has something to do
		// on day one.
		M::Wood => 1,
		M::Dirt | M::Sand => 5,
		// Planed timber: the joinery material.
		M::Lumber => 15,
		// The mason's unlock (§7.1) — clean rock exists only pristine in
		// terrain or transmuted here, which is what makes Rubble matter.
		M::Stone => 20,
		M::DarkLumber | M::StoneBrick => 25,
		M::Sandstone | M::Snow | M::Grass | M::Mud | M::Glass => 30,
		M::RedLumber => 40,
		M::PolishedStone | M::SandstoneBrick => 45,
		// Scarce by construction: Metal comes only from ore.
		M::Metal => 50,
		M::Crystal => 65,
		M::Rubble
		| M::Gravel
		| M::DryGrass
		| M::Leaf
		| M::PineLeaf
		| M::Emberore
		| M::Duskstone
		| M::Glowvein
		| M::Aetherock
		| M::DepletedStone => return None,
	})
}

/// The Alchemy level for a FUNCTIONAL item — one §7.3 prices by hand
/// because it is a function rather than a substance. `None` = not a
/// functional item (ask the grid instead).
///
/// The twin of [`alchemy::functional_ideal`]: a level and a price are one
/// decision, pinned so neither can exist without the other.
pub fn functional(b: BlockType) -> Option<u32> {
	Some(match b {
		// First light, pre-ore on purpose (§7.3).
		BlockType::Torch => 8,
		// The ore tool, ready before the first vein reveals — the game's
		// first cross-skill handshake, and the reason neither
		// skill can be levelled entirely from inside the other.
		BlockType::EssenceSiphon => 10,
		// Furniture: the first things you make because you want them
		// rather than because you need them, so they sit just past the
		// timber set that supplies them.
		BlockType::Chair => 18,
		BlockType::Table => 18,
		// The bed waits for the Aqua economy it costs, which is the whole
		// reason its price crosses families.
		BlockType::Bed => 35,
		// Crafting the bucket charge, lossy against extracting it.
		BlockType::Water => 50,
		// The pick: crafted here, usable at the matching MINING level
		// (`pick_cells`) — two skills, one item, and the reason Metal is
		// worth mining at all.
		BlockType::ResonantPick => 55,
		// The lamps: the first Metal sink outside tools, and the standing
		// Aether one — a repeatable per-lamp sink is what keeps Aetherock
		// worth mining late, where the pick and the sigil are both
		// one-offs.
		BlockType::Lantern => 40,
		BlockType::Aetherlamp => 80,
		// A pure Aether price for a thing that does nothing: the point IS
		// that it is the last line.
		BlockType::MastersSigil => 99,
		_ => return None,
	})
}

/// Alchemy level that unlocks crafting `b`; `None` = never craftable.
///
/// §7.2's grid rule, now enforced rather than described: a material ×
/// shape pair unlocks at `max(shape level, material level)`. StoneDoor
/// lands where Stone does, not because anyone curated it; and reaching
/// Stone opens clean stone in the block, slab, stairs and door forms
/// together, because those shapes were already learned.
pub fn craft(b: BlockType) -> Option<u32> {
	if let Some(level) = functional(b) {
		return Some(level);
	}
	let (m, s) = b.grid_cell()?;
	Some(material(m)?.max(shape(s)))
}

/// The Mining level a vein of `b` becomes VISIBLE at (SKILLS-DESIGN
/// §5.3), or `None` for anything that is not an ore.
///
/// This is a stranger gate than the others, and deliberately: below it the
/// cell is Stone to that player in every respect — it renders as stone,
/// raycasts as stone, digs as stone, and the server agrees, so a
/// low-level miner tunnelling through a deposit spends it without ever
/// knowing. You dig what you can see. That is what makes crossing a
/// threshold an event (veins fade in around you) rather than a number.
pub fn reveal(b: BlockType) -> Option<u32> {
	Some(match b {
		// Deliberately ABOVE the Rock family's strength wall (§5.1), not on
		// it. Ore that reveals at exactly the level rock opens is ore you
		// have never not seen: the first seam would appear in the first
		// stone you were ever able to break, and the stretch of plain
		// quarrying the ladder is built around would not exist. It also
		// leaves the property untestable — an unrevealed vein digs as
		// ordinary stone, and there is no level that can dig stone and not
		// see the very first vein in it.
		BlockType::Emberore => 15,
		BlockType::Duskstone => 30,
		BlockType::Glowvein => 55,
		BlockType::Aetherock => 80,
		_ => return None,
	})
}

/// How many cells one hold breaks with the Resonant Pick in hand
/// (SKILLS-DESIGN §5.6): a single cell below the tool's own Mining level,
/// then a row, then the full square plane.
///
/// A width, not a boolean, because the pick's two ranks are one
/// progression: the same machinery widens, and the timing multiplier
/// beside it in `skills` steps with it.
pub fn pick_cells(level: u32) -> usize {
	match level {
		l if l >= 85 => PICK_PLANE,
		l if l >= 55 => 3,
		_ => 1,
	}
}

/// The pick's second rank: the full plane, the widest it ever breaks.
///
/// Named because two things key on "the pick is at its last rank" and
/// neither may own a threshold of its own — [`pick_cells`] here, and
/// [`crate::gi::carried_light`], which brightens the tool's glow at
/// exactly the level the swing widens.
pub const PICK_PLANE: usize = 9;

/// The late-Mining **prospector's sense** (SKILLS-DESIGN §5.5): within
/// [`PROSPECT_RANGE`] of a deposit you cannot yet SEE, a faint shimmer.
/// Audio only, in the `Footsteps` cadence mould — never an overlay, or
/// the reveal levels would stop meaning anything.
pub fn prospector(level: u32) -> bool {
	level >= 90
}

/// How far the prospector's sense reaches, in blocks.
pub const PROSPECT_RANGE: i32 = 8;

/// The capstone Mining mastery (§5.5): every field extraction yields one
/// extra raw ore. The last level has to hand you something, and a flat
/// bonus on the rarest action in the game is the smallest thing that
/// qualifies.
pub fn ore_mastery(level: u32) -> u32 {
	u32::from(level >= 99)
}

/// Alchemy level required to break `b` down at the bench; the first level
/// means always. The bucket charge waits — the Aqua economy's opening
/// move.
pub fn extract(b: BlockType) -> u32 {
	match b {
		BlockType::Water => 15,
		_ => 1,
	}
}

/// §5.1's strength wall: the Mining level below which `b` refuses to be
/// dug at all; the first level means ungated. Deliberately a wall — the doc's original "no
/// strength wall" stance was reversed by design.
///
/// TWO gates, and a block waits for the later of them. [`mine_family`] is
/// the strength ladder every diggable block sits on; [`mine_material`] is
/// the handful of substances that additionally have to be EARNED, and it
/// can only ever push a block later, never earlier.
pub fn mine(b: BlockType) -> u32 {
	let family = mine_family(b.dig_family());
	let material = b.material().map_or(1, mine_material);
	family.max(material)
}

/// The strength wall keyed on the DIG FAMILY — the same axis that decides
/// how long a hold takes (§5.1), which is the only thing raw strength can
/// honestly be about. Soil and foliage from the first swing, then timber,
/// then rock: the ladder a player climbs is soil → trees → stone, and each
/// rung is opened by digging the one below it.
///
/// Keying it here rather than on the substance is what stopped rock being
/// the only family with a wall at all. It also puts GRAVEL behind the
/// same wall as stone — it used to be the ungated "entry rock", and it
/// isn't one any more, because timber is the thing that now stands
/// between bare ground and the quarry.
fn mine_family(f: crate::block::DigFamily) -> u32 {
	use crate::block::DigFamily as F;
	match f {
		F::Fixed | F::Soft => 1,
		F::Wood => 5,
		F::Rock => 10,
	}
}

/// The substances that have to be earned on top of their family's wall,
/// keyed on the SUBSTANCE because that is what being earned is about — a
/// metal stair resists exactly as its metal does, and listing every shape
/// by hand was how the old table came to name four blocks and miss the
/// fifth. Everything whose only wall is its strength answers the first
/// level here and takes [`mine_family`]'s number.
fn mine_material(m: Material) -> u32 {
	use Material as M;
	match m {
		// The ORES, at exactly the level that reveals them. A vein in
		// pristine stone is already invisible below its tier, so this arm
		// is about the other way an ore block exists: one somebody PLACED.
		// That is ordinary chunk data every visitor sees at any level
		// (`apply_ore`'s second pass reverts only cells it claimed, never
		// a placed block), so without these arms the whole table fell
		// through to the ungated arm and a brand-new player could strip a
		// late-game player's Aetherock wall. The tell was the neighbour:
		// DepletedStone — the SPENT face, worth Rubble — sat behind the
		// timber wall while the ore itself was ungated. The family wall
		// does not cover this: ore is Rock, so on strength alone it would
		// open with the rest of the rock.
		//
		// Keyed to `reveal` rather than hand-numbered, so a retune of the
		// reveal ladder cannot leave the two disagreeing;
		// `every_ore_is_gated_at_its_own_reveal` pins that they match.
		M::Emberore => reveal_of(BlockType::Emberore),
		M::Duskstone => reveal_of(BlockType::Duskstone),
		M::Glowvein => reveal_of(BlockType::Glowvein),
		M::Aetherock => reveal_of(BlockType::Aetherock),
		// The hard substances ore pays for: a pick has to be worth
		// something before someone else's metal wall comes apart.
		M::Metal => 40,
		M::Crystal => 60,
		// Everything else waits only for its family's strength wall.
		_ => 1,
	}
}

/// [`reveal`] for a block known to be an ore, for use inside
/// [`mine_material`]'s table. Panics on anything else, which is a
/// compile-time-shaped mistake made loud at first call rather than
/// silently answering as if ungated — the exact failure this helper exists
/// to fix.
fn reveal_of(b: BlockType) -> u32 {
	match reveal(b) {
		Some(l) => l,
		None => panic!("{b:?} is in the ore gate but has no reveal level"),
	}
}

/// The strength wall's Alchemy counterpart: the level below which `b` refuses
/// to be dug because picking it up would hand you an item you cannot make
/// (the first level means ungated).
///
/// Derived from [`craft`] rather than tabled again, so the two can never
/// disagree about what a torch costs to own. It keys on the DROP, not the
/// block, because the gate is about the item that lands in the inventory
/// — mined rock is Rubble whatever the cell was, so quarrying pristine
/// Stone stays a Mining question. And it keys on `block::crafted` rather
/// than on craftability alone: `Wood` and `Lumber` are both craftable, but
/// only one of them grows on a tree, and gating the log would shut a
/// brand-new player out of the first thing the world offers. Craft-only
/// blocks are the ones a player can ONLY have crafted, so requiring their
/// own recipe level closes the loop — you cannot mine your way around an
/// unlock, in your own world or through someone else's build.
pub fn pickup(b: BlockType) -> u32 {
	match b.dig_drop() {
		Some(d) if d.is_crafted_only() => craft(d).unwrap_or(1),
		_ => 1,
	}
}

// ---- the questions ---------------------------------------------------

/// May a player at `level` transmute `b` at all? (False for anything with
/// no recipe, which is what makes `craft_ideal`'s `expect` sound.)
pub fn can_craft(b: BlockType, level: u32) -> bool {
	craft(b).is_some_and(|need| level >= need)
}

/// May a player at `level` break `b` down at the bench?
pub fn can_extract(b: BlockType, level: u32) -> bool {
	level >= extract(b)
}

/// Which gate a dig ran into — the two read differently to a player, and
/// each carries the wording the feed speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	/// The block is too hard for this Mining level (the strength wall).
	Strength,
	/// The block is craft-only and this Alchemy level cannot make one, so
	/// picking it up would hand out an item the player can't own yet.
	Recipe,
	/// A revealed ore vein and the wrong tool in hand (§5.3). Unlike the
	/// other two this is not about a LEVEL at all — it is the one refusal
	/// a player can clear without gaining anything, by switching tools.
	Tool,
}

impl Kind {
	/// The feed line this refusal speaks — beside `protect::REFUSE_*_MSG`
	/// in spirit: one wording shared by prediction and the server verdict,
	/// which is why the server sends a REASON and never a sentence.
	pub fn message(self) -> &'static str {
		match self {
			Kind::Strength => "this rock defies your mining level",
			Kind::Recipe => "you haven't unlocked this yet",
			Kind::Tool => "this vein needs an essence siphon",
		}
	}
}

/// One thing a dig needs before it may start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Requirement {
	pub skill: Skill,
	/// The level `skill` must have reached; the first level means no
	/// requirement at all.
	pub level: u32,
	pub kind: Kind,
}

/// Everything digging `b` asks for, in refusal priority order. Level 1
/// entries are satisfied by definition — they are kept in the array so
/// callers that GRANT levels (the test harness) can walk one list.
pub fn dig_requirements(b: BlockType) -> [Requirement; 2] {
	[
		Requirement {
			skill: Skill::Mining,
			level: mine(b),
			kind: Kind::Strength,
		},
		Requirement {
			skill: Skill::Alchemy,
			level: pickup(b),
			kind: Kind::Recipe,
		},
	]
}

/// THE dig-permission question: the first requirement `skills` fails, or
/// `None` when the block may be dug.
///
/// Every consumer of the gates routes through this — client prediction
/// (`diggable`, and the feed line `step_dig` speaks), the server's
/// `DigStart` backstop, the bot's target filter — so a new gate is added
/// in exactly one place and every one of them enforces it.
pub fn dig_blocked(b: BlockType, skills: &SkillSet) -> Option<Requirement> {
	dig_requirements(b)
		.into_iter()
		.find(|r| skills.level(r.skill) < r.level)
}

// ---- what a level opens ----------------------------------------------

/// Does reaching `level` in `skill` open something that was shut at
/// `level - 1`? The level-up fanfare asks this to pick its voice (a
/// bigger sound for a level that hands you something) and the character
/// screen's teasers are pinned against it, and the answer is DERIVED from
/// the tables themselves rather than a second list — retuning a threshold
/// moves the fanfare with it, and cannot drift.
///
/// The first level is never an unlock: it is where everyone starts, and no one
/// levels up INTO it.
pub fn opens(skill: Skill, level: u32) -> bool {
	if level <= 1 || level > MAX_LEVEL {
		return false;
	}
	BlockType::ALL.iter().any(|&b| match skill {
		// A rock whose strength wall stands exactly here. The craft gate
		// needs no arm of its own: it derives from `craft`, which the
		// Alchemy arm already covers.
		Skill::Mining => {
			mine(b) == level
				|| reveal(b) == Some(level)
				// The pick's two ranks — a level that hands you a wider
				// swing hands you something, exactly like a reveal.
				|| (b == BlockType::ResonantPick && pick_cells(level) != pick_cells(level - 1))
				// And the two endgame lines, which hand you something
				// without handing you an item.
				|| (b == BlockType::Air
					&& (prospector(level) != prospector(level - 1)
						|| ore_mastery(level) != ore_mastery(level - 1)))
		}
		// A new recipe, a newly extractable block, or a bonus line that
		// wasn't paying at the level below.
		Skill::Alchemy => {
			craft(b) == Some(level)
				|| extract(b) == level
				|| alchemy::bonus_lines(b, level) != alchemy::bonus_lines(b, level - 1)
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::skills::total_xp_for;

	/// The craft table, spot by spot — every number a player feels.
	#[test]
	fn craft_levels_are_pinned() {
		use BlockType as B;
		// The starter timber, in every form you have learned so far.
		assert_eq!(craft(B::Wood), Some(1));
		assert_eq!(craft(B::WoodSlab), Some(1));
		assert_eq!(craft(B::WoodStairs), Some(5), "the stairs SHAPE opens");
		assert_eq!(craft(B::Dirt), Some(5));
		assert_eq!(craft(B::Sand), Some(5));
		assert_eq!(craft(B::Torch), Some(8));
		// §7.2's grid rule: the whole planed-timber set opens together at
		// its MATERIAL's level, whatever each shape's own level is.
		for b in [B::Lumber, B::LumberSlab, B::LumberStairs, B::LumberDoor] {
			assert_eq!(craft(b), Some(15), "{b:?}");
		}
		// And the mason's unlock does the same for stone: the slab and the
		// stairs no longer arrive nineteen levels before the block they are
		// cut from, which is the inversion the grid exists to fix.
		for b in [B::Stone, B::StoneSlab, B::StoneStairs] {
			assert_eq!(craft(b), Some(20), "{b:?}");
		}
		for b in [B::Sandstone, B::Snow, B::Grass, B::Mud] {
			assert_eq!(craft(b), Some(30), "{b:?}");
		}
		assert_eq!(craft(B::Water), Some(50));
		assert_eq!(craft(B::Rubble), None, "mined rock is never crafted");
		assert_eq!(extract(B::Water), 15, "the bucket charge");
		for b in [B::Stone, B::Leaves, B::Gravel] {
			assert_eq!(extract(b), 1, "{b:?} extracts from the start");
		}
		// A level and a recipe are one decision: neither exists alone.
		for &b in &BlockType::ALL {
			assert_eq!(
				craft(b).is_some(),
				alchemy::craft_ideal(b).is_some(),
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
				M::RedLumber,
				M::PolishedStone,
				M::SandstoneBrick,
				M::Rubble,
				M::Gravel,
				M::DryGrass,
				M::Leaf,
				M::PineLeaf,
				M::DepletedStone,
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
					alchemy::functional_ideal(b).is_some(),
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
			assert_eq!(mine(b), 5, "{b:?} is Wood family");
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
		// before it — the max, not the material table alone.
		assert_eq!(mine(B::Metal), 40);
		assert_eq!(mine(B::Crystal), 60);
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
		assert_eq!(pickup(B::Torch), 8);
		for b in [B::Lumber, B::LumberSlab, B::LumberStairs, B::LumberDoor] {
			assert_eq!(pickup(b), 15, "{b:?} — the timber set");
		}
		for b in [B::StoneSlab, B::StoneStairs] {
			assert_eq!(pickup(b), 20, "{b:?} — clean stone in any form");
		}
		// Craftable is NOT the question — being craft-ONLY is. Every one of
		// these has a recipe (Stone at 20, Wood at 1, Water at 50), and
		// every one of them also grows, erodes or pools in a fresh world.
		for b in [B::Stone, B::Wood, B::Dirt, B::Sand, B::Grass, B::Water] {
			assert!(craft(b).is_some(), "{b:?} is craftable");
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
		// A fresh player: soil yes, rock no, other people's torches no.
		let fresh = at(1, 1);
		assert_eq!(dig_blocked(B::Dirt, &fresh), None);
		assert_eq!(
			dig_blocked(B::Wood, &fresh).map(|r| (r.skill, r.level, r.kind)),
			Some((Skill::Mining, 5, Kind::Strength)),
			"timber is the second rung"
		);
		for b in [B::Gravel, B::Stone] {
			assert_eq!(
				dig_blocked(b, &fresh).map(|r| (r.skill, r.level, r.kind)),
				Some((Skill::Mining, 10, Kind::Strength)),
				"{b:?} is the third rung"
			);
		}
		assert_eq!(
			dig_blocked(B::Torch, &fresh).map(|r| (r.skill, r.level, r.kind)),
			Some((Skill::Alchemy, 8, Kind::Recipe))
		);
		// A stone stair asks BOTH; strength is reported first, and clearing
		// it exposes the recipe underneath rather than opening the dig.
		assert_eq!(
			dig_blocked(B::StoneStairs, &fresh).map(|r| r.kind),
			Some(Kind::Strength)
		);
		assert_eq!(
			dig_blocked(B::StoneStairs, &at(10, 1)).map(|r| (r.level, r.kind)),
			Some((20, Kind::Recipe))
		);
		assert_eq!(dig_blocked(B::StoneStairs, &at(10, 20)), None, "both met");
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
		// And the two gates never share a sentence.
		assert_ne!(Kind::Strength.message(), Kind::Recipe.message());
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
			vec![5, 10, 15, 30, 40, 55, 60, 80, 85, 90, 99],
			"timber and rock, the reveals, metal and crystal, the pick, the endgame"
		);
		// Alchemy: the shape and material unlocks, the hand-priced items, the
		// bucket charge's extraction, and the bonus lines.
		let alchemy: Vec<u32> = (1..=MAX_LEVEL)
			.filter(|&l| opens(Skill::Alchemy, l))
			.collect();
		assert_eq!(
			alchemy,
			vec![
				5, 8, 10, 15, 18, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 75, 80, 90, 99
			]
		);
		// The first level is where everyone starts — you never level up into
		// it, and the slabs sitting at that unlock must not say otherwise.
		for s in Skill::ALL {
			assert!(!opens(s, 1), "{s:?}");
			assert!(!opens(s, 0));
			assert!(!opens(s, MAX_LEVEL + 1));
		}
	}
}
