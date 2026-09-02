//! The unified NODE layer: every world source of essence behind one question. **Essence is found,
//! not ground.**
//!
//! A node is a derived find: a salted hash or a pure worldgen query says where one stands, the
//! mesh stamp reveals it, the siphon draws it in place, and the draw leaves the kind's own SPENT
//! face. Nothing is stored, so nodes appear retroactively in existing worlds and the goldens do
//! not move. Seven live faces, two reveal gates: the four ORE tiers in pristine deep Stone on a
//! MINING level; Leystone, Heartwood and Springstone on the SIPHON itself — the moment you make
//! the essence tool, every surface source fades into the world at once.
//!
//! **The unedited rule is the anti-dupe**: a node manifests only where the cell still holds its
//! pristine host and has never been written, and `JournalIndex::record` only ever inserts, so a
//! spent node cannot be regrown by placing the host back. The one node that IS stored is a spent
//! face the regrowth sweep ([`crate::worldjob::regrow`]) gave back — safe because no live face is
//! ever an item ([`BlockType::is_slottable`]), so [`revealed`] finds a stored one at any level.

use crate::block::BlockType;
use crate::worldgen::Seed;
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

/// Everything a node question is asked at: the two skill levels, and whether the player holds an
/// essence siphon — the surface nodes' whole gate. A plain struct rather than a `SkillSet` and
/// an inventory, because the mesh stamp crosses a worker boundary carrying answers, not state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Levels {
	pub mining: u32,
	pub alchemy: u32,
	/// Does the pack hold an [`BlockType::EssenceSiphon`]? Owning one IS
	/// having made one: the siphon is never consumed and never placed, so
	/// the bit only ever rises within a world.
	pub siphon: bool,
}

impl Levels {
	#[inline]
	pub fn of(skills: &crate::skills::SkillSet, inventory: &crate::inventory::Inventory) -> Self {
		Self {
			mining: skills.level(crate::skills::Skill::Mining),
			alchemy: skills.level(crate::skills::Skill::Alchemy),
			siphon: inventory.count(BlockType::EssenceSiphon) > 0,
		}
	}
}

/// Does this player SEE this node kind at all? Each kind answers to exactly one reveal table —
/// the ore tiers to a Mining level, the surface nodes to the siphon bit; a block in neither
/// table is not a node and is never revealed. Public because it must be the ONLY spelling of the
/// question — a disjunction instead of this if/else answers the opposite way for a block in
/// neither table, and the mesh stamp's second pass reverts a claim on exactly that answer,
/// leaving a tier with no reveal entry in the picture and refused at the dig.
#[inline]
pub fn reveal_met(node: BlockType, levels: Levels) -> bool {
	if let Some(need) = crate::unlock::reveal(node) {
		levels.mining >= need
	} else if crate::unlock::siphon_reveal(node) {
		levels.siphon
	} else {
		false
	}
}

/// THE visibility question, asked identically by the mesh stamp, the client's dig prediction and
/// the server's validation. Both gates matter: the host-id test makes a spent node stay spent, the
/// `edited` test stops someone placing the host back to regrow it, and the host id is also what
/// [`node_at`] routes on. A cell holding a live face OUTRIGHT — a regrown node — is found by
/// anyone ahead of both: it is stored, so every client's mesh already shows it.
pub fn revealed(
	cell: u16,
	at: (i32, i32, i32),
	seed: Seed,
	levels: Levels,
	edited: bool,
) -> Option<BlockType> {
	let block = BlockType::try_from_id(crate::chunk::cell_id(cell))?;
	if block.is_node_block() {
		return Some(block);
	}
	if edited {
		return None;
	}
	let node = node_at(block, at, seed)?;
	reveal_met(node, levels).then_some(node)
}

/// The node worldgen puts at this coordinate IN THIS HOST — no edit bit and no level, but the same
/// routing [`revealed`] does, because it IS the routing. The host is load-bearing rather than a
/// shortcut: asked of the position alone the three streams overlap, and the regrowth sweep is the
/// caller that notices — it asks whether a spent face stands where its find was, and a host-blind
/// answer would let one carried in and placed manufacture a node site.
pub fn node_at(host: BlockType, at: (i32, i32, i32), seed: Seed) -> Option<BlockType> {
	let (x, y, z) = at;
	match host {
		BlockType::Stone => ore::ore_at(x, y, z, seed.value),
		BlockType::Wood => heartwood_at(x, y, z, seed).then_some(BlockType::Heartwood),
		BlockType::Sand => springstone_at(x, y, z, seed).then_some(BlockType::Springstone),
		_ => None,
	}
}

/// The pristine host a node of this kind manifests in — the id [`node_at`] routes on, inverted for
/// a caller holding the KIND and asking where it could stand. Total on [`NODES`]. Leystone's host
/// is Stone and not soil: it rides the ore deposit stream as its surface tier, and only its reveal
/// is on the other ladder.
#[inline]
pub fn host_block(node: BlockType) -> Option<BlockType> {
	Some(match node {
		BlockType::Emberore
		| BlockType::Duskstone
		| BlockType::Glowvein
		| BlockType::Aetherock
		| BlockType::Leystone => BlockType::Stone,
		BlockType::Heartwood => BlockType::Wood,
		BlockType::Springstone => BlockType::Sand,
		_ => return None,
	})
}

/// The kind whose draw left `face` standing — [`spent_block`]'s inverse,
/// found over [`NODES`] rather than tabled a second time, so the two can
/// never name different sets.
pub fn drawn_kind(face: BlockType) -> Option<BlockType> {
	NODES.into_iter().find(|&n| spent_block(n) == Some(face))
}

/// What a dig on one cell actually IS, for this player with this tool in
/// hand (SKILLS-DESIGN §5.3). The one question the client's prediction and
/// the server's validation both ask, so they cannot disagree about whether
/// an action was a dig, a draw, or a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DigMode {
	/// Ordinary: the cell breaks and drops what it drops.
	Break,
	/// Siphon in hand: the cell becomes the kind's [`spent_block`] — never air — and pays essence.
	Extract(BlockType),
	/// A revealed node and no siphon: refused at `DigStart`, not at the completing edit.
	NeedsSiphon,
}

pub fn dig_mode(
	cell: u16,
	at: (i32, i32, i32),
	seed: Seed,
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

/// The block a dig is TIMED and paid XP as. A node lives in a cell that still holds its host — the
/// reveal is a mesh stamp, not a world write — so without this a draw would take the host's own
/// hold time and pay the host's XP however rich the find was. It does not decide what the dig
/// CREDITS: a draw pays essence, not a block (see [`DigMode::Extract`]).
#[inline]
pub fn dig_block(mode: DigMode, cell_block: BlockType) -> BlockType {
	match mode {
		DigMode::Extract(node) => node,
		_ => cell_block,
	}
}

/// The face a drawn node leaves standing — the kind's own spent block, one per kind rather than
/// one grey face for all, and the ONLY thing a node ever puts in a player's hands, since the live
/// face is never an item ([`BlockType::is_slottable`]). Total on [`NODES`], so a caller holding a
/// [`DigMode::Extract`] cannot be handed one it has no face for; anything else answers `None`.
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
const HEARTWOOD_IN_100: u64 = 20;

/// How many trunk cells of a heartwood tree manifest, counted up from the cell above the root —
/// the heart sits low, at eye height, where a standing player's crosshair naturally lands. Every
/// trunk column that carries a heart carries this much of one, a landmark's four included: a heart
/// that grew with the trunk would make one tree worth a session.
const HEART_CELLS: i32 = 3;

/// Does this cell hold heartwood — the living core of a worldgen tree? Pure in `(x, y, z, seed)`
/// like [`ore::ore_at`]: the trunk comes from [`crate::decoration::trunk_at`], and whether a tree
/// HAS a heart is one salted roll on its anchor column, so every cell of one trunk agrees. A
/// landmark skips the roll but gets no BIGGER heart — its reward is its four trunk columns and the
/// certainty. Callers still need the live cell to hold Wood and be unedited ([`revealed`] checks).
pub fn heartwood_at(x: i32, y: i32, z: i32, seed: Seed) -> bool {
	let Some(t) = crate::decoration::trunk_at(x, z, seed) else {
		return false;
	};
	let heart = (t.y0 + 1)..=(t.y0 + HEART_CELLS).min(t.y1);
	heart.contains(&y)
		&& (t.landmark
			|| ore::mix(
				t.anchor.0,
				0,
				t.anchor.1,
				seed.value.to_bits(),
				HEARTWOOD_SALT,
			) % 100 < HEARTWOOD_IN_100)
}

/// Salt for the springstone stream.
const SPRING_SALT: u64 = 0x5b41_0e57_04e5_ea11;

/// The share of lake- and river-bed cells that carry a spring, in 256ths.
const SPRING_IN_256: u64 = 8;

/// The share of ocean SHORELINE bed cells that carry one, in the same
/// 256ths — rarer than the fresh beds, a find for the wader rather than
/// a reason to work the coast. Must not exceed [`SPRING_IN_256`]: the
/// fresh rate is the prune every column pays before it is classified,
/// and a shore rate above it would be cut down to it silently.
const SPRING_SHORE_IN_256: u64 = 3;
const _: () = assert!(
	SPRING_SHORE_IN_256 <= SPRING_IN_256,
	"the shore rate would be pruned to the fresh one"
);

/// How much water may stand over an ocean bed for it to count as the
/// shoreline — the wading shallows off a beach, where the find can be
/// seen from the sand, not the open floor.
const SHORE_DEPTH: i32 = 3;

/// Does this cell hold a springstone — a charged stone in the pristine bed of standing water? The
/// rate rolls first because it is a hash where the bed test is five field evaluations
/// ([`crate::worldgen::column_shape`]); only the cells that pass pay for the classification. Lake
/// and river beds at the full rate, the ocean's shoreline shallows at the rarer one, never the
/// open floor or a marsh puddle, and only the bed surface cell — which puts the find under water.
pub fn springstone_at(x: i32, y: i32, z: i32, seed: Seed) -> bool {
	springstone_bed(x, z, seed) == Some(y)
}

/// The bed cell this column's spring occupies, or `None` for a column that carries none — the
/// column-level half of [`springstone_at`], separate because a caller that wants to FIND springs
/// (the `DIG_NODE` capture scene) would otherwise evaluate [`crate::worldgen::column_shape`] just
/// to learn the height to ask about and pay for it a second time in here. Sharing one answer also
/// keeps the rate test genuinely first: the hash rejects all but a small share of columns.
pub fn springstone_bed(x: i32, z: i32, seed: Seed) -> Option<i32> {
	let roll = ore::mix(x, 0, z, seed.value.to_bits(), SPRING_SALT) & 0xFF;
	if roll >= SPRING_IN_256 {
		return None;
	}
	let s = crate::worldgen::column_shape(x, z, seed);
	if s.water <= s.height {
		return None;
	}
	use crate::worldgen::SurfaceKind;
	let bed = match s.surface {
		SurfaceKind::Lake | SurfaceKind::River => true,
		SurfaceKind::Ocean => s.water - s.height <= SHORE_DEPTH && roll < SPRING_SHORE_IN_256,
		_ => false,
	};
	bed.then_some(s.height)
}

#[cfg(test)]
mod tests;
