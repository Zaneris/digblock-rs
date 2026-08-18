//! The unified NODE layer — every world source of essence behind one
//! question (the essence redesign: **essence is found, not ground**).
//!
//! A node is a derived find: a salted hash or a pure worldgen query says
//! where one stands, the mesh stamp reveals it at a skill level, the
//! siphon draws it in place, and the draw leaves the kind's own SPENT
//! face. Nothing is stored: worldgen still places the host block (Stone,
//! Wood, Sand), the journal never carries a node cell, and the golden
//! worldgen hashes do not move — so nodes appear retroactively in worlds
//! that already exist, exactly as ore always has.
//!
//! Seven live faces, two reveal skills:
//!
//! - the four ORE tiers ([`ore`]), in pristine deep Stone, revealed by
//!   MINING ([`crate::unlock::reveal`]);
//! - **Leystone**, riding the same deposit stream in the shallow band the
//!   deep tiers vacate, **Heartwood** in the trunks of worldgen trees, and
//!   **Springstone** in pristine lake and river beds — all revealed by
//!   ALCHEMY ([`crate::unlock::alchemy_reveal`]), so Alchemy owns the
//!   surface hunt the way Mining owns the deep one.
//!
//! **The unedited rule is the anti-dupe**, unchanged and load-bearing:
//! a node manifests only where the cell still holds its pristine host and
//! has never been written. `JournalIndex::record` only ever inserts, so a
//! spent node cannot be regrown by placing the host back.

use crate::block::BlockType;
use crate::worldgen::ore;

/// Every live node face. The four Mining tiers first, then the three
/// Alchemy surface nodes — [`BlockType::is_node_block`] is the block
/// layer's mirror of this roster, pinned against it.
pub const NODES: [BlockType; 7] = [
	BlockType::Emberore,
	BlockType::Duskstone,
	BlockType::Glowvein,
	BlockType::Aetherock,
	BlockType::Leystone,
	BlockType::Heartwood,
	BlockType::Springstone,
];

/// The two levels every node question is asked at. A plain pair rather
/// than a `SkillSet`, because the mesh stamp crosses a worker boundary
/// carrying levels, not XP totals.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Levels {
	pub mining: u32,
	pub alchemy: u32,
}

impl Levels {
	#[inline]
	pub fn of(skills: &crate::skills::SkillSet) -> Self {
		Self {
			mining: skills.level(crate::skills::Skill::Mining),
			alchemy: skills.level(crate::skills::Skill::Alchemy),
		}
	}
}

/// Does this player SEE this node kind at all? Each kind answers to
/// exactly one skill's reveal table; a block in neither table is not a
/// node and is never revealed.
#[inline]
fn reveal_met(node: BlockType, levels: Levels) -> bool {
	if let Some(need) = crate::unlock::reveal(node) {
		levels.mining >= need
	} else if let Some(need) = crate::unlock::alchemy_reveal(node) {
		levels.alchemy >= need
	} else {
		false
	}
}

/// THE visibility question, asked identically by the mesh stamp, the
/// client's dig prediction and the server's validation: what node does
/// this cell show to a player at these levels?
///
/// `cell` is the world's CURRENT cell value and `edited` says whether that
/// coordinate has ever been written. Both gates matter and neither is
/// redundant: the host-id test is what makes a spent node stay spent, and
/// the edit test is what stops someone placing the host back into the
/// coordinate to regrow it. The host id also routes the query — only a
/// Stone cell can hold a deposit, only a Wood cell heartwood, only a Sand
/// cell a spring — so the wrong-host case costs one integer compare.
pub fn revealed(
	cell: u16,
	at: (i32, i32, i32),
	seed: f64,
	levels: Levels,
	edited: bool,
) -> Option<BlockType> {
	if edited {
		return None;
	}
	let id = crate::chunk::cell_id(cell);
	let node = if id == BlockType::Stone as u16 {
		ore::ore_at(at.0, at.1, at.2, seed)?
	} else if id == BlockType::Wood as u16 {
		heartwood_at(at.0, at.1, at.2, seed).then_some(BlockType::Heartwood)?
	} else if id == BlockType::Sand as u16 {
		springstone_at(at.0, at.1, at.2, seed).then_some(BlockType::Springstone)?
	} else {
		return None;
	};
	reveal_met(node, levels).then_some(node)
}

/// What a dig on one cell actually IS, for this player with this tool in
/// hand (SKILLS-DESIGN §5.3). The one question the client's prediction and
/// the server's validation both ask, so they cannot disagree about whether
/// an action was a dig, a draw, or a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DigMode {
	/// Ordinary: the cell breaks and drops what it drops.
	Break,
	/// A revealed node with the siphon in hand: the cell becomes the
	/// kind's [`spent_block`] and the node's ESSENCE lands in the player's
	/// stocks. Drawing a node never excavates, so the mountainside — and
	/// the tree, and the lake bed — keeps its shape.
	///
	/// Essence rather than a block, which is the whole of what the tool is
	/// for: a siphon draws a find out where it stands, so a node never
	/// becomes something you carry to a bench. What you can carry home is
	/// the spent face afterwards. The yield is
	/// `alchemy::field_yield`, so the Alchemy level scales every draw the
	/// same way wherever it happens.
	Extract(BlockType),
	/// A revealed node and no siphon. Refused at `DigStart`, not at the
	/// completing edit — if you can SEE the find, the game will not let
	/// you spend it on an ordinary dig by accident, and it says so before
	/// you hold the button through a full timer for nothing.
	NeedsSiphon,
}

pub fn dig_mode(
	cell: u16,
	at: (i32, i32, i32),
	seed: f64,
	levels: Levels,
	edited: bool,
	active_tool: BlockType,
) -> DigMode {
	match revealed(cell, at, seed, levels, edited) {
		// Below the reveal there is nothing here to protect: that player's
		// client and server agree the cell is its host, so it digs as the
		// host and the node is spent. You dig what you can see.
		None => DigMode::Break,
		Some(node) if active_tool == BlockType::EssenceSiphon => DigMode::Extract(node),
		Some(_) => DigMode::NeedsSiphon,
	}
}

/// The block a dig is TIMED and paid XP as. A node lives in a cell that
/// still holds its host — the reveal is a mesh stamp, not a world write —
/// so without this a draw would take the host's own hold time and pay the
/// host's XP however rich the find was, and the node rows' hardnesses
/// would be dead numbers on blocks nobody ever digs.
///
/// It does not decide what the dig CREDITS: a draw pays essence, not a
/// block (see [`DigMode::Extract`]).
#[inline]
pub fn dig_block(mode: DigMode, cell_block: BlockType) -> BlockType {
	match mode {
		DigMode::Extract(node) => node,
		_ => cell_block,
	}
}

/// The face a drawn node leaves standing — the kind's own spent block.
///
/// One per kind rather than one grey face for all: a worked find should
/// still read as the find you worked, and the block you break out of it
/// afterwards is a trophy of that kind and no other. It is also the ONLY
/// thing a node ever puts in a player's hands, since the live face is
/// never an item ([`BlockType::is_slottable`]).
///
/// Total on [`NODES`], so a caller holding a [`DigMode::Extract`] cannot
/// be handed one it has no face for; anything else is not a node and
/// answers `None`.
#[inline]
pub fn spent_block(node: BlockType) -> Option<BlockType> {
	Some(match node {
		BlockType::Emberore => BlockType::DepletedEmberore,
		BlockType::Duskstone => BlockType::DepletedDuskstone,
		BlockType::Glowvein => BlockType::DepletedGlowvein,
		BlockType::Aetherock => BlockType::DepletedAetherock,
		BlockType::Leystone => BlockType::SpentLeystone,
		BlockType::Heartwood => BlockType::DrainedHeartwood,
		BlockType::Springstone => BlockType::SpentSpringstone,
		_ => return None,
	})
}

// ---- the surface placements ------------------------------------------

/// Salt for the per-TREE heartwood roll, distinct from every decoration
/// and deposit stream.
const HEARTWOOD_SALT: u64 = 0x8ea7_c0de_57ee_d001;

/// The share of ordinary trees whose trunk carries a heart, in 100ths —
/// the surface counterpart of `ore`'s deposit rate: how far a forager
/// walks between finds.
const HEARTWOOD_IN_100: u64 = 12;

/// How many trunk cells of an ordinary heartwood tree manifest, counted
/// up from the cell above the root — the heart sits low, at eye height,
/// where a standing player's crosshair naturally lands.
const HEART_CELLS: i32 = 2;

/// Does this cell hold heartwood — the living core of a worldgen tree?
///
/// Pure in `(x, y, z, seed)` like [`ore::ore_at`]: the trunk comes from
/// [`crate::decoration::trunk_at`] (itself a pure candidate rule), and
/// whether a tree HAS a heart is one salted roll on the tree's anchor
/// column, so every cell of one trunk agrees. A landmark tree always has
/// one, trunk entire — the rare giant is rare enough that meeting one
/// should always be worth the detour — while an ordinary tree rolls, and
/// carries its heart in the [`HEART_CELLS`] above the root.
///
/// Callers still need the LIVE cell to hold Wood and be unedited
/// ([`revealed`] checks both): a canopy written first, a cliff that left
/// the trunk cell unwritten, or a felled-and-rebuilt trunk are all cells
/// this function cannot see, and the host check is what keeps the stamp,
/// the dig and the draw agreeing anyway.
pub fn heartwood_at(x: i32, y: i32, z: i32, seed: f64) -> bool {
	let Some(t) = crate::decoration::trunk_at(x, z, seed) else {
		return false;
	};
	if t.landmark {
		return (t.y0..=t.y1).contains(&y);
	}
	let heart = (t.y0 + 1)..=(t.y0 + HEART_CELLS).min(t.y1);
	heart.contains(&y)
		&& ore::mix(t.anchor.0, 0, t.anchor.1, seed.to_bits(), HEARTWOOD_SALT) % 100
			< HEARTWOOD_IN_100
}

/// Salt for the springstone stream.
const SPRING_SALT: u64 = 0x5b41_0e57_04e5_ea11;

/// The share of lake- and river-bed cells that carry a spring, in 256ths.
const SPRING_IN_256: u64 = 18;

/// Does this cell hold a springstone — a charged stone in the pristine
/// bed of standing fresh water?
///
/// The rate rolls first because it is a hash where the bed test is five
/// field evaluations ([`crate::worldgen::column_shape`]); only the cells
/// that pass pay for the classification. Lake and river beds only — the
/// ocean is salt, and a marsh puddle is not a spring — and only the bed
/// surface cell itself, which is what puts the find under water and makes
/// water features destinations.
pub fn springstone_at(x: i32, y: i32, z: i32, seed: f64) -> bool {
	if ore::mix(x, 0, z, seed.to_bits(), SPRING_SALT) & 0xFF >= SPRING_IN_256 {
		return false;
	}
	let s = crate::worldgen::column_shape(x, z, seed);
	use crate::worldgen::SurfaceKind;
	matches!(s.surface, SurfaceKind::Lake | SurfaceKind::River)
		&& s.water > s.height
		&& y == s.height
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::make_cell;
	use crate::worldgen::SurfaceKind;

	const SEED: f64 = 42.0;

	fn maxed() -> Levels {
		Levels {
			mining: crate::skills::MAX_LEVEL,
			alchemy: crate::skills::MAX_LEVEL,
		}
	}

	/// Find one manifest cell of each surface kind by walking the world —
	/// walked rather than named, so a retune of a rate or a band moves the
	/// tests with it.
	fn find_heartwood(seed: f64) -> (i32, i32, i32) {
		for cz in -64..64 {
			for cx in -64..64 {
				for x in cx * 8..cx * 8 + 8 {
					for z in cz * 8..cz * 8 + 8 {
						let Some(t) = crate::decoration::trunk_at(x, z, seed) else {
							continue;
						};
						for y in t.y0..=t.y1 {
							if heartwood_at(x, y, z, seed) {
								return (x, y, z);
							}
						}
					}
				}
			}
		}
		panic!("no heartwood in the sampled world");
	}

	fn find_springstone(seed: f64) -> (i32, i32, i32) {
		for x in (-2048..2048).step_by(3) {
			for z in (-2048..2048).step_by(3) {
				let s = crate::worldgen::column_shape(x, z, seed);
				if !matches!(s.surface, SurfaceKind::Lake | SurfaceKind::River)
					|| s.water <= s.height
				{
					continue;
				}
				if springstone_at(x, s.height, z, seed) {
					return (x, s.height, z);
				}
			}
		}
		panic!("no springstone in the sampled world");
	}

	/// Purity: same inputs, same answer — the property every side of the
	/// wire depends on.
	#[test]
	fn surface_placement_is_pure() {
		let hw = find_heartwood(SEED);
		let sp = find_springstone(SEED);
		for _ in 0..3 {
			assert!(heartwood_at(hw.0, hw.1, hw.2, SEED));
			assert!(springstone_at(sp.0, sp.1, sp.2, SEED));
		}
		// A different seed is a different world: the exact cells found in
		// one seed are astronomically unlikely to all recur in another.
		let moved = heartwood_at(hw.0, hw.1, hw.2, 7.5) && springstone_at(sp.0, sp.1, sp.2, 7.5);
		assert!(!moved, "the seed reaches the surface nodes");
	}

	/// The anti-dupe, for every node kind: a node is spent by the write
	/// that took it, and cannot be regrown by putting the host back.
	#[test]
	fn no_node_manifests_in_an_edited_cell() {
		let cases = [
			(BlockType::Wood, find_heartwood(SEED), BlockType::Heartwood),
			(
				BlockType::Sand,
				find_springstone(SEED),
				BlockType::Springstone,
			),
		];
		for (host, at, node) in cases {
			let cell = host as u16;
			assert_eq!(
				revealed(cell, at, SEED, maxed(), false),
				Some(node),
				"pristine {node:?}"
			);
			// Somebody put the host back where the node was.
			assert_eq!(revealed(cell, at, SEED, maxed(), true), None, "{node:?}");
			// The spent face itself never re-reads as a node.
			let spent = spent_block(node).expect("a kind") as u16;
			assert_eq!(revealed(spent, at, SEED, maxed(), false), None);
			// Nor does anything else standing in that coordinate.
			for b in [BlockType::Air, BlockType::Rubble, node] {
				assert_eq!(revealed(b as u16, at, SEED, maxed(), false), None, "{b:?}");
			}
			// A shaped cell in the same coordinate is not the host either.
			assert_eq!(
				revealed(make_cell(BlockType::StoneSlab, 1), at, SEED, maxed(), false),
				None
			);
		}
	}

	/// You draw what you can see: below its Alchemy reveal a surface node
	/// is its host to that player, and the reveal is the ALCHEMY level —
	/// Mining opens nothing on the surface.
	#[test]
	fn surface_nodes_reveal_on_alchemy_alone() {
		let cases = [
			(BlockType::Wood, find_heartwood(SEED), BlockType::Heartwood),
			(
				BlockType::Sand,
				find_springstone(SEED),
				BlockType::Springstone,
			),
		];
		for (host, at, node) in cases {
			let need = crate::unlock::alchemy_reveal(node).expect("a surface node");
			let cell = host as u16;
			let at_lvl =
				|mining, alchemy| revealed(cell, at, SEED, Levels { mining, alchemy }, false);
			if need > 1 {
				assert_eq!(at_lvl(crate::skills::MAX_LEVEL, need - 1), None, "{node:?}");
			}
			assert_eq!(at_lvl(1, need), Some(node), "{node:?}");
			assert_eq!(
				crate::unlock::reveal(node),
				None,
				"{node:?} must not sit in the Mining reveal table too"
			);
		}
	}

	/// The dig-mode contract, per kind: break below the reveal, refuse
	/// above it bare-handed, draw with the siphon — and the draw is timed
	/// as the node, not the host.
	#[test]
	fn a_revealed_node_needs_the_siphon() {
		let (host, at, node) = (BlockType::Wood, find_heartwood(SEED), BlockType::Heartwood);
		let cell = host as u16;
		let fresh = Levels {
			mining: 1,
			alchemy: 1,
		};
		// Heartwood reveals at the first level (the economy starts here),
		// so "below the reveal" is exercised by the OTHER kinds' tests;
		// here even a fresh player sees it.
		let need = crate::unlock::alchemy_reveal(node).expect("a surface node");
		assert_eq!(need, 1, "heartwood is the day-one node");
		assert_eq!(
			dig_mode(cell, at, SEED, fresh, false, BlockType::Air),
			DigMode::NeedsSiphon
		);
		assert_eq!(
			dig_mode(cell, at, SEED, fresh, false, BlockType::EssenceSiphon),
			DigMode::Extract(node)
		);
		assert_eq!(
			dig_mode(cell, at, SEED, fresh, true, BlockType::EssenceSiphon),
			DigMode::Break,
			"an edited cell is just wood"
		);
		assert_eq!(dig_block(DigMode::Extract(node), host), node);
		assert_eq!(dig_block(DigMode::Break, host), host);
	}

	/// Every node has a spent face, nothing else does, and the roster
	/// matches the block layer's predicate — the pin that keeps
	/// `is_slottable` honest about what can never be an item.
	#[test]
	fn every_node_has_a_spent_face_and_nothing_else_does() {
		let mut spent = Vec::new();
		for &node in &NODES {
			let face = spent_block(node).unwrap_or_else(|| panic!("{node:?} taps into nothing"));
			assert_ne!(face, node, "{node:?} must not tap out into itself");
			assert!(
				face.dig_drop() == Some(face),
				"{face:?} has to drop ITSELF — it is the only thing a node \
				 ever puts in a player's hands"
			);
			assert_eq!(face.props().light, 0, "{face:?} still casts");
			assert!(!face.props().emissive, "{face:?} still glows");
			assert!(face.is_slottable(), "{face:?} cannot be picked up");
			assert!(!face.is_node_block(), "{face:?} reads as a live node");
			spent.push(face);
		}
		let mut ids: Vec<u16> = spent.iter().map(|&b| b as u16).collect();
		ids.sort_unstable();
		ids.dedup();
		assert_eq!(ids.len(), spent.len(), "two kinds share a face: {spent:?}");
		for &b in &BlockType::ALL {
			assert_eq!(
				spent_block(b).is_some(),
				NODES.contains(&b),
				"{b:?} disagrees with the node roster"
			);
			assert_eq!(
				b.is_node_block(),
				NODES.contains(&b),
				"{b:?}: node_flecks and the roster disagree"
			);
			if NODES.contains(&b) {
				assert!(!b.is_slottable(), "{b:?} can still be held");
				assert!(!b.is_tool(), "{b:?} must be excluded as a node, not a tool");
				// Exactly one reveal table claims each node.
				assert_ne!(
					crate::unlock::reveal(b).is_some(),
					crate::unlock::alchemy_reveal(b).is_some(),
					"{b:?} must answer to exactly one reveal skill"
				);
			}
		}
	}

	/// A landmark's whole trunk is heartwood — the rare giant is always
	/// worth the detour — while ordinary trees roll, and most decline.
	#[test]
	fn landmarks_always_carry_a_heart_and_most_trees_do_not() {
		let mut landmark_checked = false;
		'l: for cz in -24..24 {
			for cx in -24..24 {
				for x in cx * 64..cx * 64 + 64 {
					for z in cz * 64..cz * 64 + 64 {
						let Some(t) = crate::decoration::trunk_at(x, z, SEED) else {
							continue;
						};
						if !t.landmark {
							continue;
						}
						for y in t.y0..=t.y1 {
							assert!(heartwood_at(x, y, z, SEED), "landmark trunk at {x},{y},{z}");
						}
						landmark_checked = true;
						break 'l;
					}
				}
			}
		}
		assert!(landmark_checked, "sample contains a landmark");
		// Ordinary trees: some carry a heart, most do not.
		let (mut with, mut without) = (0u32, 0u32);
		for cz in -48..48 {
			for cx in -48..48 {
				for x in cx * 8..cx * 8 + 8 {
					for z in cz * 8..cz * 8 + 8 {
						let Some(t) = crate::decoration::trunk_at(x, z, SEED) else {
							continue;
						};
						if t.landmark {
							continue;
						}
						if heartwood_at(x, t.y0 + 1, z, SEED) {
							with += 1;
						} else {
							without += 1;
						}
					}
				}
			}
		}
		assert!(with > 0, "some trees carry a heart");
		assert!(without > with, "most trees are plain wood");
	}

	/// A spring stands only in the bed of standing fresh water, and only
	/// in the bed cell itself.
	#[test]
	fn springs_stand_only_in_fresh_beds() {
		let (x, y, z) = find_springstone(SEED);
		let s = crate::worldgen::column_shape(x, z, SEED);
		assert!(matches!(s.surface, SurfaceKind::Lake | SurfaceKind::River));
		assert!(s.water > s.height, "under standing water");
		assert!(!springstone_at(x, y - 1, z, SEED), "not below the bed");
		assert!(!springstone_at(x, y + 1, z, SEED), "not in the water");
		// And never in the ocean: sweep for ocean columns and assert the
		// rate never fires there.
		let mut ocean = 0u32;
		for ox in (-2048..2048).step_by(7) {
			for oz in (-2048..2048).step_by(7) {
				let s = crate::worldgen::column_shape(ox, oz, SEED);
				if s.surface == SurfaceKind::Ocean {
					ocean += 1;
					assert!(!springstone_at(ox, s.height, oz, SEED), "at {ox},{oz}");
				}
			}
		}
		assert!(ocean > 0, "sample contains ocean");
	}
}
