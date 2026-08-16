//! The MATERIAL axis of SKILLS-DESIGN §7.2's material × shape grid.
//!
//! A structural block is a *substance* worked into a *form*: Stone stairs
//! are Stone in the Stairs shape, a lumber slab is planed timber in the
//! Slab shape. Before this module the pair was hand-tabled four separate
//! times — in `unlock::craft`, in `unlock::mine`, in `shapes::craft_recipe`
//! and in the transmute screen's `FAMILIES` — and the four disagreed: the
//! wood slab opened at the very first Alchemy level while the wood *block*
//! waited most of the ladder, and `unlock::craft`'s own doc comment claimed
//! the `max(shape, material)` rule it did not implement.
//!
//! So a block declares its material once (the `mat` builder in `block.rs`,
//! read back through [`crate::block::BlockType::material`]) and everything
//! else derives:
//!
//! - **Unlock level** = `max(shape level, material level)` — [`crate::unlock::craft`].
//! - **Craft cost** = the material's whole-cube split, prorated to the shape's
//!   volume — [`crate::alchemy::craft_ideal`].
//! - **Extraction yield** = the material's base, prorated the same way —
//!   [`crate::alchemy::extract_base`].
//! - **The earned half of the strength wall** = the material's own Mining
//!   level, layered over its dig family's — [`crate::unlock::mine`].
//! - **The transmute tab's families** = one per shape, members in material
//!   order.
//!
//! Two rules keep the economy shut, and both become STRUCTURAL here rather
//! than a test's opinion:
//!
//! - [`Material::craft_units`] is `None` for everything the world only ever
//!   hands you broken — mined rock, gravel, foliage. Those are exactly the
//!   blocks that may carry §3.3's bonus lines, so "a bonus line never sits
//!   on a craftable block" is now a property of the material.
//! - Every material's craft split is at least a whole cube's worth of units
//!   and its extraction base is at most that, so prorating both by the same
//!   shape volume preserves
//!   `no_level_ever_creates_matter` at every shape for free.
//!
//! Functional items — torches, lamps, tools, the bucket charge, the bed —
//! have NO material. §7.3's standing rule is that matter is priced by
//! volume and function is priced by hand, so they are gated and costed by
//! their own small tables in `unlock` and `alchemy`.

use crate::block::Essence;

/// A substance the bench can work, in unlock order (which is also the
/// order members appear on the transmute tab).
///
/// Some rows have no authored block yet — they are declared here because
/// the level and the essence split ARE the design, and a later catalog
/// wave should be a block row and a PNG, not another table. A material
/// with no rows simply contributes nothing to any family or threshold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Material {
	/// The felled trunk — the starter timber, and the one thing a fresh
	/// player can already shape.
	Wood,
	Dirt,
	Sand,
	/// Planed timber, light: the joinery material (SKILLS-DESIGN §7.2).
	Lumber,
	Stone,
	/// Planed timber, dark.
	DarkLumber,
	/// Dressed, squared-off stone.
	StoneBrick,
	Sandstone,
	Snow,
	Grass,
	Mud,
	Glass,
	/// Planed timber, red.
	RedLumber,
	PolishedStone,
	SandstoneBrick,
	/// Refined ore — scarce by construction, the prestige substance.
	Metal,
	/// Grown from Aether: the endgame material, and it glows.
	Crystal,

	// ---- recipe-less: extract only, never crafted in any shape --------
	/// What mined rock actually is (§7.1) — the mass Earth feedstock.
	Rubble,
	Gravel,
	DryGrass,
	/// Broadleaf canopy: filler, and the first bonus line (§3.3's dew).
	Leaf,
	/// Conifer canopy. A separate material from [`Self::Leaf`] even though
	/// the two extract identically, because they are separate SUBSTANCES —
	/// pine resin is the pre-ore Aether trickle and broadleaf never carries
	/// it — and because one grid cell holds one block.
	PineLeaf,
	/// Revealed and extracted at the rock face, never at the bench in any
	/// shape (§5.3).
	Emberore,
	Duskstone,
	Glowvein,
	Aetherock,
	/// A tapped-out vein face — crumbles to Rubble like any other rock.
	/// Superseded by the four per-tier rocks below and kept only so cells
	/// written before them still decode.
	DepletedStone,
	/// The four spent vein faces (§5.3) — the rock a siphon leaves and the
	/// only thing a vein ever puts in your hands.
	///
	/// One material per tier rather than one shared "spent rock", because
	/// they are what a player keeps: the whole point of carrying one home
	/// is that it says which seam you worked. They are worth nothing at the
	/// bench and cost nothing to make, which is the same standing the live
	/// ores have and for the same reason — there is no recipe to run a loop
	/// with.
	DepletedEmberore,
	DepletedDuskstone,
	DepletedGlowvein,
	DepletedAetherock,
}

impl Material {
	pub const COUNT: usize = 31;

	/// Every material in unlock order — the transmute tab reads members in
	/// this order, so a family's tiles run from cheapest to rarest.
	pub const ALL: [Material; Self::COUNT] = [
		Material::Wood,
		Material::Dirt,
		Material::Sand,
		Material::Lumber,
		Material::Stone,
		Material::DarkLumber,
		Material::StoneBrick,
		Material::Sandstone,
		Material::Snow,
		Material::Grass,
		Material::Mud,
		Material::Glass,
		Material::RedLumber,
		Material::PolishedStone,
		Material::SandstoneBrick,
		Material::Metal,
		Material::Crystal,
		Material::Rubble,
		Material::Gravel,
		Material::DryGrass,
		Material::Leaf,
		Material::PineLeaf,
		Material::Emberore,
		Material::Duskstone,
		Material::Glowvein,
		Material::Aetherock,
		Material::DepletedStone,
		Material::DepletedEmberore,
		Material::DepletedDuskstone,
		Material::DepletedGlowvein,
		Material::DepletedAetherock,
	];

	/// The whole-cube craft split — what a FULL CUBE of this substance costs
	/// before waste, in the block's own essence ratio. `None` = the bench
	/// cannot make this substance in any shape, which is also what makes it
	/// eligible for a bonus line.
	///
	/// Totals sit at or above a whole cube's worth on purpose: the shortfall
	/// against
	/// [`Self::extract_units`] is the loss that keeps extract→craft→extract
	/// a sink (§4.2).
	pub fn craft_units(self) -> Option<&'static [(Essence, u32)]> {
		use Essence as E;
		use Material as M;
		Some(match self {
			M::Wood | M::Lumber | M::DarkLumber | M::RedLumber => &[(E::Flora, 27)],
			M::Stone | M::Sand | M::Sandstone => &[(E::Earth, 27)],
			M::StoneBrick | M::PolishedStone | M::SandstoneBrick => &[(E::Earth, 27)],
			// Sand-flavored Earth, fused: the same bulk, a different form.
			M::Glass => &[(E::Earth, 27)],
			// A whole cube's volume in the extraction ratio, each line ceiled
			// separately — which is why the total overshoots a cube by a unit.
			M::Dirt => &[(E::Earth, 23), (E::Flora, 5)],
			M::Grass => &[(E::Earth, 18), (E::Flora, 9)],
			M::Mud => &[(E::Earth, 18), (E::Flora, 5), (E::Aqua, 5)],
			M::Snow => &[(E::Aqua, 27)],
			M::Metal => &[(E::Metal, 27)],
			// Aether-bound, with a metal lattice to hold it: the only
			// repeatable Aether sink big enough to matter in the endgame.
			M::Crystal => &[(E::Aether, 18), (E::Metal, 9)],
			M::Rubble
			| M::Gravel
			| M::DryGrass
			| M::Leaf
			| M::PineLeaf
			| M::Emberore
			| M::Duskstone
			| M::Glowvein
			| M::Aetherock
			| M::DepletedStone
			| M::DepletedEmberore
			| M::DepletedDuskstone
			| M::DepletedGlowvein
			| M::DepletedAetherock => return None,
		})
	}

	/// Base extraction yield for a FULL CUBE at full efficiency, never more
	/// than the craft split — the shortfall is structure lost to the
	/// process (§3.1).
	pub fn extract_units(self) -> &'static [(Essence, u32)] {
		use Essence as E;
		use Material as M;
		match self {
			M::Wood | M::Lumber | M::DarkLumber | M::RedLumber => &[(E::Flora, 27)],
			M::Stone => &[(E::Earth, 27)],
			M::StoneBrick | M::PolishedStone => &[(E::Earth, 24)],
			M::Rubble => &[(E::Earth, 24)],
			M::Sandstone | M::SandstoneBrick | M::Glass => &[(E::Earth, 21)],
			M::Gravel => &[(E::Earth, 15)],
			M::Sand => &[(E::Earth, 12)],
			M::Dirt => &[(E::Earth, 15), (E::Flora, 3)],
			M::Grass => &[(E::Earth, 18), (E::Flora, 9)],
			M::DryGrass => &[(E::Earth, 18), (E::Flora, 6)],
			M::Mud => &[(E::Earth, 12), (E::Flora, 3), (E::Aqua, 3)],
			M::Snow => &[(E::Aqua, 9)],
			// Foliage is mostly water and structure — the timber's essence
			// is in the trunk. One unit, not three: a leaf digs almost
			// instantly and there are hundreds per tree, so at three it paid
			// Flora as fast
			// as grass from the block the world gives away with every log.
			M::Leaf | M::PineLeaf => &[(E::Flora, 1)],
			M::Metal => &[(E::Metal, 24)],
			M::Crystal => &[(E::Aether, 12), (E::Metal, 6)],
			// Raw ore is drawn whole (§3.1) — it has no shapes, so the
			// proration below is always the identity. This is what a
			// SIPHON pays out at the rock face: no ore ever reaches the
			// bench, because no ore ever reaches a hand.
			M::Emberore => &[(E::Metal, 18)],
			M::Duskstone => &[(E::Metal, 36)],
			M::Glowvein => &[(E::Metal, 60)],
			M::Aetherock => &[(E::Metal, 90), (E::Aether, 9)],
			// Spent rock is spent: the siphon already took what was in it,
			// and a tapped face that still paid at the bench would be a
			// second helping of the same vein.
			M::DepletedStone
			| M::DepletedEmberore
			| M::DepletedDuskstone
			| M::DepletedGlowvein
			| M::DepletedAetherock => &[],
		}
	}

	/// The screens' label.
	pub fn name(self) -> &'static str {
		use Material as M;
		match self {
			M::Wood => "WOOD",
			M::Dirt => "DIRT",
			M::Sand => "SAND",
			M::Lumber => "LUMBER",
			M::Stone => "STONE",
			M::DarkLumber => "DARK LUMBER",
			M::StoneBrick => "STONE BRICK",
			M::Sandstone => "SANDSTONE",
			M::Snow => "SNOW",
			M::Grass => "GRASS",
			M::Mud => "MUD",
			M::Glass => "GLASS",
			M::RedLumber => "RED LUMBER",
			M::PolishedStone => "POLISHED STONE",
			M::SandstoneBrick => "SANDSTONE BRICK",
			M::Metal => "METAL",
			M::Crystal => "CRYSTAL",
			M::Rubble => "RUBBLE",
			M::Gravel => "GRAVEL",
			M::DryGrass => "DRY GRASS",
			M::Leaf => "LEAVES",
			M::PineLeaf => "PINE LEAVES",
			M::Emberore => "EMBERORE",
			M::Duskstone => "DUSKSTONE",
			M::Glowvein => "GLOWVEIN",
			M::Aetherock => "AETHEROCK",
			M::DepletedStone => "DEPLETED STONE",
			M::DepletedEmberore => "SPENT EMBERORE",
			M::DepletedDuskstone => "SPENT DUSKSTONE",
			M::DepletedGlowvein => "SPENT GLOWVEIN",
			M::DepletedAetherock => "SPENT AETHEROCK",
		}
	}
}

/// Prorate a whole-cube essence split down to a shape's `units`, rounding
/// each line UP — the craft side, where the player pays.
///
/// A whole cube is the identity, which is what keeps every
/// raw-block recipe exactly what it was before the grid.
pub fn cost_split(lines: &[(Essence, u32)], units: u32) -> Vec<(Essence, u32)> {
	prorate(lines, units, true)
}

/// Prorate a whole-cube essence split down to a shape's `units`, rounding
/// each line DOWN — the extraction side, where the player is paid.
///
/// Rounding the two sides in opposite directions is what makes
/// "no matter creation" hold for a shape without a second proof: a shape
/// can never extract for more than its share, nor craft for less.
pub fn yield_split(lines: &[(Essence, u32)], units: u32) -> Vec<(Essence, u32)> {
	prorate(lines, units, false)
}

/// The SUM of [`yield_split`] without building it. Same arithmetic, line
/// by line, so it cannot drift from the split it totals — and no
/// allocation, which matters because the extraction pace asks for this
/// every tick for every connected player.
pub fn yield_total(lines: &[(Essence, u32)], units: u32) -> u32 {
	let subs = crate::shapes::SUBS_PER_BLOCK;
	lines.iter().map(|&(_, n)| (n * units / subs).max(1)).sum()
}

fn prorate(lines: &[(Essence, u32)], units: u32, up: bool) -> Vec<(Essence, u32)> {
	let subs = crate::shapes::SUBS_PER_BLOCK;
	lines
		.iter()
		.map(|&(e, n)| {
			let scaled = if up {
				(n * units).div_ceil(subs)
			} else {
				n * units / subs
			};
			// A listed line never vanishes: the thinnest shape of a
			// one-unit material still pays its one unit.
			(e, scaled.max(1))
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `ALL` is the roster every derived table walks — a material listed
	/// twice would double a family's tiles, and one left out would silently
	/// vanish from the transmute tab.
	#[test]
	fn all_lists_every_material_once() {
		assert_eq!(Material::ALL.len(), Material::COUNT);
		for (i, &m) in Material::ALL.iter().enumerate() {
			assert!(
				!Material::ALL[..i].contains(&m),
				"{m:?} listed twice in ALL"
			);
			assert!(!m.name().is_empty(), "{m:?} has no label");
		}
	}

	/// The grid's economic floor, proved once for the axis instead of once
	/// per authored pair: a material always costs more to make than it
	/// yields, in every essence, at the full-cube scale the shapes prorate
	/// from. §4.2's waste multiplier and §3.2's efficiency then only widen
	/// the gap.
	#[test]
	fn craft_always_costs_more_than_extraction_pays() {
		for &m in &Material::ALL {
			let Some(cost) = m.craft_units() else {
				continue;
			};
			let total: u32 = cost.iter().map(|&(_, n)| n).sum();
			assert!(
				total >= crate::shapes::SUBS_PER_BLOCK,
				"{m:?} craft total {total} is below a whole block"
			);
			for &(e, got) in m.extract_units() {
				let paid: u32 = cost
					.iter()
					.filter(|(ce, _)| *ce == e)
					.map(|&(_, n)| n)
					.sum();
				assert!(
					paid >= got,
					"{m:?}: {e:?} yields {got} but costs only {paid}"
				);
			}
		}
	}

	/// Extraction never exceeds a whole block — the §3.1 cap, and the other
	/// half of what makes the proration safe. Ore is the deliberate
	/// exception: it is a concentrate, not a substance you shape, so it has
	/// no craft recipe to pump and no shapes to prorate.
	#[test]
	fn extraction_bases_stay_under_a_block() {
		for &m in &Material::ALL {
			let total: u32 = m.extract_units().iter().map(|&(_, n)| n).sum();
			let is_ore = matches!(
				m,
				Material::Emberore | Material::Duskstone | Material::Glowvein | Material::Aetherock
			);
			if is_ore {
				assert!(m.craft_units().is_none(), "{m:?} must never be craftable");
			} else {
				assert!(
					total <= crate::shapes::SUBS_PER_BLOCK,
					"{m:?} base {total} exceeds a whole block"
				);
			}
		}
	}

	/// A whole cube prorates to itself — which is what makes the grid a
	/// refactor rather than a retune for every raw-block recipe.
	#[test]
	fn a_whole_cube_prorates_to_itself() {
		for &m in &Material::ALL {
			let subs = crate::shapes::SUBS_PER_BLOCK;
			if let Some(c) = m.craft_units() {
				assert_eq!(cost_split(c, subs), c.to_vec(), "{m:?} cost");
			}
			let e = m.extract_units();
			if !e.is_empty() {
				assert_eq!(yield_split(e, subs), e.to_vec(), "{m:?} yield");
			}
		}
	}

	/// The shipped shape volumes, prorated — these are the numbers the
	/// launch roster had hand-tabled, and they must not move.
	#[test]
	fn shipped_shape_splits_are_unchanged() {
		use Essence as E;
		let wood = Material::Wood.craft_units().unwrap();
		assert_eq!(cost_split(wood, 18), vec![(E::Flora, 18)], "stairs");
		assert_eq!(cost_split(wood, 10), vec![(E::Flora, 10)], "door");
		assert_eq!(cost_split(wood, 9), vec![(E::Flora, 9)], "slab");
		let stone = Material::Stone.extract_units();
		assert_eq!(yield_split(stone, 18), vec![(E::Earth, 18)]);
		assert_eq!(yield_split(stone, 9), vec![(E::Earth, 9)]);
	}
}
