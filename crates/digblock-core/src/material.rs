//! The MATERIAL axis of SKILLS-DESIGN §7.2's material × shape grid.
//!
//! A structural block is a *substance* worked into a *form*: Stone stairs are
//! Stone in the Stairs shape. A block declares its material once (the `mat`
//! builder in `block.rs`) and four things then DERIVE from the pair rather
//! than being tabled separately, which is what stopped them disagreeing:
//!
//! - **Unlock level** = `max(shape, material)` — [`crate::unlock::craft`].
//! - **Craft cost** — [`crate::alchemy::craft_ideal`].
//! - **The earned half of the strength wall** — [`crate::unlock::mine`].
//! - **The transmute tab's families** — one per shape, in material order.
//!
//! [`Material::craft_units`] is what a substance COSTS and
//! [`Material::node_units`] is what a live node of it PAYS at a draw. No
//! material carries both, so essence cannot be manufactured from essence by
//! construction. Functional items have NO material: §7.3 prices function by
//! hand, in `unlock` and `alchemy`'s own small tables.

use crate::block::{BlockType, Essence};

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

	// ---- recipe-less: never crafted in any shape ----------------------
	/// What mined rock actually is (§7.1) — the mass Earth feedstock.
	Rubble,
	Gravel,
	DryGrass,
	/// Broadleaf canopy: filler.
	Leaf,
	/// Conifer canopy. A separate material from [`Self::Leaf`] because
	/// they are separate SUBSTANCES, and because one grid cell holds one
	/// block.
	PineLeaf,
	/// Revealed and drawn at the rock face, never crafted in any shape
	/// (§5.3).
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
	/// The three surface nodes and their spent faces (the essence
	/// redesign): the ores' standing exactly — never crafted in any shape,
	/// drawn where they stand, and the spent face is the trophy.
	Heartwood,
	DrainedHeartwood,
	Leystone,
	SpentLeystone,
	Springstone,
	SpentSpringstone,
}

impl Material {
	pub const COUNT: usize = 37;

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
		Material::Heartwood,
		Material::DrainedHeartwood,
		Material::Leystone,
		Material::SpentLeystone,
		Material::Springstone,
		Material::SpentSpringstone,
	];

	/// The blocks a craft of this substance consumes — the MATTER half of a
	/// feedstock-and-catalyst recipe (the essence redesign's craft shape).
	/// One cube of feedstock per craft; the shape's batch
	/// ([`crate::shapes::craft_batch`]) says how many pieces that cube
	/// cuts into. `None` = the substance is transmuted from essence alone
	/// (Metal, Crystal — nothing in the world IS them yet).
	///
	/// Bulk building stays affordable because of this split: the weight of
	/// a wall's price is the rock you quarried for it, and the essence line
	/// beside it is a CATALYST — small, universal, and the whole reason the
	/// currency has purpose.
	pub fn feedstock(self) -> Option<BlockType> {
		use Material as M;
		Some(match self {
			// The timbers are worked from the felled log.
			M::Wood | M::Lumber | M::DarkLumber | M::RedLumber => BlockType::Wood,
			// The mason's loop (§7.1): broken rock back into clean forms.
			M::Stone | M::StoneBrick | M::PolishedStone => BlockType::Rubble,
			// The sand-flavoured substances fuse from sand itself.
			M::Sand | M::Sandstone | M::SandstoneBrick | M::Glass => BlockType::Sand,
			// The loose grounds reshape from their own loose block.
			M::Dirt => BlockType::Dirt,
			M::Grass => BlockType::Grass,
			M::Mud => BlockType::Mud,
			M::Snow => BlockType::Snow,
			_ => return None,
		})
	}

	/// The essence half of a cube's price, before waste. For a substance
	/// with [`Self::feedstock`] this is the CATALYST — a small per-cube
	/// line, never prorated by the shape, since a craft consumes a whole
	/// feedstock cube whatever it cuts it into. For the essence-only
	/// substances it is the whole price. `None` = the bench cannot make
	/// this substance in any shape.
	///
	/// With bench extraction gone this is a pure SINK price: essence only
	/// ever enters the world at a node draw ([`Self::node_units`]) and only
	/// ever leaves it here, so no split can loop back on itself.
	pub fn craft_units(self) -> Option<&'static [(Essence, u32)]> {
		use Essence as E;
		use Material as M;
		Some(match self {
			// Catalysts, matching each substance's essence identity.
			M::Wood | M::Lumber | M::DarkLumber | M::RedLumber => &[(E::Vitae, 3)],
			M::Stone | M::Sand | M::Sandstone => &[(E::Terra, 3)],
			M::StoneBrick | M::PolishedStone | M::SandstoneBrick => &[(E::Terra, 3)],
			M::Glass => &[(E::Terra, 3)],
			M::Dirt => &[(E::Terra, 3), (E::Vitae, 1)],
			M::Grass => &[(E::Terra, 2), (E::Vitae, 1)],
			M::Mud => &[(E::Terra, 2), (E::Vitae, 1), (E::Unda, 1)],
			M::Snow => &[(E::Unda, 3)],
			// The essence-only substances: transmutation proper, a whole
			// cube's worth of the currency.
			M::Metal => &[(E::Ferrum, 27)],
			// Aether-bound, with a metal lattice to hold it: the only
			// repeatable Aether sink big enough to matter in the endgame.
			M::Crystal => &[(E::Aether, 18), (E::Ferrum, 9)],
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
			| M::DepletedAetherock
			| M::Heartwood
			| M::DrainedHeartwood
			| M::Leystone
			| M::SpentLeystone
			| M::Springstone
			| M::SpentSpringstone => return None,
		})
	}

	/// What one DRAW of this substance pays, per node cell, at full
	/// efficiency — the NODE yield basis (the essence redesign: essence is
	/// found, not ground). Non-empty exactly for the live node kinds;
	/// everything else answers empty, because bench extraction is gone and
	/// a siphon draw is the only way matter becomes essence.
	pub fn node_units(self) -> &'static [(Essence, u32)] {
		use Essence as E;
		use Material as M;
		match self {
			// The ore tiers, drawn whole at the rock face: concentrates,
			// rising steeply with depth.
			M::Emberore => &[(E::Ferrum, 18)],
			M::Duskstone => &[(E::Ferrum, 36)],
			// The first tier that pays Aether at all, and the reason the
			// Resonant Pick's Aether line is payable by anyone who has
			// earned the pick: this vein reveals at exactly the Mining
			// level the pick's wider swing opens at, so the supply arrives
			// with the demand instead of a whole ladder later. Before this
			// row existed the only reliable Aether in the game sat behind
			// the deepest tier, and the pick was unlocked for a long
			// stretch in which it could not be built.
			M::Glowvein => &[(E::Ferrum, 60), (E::Aether, 1)],
			M::Aetherock => &[(E::Ferrum, 90), (E::Aether, 9)],
			// The surface nodes: concentrates like the ore tiers, each in
			// its own single essence — a find must out-pay the bulk matter
			// around it or it is not one.
			M::Heartwood => &[(E::Vitae, 36)],
			M::Leystone => &[(E::Terra, 36)],
			M::Springstone => &[(E::Unda, 36)],
			// Everything else — bulk matter, worked blocks, spent faces —
			// yields nothing: a spent node is a find already taken, and a
			// block is a block. The bench transmutes; only the siphon
			// makes essence.
			_ => &[],
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
			M::Heartwood => "HEARTWOOD",
			M::DrainedHeartwood => "DRAINED HEARTWOOD",
			M::Leystone => "LEYSTONE",
			M::SpentLeystone => "SPENT LEYSTONE",
			M::Springstone => "SPRINGSTONE",
			M::SpentSpringstone => "SPENT SPRINGSTONE",
		}
	}
}

/// Prorate a whole-cube essence split down to a shape's `units`, rounding
/// each line UP — the craft side, where the player pays.
///
/// A whole cube is the identity, which is what keeps every
/// raw-block recipe exactly what it was before the grid. (The DOWN-rounded
/// yield twin retired with bench extraction: nothing is prorated for a
/// payout any more — a node is drawn whole, and only a node pays.)
pub fn cost_split(lines: &[(Essence, u32)], units: u32) -> Vec<(Essence, u32)> {
	let subs = crate::shapes::SUBS_PER_BLOCK;
	lines
		.iter()
		.map(|&(e, n)| {
			// A listed line never vanishes: the thinnest shape of a
			// one-unit material still costs its one unit.
			(e, (n * units).div_ceil(subs).max(1))
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

	/// The node yield basis is exactly the live node roster: only a find
	/// pays essence, a spent face pays nothing, and nothing with a yield
	/// has a recipe — which is what makes essence un-manufacturable by
	/// construction now that the bench only ever consumes it.
	#[test]
	fn only_nodes_pay_and_no_node_is_craftable() {
		for &m in &Material::ALL {
			let total: u32 = m.node_units().iter().map(|&(_, n)| n).sum();
			let is_node = matches!(
				m,
				Material::Emberore
					| Material::Duskstone
					| Material::Glowvein
					| Material::Aetherock
					| Material::Heartwood
					| Material::Leystone
					| Material::Springstone
			);
			if is_node {
				assert!(m.craft_units().is_none(), "{m:?} must never be craftable");
				assert!(total > 0, "{m:?} is a find and must pay something");
			} else {
				assert_eq!(total, 0, "{m:?} pays essence without being a node");
			}
		}
	}

	/// A whole cube prorates to itself — which is what keeps the
	/// essence-only substances' cube recipes exactly their split.
	#[test]
	fn a_whole_cube_prorates_to_itself() {
		for &m in &Material::ALL {
			let subs = crate::shapes::SUBS_PER_BLOCK;
			if let Some(c) = m.craft_units() {
				assert_eq!(cost_split(c, subs), c.to_vec(), "{m:?} cost");
			}
		}
	}

	/// The feedstock-and-catalyst shape, held together: every craftable
	/// substance either names a world-handed feedstock cube plus a SMALL
	/// catalyst, or transmutes from essence alone at a whole cube's worth
	/// — never both readings at once, and never a feedstock the bench
	/// itself is the only source of (which would nest the bootstrap
	/// deadlock one table deeper).
	#[test]
	fn feedstock_recipes_carry_catalysts_and_essence_recipes_carry_cubes() {
		for &m in &Material::ALL {
			let Some(units) = m.craft_units() else {
				assert!(m.feedstock().is_none(), "{m:?} feeds an uncraftable");
				continue;
			};
			let total: u32 = units.iter().map(|&(_, n)| n).sum();
			match m.feedstock() {
				Some(f) => {
					assert!(!f.is_bench_only(), "{m:?} feeds on bench-only {f:?}");
					assert!(f.is_slottable(), "{m:?} feeds on unholdable {f:?}");
					assert!(
						total < crate::shapes::SUBS_PER_BLOCK / 3,
						"{m:?}'s catalyst is a price, not a pinch"
					);
				}
				None => assert!(
					total >= crate::shapes::SUBS_PER_BLOCK,
					"{m:?} transmutes from essence alone and must cost a cube's worth"
				),
			}
		}
		// The rosters that make the split meaningful, named.
		assert_eq!(Material::Stone.feedstock(), Some(BlockType::Rubble));
		assert_eq!(Material::Lumber.feedstock(), Some(BlockType::Wood));
		assert_eq!(Material::Glass.feedstock(), Some(BlockType::Sand));
		assert_eq!(Material::Metal.feedstock(), None);
		assert_eq!(Material::Crystal.feedstock(), None);
	}
}
