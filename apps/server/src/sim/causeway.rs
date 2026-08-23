//! The Causeway's server half: the cast, each plank the caster claims, and the road coming up — at
//! expiry, on an early end, or when the caster leaves.
//!
//! Planks are ordinary journaled edits, so a joiner, the resync rotation and an evicted column read
//! them like any other glass. What is session-local is the TRAIL, which is why a departure takes
//! the road up: a trail nobody holds could never retire itself, and the planks would stand forever.
//!
//! That makes retirement the one write here that has to LAND rather than be skipped the way
//! fluid's and the sweeps' are, and it can only be written to a RESIDENT column. Two things answer
//! for it: a teleport retires the road on the spot, before the interest centre moves; and a
//! removal that still finds nowhere to go is OWED (`Sim::owed_planks`) and retried, rather than
//! dropped with the trail that knew about it.

use super::{Client, NO_CLIENT_SEQ, SERVER_ID, Sim, TICK_RATE, client_send};
use digblock_core::causeway::{self, Cell};
use digblock_core::skills::Skill;
use digblock_net::messages::{CastRefusal, RejectReason, ServerMsg};

/// How many unreturned planks the sim carries before the oldest gives
/// way. A road owes at most `causeway::TRAIL`, so this is several roads'
/// worth — a bound on a leak that only opens when a column unloads (or
/// the journal fails) between a road being laid and coming up.
const OWED_CAP: usize = 64;

impl Sim {
	/// Begin the road, or — recast while one stands — take it up. The
	/// row is a toggle because a caster who wants to descend cannot wait
	/// out a causeway that keeps catching them.
	pub(super) fn cast_causeway(&mut self, conn_id: u64) {
		let refuse = |client: &Client, reason: CastRefusal| {
			client_send(client, &ServerMsg::CastRefused { reason });
		};
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		let level = client.skills.level(Skill::Arcana);
		let secs = digblock_core::unlock::causeway_secs(level);
		if secs == 0 {
			return refuse(client, CastRefusal::Locked);
		}
		if client.causeway_until.is_some() {
			return self.lift_causeway(conn_id);
		}
		// Planks are journaled cells; an unhealthy journal stops them
		// where it stops fluid and the sweeps, and `BadTarget` is the
		// nearest true answer the wire has — rung and purse are fine,
		// there is nowhere to put the glass.
		if !self.journal.healthy() {
			return refuse(client, CastRefusal::BadTarget);
		}
		let (essence, fare) = digblock_core::alchemy::causeway_price(level);
		let until = self.tick + secs * TICK_RATE;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		if !client.inventory.try_take_sub(essence, fare) {
			let client = self.clients.get(&conn_id).expect("checked above");
			return refuse(client, CastRefusal::CantPay);
		}
		let counts = client.inventory.to_wire();
		client_send(client, &ServerMsg::InventoryFull { counts });
		client.causeway_until = Some(until);
		client.causeway.take();
		self.process_cast(conn_id, digblock_core::skills::cast_xp(essence, fare));
	}

	/// One claimed plank. The whole geometric check is [`causeway::claim_ok`] against the server's
	/// own copy of the caster's feet — the tick that produced the claim is gone, so nothing is left
	/// to re-derive. Everything else is the ordinary place contract: the cell must be claimable,
	/// unprotected, and nobody may be standing in it.
	pub(super) fn causeway_step(
		&mut self,
		conn_id: u64,
		claim: Cell,
		seq: u32,
	) -> Result<(), RejectReason> {
		let Some(client) = self.clients.get(&conn_id) else {
			return Err(RejectReason::NotLoaded);
		};
		// No road standing means the client is asserting into a spell it
		// is not casting. An honest one never does.
		if client.causeway_until.is_none() {
			return Err(RejectReason::BadBlock);
		}
		if client.causeway_tokens < 1.0 {
			return Err(RejectReason::RateLimited);
		}
		let (by, pos) = (client.id, client.player.pos);
		// Spent before the first refusal that can return, the cast
		// bucket's rule: a claim costs its work whether or not it lands,
		// so a flood of garbage claims must not be free.
		self.clients
			.get_mut(&conn_id)
			.expect("checked above")
			.causeway_tokens -= 1.0;
		if !causeway::claim_ok(claim, pos) {
			return Err(RejectReason::OutOfReach);
		}
		let Some(standing) = self.store.cell_at(claim.0, claim.1, claim.2) else {
			return Err(RejectReason::NotLoaded);
		};
		if !causeway::claims(standing) {
			return Err(RejectReason::Occluded);
		}
		let val = digblock_core::chunk::make_cell(digblock_core::block::BlockType::Wardglass, 0);
		let regions = self.requester_regions(conn_id);
		if digblock_core::protect::immutable(claim.0, claim.1, claim.2, &regions) {
			return Err(RejectReason::Protected);
		}
		// A plank closes on nobody, the Ward's rule at one cell.
		if self
			.clients
			.values()
			.filter(|c| c.live)
			.any(|c| digblock_core::player::placement_blocked(c.player.pos, claim, val))
		{
			return Err(RejectReason::InsidePlayer);
		}
		// The store write is the caller's, and its answer is the
		// authoritative previous value the broadcast must carry.
		let Some(prev) = self.store.set_cell(claim.0, claim.1, claim.2, val) else {
			return Err(RejectReason::NotLoaded);
		};
		self.apply_write(claim, val, prev, by, seq, false);
		// Only a plank actually WRITTEN joins the trail, which is what
		// stops a road ever taking up a pane it merely crossed.
		let retired = self
			.clients
			.get_mut(&conn_id)
			.expect("checked above")
			.causeway
			.lay(claim);
		if let Some(c) = retired {
			self.take_plank(c, by);
		}
		Ok(())
	}

	/// The road's per-tick half: planks the world is still owed go back
	/// up, then every road whose span has run out comes up. Expiry is a
	/// comparison rather than a countdown, so a client's own clock and
	/// the server's agree.
	pub(super) fn pump_causeways(&mut self) {
		// A retry costs a lookup per owed plank, and the list is empty in
		// every world where nothing went wrong. `take_plank` re-owes what
		// still cannot land, so this converges rather than spinning.
		if !self.owed_planks.is_empty() {
			for c in std::mem::take(&mut self.owed_planks) {
				self.take_plank(c, SERVER_ID);
			}
		}
		let tick = self.tick;
		let done: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.causeway_until.is_some_and(|u| u <= tick))
			.map(|(&id, _)| id)
			.collect();
		for conn_id in done {
			self.lift_causeway(conn_id);
		}
	}

	/// Take one caster's road up and forget it. Idempotent, so the
	/// expiry sweep, an early end and a departure can all call it.
	pub(super) fn lift_causeway(&mut self, conn_id: u64) {
		if let Some(client) = self.clients.get_mut(&conn_id) {
			client.causeway_until = None;
		}
		self.retire_road(conn_id);
	}

	/// Take the planks up but leave the span running: the road follows
	/// the caster, and a teleport is the caster leaving it behind. It has
	/// to come up HERE rather than at expiry, because only a resident
	/// column can be written and the destination is a world away — by the
	/// time the span runs out the columns under the old road are gone.
	pub(super) fn retire_road(&mut self, conn_id: u64) {
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		let (by, road) = (client.id, client.causeway.take());
		for c in road {
			self.take_plank(c, by);
		}
	}

	/// Take one plank up — but only if it is still the glass this road
	/// laid. A pane somebody dug out stays dug, and one something else
	/// has since replaced is not ours to remove.
	fn take_plank(&mut self, c: Cell, by: digblock_net::protocol::PlayerId) {
		// Removing a plank is a journaled edit exactly like laying one, so
		// an unhealthy journal stops it where it stops fluid and the
		// sweeps — but OWED rather than skipped, because the trail that
		// held it is already gone and nothing else would ever take it up.
		if !self.journal.healthy() {
			return self.owe_plank(c);
		}
		let Some(standing) = self.store.cell_at(c.0, c.1, c.2) else {
			// The column unloaded. It comes back holding this plank (the
			// journal has it), so the debt outlives the residency.
			return self.owe_plank(c);
		};
		use digblock_core::block::BlockType;
		if BlockType::from_id(digblock_core::chunk::cell_id(standing)) != BlockType::Wardglass {
			return;
		}
		let air = digblock_core::chunk::make_cell(BlockType::Air, 0);
		let Some(prev) = self.store.set_cell(c.0, c.1, c.2, air) else {
			return self.owe_plank(c);
		};
		self.apply_write(c, air, prev, by, NO_CLIENT_SEQ, false);
	}

	/// Remember a plank the world is still owed. The cap bounds the leak
	/// rather than scheduling anything: reaching it means many roads'
	/// worth of columns unloaded before their spans ran out, and a cell
	/// in a column nobody loads is one nobody can see either.
	fn owe_plank(&mut self, c: Cell) {
		if self.owed_planks.len() >= OWED_CAP {
			log::warn!("causeway: {OWED_CAP} planks owed, dropping the oldest");
			self.owed_planks.pop_front();
		}
		self.owed_planks.push_back(c);
	}
}
