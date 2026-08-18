//! Per-block-type inventory counts. One type serves both sides of the
//! wire: the server's authoritative copy and the client's predicted copy
//! are the same struct, so reconciliation is plain arithmetic. Sized for
//! every block type — the hotbar is a *view* over this (a future
//! overflow-inventory UI reads `iter_nonzero` directly).
//!
//! One count is not an item at all: water's, which is the water tool's
//! charge (empty or full — see [`Inventory::bucket_full`]). Keeping it here
//! rather than in a field of its own is deliberate; the bucket then
//! predicts, rolls back on rejection, rebases and persists through the
//! machinery that already does all four correctly for stacks.
//!
//! Alchemy sub-materials (sub-wood, sub-stone) live in a parallel array
//! and ride the SAME wire pairs as flagged ids (`SUB_WIRE_FLAG | family`)
//! — so `InventoryFull`, the server's identity store, and both client
//! save formats carry them with zero format changes; old readers drop
//! the unknown ids harmlessly.

use crate::block::{BlockType, Essence};

/// Per-type count ceiling; keeps wire varints and UI text bounded.
pub const MAX_STACK: u32 = 1_000_000;

/// Water's ceiling: the water tool holds exactly one source or none, and
/// that count IS the tool's charge (see [`Inventory::bucket_full`]).
/// Water is not a carried item — [`BlockType::is_slottable`] keeps it out
/// of every slot — so there is nowhere for a second unit to live.
pub const BUCKET_CHARGE: u32 = 1;

/// The ceiling for one type. Applied at BOTH ends of every write —
/// [`Inventory::add`] and [`Inventory::from_wire`] — so a save or a
/// server record written back when water was an ordinary stack decodes
/// as a full bucket rather than a windfall of free sources.
///
/// Tools cap at ONE (SKILLS-DESIGN §7.3): owning a tool is a yes/no, so a
/// second craft has nowhere to go and is refused before any debit by the
/// standing `InventoryFull` rebase. That also makes ownership itself a
/// count, which is what lets the tool rail, prediction and both
/// persistence formats carry a new tool with no machinery at all.
#[inline]
fn cap(b: BlockType) -> u32 {
	if b == BlockType::Water {
		BUCKET_CHARGE
	} else if b.is_tool() {
		1
	} else {
		MAX_STACK
	}
}

/// High bit marks a wire pair as a sub-material family, not a block id.
/// Safely disjoint from block ids (see `chunk::ID_BITS`) and from the hotbar's
/// `u16::MAX`-area sentinels, which never appear in inventory pairs.
pub const SUB_WIRE_FLAG: u16 = 0x8000;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Inventory {
	counts: [u32; BlockType::COUNT],
	subs: [u32; Essence::COUNT],
}

/// Hand-written rather than derived: std only implements `Default` for
/// arrays up to 32 entries, and the block table passed that. An empty
/// inventory is all zeros either way.
impl Default for Inventory {
	fn default() -> Self {
		Self {
			counts: [0; BlockType::COUNT],
			subs: [0; Essence::COUNT],
		}
	}
}

impl Inventory {
	#[inline]
	pub fn count(&self, b: BlockType) -> u32 {
		self.counts[b as usize]
	}

	/// Credit `n`, saturating at the type's [`cap`].
	pub fn add(&mut self, b: BlockType, n: u32) {
		let c = &mut self.counts[b as usize];
		*c = c.saturating_add(n).min(cap(b));
	}

	/// Is the water tool charged? The bucket's whole state is one unit of
	/// water in the inventory, which is what makes it predict, roll back
	/// on rejection, rebase from `InventoryFull` and persist by exactly
	/// the same machinery every other item does.
	#[inline]
	pub fn bucket_full(&self) -> bool {
		self.count(BlockType::Water) >= BUCKET_CHARGE
	}

	/// How many more of `b` this inventory can accept — its [`cap`] less
	/// what it holds. Zero for a tool already owned and for a charged
	/// bucket; effectively unbounded for everything else.
	///
	/// [`apply_alchemy`] refuses a craft on exactly this number and the
	/// transmute screen greys its button on it, so the offer and the rule
	/// cannot drift: [`Self::add`] SATURATES, so a craft into a full cap
	/// would otherwise debit the essence and hand back nothing.
	#[inline]
	pub fn headroom(&self, b: BlockType) -> u32 {
		cap(b).saturating_sub(self.count(b))
	}

	/// Does the player have this tool? Tools cap at one, so ownership IS
	/// the count — the bucket included, since it joined the catalog
	/// (SKILLS-DESIGN §13.5; records written while it was owned by
	/// everyone are granted one on load, so nothing loses a bucket it
	/// had). Non-tools are never owned in this sense, whatever their
	/// stack says.
	#[inline]
	pub fn owns_tool(&self, b: BlockType) -> bool {
		b.is_tool() && self.count(b) > 0
	}

	/// The owned tool roster, in block-id order: what tapping 9 cycles
	/// through, what the rail lists, and — via
	/// [`crate::inv_grid::reconcile_layout`] — the only tools a client may
	/// claim to be holding. One definition, because the client's roster
	/// and the server's gate disagreeing is exactly how a tool gets used
	/// without being owned.
	///
	/// EMPTY for a fresh player: the first vessel is crafted, and until it
	/// is, the tool slot stands empty and every tool gate refuses.
	pub fn owned_tools(&self) -> impl Iterator<Item = BlockType> + '_ {
		BlockType::ALL
			.iter()
			.copied()
			.filter(|&b| self.owns_tool(b))
	}

	/// Debit `n` if the stock covers it; false (and no change) otherwise.
	#[must_use]
	pub fn try_take(&mut self, b: BlockType, n: u32) -> bool {
		let c = &mut self.counts[b as usize];
		match c.checked_sub(n) {
			Some(rest) => {
				*c = rest;
				true
			}
			None => false,
		}
	}

	pub fn iter_nonzero(&self) -> impl Iterator<Item = (BlockType, u32)> + '_ {
		BlockType::ALL
			.iter()
			.map(|&b| (b, self.count(b)))
			.filter(|&(_, n)| n > 0)
	}

	/// Fold `other` into this one, every line saturating at its own cap.
	///
	/// Written for the legacy-character migration, which is the only place
	/// two inventories one player owns are ever combined. It goes through
	/// [`Self::add`] and [`Self::add_sub`] rather than touching the arrays,
	/// so the caps that make a tool unique and a bucket single-charge apply
	/// to a merge exactly as they apply to a pickup — a player who owned the
	/// same tool on both sides ends with one, not two.
	///
	/// Saturation means a merge can LOSE units at a full cap. That is the
	/// honest outcome and the same one `add` gives everywhere else; the
	/// alternative is a cap that is not a cap.
	pub fn merge_from(&mut self, other: &Self) {
		for (b, n) in other.iter_nonzero() {
			self.add(b, n);
		}
		for m in Essence::ALL {
			self.add_sub(m, other.sub_count(m));
		}
	}

	#[inline]
	pub fn sub_count(&self, m: Essence) -> u32 {
		self.subs[m as usize]
	}

	/// Credit sub-material units, saturating at [`MAX_STACK`].
	pub fn add_sub(&mut self, m: Essence, n: u32) {
		let c = &mut self.subs[m as usize];
		*c = c.saturating_add(n).min(MAX_STACK);
	}

	/// Debit sub-material units if the stock covers it.
	#[must_use]
	pub fn try_take_sub(&mut self, m: Essence, n: u32) -> bool {
		let c = &mut self.subs[m as usize];
		match c.checked_sub(n) {
			Some(rest) => {
				*c = rest;
				true
			}
			None => false,
		}
	}

	/// Strict decode of wire pairs: unknown block ids are dropped (an
	/// older client talking to a newer server must not panic), counts
	/// clamp to the type's [`cap`], duplicate ids last-write-win. Flagged
	/// ids decode as sub-material families.
	pub fn from_wire(pairs: &[(u16, u32)]) -> Self {
		let mut inv = Self::default();
		for &(id, n) in pairs {
			if id & SUB_WIRE_FLAG != 0 {
				if let Some(m) = Essence::try_from_index(id & !SUB_WIRE_FLAG) {
					inv.subs[m as usize] = n.min(MAX_STACK);
				}
			} else if let Some(b) = BlockType::try_from_id(id) {
				inv.counts[b as usize] = n.min(cap(b));
			}
		}
		inv
	}

	pub fn to_wire(&self) -> Vec<(u16, u32)> {
		let mut out = Vec::new();
		self.write_wire(&mut out);
		out
	}

	/// [`Self::to_wire`] into a caller-owned buffer (cleared first).
	///
	/// The server's player store serializes every stored record on each
	/// flush, so a fresh `Vec` per record was thousands of allocations per
	/// write; one reused buffer covers the whole file.
	pub fn write_wire(&self, out: &mut Vec<(u16, u32)>) {
		out.clear();
		out.extend(self.iter_nonzero().map(|(b, n)| (b as u16, n)));
		out.extend(
			Essence::ALL
				.iter()
				.map(|&m| (SUB_WIRE_FLAG | m as u16, self.sub_count(m)))
				.filter(|&(_, n)| n > 0),
		);
	}
}

/// One validated alchemy operation, in core vocabulary (the wire enum in
/// digblock-net decodes u16 ids into this strictly).
///
/// One variant now: bench EXTRACTION left with the essence redesign — the
/// wire's legacy `Convert` still decodes as a frame and the server refuses
/// it with the standing rebase, but no core rule exists for it any more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlchemyOp {
	/// Craft `count` batches of a shaped item from essence and feedstock.
	Craft { item: BlockType, count: u32 },
}

/// Per-op batch clamp: bounds a single request's work and keeps the
/// arithmetic far from u32 range even at MAX_STACK holdings.
pub const ALCHEMY_MAX_COUNT: u32 = 999;

/// THE alchemy rule, shared verbatim by the server's validation and the
/// offline engine (single-player must match multiplayer bit-for-bit).
/// `level` is the actor's Alchemy level — costs key on it
/// ([`crate::alchemy`]), and so does whether the op is open at all
/// ([`crate::unlock`]). Returns false — with the inventory untouched —
/// on any invalid, locked, or unaffordable op.
#[must_use]
pub fn apply_alchemy(inv: &mut Inventory, op: AlchemyOp, level: u32) -> bool {
	use crate::{alchemy, unlock};
	match op {
		AlchemyOp::Craft { item, count } => {
			if !(1..=ALCHEMY_MAX_COUNT).contains(&count) {
				return false;
			}
			if !unlock::can_craft(item, level) {
				return false;
			}
			let ideal = alchemy::craft_ideal(item).expect("unlock implies a recipe");
			let feedstock = alchemy::craft_feedstock(item);
			let per_batch = crate::shapes::craft_batch(item);
			// A craft that would VANISH into the type's cap is refused,
			// never silently paid for — `add` saturates, so the price
			// would go and nothing would come back. Two capped kinds
			// today: the bucket's one charge, and tools, where owning one
			// is a yes/no (§7.3) — which is also what makes the transmute
			// tab's batch multipliers safe to point at a tool.
			if inv.headroom(item) < per_batch.saturating_mul(count) {
				return false;
			}
			// All-or-nothing across every cost line, essence and feedstock
			// alike: check everything, then debit — a multi-line craft
			// never half-charges. Waste applies to the essence half only;
			// matter is matter, and a block in is a block in.
			for &(e, n) in &ideal {
				if inv.sub_count(e) < alchemy::craft_cost(n, level) * count {
					return false;
				}
			}
			for &(f, n) in &feedstock {
				if inv.count(f) < n * count {
					return false;
				}
			}
			for &(e, n) in &ideal {
				let taken = inv.try_take_sub(e, alchemy::craft_cost(n, level) * count);
				debug_assert!(taken, "checked above");
			}
			for &(f, n) in &feedstock {
				let taken = inv.try_take(f, n * count);
				debug_assert!(taken, "checked above");
			}
			inv.add(item, per_batch * count);
			true
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The legacy-character migration's inventory rule: two piles one
	/// player owns become one, and every cap still holds.
	///
	/// The tool line is the point. Both sides own a bucket, and a merge
	/// that summed counts would hand back two — an item the game has no
	/// way to represent, from a path no player ever asked for.
	#[test]
	fn merging_two_of_a_players_inventories_respects_every_cap() {
		let mut mine = Inventory::default();
		mine.add(BlockType::Dirt, 5);
		mine.add(BlockType::Bucket, 1);
		mine.add_sub(Essence::Earth, 40);

		let mut old = Inventory::default();
		old.add(BlockType::Dirt, 7);
		old.add(BlockType::Stone, 3);
		old.add(BlockType::Bucket, 1);
		old.add_sub(Essence::Earth, 60);
		old.add_sub(Essence::Flora, 2);

		mine.merge_from(&old);
		assert_eq!(mine.count(BlockType::Dirt), 12, "stacks add");
		assert_eq!(mine.count(BlockType::Stone), 3, "and carry over");
		assert_eq!(
			mine.count(BlockType::Bucket),
			cap(BlockType::Bucket),
			"a tool owned on both sides is still one tool"
		);
		assert_eq!(mine.sub_count(Essence::Earth), 100, "essence adds too");
		assert_eq!(mine.sub_count(Essence::Flora), 2);

		// Merging with nothing changes nothing, which is the common case:
		// most players have no old character on most servers.
		let before = mine.clone();
		mine.merge_from(&Inventory::default());
		assert_eq!(mine, before);
	}

	#[test]
	fn add_take_and_clamp() {
		let mut inv = Inventory::default();
		assert_eq!(inv.count(BlockType::Dirt), 0);
		assert!(!inv.try_take(BlockType::Dirt, 1), "empty refuses");

		inv.add(BlockType::Dirt, 3);
		assert!(inv.try_take(BlockType::Dirt, 2));
		assert_eq!(inv.count(BlockType::Dirt), 1);
		assert!(!inv.try_take(BlockType::Dirt, 2), "insufficient refuses");
		assert_eq!(inv.count(BlockType::Dirt), 1, "refusal leaves count intact");

		inv.add(BlockType::Stone, u32::MAX);
		assert_eq!(inv.count(BlockType::Stone), MAX_STACK, "saturates");
	}

	/// The bucket carries a charge, not a stack: one source or none, and
	/// a stack recorded before that was true decodes as one full bucket
	/// rather than a pile of free sources.
	#[test]
	fn water_is_a_charge_not_a_stack() {
		let mut inv = Inventory::default();
		assert!(!inv.bucket_full());
		inv.add(BlockType::Water, 1);
		assert!(inv.bucket_full());
		inv.add(BlockType::Water, 40);
		assert_eq!(inv.count(BlockType::Water), BUCKET_CHARGE, "no second unit");
		assert!(inv.try_take(BlockType::Water, 1), "pouring empties it");
		assert!(!inv.bucket_full());

		// A save or identity record from when water WAS an item.
		let legacy = Inventory::from_wire(&[(BlockType::Water as u16, 40)]);
		assert_eq!(legacy.count(BlockType::Water), BUCKET_CHARGE);
	}

	#[test]
	fn wire_round_trip_drops_unknown_ids() {
		let mut inv = Inventory::default();
		inv.add(BlockType::Sand, 7);
		inv.add(BlockType::Water, 2);
		let wire = inv.to_wire();
		assert_eq!(Inventory::from_wire(&wire), inv);

		let hostile = [(9999u16, 5u32), (BlockType::Wood as u16, u32::MAX)];
		let decoded = Inventory::from_wire(&hostile);
		assert_eq!(decoded.count(BlockType::Wood), MAX_STACK, "clamped");
		assert_eq!(decoded.iter_nonzero().count(), 1, "unknown id dropped");
	}

	#[test]
	fn essences_ride_the_wire_as_flagged_ids() {
		let mut inv = Inventory::default();
		inv.add(BlockType::Wood, 3);
		inv.add_sub(Essence::Flora, 54);
		inv.add_sub(Essence::Earth, 1);
		inv.add_sub(Essence::Aether, 2);
		let wire = inv.to_wire();
		assert!(wire.contains(&(SUB_WIRE_FLAG, 54)), "Flora flagged at 0");
		assert!(wire.contains(&(SUB_WIRE_FLAG | 1, 1)), "Earth flagged at 1");
		assert!(
			wire.contains(&(SUB_WIRE_FLAG | 4, 2)),
			"Aether flagged at 4"
		);
		assert_eq!(Inventory::from_wire(&wire), inv, "round-trips");

		// THE legacy migration, which is a numbering choice rather than
		// code: a record written under the two-family Material economy
		// (sub-wood at 0, sub-stone at 1) decodes 1:1 into Flora and
		// Earth because the Essence enum kept those indices.
		let legacy = Inventory::from_wire(&[(SUB_WIRE_FLAG, 54), (SUB_WIRE_FLAG | 1, 27)]);
		assert_eq!(legacy.sub_count(Essence::Flora), 54, "sub-wood → Flora");
		assert_eq!(legacy.sub_count(Essence::Earth), 27, "sub-stone → Earth");

		// Unknown families (incl. the hotbar's u16::MAX-area sentinels,
		// which never belong in inventory pairs) drop harmlessly.
		let hostile = [(SUB_WIRE_FLAG | 7, 5u32), (u16::MAX, 9u32)];
		assert_eq!(Inventory::from_wire(&hostile), Inventory::default());
	}

	#[test]
	fn alchemy_craft_scales_with_level() {
		let mut inv = Inventory::default();
		inv.add(BlockType::Wood, 3);
		inv.add_sub(Essence::Flora, 20);

		// Craft a door batch and two stairs: each craft consumes one
		// timber cube and the catalyst at mastery's smallest waste markup;
		// the door's cube cuts TWO leaves, the stairs' one flight each.
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::LumberDoor,
				count: 1
			},
			99
		));
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::WoodStairs,
				count: 2
			},
			99
		));
		assert_eq!(inv.count(BlockType::LumberDoor), 2, "one cube, two leaves");
		assert_eq!(inv.count(BlockType::WoodStairs), 2);
		assert_eq!(inv.count(BlockType::Wood), 0, "three cubes consumed");
		assert_eq!(
			inv.sub_count(Essence::Flora),
			20 - 3 * 4,
			"ceil(3·1.05) each"
		);

		// The bottom of the curve: the steepest waste, so the same essence
		// buys less — and no timber refuses outright.
		let mut low = Inventory::default();
		low.add(BlockType::Wood, 1);
		low.add_sub(Essence::Flora, 4);
		assert!(
			!apply_alchemy(
				&mut low,
				AlchemyOp::Craft {
					item: BlockType::WoodSlab,
					count: 1
				},
				1
			),
			"the catalyst costs ceil(3·1.60) = 5 > 4"
		);
		low.add_sub(Essence::Flora, 1);
		assert!(apply_alchemy(
			&mut low,
			AlchemyOp::Craft {
				item: BlockType::WoodSlab,
				count: 1
			},
			1
		));
		assert_eq!(low.count(BlockType::WoodSlab), 3, "a cube cuts three slabs");
		assert_eq!(low.count(BlockType::Wood), 0);

		// Unaffordable, level-locked, invalid, zero/oversized counts:
		// refused with no mutation.
		let before = inv.clone();
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::StoneStairs,
				count: 1
			},
			99
		));
		assert!(
			!apply_alchemy(
				&mut inv,
				AlchemyOp::Craft {
					item: BlockType::Stone,
					count: 1
				},
				14
			),
			"clean Stone is locked below the mason's unlock"
		);
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::LumberDoor,
				count: 0
			},
			99
		));
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::LumberDoor,
				count: ALCHEMY_MAX_COUNT + 1
			},
			99
		));
		assert_eq!(inv, before, "failed ops never mutate");
	}

	/// The feedstock half of a price: the two vessels cost blocks and no
	/// essence, the debit is all-or-nothing beside the essence lines, and
	/// a fresh player's first craft is exactly this.
	#[test]
	fn a_vessel_is_crafted_from_feedstock_alone() {
		let need: u32 = crate::alchemy::craft_feedstock(BlockType::Bucket)
			.iter()
			.map(|&(_, n)| n)
			.sum();
		let mut inv = Inventory::default();
		inv.add(BlockType::Wood, need - 1);
		assert!(
			!apply_alchemy(
				&mut inv,
				AlchemyOp::Craft {
					item: BlockType::Bucket,
					count: 1
				},
				1
			),
			"one log short is short"
		);
		assert_eq!(inv.count(BlockType::Wood), need - 1, "no partial debit");
		inv.add(BlockType::Wood, 1);
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Bucket,
				count: 1
			},
			1
		));
		assert_eq!(inv.count(BlockType::Bucket), 1, "the vessel is owned");
		assert_eq!(inv.count(BlockType::Wood), 0, "the timber is spent");
		for &e in &Essence::ALL {
			assert_eq!(inv.sub_count(e), 0, "no essence moved");
		}
	}

	/// A multi-essence craft (a torch = Flora + Earth) debits every line or
	/// none: short on the SECOND line must not consume the first.
	#[test]
	fn multi_essence_crafts_never_half_charge() {
		let mut inv = Inventory::default();
		inv.add(BlockType::Wood, 1);
		inv.add_sub(Essence::Flora, 100);
		inv.add_sub(Essence::Earth, 1); // deliberately short of the torch's Earth line
		let before = inv.clone();
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Torch,
				count: 1
			},
			99
		));
		assert_eq!(inv, before, "no partial debit — the timber included");
		inv.add_sub(Essence::Earth, 5);
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Torch,
				count: 1
			},
			99
		));
		assert_eq!(inv.count(BlockType::Torch), 1);
		assert_eq!(inv.count(BlockType::Wood), 0, "the stick is spent");
		assert_eq!(inv.sub_count(Essence::Flora), 100 - 3, "ceil(2·1.05)");
		assert_eq!(inv.sub_count(Essence::Earth), 6 - 3, "ceil(2·1.05)");
	}

	/// The bucket charge is never crafted: water is something the world
	/// hands you, so no amount of Aqua buys a charge at any level — the
	/// only way to fill the tool is to go and scoop one. (Its old
	/// extraction half left with the bench's extract slot: the springstone
	/// node is the Aqua economy now.)
	#[test]
	fn the_bucket_charge_is_never_crafted() {
		let mut inv = Inventory::default();
		inv.add_sub(Essence::Aqua, 100_000);
		let before = inv.clone();
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Water,
				count: 1
			},
			crate::skills::MAX_LEVEL
		));
		assert_eq!(inv, before, "nothing paid, nothing filled");
	}

	/// Owning a tool is a yes/no (§7.3), so the bench sells you exactly
	/// one. Both ways to ask for a second are refused BEFORE any debit:
	/// a batch multiplier — the transmute tab aims those at every member,
	/// tools included — and a plain craft once it's in the bag. `add`
	/// saturates, so an unrefused one would take the whole batch's essence
	/// and hand back nothing.
	#[test]
	fn a_tool_is_crafted_once_and_never_paid_for_twice() {
		let held = |inv: &Inventory| -> Vec<u32> {
			Essence::ALL.iter().map(|&e| inv.sub_count(e)).collect()
		};
		let pick = BlockType::ResonantPick;
		let mut inv = Inventory::default();
		for &e in &Essence::ALL {
			inv.add_sub(e, 100_000);
		}

		let before = held(&inv);
		assert!(
			!apply_alchemy(
				&mut inv,
				AlchemyOp::Craft {
					item: pick,
					count: 10
				},
				99
			),
			"nine of the ten would vanish into the cap"
		);
		assert_eq!(held(&inv), before, "nothing paid");
		assert_eq!(inv.count(pick), 0);

		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: pick,
				count: 1
			},
			99
		));
		assert_eq!(inv.count(pick), 1);
		let owned = held(&inv);
		assert_ne!(owned, before, "the first one costs");

		assert!(
			!apply_alchemy(
				&mut inv,
				AlchemyOp::Craft {
					item: pick,
					count: 1
				},
				99
			),
			"a tool already owned has nowhere to go"
		);
		assert_eq!(held(&inv), owned, "and so costs nothing");
		assert_eq!(inv.count(pick), 1);
		assert_eq!(inv.headroom(pick), 0, "the screen greys on this number");
	}
}
