//! Per-block-type inventory counts. One type serves both sides of the
//! wire: the server's authoritative copy and the client's predicted copy
//! are the same struct, so reconciliation is plain arithmetic. Sized for
//! every block type — the hotbar is a *view* over this (a future
//! overflow-inventory UI reads `iter_nonzero` directly).

use crate::block::BlockType;

/// Per-type count ceiling; keeps wire varints and UI text bounded.
pub const MAX_STACK: u32 = 1_000_000;

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Inventory {
	counts: [u32; BlockType::COUNT],
}

impl Inventory {
	#[inline]
	pub fn count(&self, b: BlockType) -> u32 {
		self.counts[b as usize]
	}

	/// Credit `n`, saturating at [`MAX_STACK`].
	pub fn add(&mut self, b: BlockType, n: u32) {
		let c = &mut self.counts[b as usize];
		*c = c.saturating_add(n).min(MAX_STACK);
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

	/// Strict decode of wire pairs: unknown block ids are dropped (an
	/// older client talking to a newer server must not panic), counts
	/// clamp to [`MAX_STACK`], duplicate ids last-write-win.
	pub fn from_wire(pairs: &[(u16, u32)]) -> Self {
		let mut inv = Self::default();
		for &(id, n) in pairs {
			if let Some(b) = BlockType::try_from_id(id) {
				inv.counts[b as usize] = n.min(MAX_STACK);
			}
		}
		inv
	}

	pub fn to_wire(&self) -> Vec<(u16, u32)> {
		self.iter_nonzero().map(|(b, n)| (b as u16, n)).collect()
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
}
