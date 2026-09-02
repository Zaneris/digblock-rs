//! The MATERIAL axis of the material × shape grid.
//!
//! A structural block is a *substance* worked into a *form*: Stone stairs are
//! Stone in the Stairs shape. A block declares its material once (the `mat`
//! builder in `block.rs`) and four things then DERIVE from the pair rather than
//! being tabled separately — the unlock level (`max(shape, material)`), the
//! craft cost, the earned half of the strength wall, and the transmute tab's
//! families.
//!
//! Substances form REFINEMENT CHAINS rooted in the world: a refined cube is
//! made from its parent's cube ([`Material::refined_from`]) beside a catalyst
//! ([`Material::refine_units`]), and every shape is cut from its own cube.
//! [`Material::node_units`] is what a live node PAYS at a draw; no material
//! carries both a recipe and a yield, so essence cannot be manufactured from
//! essence by construction. Functional items have NO material: priced by hand.

use crate::block::{BlockType, Essence};

/// A substance the bench can work, in unlock order (which is also the order members appear on the
/// transmute tab).
///
/// Some rows have no authored block yet: the level and the essence split ARE the design, so a later
/// catalog wave should be a block row and a PNG rather than another table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Material {
	/// The felled trunk — the starter timber, and the one thing a fresh player can already shape.
	Wood,
	Dirt,
	Sand,
	/// Planed timber, light: the joinery material.
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
	/// Stained panes: separate SUBSTANCES, not styles — a tint is what its light does to a room.
	RedGlass,
	GreenGlass,
	BlueGlass,
	AmberGlass,
	/// Planed timber, red.
	RedLumber,
	PolishedStone,
	SandstoneBrick,
	/// Refined ore — scarce by construction, the prestige substance.
	Metal,
	/// Grown from Aether: the endgame material, and it glows.
	Crystal,

	// ---- the rest of the raw matter, and the recipe-less rows ----
	/// What mined rock actually is: broken stone, the mason's feedstock, shaped as it comes.
	Rubble,
	Gravel,
	DryGrass,
	/// Broadleaf canopy: filler.
	Leaf,
	/// Conifer canopy. A separate material from [`Self::Leaf`] — one grid cell holds one block.
	PineLeaf,
	/// Revealed and drawn at the rock face, never crafted in any shape.
	Emberore,
	Duskstone,
	Glowvein,
	Aetherock,
	/// A tapped-out vein face, superseded by the per-tier rocks below; kept so old cells decode.
	DepletedStone,
	/// The four spent vein faces — one per tier, so a trophy says which seam you worked.
	DepletedEmberore,
	DepletedDuskstone,
	DepletedGlowvein,
	DepletedAetherock,
	/// The three surface nodes and their spent faces — the ores' standing exactly, never crafted.
	Heartwood,
	DrainedHeartwood,
	Leystone,
	SpentLeystone,
	Springstone,
	SpentSpringstone,
}

impl Material {
	pub const COUNT: usize = 41;

	/// Every material in unlock order — the transmute tab reads members in
	/// this order, so a family's tiles run from cheapest to rarest: the raw
	/// matter first (its shapes are the day-one recipes), then each
	/// refinement rung, then everything with no recipe at all.
	pub const ALL: [Material; Self::COUNT] = [
		Material::Dirt,
		Material::Wood,
		Material::Rubble,
		Material::Sand,
		Material::Snow,
		Material::Grass,
		Material::Mud,
		Material::Gravel,
		Material::Lumber,
		Material::Stone,
		Material::DarkLumber,
		Material::StoneBrick,
		Material::Sandstone,
		Material::Glass,
		Material::RedGlass,
		Material::GreenGlass,
		Material::BlueGlass,
		Material::AmberGlass,
		Material::RedLumber,
		Material::PolishedStone,
		Material::SandstoneBrick,
		Material::Metal,
		Material::Crystal,
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

	/// The block this substance is REFINED from — the matter half of its cube recipe, one parent
	/// cube per craft. `None` for the raw substances the world hands out (their cubes are never
	/// crafted) and for the two transmuted from essence alone (Metal, Crystal). One parent per
	/// substance makes the chains a tree rooted in the world, pinned to bottom out within a few
	/// links; the child's level never precedes the parent's, so every link is craftable in turn.
	pub fn refined_from(self) -> Option<BlockType> {
		use Material as M;
		Some(match self {
			// Planed from the felled log, then the coloured timbers from the plain plank.
			M::Lumber => BlockType::Wood,
			M::DarkLumber | M::RedLumber => BlockType::Lumber,
			// The mason's loop: broken rock back into clean forms, then dressed, then polished.
			M::Stone => BlockType::Rubble,
			M::StoneBrick => BlockType::Stone,
			M::PolishedStone => BlockType::StoneBrick,
			// The sand-flavoured substances fuse from sand itself; the tints from clean glass.
			M::Sandstone | M::Glass => BlockType::Sand,
			M::SandstoneBrick => BlockType::Sandstone,
			M::RedGlass | M::GreenGlass | M::BlueGlass | M::AmberGlass => BlockType::Glass,
			_ => return None,
		})
	}

	/// The essence half of the CUBE recipe, before waste: the catalyst beside a refined
	/// substance's parent block, or the whole price of one transmuted from essence alone. `None` =
	/// the bench cannot make this substance's cube (raw matter, and everything with no recipe).
	/// Climbs with the chain's depth, so a rung is a price and not just a look, and the second
	/// rung of each family names a second essence so the mid ladder spends every faucet. A pure
	/// SINK: essence enters only at a node draw ([`Self::node_units`]) and leaves only here.
	pub fn refine_units(self) -> Option<&'static [(Essence, u32)]> {
		use Essence as E;
		use Material as M;
		Some(match self {
			// Tier 1: a third of a node cell at the first level.
			M::Lumber => &[(E::Vitae, 4)],
			M::Stone | M::Sandstone => &[(E::Terra, 5)],
			M::Glass => &[(E::Terra, 6)],
			// Tier 2: dressed, coloured, tinted.
			M::StoneBrick | M::SandstoneBrick => &[(E::Terra, 8)],
			M::DarkLumber => &[(E::Vitae, 6), (E::Terra, 2)],
			M::RedLumber => &[(E::Vitae, 6), (E::Ferrum, 2)],
			M::RedGlass => &[(E::Terra, 4), (E::Ferrum, 4)],
			M::GreenGlass => &[(E::Terra, 4), (E::Vitae, 4)],
			M::BlueGlass => &[(E::Terra, 4), (E::Unda, 4)],
			M::AmberGlass => &[(E::Terra, 8)],
			// Tier 3: a whole node cell's worth.
			M::PolishedStone => &[(E::Terra, 14)],
			// The essence-only substances: transmutation proper, more
			// than a cube's worth of the currency.
			M::Metal => &[(E::Ferrum, 45)],
			// Aether-bound, with a metal lattice to hold it: the only
			// repeatable Aether sink big enough to matter in the endgame.
			M::Crystal => &[(E::Aether, 27), (E::Ferrum, 18)],
			_ => return None,
		})
	}

	/// The substance's primary essence — the family its catalyst leads with, and what cutting a
	/// refined cube into a shape costs a unit of per tier ([`Self::shape_units`]). Every substance
	/// answers, raw ones included, so the grid never special-cases one.
	pub fn essence(self) -> Essence {
		use Essence as E;
		use Material as M;
		match self {
			M::Wood
			| M::Lumber
			| M::DarkLumber
			| M::RedLumber
			| M::Grass
			| M::DryGrass
			| M::Mud
			| M::Leaf
			| M::PineLeaf
			| M::Heartwood
			| M::DrainedHeartwood => E::Vitae,
			M::Dirt
			| M::Sand
			| M::Stone
			| M::StoneBrick
			| M::PolishedStone
			| M::Sandstone
			| M::SandstoneBrick
			| M::Glass
			| M::RedGlass
			| M::GreenGlass
			| M::BlueGlass
			| M::AmberGlass
			| M::Rubble
			| M::Gravel
			| M::DepletedStone
			| M::DepletedEmberore
			| M::DepletedDuskstone
			| M::DepletedGlowvein
			| M::DepletedAetherock
			| M::Leystone
			| M::SpentLeystone => E::Terra,
			M::Snow | M::Springstone | M::SpentSpringstone => E::Unda,
			M::Metal | M::Emberore | M::Duskstone | M::Glowvein => E::Ferrum,
			M::Crystal | M::Aetherock => E::Aether,
		}
	}

	/// Raw matter: the world hands the cube out and the bench never makes one. Its shapes are cut
	/// for free — nothing was refined, so there is nothing to catalyse — which is the rule the
	/// whole ladder rests on: essence enters a price the moment anything is refined, never before.
	pub fn is_raw(self) -> bool {
		self.refined_from().is_none() && self.refine_units().is_none()
	}

	/// How many refinements deep this substance sits: 0 for raw matter and for the essence-only
	/// substances (transmuted, not refined), one more than its parent otherwise. Terminates because
	/// the chains are pinned to.
	pub fn tier(self) -> u32 {
		match self.refined_from().and_then(|p| p.material()) {
			Some(p) => 1 + p.tier(),
			None => 0,
		}
	}

	/// The essence a SHAPE of this substance costs per craft, beside its own cube: nothing for
	/// raw matter, the tier in units of the primary essence for a refined one, and a hand-set
	/// pinch for the essence-only two (a tier of zero would cut metal for free).
	pub fn shape_units(self) -> Vec<(Essence, u32)> {
		let n = match self {
			Material::Metal => 3,
			Material::Crystal => 2,
			_ => self.tier(),
		};
		if n == 0 {
			Vec::new()
		} else {
			vec![(self.essence(), n)]
		}
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
			// The first tier also pays Terra: it sits at surface-cave
			// depth, where the earth-essence line is the surface hunt's,
			// so the miner's first vein feeds the same crafts the first
			// leystone does.
			M::Emberore => &[(E::Ferrum, 18), (E::Terra, 9)],
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

	/// What the substance is CALLED, stored in the one case it is
	/// written in. A screen that shouts its labels calls
	/// `to_uppercase` at the point it draws — casing is styling, and
	/// doing it to the stored form would be a transform on text a
	/// translation has to own (and wrong in Turkish besides).
	pub fn name(self) -> &'static str {
		use Material as M;
		match self {
			M::Wood => "Wood",
			M::Dirt => "Dirt",
			M::Sand => "Sand",
			M::Lumber => "Lumber",
			M::Stone => "Stone",
			M::DarkLumber => "Dark Lumber",
			M::StoneBrick => "Stone Brick",
			M::Sandstone => "Sandstone",
			M::Snow => "Snow",
			M::Grass => "Grass",
			M::Mud => "Mud",
			M::Glass => "Glass",
			M::RedGlass => "Red Glass",
			M::GreenGlass => "Green Glass",
			M::BlueGlass => "Blue Glass",
			M::AmberGlass => "Amber Glass",
			M::RedLumber => "Red Lumber",
			M::PolishedStone => "Polished Stone",
			M::SandstoneBrick => "Sandstone Brick",
			M::Metal => "Metal",
			M::Crystal => "Crystal",
			M::Rubble => "Rubble",
			M::Gravel => "Gravel",
			M::DryGrass => "Dry Grass",
			M::Leaf => "Leaves",
			M::PineLeaf => "Pine Leaves",
			M::Emberore => "Emberore",
			M::Duskstone => "Duskstone",
			M::Glowvein => "Glowvein",
			M::Aetherock => "Aetherock",
			M::DepletedStone => "Depleted Stone",
			M::DepletedEmberore => "Spent Emberore",
			M::DepletedDuskstone => "Spent Duskstone",
			M::DepletedGlowvein => "Spent Glowvein",
			M::DepletedAetherock => "Spent Aetherock",
			M::Heartwood => "Heartwood",
			M::DrainedHeartwood => "Drained Heartwood",
			M::Leystone => "Leystone",
			M::SpentLeystone => "Spent Leystone",
			M::Springstone => "Springstone",
			M::SpentSpringstone => "Spent Springstone",
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::ShapeFamily;

	/// `ALL` is the roster every derived table walks — a material listed
	/// twice would double a family's tiles, and one left out would silently
	/// vanish from the transmute tab.
	#[test]
	fn all_lists_every_material_once() {
		// `COUNT` is written by hand, so `ALL`'s length proves nothing —
		// what the type cannot catch is a duplicate squeezing a material
		// out, and a NEW variant never added at all. The exhaustive match
		// is the compile gate for the second: a 42nd material fails to
		// build here until this test — the `ALL` sweep — is touched.
		for (i, &m) in Material::ALL.iter().enumerate() {
			match m {
				Material::Wood
				| Material::Dirt
				| Material::Sand
				| Material::Lumber
				| Material::Stone
				| Material::DarkLumber
				| Material::StoneBrick
				| Material::Sandstone
				| Material::Snow
				| Material::Grass
				| Material::Mud
				| Material::Glass
				| Material::RedGlass
				| Material::GreenGlass
				| Material::BlueGlass
				| Material::AmberGlass
				| Material::RedLumber
				| Material::PolishedStone
				| Material::SandstoneBrick
				| Material::Metal
				| Material::Crystal
				| Material::Rubble
				| Material::Gravel
				| Material::DryGrass
				| Material::Leaf
				| Material::PineLeaf
				| Material::Emberore
				| Material::Duskstone
				| Material::Glowvein
				| Material::Aetherock
				| Material::DepletedStone
				| Material::DepletedEmberore
				| Material::DepletedDuskstone
				| Material::DepletedGlowvein
				| Material::DepletedAetherock
				| Material::Heartwood
				| Material::DrainedHeartwood
				| Material::Leystone
				| Material::SpentLeystone
				| Material::Springstone
				| Material::SpentSpringstone => {}
			}
			assert!(
				!Material::ALL[..i].contains(&m),
				"{m:?} listed twice in ALL"
			);
			assert!(!m.name().is_empty(), "{m:?} has no label");
		}
		// Unlock order, which is what the tab's tiles read: a material
		// never lists before one that opens earlier.
		let levels: Vec<u32> = Material::ALL
			.iter()
			.filter_map(|&m| crate::unlock::material(m))
			.collect();
		assert!(levels.windows(2).all(|w| w[0] <= w[1]), "{levels:?}");
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
				assert!(m.refine_units().is_none(), "{m:?} must never be craftable");
				assert!(m.is_raw(), "{m:?} must never be craftable");
				assert!(total > 0, "{m:?} is a find and must pay something");
			} else {
				assert_eq!(total, 0, "{m:?} pays essence without being a node");
			}
		}
	}

	/// The refinement chains, held together: every refined substance names a parent CUBE that is
	/// itself raw or refined, the chain reaches the world within a few links with no cycle, the
	/// catalyst is a price that climbs with the tier and never a cube's worth, and the essence-only
	/// two cost more than a cube's worth with no parent at all. A raw substance's shapes are free.
	#[test]
	fn refinement_chains_bottom_out_in_the_world() {
		for &m in &Material::ALL {
			// Walk the chain: a parent is always a full cube of some material.
			let mut depth = 0;
			let mut at = m;
			while let Some(p) = at.refined_from() {
				let (pm, ps) = p
					.grid_cell()
					.unwrap_or_else(|| panic!("{m:?} is refined from {p:?}, which is no cube"));
				assert_eq!(ps, ShapeFamily::Block, "{m:?} is refined from a shape");
				assert!(
					m.refine_units().is_some(),
					"{m:?} has a parent and no catalyst"
				);
				depth += 1;
				assert!(depth <= 4, "{m:?}'s chain never reaches the world");
				at = pm;
			}
			assert!(
				at.is_raw() || at.refine_units().is_some(),
				"{m:?} bottoms out in {at:?}"
			);
			assert_eq!(m.tier(), depth, "{m:?}");
			match (m.refined_from(), m.refine_units()) {
				(Some(_), Some(units)) => {
					let total: u32 = units.iter().map(|&(_, n)| n).sum();
					assert!(
						total < crate::shapes::SUBS_PER_BLOCK,
						"{m:?}'s catalyst is a cube's worth, not a catalyst"
					);
					assert!(total >= 4, "{m:?}'s catalyst is a pinch, not a price");
					assert_eq!(units[0].0, m.essence(), "{m:?} leads with its own essence");
					assert!(!m.shape_units().is_empty(), "{m:?}'s shapes cost nothing");
				}
				(None, Some(units)) => {
					let total: u32 = units.iter().map(|&(_, n)| n).sum();
					assert!(
						total >= crate::shapes::SUBS_PER_BLOCK,
						"{m:?} transmutes from essence alone and must cost a cube's worth"
					);
					assert!(!m.shape_units().is_empty(), "{m:?}'s shapes cost nothing");
				}
				(None, None) => {
					assert!(m.is_raw());
					assert!(
						m.shape_units().is_empty(),
						"{m:?} is raw and its shapes cost"
					);
				}
				(Some(_), None) => unreachable!("asserted above"),
			}
		}
		// The catalyst climbs the chain: a rung costs more than the one below it.
		for &m in &Material::ALL {
			if let Some(p) = m.refined_from().and_then(|p| p.material())
				&& let (Some(mine), Some(theirs)) = (m.refine_units(), p.refine_units())
			{
				let sum = |u: &[(Essence, u32)]| u.iter().map(|&(_, n)| n).sum::<u32>();
				assert!(
					sum(mine) > sum(theirs),
					"{m:?} is cheaper than {p:?} below it"
				);
			}
		}
		// The rosters that make the chains meaningful, named.
		assert_eq!(Material::Stone.refined_from(), Some(BlockType::Rubble));
		assert_eq!(Material::StoneBrick.refined_from(), Some(BlockType::Stone));
		assert_eq!(
			Material::PolishedStone.refined_from(),
			Some(BlockType::StoneBrick)
		);
		assert_eq!(Material::Lumber.refined_from(), Some(BlockType::Wood));
		assert_eq!(Material::RedLumber.refined_from(), Some(BlockType::Lumber));
		assert_eq!(Material::Glass.refined_from(), Some(BlockType::Sand));
		assert_eq!(Material::BlueGlass.refined_from(), Some(BlockType::Glass));
		assert_eq!(Material::Metal.refined_from(), None);
		assert_eq!(Material::Crystal.refined_from(), None);
		assert_eq!(
			[
				Material::Dirt.tier(),
				Material::Stone.tier(),
				Material::StoneBrick.tier(),
				Material::PolishedStone.tier(),
				Material::Metal.tier()
			],
			[0, 1, 2, 3, 0]
		);
		for m in [
			Material::Dirt,
			Material::Wood,
			Material::Rubble,
			Material::Sand,
		] {
			assert!(m.is_raw(), "{m:?}");
		}
	}
}
