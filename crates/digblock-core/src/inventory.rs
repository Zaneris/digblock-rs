//! Per-block-type inventory counts. One type serves both sides of the
//! wire: the server's authoritative copy and the client's predicted copy
//! are the same struct, so reconciliation is plain arithmetic. Sized for
//! every block type — the hotbar is a *view* over this (a future
//! overflow-inventory UI reads `iter_nonzero` directly).
//!
//! One count is not an item at all: water's, which is the water tool's
//! charge (0 or 1 — see [`Inventory::bucket_full`]). Keeping it here
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
/// Safely disjoint from block ids (10-bit) and from the hotbar's
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlchemyOp {
	/// Break whole blocks into their sub-material: `count` blocks →
	/// `count × SUBS_PER_BLOCK` units. One-way — a strict sink.
	Convert { block: BlockType, count: u32 },
	/// Craft `count` batches of a shaped item from sub-material.
	Craft { item: BlockType, count: u32 },
}

/// Per-op batch clamp: bounds a single request's work and keeps the
/// arithmetic far from u32 range even at MAX_STACK holdings.
pub const ALCHEMY_MAX_COUNT: u32 = 999;

/// THE alchemy rule, shared verbatim by the server's validation and the
/// offline engine (single-player must match multiplayer bit-for-bit).
/// `level` is the actor's Alchemy level — yields and costs key on it
/// ([`crate::alchemy`]), and so does whether the op is open at all
/// ([`crate::unlock`]). Returns false — with the inventory untouched —
/// on any invalid, locked, or unaffordable op.
#[must_use]
pub fn apply_alchemy(inv: &mut Inventory, op: AlchemyOp, level: u32) -> bool {
	use crate::{alchemy, unlock};
	match op {
		AlchemyOp::Convert { block, count } => {
			if !(1..=ALCHEMY_MAX_COUNT).contains(&count) {
				return false;
			}
			if !unlock::can_extract(block, level) {
				return false;
			}
			let yields = alchemy::extract_yields(block, level);
			if yields.is_empty() {
				return false;
			}
			if !inv.try_take(block, count) {
				return false;
			}
			for (e, per_block) in yields {
				inv.add_sub(e, per_block * count);
			}
			true
		}
		AlchemyOp::Craft { item, count } => {
			if !(1..=ALCHEMY_MAX_COUNT).contains(&count) {
				return false;
			}
			if !unlock::can_craft(item, level) {
				return false;
			}
			let ideal = alchemy::craft_ideal(item).expect("unlock implies a recipe");
			// The bucket charge caps at one: a craft that would vanish
			// into the cap is refused, never silently paid for.
			if item == BlockType::Water && (count != 1 || inv.bucket_full()) {
				return false;
			}
			// All-or-nothing across the cost lines: check everything,
			// then debit — a multi-essence craft never half-charges.
			for &(e, n) in &ideal {
				if inv.sub_count(e) < alchemy::craft_cost(n, level) * count {
					return false;
				}
			}
			for &(e, n) in &ideal {
				let taken = inv.try_take_sub(e, alchemy::craft_cost(n, level) * count);
				debug_assert!(taken, "checked above");
			}
			let per_batch = crate::shapes::craft_batch(item);
			inv.add(item, per_batch * count);
			true
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

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
	fn alchemy_convert_and_craft_scale_with_level() {
		let mut inv = Inventory::default();
		inv.add(BlockType::Wood, 2);

		// Convert 2 wood at level 99 → the full 27 Flora each.
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Convert {
				block: BlockType::Wood,
				count: 2
			},
			99
		));
		assert_eq!(inv.count(BlockType::Wood), 0);
		assert_eq!(inv.sub_count(Essence::Flora), 54);

		// Craft a door (ideal 10, waste 1.05 → 11) and two stairs
		// (ideal 18 → 19 each) = 49 of the 54; 5 left.
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
		assert_eq!(inv.count(BlockType::LumberDoor), 1);
		assert_eq!(inv.count(BlockType::WoodStairs), 2);
		assert_eq!(inv.sub_count(Essence::Flora), 5);

		// Level 1: 40% yields, 1.60× costs — the same wood buys less.
		let mut low = Inventory::default();
		low.add(BlockType::Wood, 1);
		assert!(apply_alchemy(
			&mut low,
			AlchemyOp::Convert {
				block: BlockType::Wood,
				count: 1
			},
			1
		));
		assert_eq!(low.sub_count(Essence::Flora), 10, "floor(27·0.40)");
		assert!(
			!apply_alchemy(
				&mut low,
				AlchemyOp::Craft {
					item: BlockType::WoodSlab,
					count: 1
				},
				1
			),
			"slab costs ceil(9·1.60) = 15 > 10"
		);

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
				19
			),
			"clean Stone is locked below 20"
		);
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Convert {
				block: BlockType::Wood,
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

	/// A multi-essence craft (Dirt = Earth + Flora) debits every line or
	/// none: short on the SECOND line must not consume the first.
	#[test]
	fn multi_essence_crafts_never_half_charge() {
		let mut inv = Inventory::default();
		inv.add_sub(Essence::Earth, 100);
		inv.add_sub(Essence::Flora, 1); // Dirt needs ceil(5·1.05) = 6
		let before = inv.clone();
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Dirt,
				count: 1
			},
			99
		));
		assert_eq!(inv, before, "no partial debit");
		inv.add_sub(Essence::Flora, 5);
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Dirt,
				count: 1
			},
			99
		));
		assert_eq!(inv.count(BlockType::Dirt), 1);
		assert_eq!(inv.sub_count(Essence::Earth), 100 - 25, "ceil(23·1.05)");
		assert_eq!(inv.sub_count(Essence::Flora), 0);
	}

	/// The bucket-charge economy: extraction needs Alch 15 and empties
	/// the tool; crafting a charge needs 50, refuses on a full bucket,
	/// and is lossy against the extraction — no loop.
	#[test]
	fn bucket_charge_extracts_and_crafts_without_a_loop() {
		let mut inv = Inventory::default();
		inv.add(BlockType::Water, 1);
		assert!(
			!apply_alchemy(
				&mut inv,
				AlchemyOp::Convert {
					block: BlockType::Water,
					count: 1
				},
				14
			),
			"the Aqua economy opens at 15"
		);
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Convert {
				block: BlockType::Water,
				count: 1
			},
			99
		));
		assert!(!inv.bucket_full(), "extracting the charge empties the tool");
		assert_eq!(inv.sub_count(Essence::Aqua), 18);

		// Craft a fresh charge at 50+: 27·1.05 → 29 Aqua for 18 back.
		inv.add_sub(Essence::Aqua, 11);
		assert!(apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Water,
				count: 1
			},
			99
		));
		assert!(inv.bucket_full());
		assert_eq!(inv.sub_count(Essence::Aqua), 0);
		// Full bucket: refused before any debit.
		inv.add_sub(Essence::Aqua, 100);
		assert!(!apply_alchemy(
			&mut inv,
			AlchemyOp::Craft {
				item: BlockType::Water,
				count: 1
			},
			99
		));
		assert_eq!(inv.sub_count(Essence::Aqua), 100, "nothing paid");
	}
}
