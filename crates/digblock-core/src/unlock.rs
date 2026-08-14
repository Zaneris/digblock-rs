//! Every level threshold in the game, and the one question each gate
//! asks. Nothing else may hold an unlock number.
//!
//! Progression used to be spread across the modules that consumed it —
//! craft and extraction levels in `alchemy`, the rock gate in `skills`,
//! and each call site re-deriving "may this player do this?" by hand from
//! two tables and a comparison. Five copies of a comparison is five
//! chances to add a third gate and miss one. So the tables live here
//! together, and so does the question:
//!
//! - **The levels** — [`craft`], [`extract`], [`mine`], [`pickup`]. Four
//!   small tables, each the sole authority for its own axis.
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
use crate::block::BlockType;
use crate::skills::{Skill, SkillSet, MAX_LEVEL};

// ---- the tables ------------------------------------------------------

/// Alchemy level that unlocks crafting `b`; `None` = never craftable.
///
/// §7.2's grid rule holds throughout: a material × shape pair unlocks at
/// `max(shape level, material level)`.
pub fn craft(b: BlockType) -> Option<u32> {
	use BlockType as B;
	Some(match b {
		B::WoodSlab | B::StoneSlab => 1,
		B::WoodStairs | B::StoneStairs => 5,
		B::Torch => 8,
		// Planed timber is a level-15 MATERIAL, so its slab (shape 1),
		// stairs (shape 5) and door (shape 10) all arrive WITH it rather
		// than at their shapes' own levels — the whole joinery set opens
		// at once, which is what makes the material read as a material.
		B::Lumber | B::LumberSlab | B::LumberStairs | B::LumberDoor => 15,
		B::Stone | B::Wood | B::Dirt | B::Sand => 20,
		B::Sandstone | B::Snow | B::Grass | B::Mud => 30,
		B::Water => 50,
		_ => return None,
	})
}

/// Alchemy level required to break `b` down at the bench (1 = always).
/// The bucket charge waits for 15 — the Aqua economy's opening move.
pub fn extract(b: BlockType) -> u32 {
	match b {
		BlockType::Water => 15,
		_ => 1,
	}
}

/// §5.1's rock gate: the Mining level below which `b` refuses to be dug
/// at all (1 = ungated). Deliberately a strength wall — the doc's
/// original "no strength wall" stance was reversed by design: rock opens
/// a few levels in, after soft ground and loose gravel have paid for
/// them.
pub fn mine(b: BlockType) -> u32 {
	match b {
		BlockType::Sandstone => 3,
		BlockType::Stone | BlockType::StoneStairs | BlockType::StoneSlab | BlockType::Rubble => 5,
		_ => 1, // Gravel stays the entry rock; everything else ungated
	}
}

/// The rock gate's Alchemy counterpart: the level below which `b` refuses
/// to be dug because picking it up would hand you an item you cannot make
/// (1 = ungated).
///
/// Derived from [`craft`] rather than tabled again, so the two can never
/// disagree about what a torch costs to own. It keys on the DROP, not the
/// block, because the gate is about the item that lands in the inventory
/// — mined rock is Rubble whatever the cell was, so quarrying pristine
/// Stone stays a Mining question. And it keys on `block::crafted` rather
/// than on craftability alone: `Wood` and `Lumber` are both craftable, but
/// only one of them grows on a tree, and gating the log would shut a
/// level-1 player out of the first thing the world offers. Craft-only
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
	/// The material is too hard for this Mining level (the rock gate).
	Strength,
	/// The block is craft-only and this Alchemy level cannot make one, so
	/// picking it up would hand out an item the player can't own yet.
	Recipe,
}

impl Kind {
	/// The feed line this refusal speaks — beside `protect::REFUSE_*_MSG`
	/// in spirit: one wording shared by prediction and the server verdict,
	/// which is why the server sends a REASON and never a sentence.
	pub fn message(self) -> &'static str {
		match self {
			Kind::Strength => "this rock defies your mining level",
			Kind::Recipe => "you haven't unlocked this yet",
		}
	}
}

/// One thing a dig needs before it may start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Requirement {
	pub skill: Skill,
	/// The level `skill` must have reached; 1 = no requirement at all.
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
/// Level 1 is never an unlock: it is where everyone starts, and no one
/// levels up INTO it.
pub fn opens(skill: Skill, level: u32) -> bool {
	if level <= 1 || level > MAX_LEVEL {
		return false;
	}
	BlockType::ALL.iter().any(|&b| match skill {
		// A rock whose strength wall stands exactly here. The craft gate
		// needs no arm of its own: it derives from `craft`, which the
		// Alchemy arm already covers.
		Skill::Mining => mine(b) == level,
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
		assert_eq!(craft(B::WoodSlab), Some(1));
		assert_eq!(craft(B::StoneSlab), Some(1));
		assert_eq!(craft(B::WoodStairs), Some(5));
		assert_eq!(craft(B::Torch), Some(8));
		// §7.2's grid rule: the whole planed-timber set opens together at
		// its MATERIAL's level, whatever each shape's own level is.
		for b in [B::Lumber, B::LumberSlab, B::LumberStairs, B::LumberDoor] {
			assert_eq!(craft(b), Some(15), "{b:?}");
		}
		assert_eq!(craft(B::Stone), Some(20));
		assert_eq!(craft(B::Sandstone), Some(30));
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

	/// The rock gate (§5.1, the reversed-by-design "strength wall"): rock
	/// refuses below its Mining level; gravel is the ungated entry rock.
	#[test]
	fn rock_is_gated_until_its_level() {
		use BlockType as B;
		assert_eq!(mine(B::Sandstone), 3);
		for b in [B::Stone, B::StoneStairs, B::StoneSlab, B::Rubble] {
			assert_eq!(mine(b), 5, "{b:?}");
		}
		for b in [B::Gravel, B::Dirt, B::Wood, B::Leaves, B::Water, B::Air] {
			assert_eq!(mine(b), 1, "{b:?} ungated");
		}
	}

	/// The craft gate: a craft-only block refuses to be dug below the
	/// Alchemy level that makes it, and everything the world grows stays
	/// ungated however craftable it happens to be.
	#[test]
	fn craft_only_blocks_are_gated_until_they_can_be_made() {
		use BlockType as B;
		// The gate IS the recipe's level, item by item.
		for b in [B::WoodSlab, B::StoneSlab] {
			assert_eq!(pickup(b), 1, "{b:?} — a level-1 recipe");
		}
		for b in [B::WoodStairs, B::StoneStairs] {
			assert_eq!(pickup(b), 5, "{b:?}");
		}
		assert_eq!(pickup(B::Torch), 8);
		for b in [B::Lumber, B::LumberSlab, B::LumberStairs, B::LumberDoor] {
			assert_eq!(pickup(b), 15, "{b:?} — the timber set");
		}
		// Craftable is NOT the question — being craft-ONLY is. Every one of
		// these has a recipe (Stone at 20, Wood at 20, Water at 50), and
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
		assert_eq!(dig_blocked(B::Gravel, &fresh), None, "the entry rock");
		assert_eq!(
			dig_blocked(B::Stone, &fresh).map(|r| (r.skill, r.level, r.kind)),
			Some((Skill::Mining, 5, Kind::Strength))
		);
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
			dig_blocked(B::StoneStairs, &at(5, 1)).map(|r| (r.level, r.kind)),
			Some((5, Kind::Recipe))
		);
		assert_eq!(dig_blocked(B::StoneStairs, &at(5, 5)), None, "both met");
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
		// Mining: the two rock walls, and nothing else.
		let mining: Vec<u32> = (1..=MAX_LEVEL)
			.filter(|&l| opens(Skill::Mining, l))
			.collect();
		assert_eq!(mining, vec![3, 5], "sandstone, then stone");
		// Alchemy: craft unlocks (5/8/20/30/50), the bucket charge's
		// extraction and the whole planed-timber set (both 15), and the
		// bonus lines (25/40/60/75/90).
		let alchemy: Vec<u32> = (1..=MAX_LEVEL)
			.filter(|&l| opens(Skill::Alchemy, l))
			.collect();
		assert_eq!(alchemy, vec![5, 8, 15, 20, 25, 30, 40, 50, 60, 75, 90]);
		// Level 1 is where everyone starts — you never level up into it,
		// and the slabs sitting at unlock 1 must not say otherwise.
		for s in Skill::ALL {
			assert!(!opens(s, 1), "{s:?}");
			assert!(!opens(s, 0));
			assert!(!opens(s, MAX_LEVEL + 1));
		}
	}
}
