//! The unified NODE layer — every world source of essence behind one question.
//! **Essence is found, not ground.**
//!
//! A node is a derived find: a salted hash or a pure worldgen query says where
//! one stands, the mesh stamp reveals it at a skill level, the siphon draws it
//! in place, and the draw leaves the kind's own SPENT face. Nothing is stored,
//! so nodes appear retroactively in existing worlds and the goldens do not move.
//!
//! Seven live faces, two reveal skills: the four ORE tiers in pristine deep
//! Stone on MINING; and Leystone, Heartwood and Springstone on ALCHEMY, so
//! Alchemy owns the surface hunt the way Mining owns the deep one.
//!
//! **The unedited rule is the anti-dupe**: a node manifests only where the cell
//! still holds its pristine host and has never been written.
//! `JournalIndex::record` only ever inserts, so a spent node cannot be regrown
//! by placing the host back.
//!
//! The one node that IS stored: a spent face the regrowth sweep
//! ([`crate::worldjob::regrow`]) has given back. The edited bit is monotone, so
//! the only way a find can return is the sweep writing the live face itself
//! into the cell — which the anti-dupe permits because only the sweep can:
//! no live face is ever an item ([`BlockType::is_slottable`]), so no player
//! can place one. A stored face is ordinary chunk data everyone sees, and
//! [`revealed`] treats it as found at any level.

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
///
/// Public because it must be the ONLY spelling of the question. The mesh
/// stamp used to carry its own, written as a disjunction rather than this
/// if/else — which answers the opposite way for a block in neither table,
/// and the stamp's second pass reverts a claim on exactly that answer. A
/// tier shipped or retired without a reveal entry would have been left
/// standing in the picture and refused at the dig.
#[inline]
pub fn reveal_met(node: BlockType, levels: Levels) -> bool {
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
///
/// A cell holding a live face OUTRIGHT — a regrown node — is found by
/// anyone, ahead of both gates: it is stored, so every client's mesh
/// already shows it, and a node you can see and cannot draw is the worst
/// outcome this whole system has.
pub fn revealed(
	cell: u16,
	at: (i32, i32, i32),
	seed: f64,
	levels: Levels,
	edited: bool,
) -> Option<BlockType> {
	let id = crate::chunk::cell_id(cell);
	if let Some(stored) = BlockType::try_from_id(id).filter(|b| b.is_node_block()) {
		return Some(stored);
	}
	if edited {
		return None;
	}
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

/// The node worldgen puts at this coordinate, asked of the position
/// ALONE — no host, no edit bit, no level. The placement half of
/// [`revealed`], for a caller that already knows the cell is not its
/// pristine host: the regrowth sweep, deciding whether a spent face
/// stands where its find was, or somewhere a player carried it to.
pub fn node_at(at: (i32, i32, i32), seed: f64) -> Option<BlockType> {
	let (x, y, z) = at;
	if heartwood_at(x, y, z, seed) {
		return Some(BlockType::Heartwood);
	}
	if springstone_at(x, y, z, seed) {
		return Some(BlockType::Springstone);
	}
	ore::ore_at(x, y, z, seed)
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
	// Named for the question the callers ask, which is `tool_in_hand`
	// and not `active_tool`: holding a tool is which one the tool slot
	// holds AND whether that slot is selected, and the two used to be
	// conflated here — a player with dirt in hand drew veins with the
	// siphon (SIM v20).
	tool_in_hand: BlockType,
) -> DigMode {
	match revealed(cell, at, seed, levels, edited) {
		// Below the reveal there is nothing here to protect: that player's
		// client and server agree the cell is its host, so it digs as the
		// host and the node is spent. You dig what you can see.
		None => DigMode::Break,
		Some(node) if tool_in_hand == BlockType::EssenceSiphon => DigMode::Extract(node),
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

/// How many trunk cells of a heartwood tree manifest, counted up from the
/// cell above the root — the heart sits low, at eye height, where a
/// standing player's crosshair naturally lands.
///
/// Every trunk column that carries a heart carries this much of one, a
/// landmark's four included: what the giant is worth is settled by its
/// footprint and by never rolling, and a heart that also grew with the
/// trunk would make one tree worth a session.
const HEART_CELLS: i32 = 2;

/// Does this cell hold heartwood — the living core of a worldgen tree?
///
/// Pure in `(x, y, z, seed)` like [`ore::ore_at`]: the trunk comes from
/// [`crate::decoration::trunk_at`], and whether a tree HAS a heart is one
/// salted roll on the tree's anchor column, so every cell of one trunk agrees.
/// A landmark tree skips the roll and always has one. What it does NOT get is a
/// bigger heart — both carry it in the [`HEART_CELLS`] above the root, so the
/// giant's reward is its four trunk columns and the certainty. Paying by the
/// cell, a full-trunk landmark was worth more Alchemy XP than the first dozen
/// levels cost put together.
///
/// Callers still need the LIVE cell to hold Wood and be unedited ([`revealed`]
/// checks both): a canopy written first, a cliff that left the trunk cell
/// unwritten, or a felled-and-rebuilt trunk are all cells this cannot see.
pub fn heartwood_at(x: i32, y: i32, z: i32, seed: f64) -> bool {
	let Some(t) = crate::decoration::trunk_at(x, z, seed) else {
		return false;
	};
	let heart = (t.y0 + 1)..=(t.y0 + HEART_CELLS).min(t.y1);
	heart.contains(&y)
		&& (t.landmark
			|| ore::mix(t.anchor.0, 0, t.anchor.1, seed.to_bits(), HEARTWOOD_SALT) % 100
				< HEARTWOOD_IN_100)
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
	springstone_bed(x, z, seed) == Some(y)
}

/// The bed cell this column's spring occupies, or `None` for a column
/// that carries none.
///
/// The column-level half of [`springstone_at`], separate because a
/// caller that wants to FIND springs — the `DIG_NODE` capture scene —
/// otherwise has to evaluate [`crate::worldgen::column_shape`] itself
/// just to learn the height to ask about, and then pays for it a second
/// time in here. Sharing one answer is also what keeps the rate test
/// genuinely first: it is a hash, the classification behind it is five
/// field evaluations, and the hash rejects all but a small share of
/// columns before any of them run.
pub fn springstone_bed(x: i32, z: i32, seed: f64) -> Option<i32> {
	if ore::mix(x, 0, z, seed.to_bits(), SPRING_SALT) & 0xFF >= SPRING_IN_256 {
		return None;
	}
	let s = crate::worldgen::column_shape(x, z, seed);
	use crate::worldgen::SurfaceKind;
	(matches!(s.surface, SurfaceKind::Lake | SurfaceKind::River) && s.water > s.height)
		.then_some(s.height)
}

#[cfg(test)]
mod tests;
