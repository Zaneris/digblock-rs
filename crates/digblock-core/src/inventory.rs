//! Per-block-type inventory counts. One type serves both sides of the wire, so
//! reconciliation is plain arithmetic. Sized for every block type — the hotbar
//! is a *view* over this.
//!
//! One count is not an item at all: water's, which is the water tool's charge
//! (see [`Inventory::bucket_full`]). Keeping it here rather than in a field of
//! its own is what lets the bucket predict, roll back on rejection, rebase and
//! persist through machinery that already does all four correctly for stacks.
//!
//! Essences live in a parallel array and ride the SAME wire pairs as flagged
//! ids (`SUB_WIRE_FLAG | index`), so every format carries them with no format
//! change and old readers drop the unknown ids harmlessly.

use crate::block::{BlockType, Essence};

/// Per-type count ceiling; keeps wire varints and UI text bounded.
pub const MAX_STACK: u32 = 1_000_000;

/// Water's ceiling: the water tool holds exactly one source or none, and
/// that count IS the tool's charge (see [`Inventory::bucket_full`]).
/// Water is not a carried item — [`BlockType::is_slottable`] keeps it out
/// of every slot — so there is nowhere for a second unit to live.
pub const BUCKET_CHARGE: u32 = 1;

/// The ceiling for one type. Applied at BOTH ends of every write — [`Inventory::add`] and
/// [`Inventory::from_wire`] — so a save written back when water was an ordinary stack decodes as a
/// full bucket rather than a windfall of free sources. Tools cap at ONE (SKILLS-DESIGN §7.3), so a
/// second craft has nowhere to go and is refused before any debit by the standing `InventoryFull`
/// rebase — which also makes ownership a count prediction and both save formats already carry.
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

	/// How many more of `b` this inventory can accept — its [`cap`] less what it holds. Zero for a
	/// tool already owned and for a charged bucket; effectively unbounded for everything else.
	/// [`apply_alchemy`] refuses a craft on exactly this number and the transmute screen greys its
	/// button on it, so the offer and the rule cannot drift: [`Self::add`] SATURATES, so a craft
	/// into a full cap would otherwise debit the essence and hand back nothing.
	#[inline]
	pub fn headroom(&self, b: BlockType) -> u32 {
		cap(b).saturating_sub(self.count(b))
	}

	/// Does the player have this tool? Tools cap at one, so ownership IS the count — the bucket
	/// included, since it joined the catalog (SKILLS-DESIGN §13.5; records written while it was
	/// owned by everyone are granted one on load, so nothing loses a bucket it had). Non-tools
	/// are never owned in this sense, whatever their stack says.
	#[inline]
	pub fn owns_tool(&self, b: BlockType) -> bool {
		b.is_tool() && self.count(b) > 0
	}

	/// The owned tool roster, in block-id order: what tapping 9 cycles through, what the rail
	/// lists, and — via [`crate::inv_grid::reconcile_layout`] — the only tools a client may claim
	/// to be holding. One definition, because the client's roster and the server's gate
	/// disagreeing is exactly how a tool gets used without being owned. EMPTY for a fresh player,
	/// whose tool slot stands bare until the first vessel is crafted.
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

	/// Fold `other` into this one, every line saturating at its own cap. Written for the
	/// legacy-character migration, the only place two inventories one player owns are combined.
	/// It goes through [`Self::add`] and [`Self::add_sub`] rather than touching the arrays, so the
	/// caps that make a tool unique and a bucket single-charge apply to a merge as to a pickup.
	/// Saturation can LOSE units at a full cap; the alternative is a cap that is not a cap.
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

	/// The essence redesign's one-time balance reset (SKILLS-DESIGN §14): clear every essence
	/// EXCEPT Aether, always the scarce one and what the sigil and the pick are saved up in.
	/// Pre-redesign balances were earned against an unbounded bench, and carrying them into an
	/// economy where essence is FOUND would hand the hardest grinders a head start. Called from
	/// the two persistence migrations only, never live play — the VERSION triggers it once.
	pub fn clear_pre_redesign_essence(&mut self) {
		for e in Essence::ALL {
			if e != Essence::Aether {
				self.subs[e as usize] = 0;
			}
		}
	}

	/// Credit sub-material units, saturating at [`MAX_STACK`].
	pub fn add_sub(&mut self, m: Essence, n: u32) {
		let c = &mut self.subs[m as usize];
		*c = c.saturating_add(n).min(MAX_STACK);
	}

	/// Can the purse cover every one of these fares AT ONCE? Summed per essence first, so two
	/// fares in one essence are one question — checked separately, each would pass alone and the
	/// second debit would fail after the first had gone through.
	pub fn can_pay(&self, fares: &[(Essence, u32)]) -> bool {
		Essence::ALL.iter().all(|&e| {
			let owed: u32 = fares
				.iter()
				.filter(|f| f.0 == e)
				.map(|f| f.1)
				.fold(0, u32::saturating_add);
			self.sub_count(e) >= owed
		})
	}

	/// Debit every fare or none — [`Self::can_pay`] then the takes, which cannot fail after it.
	#[must_use]
	pub fn try_take_fares(&mut self, fares: &[(Essence, u32)]) -> bool {
		if !self.can_pay(fares) {
			return false;
		}
		for &(e, n) in fares {
			let took = self.try_take_sub(e, n);
			debug_assert!(took, "can_pay answered for this");
		}
		true
	}

	/// The refund of [`Self::try_take_fares`].
	pub fn add_fares(&mut self, fares: &[(Essence, u32)]) {
		for &(e, n) in fares {
			self.add_sub(e, n);
		}
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

	/// Wire pairs with the retired crests' ids struck out — the migration a store runs on a record
	/// written BEFORE the raw shapes reused those ids ([`BlockType::REUSED_CREST_IDS`]), keyed on
	/// the store's version and never on the contents: at those versions the ids meant crests,
	/// which dropped on decode, and reading them as shapes would hand a dirt slab to whoever
	/// held a trophy.
	pub fn without_reused_crests(pairs: &[(u16, u32)]) -> Vec<(u16, u32)> {
		pairs
			.iter()
			.copied()
			.filter(|&(id, _)| {
				id & SUB_WIRE_FLAG != 0 || !BlockType::REUSED_CREST_IDS.contains(&id)
			})
			.collect()
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

/// One validated alchemy operation, in core vocabulary (the wire enum in digblock-net decodes u16
/// ids into this strictly). One variant: the wire's legacy `Convert` still decodes as a frame and
/// the server refuses it with the standing rebase, but no core rule exists for it any more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlchemyOp {
	/// Craft `count` batches of a shaped item from essence and feedstock.
	Craft { item: BlockType, count: u32 },
}

/// Per-op batch clamp: bounds a single request's work and keeps the
/// arithmetic far from u32 range even at MAX_STACK holdings.
pub const ALCHEMY_MAX_COUNT: u32 = 999;

/// Fold cost lines so each key appears once, keeping first-seen order.
/// The lists are a handful of entries, so the linear scan is cheaper than
/// anything that would need a hasher — and this runs once per craft.
fn totals<K: Copy + PartialEq>(lines: impl IntoIterator<Item = (K, u32)>) -> Vec<(K, u32)> {
	let mut out: Vec<(K, u32)> = Vec::new();
	for (k, n) in lines {
		match out.iter_mut().find(|(seen, _)| *seen == k) {
			Some((_, sum)) => *sum += n,
			None => out.push((k, n)),
		}
	}
	out
}

/// THE alchemy rule, shared verbatim by the server's validation and the offline engine
/// (single-player must match multiplayer bit-for-bit). `skills` is the actor's whole set: costs
/// key on the Alchemy level ([`crate::alchemy`]), and whether the op is open at all
/// ([`crate::unlock::can_craft`]) may ask another skill. Returns false — inventory untouched —
/// on any invalid, locked or unaffordable op.
#[must_use]
pub fn apply_alchemy(inv: &mut Inventory, op: AlchemyOp, skills: &crate::skills::SkillSet) -> bool {
	use crate::{alchemy, unlock};
	let level = skills.level(crate::skills::Skill::Alchemy);
	match op {
		AlchemyOp::Craft { item, count } => {
			if !(1..=ALCHEMY_MAX_COUNT).contains(&count) {
				return false;
			}
			if !unlock::can_craft(item, skills) {
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
			// All-or-nothing across every cost line, essence and feedstock alike: check
			// everything, then debit. Waste applies to the essence half only; matter is matter.
			// Summed per key BEFORE either half runs, so the amount checked and the amount
			// debited are one number — a price naming one essence twice would pass a check it
			// cannot pay. Costed before summed, since waste rounds per line and folding re-prices.
			let ideal = totals(
				ideal
					.iter()
					.map(|&(e, n)| (e, alchemy::craft_cost(n, level))),
			);
			let feedstock = totals(feedstock.iter().map(|&(f, n)| (f, n)));
			for &(e, n) in &ideal {
				if inv.sub_count(e) < n * count {
					return false;
				}
			}
			for &(f, n) in &feedstock {
				if inv.count(f) < n * count {
					return false;
				}
			}
			for &(e, n) in &ideal {
				let taken = inv.try_take_sub(e, n * count);
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
mod tests;
