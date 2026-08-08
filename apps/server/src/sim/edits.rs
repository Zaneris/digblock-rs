//! Edit validation (re-raycast from the claimed origin) and the fluid
//! step, which turns settled flow into ordinary journaled edits.

use super::{client_send, in_radius, send_frame, Sim};
use crate::store::StoreWorld;
use digblock_core::block::BlockType;
use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
use digblock_core::player::TICK;
use digblock_core::raycast::{raycast, raycast_fluid, REACH};
use digblock_net::messages::{BlockEdit, EditKind, RejectReason, ServerMsg};
use digblock_net::protocol::{
	dig_grace, PlayerId, CHAT_BURST, CHAT_RATE_PER_SEC, DIG_OFF_TARGET_RESET, EDIT_BURST,
	EDIT_POS_TOLERANCE, EDIT_RATE_PER_SEC, SERVER_ID, TICK_RATE,
};
use glam::Vec3;

pub(super) struct PendingEditReq {
	pub(super) conn_id: u64,
	pub(super) seq: u32,
	pub(super) origin: Vec3,
	pub(super) dir: Vec3,
	pub(super) kind: EditKind,
	pub(super) cell: (i32, i32, i32),
	pub(super) normal: (i32, i32, i32),
	/// The hotbar slot the edit used — Place/Collect must match its item.
	pub(super) slot: u8,
}

/// One queued client action, processed in WIRE ORDER within the tick.
/// DigStart/DigCancel must ride the same queue as edit requests: back-to-
/// back digs put "complete block A" and "start block B" in the same tick
/// drain, and applying the start out of order would clobber the hold that
/// A's completion is about to be validated against (an honest dig would
/// bounce with DigIncomplete).
pub(super) enum ClientAction {
	Edit(PendingEditReq),
	DigStart {
		conn_id: u64,
		cell: (i32, i32, i32),
	},
	DigCancel {
		conn_id: u64,
	},
	/// Alchemy rides the same queue: a craft-then-place in one tick must
	/// debit before the place's stock gate runs (wire order).
	Alchemy {
		conn_id: u64,
		op: digblock_net::messages::AlchemyOp,
	},
	/// Layout updates ride the same queue: an assign-to-bar-then-place in
	/// one tick must land the arrangement before the place's slot gate.
	Layout {
		conn_id: u64,
		hotbar: Vec<u16>,
		grid: Vec<u16>,
	},
}

/// Server-side hold-to-dig record. The cell value is captured at DigStart
/// from the server's own world — the client never claims it, so a
/// mid-hold content change (a swap like leaves → stone, or a slab grown
/// under the hold) resets rather than inheriting the cheaper timer.
pub(super) struct DigState {
	pub(super) cell: (i32, i32, i32),
	/// Full cell value (id + state) — state changes reset too.
	pub(super) cell_value: u16,
	/// Ticks the server's own aim ray has confirmed on this cell. Only
	/// these count toward completion — wall ticks with a missed aim are
	/// a pause, not progress and not a reset.
	pub(super) progress: u32,
	/// Consecutive ticks the server's own aim ray missed the cell.
	pub(super) off_target: u32,
}

impl Sim {
	/// Aim-tracking for in-progress holds: every tick, re-raycast each
	/// digging client from the server's OWN simulated position and look.
	/// An on-target tick advances `progress`; a missed tick merely
	/// doesn't (the server's view of the aim lags the client's by jitter
	/// buffering and reconciliation offsets, so transient misses are
	/// expected from honest clients). Only a sustained miss — longer
	/// than [`DIG_OFF_TARGET_RESET`] consecutive ticks — or the cell's
	/// content changing abandons the hold.
	///
	/// This is what makes the completion check unforgeable: the only
	/// client-supplied input is the initial cell, and every counted tick
	/// was verified against the server's own position and look. Do NOT
	/// "optimize" this away — it is ≤254 short DDA walks per tick, and
	/// it is the anti-cheat.
	pub(super) fn verify_digs(&mut self) {
		let store = &self.store;
		for client in self.clients.values_mut() {
			let Some(state) = &mut client.digging else {
				continue;
			};
			// Content change: reset immediately (never pause-and-resume) —
			// a collaborator can't cycle a cell under a banked timer, nor
			// grow a slab under a 1/3-priced hold.
			if store.cell_at(state.cell.0, state.cell.1, state.cell.2) != Some(state.cell_value) {
				client.digging = None;
				continue;
			}
			let block = BlockType::from_id(digblock_core::chunk::cell_id(state.cell_value));
			// Water sources are picked with the fluid-aware ray; solid
			// digs with the plain one — same filters the client used.
			let cast = if block.is_fluid_source() {
				raycast_fluid
			} else {
				raycast
			};
			// Reach slack: this ray starts at the server's eye, which may
			// trail the client's by up to the claimed-origin tolerance — a
			// target the client legitimately picked at max reach would
			// otherwise miss every verify tick and never bank progress.
			// The completing edit still enforces exact REACH from its
			// (tolerance-checked) claimed origin.
			let hit = cast(
				client.player.pos,
				client.look,
				REACH + EDIT_POS_TOLERANCE,
				&|x, y, z| store.cell_at(x, y, z),
			);
			let on_target =
				hit.is_some_and(|h| h.cell == state.cell && h.cell_value == state.cell_value);
			if on_target {
				state.progress += 1;
				state.off_target = 0;
			} else {
				state.off_target += 1;
				if state.off_target > DIG_OFF_TARGET_RESET {
					client.digging = None;
				}
			}
		}
	}

	pub(super) fn process_edits(&mut self) {
		// Refill token buckets.
		for client in self.clients.values_mut() {
			client.edit_tokens =
				(client.edit_tokens + EDIT_RATE_PER_SEC as f32 / TICK_RATE as f32).min(EDIT_BURST);
			client.chat_tokens =
				(client.chat_tokens + CHAT_RATE_PER_SEC as f32 / TICK_RATE as f32).min(CHAT_BURST);
		}
		let queue = std::mem::take(&mut self.action_queue);
		for action in queue {
			match action {
				ClientAction::Edit(req) if req.kind == EditKind::Interact => {
					self.process_interact(req)
				}
				ClientAction::Edit(req) => self.process_one_edit(req),
				ClientAction::DigStart { conn_id, cell } => self.handle_dig_start(conn_id, cell),
				ClientAction::DigCancel { conn_id } => {
					if let Some(client) = self.clients.get_mut(&conn_id) {
						client.digging = None;
					}
				}
				ClientAction::Alchemy { conn_id, op } => self.process_alchemy(conn_id, op),
				ClientAction::Layout {
					conn_id,
					hotbar,
					grid,
				} => self.process_layout(conn_id, &hotbar, &grid),
			}
		}
	}

	/// Adopt a client's item arrangement: sanitizing decode (hostile input
	/// yields a valid bar/grid), reconcile against the authoritative
	/// counts, write through to the store. The client owns arrangement;
	/// the server owns what it can afford to use.
	fn process_layout(&mut self, conn_id: u64, hotbar: &[u16], grid: &[u16]) {
		use digblock_core::hotbar::{Hotbar, HOTBAR_SLOTS, WIRE_EMPTY};
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		let mut bar_wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		for (d, s) in bar_wire.iter_mut().zip(hotbar.iter()) {
			*d = *s;
		}
		client.hotbar = Hotbar::from_wire(&bar_wire);
		client.grid = digblock_core::inv_grid::InvGrid::from_wire(grid);
		digblock_core::inv_grid::reconcile_layout(
			&client.inventory,
			&mut client.hotbar,
			&mut client.grid,
		);
		if let Some(hash) = client.identity {
			self.player_store.put(hash, client.record());
		}
	}

	fn process_one_edit(&mut self, req: PendingEditReq) {
		use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS};
		// Copy what we need out of the client entry so the &mut borrow
		// ends before the all-players InsidePlayer scan below.
		let (id, out, live, player_pos, has_token, dig_state, slot_item) = {
			let Some(client) = self.clients.get_mut(&req.conn_id) else {
				return;
			};
			let has_token = client.edit_tokens >= 1.0;
			if client.live && has_token {
				client.edit_tokens -= 1.0;
			}
			// The declared slot's item (None for an out-of-range slot).
			// An edit is also evidence of selection — mirror it so the
			// held-item broadcast tracks a click that outran the
			// jitter-buffered input path.
			let slot_item = ((req.slot as usize) < HOTBAR_SLOTS).then(|| {
				client.active_slot = req.slot;
				client.hotbar.slots()[req.slot as usize]
			});
			(
				client.id,
				client.out.clone(),
				client.live,
				client.player.pos,
				has_token,
				client
					.digging
					.as_ref()
					.map(|d| (d.cell, d.cell_value, d.progress)),
				slot_item,
			)
		};
		// A refused push here just drops the reply; a stalled peer is
		// caught (and killed) by the snapshot/broadcast paths within ticks.
		let reject = |reason: RejectReason| {
			send_frame(
				&out,
				&ServerMsg::EditRejected {
					client_seq: req.seq,
					reason,
				},
			);
		};

		if !live {
			return reject(RejectReason::NotLoaded);
		}
		if !has_token {
			return reject(RejectReason::RateLimited);
		}
		// A journal that can't write means edits would exist in memory
		// only — refuse visibly rather than silently losing the world.
		if !self.journal.healthy() {
			return reject(RejectReason::ServerError);
		}

		let place_cell = match req.kind {
			EditKind::Dig | EditKind::Collect => None,
			EditKind::Interact => unreachable!("routed to process_interact"),
			EditKind::Place { block } => {
				// Strict: unknown ids, spare bits, and states the id's
				// shape doesn't define are refused (`state_valid` is the
				// one gate). Door requests must be the LOWER+CLOSED cell —
				// the server derives the upper half itself.
				if block == BlockType::Air as u16 || !digblock_core::shapes::state_valid(block) {
					return reject(RejectReason::BadBlock);
				}
				let b = BlockType::from_id(digblock_core::chunk::cell_id(block));
				if b.is_door() {
					let (_, open, upper) =
						digblock_core::shapes::door_parts(digblock_core::chunk::cell_state(block));
					if open || upper {
						return reject(RejectReason::BadBlock);
					}
				}
				// Slab requests must be the canonical 1/3 (state 0) — the
				// server derives grown values itself, so a client can never
				// conjure a 2/3 fill (two thirds of material) from one item.
				if b.is_slab() && digblock_core::chunk::cell_state(block) != 0 {
					return reject(RejectReason::BadBlock);
				}
				Some(block)
			}
		};
		if !req.origin.is_finite() || !req.dir.is_finite() {
			return reject(RejectReason::OutOfReach);
		}
		let dir = req.dir.normalize_or_zero();
		if dir == Vec3::ZERO {
			return reject(RejectReason::OutOfReach);
		}
		if req.origin.distance(player_pos) > EDIT_POS_TOLERANCE {
			return reject(RejectReason::OutOfReach);
		}

		// Re-cast the client's ray against the server world with the
		// kind's filter: an honest client computed the identical result.
		let store = &self.store;
		let cell_at = |x: i32, y: i32, z: i32| store.cell_at(x, y, z);
		let hit = match req.kind {
			EditKind::Collect => raycast_fluid(req.origin, dir, REACH, &cell_at),
			_ => raycast(req.origin, dir, REACH, &cell_at),
		};
		let Some(hit) = hit else {
			return reject(RejectReason::OutOfReach);
		};
		if (hit.cell.0, hit.cell.1, hit.cell.2) != req.cell {
			return reject(RejectReason::Occluded);
		}
		// Collect must stop IN a fluid source (the fluid ray also stops
		// in solids — scooping stone is not a thing).
		if req.kind == EditKind::Collect && !hit.block.is_fluid_source() {
			return reject(RejectReason::Occluded);
		}

		// Timed-dig gate for dig/collect: a hold recorded by DigStart on
		// this exact cell with this exact block, with enough aim-verified
		// ticks banked (`verify_digs` counted them the whole interval).
		// The hardness-scaled grace absorbs honest shortfall: the hold's
		// first verify lands a tick after DigStart, and the server's look
		// lags the client's by the input jitter buffer.
		if place_cell.is_none() {
			let satisfied = dig_state.is_some_and(|(cell, cell_value, progress)| {
				cell == req.cell
					&& cell_value == hit.cell_value
					&& hit
						.block
						.dig_hardness(req.cell.1)
						.is_some_and(|h| progress + dig_grace(h as u32) >= h as u32)
			});
			if !satisfied {
				return reject(RejectReason::DigIncomplete);
			}
		}

		// The full (cell, value) list this action writes: one for a plain
		// dig/place, two for a door (place derives the upper half; digging
		// either half clears both).
		let air = BlockType::Air as u16;
		let mut writes: Vec<((i32, i32, i32), u16)> = Vec::with_capacity(2);
		let place_b = place_cell.map(|v| BlockType::from_id(digblock_core::chunk::cell_id(v)));
		match place_cell {
			None => {
				// Slabs shrink one third per completed dig; everything
				// else clears. Collect never stops in a slab (fluid ray).
				let new_val = if hit.block.is_slab() {
					digblock_core::shapes::slab_shrunk(hit.cell_value)
				} else {
					air
				};
				writes.push((req.cell, new_val));
				if hit.block.is_door() {
					let partner = digblock_core::shapes::door_partner(
						req.cell,
						digblock_core::chunk::cell_state(hit.cell_value),
					);
					if self
						.store
						.cell_at(partner.0, partner.1, partner.2)
						.is_some_and(|c| {
							BlockType::from_id(digblock_core::chunk::cell_id(c)).is_door()
						}) {
						writes.push((partner, air));
					}
				}
			}
			Some(v) => {
				if hit.normal == (0, 0, 0) {
					return reject(RejectReason::Occluded);
				}
				if (hit.normal.0, hit.normal.1, hit.normal.2) != req.normal {
					return reject(RejectReason::Occluded);
				}
				// A slab onto a same-material slab's TOP face grows the hit
				// cell in place — the server derives the grown value from
				// its own world, mirroring the client's prediction exactly.
				let grow = place_b.is_some_and(|b| b.is_slab())
					&& hit.normal == (0, 1, 0)
					&& digblock_core::chunk::cell_id(hit.cell_value)
						== digblock_core::chunk::cell_id(v);
				if grow {
					writes.push((req.cell, digblock_core::shapes::slab_grown(hit.cell_value)));
				} else {
					let target = (
						req.cell.0 + hit.normal.0,
						req.cell.1 + hit.normal.1,
						req.cell.2 + hit.normal.2,
					);
					writes.push((target, v));
					if place_b.is_some_and(|b| b.is_door()) {
						// The derived upper half must be loaded and free.
						let upper = (target.0, target.1 + 1, target.2);
						let free = self
							.store
							.cell_at(upper.0, upper.1, upper.2)
							.is_some_and(|c| {
								!BlockType::from_id(digblock_core::chunk::cell_id(c))
									.props()
									.collides
							});
						if !free {
							return reject(RejectReason::Occluded);
						}
						let upper_cell = v | (0b1000 << digblock_core::chunk::STATE_SHIFT);
						writes.push((upper, upper_cell));
					}
				}
				// Mirrors the local-engine rejection via the shared
				// shape-aware contract, but for every live player.
				for c in self.clients.values() {
					if !c.live {
						continue;
					}
					for &(cell, val) in &writes {
						if digblock_core::player::placement_blocked(c.player.pos, cell, val) {
							return reject(RejectReason::InsidePlayer);
						}
					}
				}
			}
		};

		// Protected cells: the world floor, and the spawn lock's region
		// when one is active. Checked against the assembled `writes` so
		// every path is covered at once — a plain dig, a place landing on
		// the neighbour cell, a door's derived upper half, a slab grow.
		// Honest clients predict this and never send the edit.
		let lock = self.spawn_lock();
		if writes
			.iter()
			.any(|&(c, _)| digblock_core::protect::immutable(c.0, c.1, c.2, lock.as_ref()))
		{
			return reject(RejectReason::Protected);
		}

		// Slot enforcement: the edit must use the item actually in its
		// declared hotbar slot (the arrangement arrived in-order on the
		// same stream, so honest clients can never trip this). Digs are
		// tool-free and exempt. Sits with the stock gate, after every
		// world-shaped rejection, so rejection precedence is unchanged
		// for pre-v10-shaped mistakes.
		let slot_ok = match req.kind {
			EditKind::Dig => true,
			EditKind::Collect => slot_item == Some(Some(HotbarItem::WaterTool)),
			EditKind::Place { .. } => match slot_item {
				Some(Some(HotbarItem::Block(b))) => place_b == Some(b),
				// Pouring the bucket. Water is the ONLY thing the tool
				// places, and the tool is the only thing that places water:
				// `reconcile_layout` refuses to leave water in a block slot,
				// so the arm above can no longer match it. The stock gate
				// below enforces the charge — an empty bucket's pour bounces
				// as NoStock.
				Some(Some(HotbarItem::WaterTool)) => place_b == Some(BlockType::Water),
				_ => false,
			},
			EditKind::Interact => unreachable!("routed to process_interact"),
		};
		if !slot_ok {
			return reject(RejectReason::WrongSlot);
		}

		// Stock gate: the LAST check before mutating the world, so a
		// refused take never needs unwinding of anything else. The
		// InsidePlayer scan's shared borrow has ended — re-fetch mutably.
		// One debit covers a door's two cells.
		let client = self
			.clients
			.get_mut(&req.conn_id)
			.expect("checked live above");
		// The scoop's half of the same gate: a charged bucket has nowhere
		// to put a second source. Refusing HERE rather than letting the
		// credit silently saturate is what stops a full bucket deleting
		// the spring it's aimed at for nothing.
		if req.kind == EditKind::Collect && client.inventory.bucket_full() {
			return reject(RejectReason::BucketFull);
		}
		if let Some(b) = place_b {
			if !client.inventory.try_take(b, 1) {
				let inv = client.inventory.to_wire();
				reject(RejectReason::NoStock);
				// Drift-repair backstop: the client predicted stock it
				// doesn't have — hand it the authoritative base.
				send_frame(&out, &ServerMsg::InventoryFull { counts: inv });
				return;
			}
		}

		let (first_cell, first_val) = writes[0];
		let Some(prev_cell) =
			self.store
				.set_cell(first_cell.0, first_cell.1, first_cell.2, first_val)
		else {
			// Refund the debit — the world never changed.
			if let Some(b) = place_b {
				if let Some(client) = self.clients.get_mut(&req.conn_id) {
					client.inventory.add(b, 1);
				}
			}
			return reject(RejectReason::NotLoaded);
		};
		let prev = BlockType::from_id(digblock_core::chunk::cell_id(prev_cell));

		// Dig/collect credit comes from the journaled truth (the actual
		// replaced cell), not the raycast hit. One credit per door pair.
		let client = self
			.clients
			.get_mut(&req.conn_id)
			.expect("checked live above");
		if place_cell.is_none() {
			if let Some(drop) = prev.dig_drop() {
				client.inventory.add(drop, 1);
				// Mirror the client's auto-assign of a newly acquired type
				// (same deterministic derivation), so the very next place
				// from that slot validates even before the client's Layout
				// echo lands.
				digblock_core::inv_grid::reconcile_layout(
					&client.inventory,
					&mut client.hotbar,
					&mut client.grid,
				);
			}
			client.digging = None;
		}
		if let Some(hash) = client.identity {
			self.player_store.put(hash, client.record());
		}

		// Journal + broadcast the first write under the client's seq, the
		// companion (a door's other half) under seq 0 — the in-order
		// stream confirms the pair before the companion applies
		// idempotently over the client's identical prediction.
		self.apply_write(first_cell, first_val, prev_cell, id, req.seq, false);
		for &(cell, val) in &writes[1..] {
			if let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) {
				self.apply_write(cell, val, prev, id, 0, false);
			}
		}
	}

	/// Journal, fluid-schedule, and broadcast one landed world write (the
	/// store mutation already happened). `already_set` is a lie-guard for
	/// the doc only — callers pass the actual prev they observed.
	fn apply_write(
		&mut self,
		cell: (i32, i32, i32),
		val: u16,
		prev_cell: u16,
		by: PlayerId,
		client_seq: u32,
		_already_set: bool,
	) {
		let edit = BlockEdit {
			x: cell.0,
			y: cell.1,
			z: cell.2,
			block: val,
		};
		let (seq, new_column) = self.journal.append(edit, by);
		if new_column {
			self.enroll_new_column(edit);
		}
		self.fluid.schedule_around(cell.0, cell.1, cell.2);
		self.broadcast_edit(seq, by, client_seq, edit, prev_cell);
	}

	/// A use/interact click: re-raycast with the interact-aware ray and
	/// toggle the hit door pair. Same preamble gates as edits; `normal`
	/// is not required to match (a swung open leaf has no meaningful
	/// entry face).
	fn process_interact(&mut self, req: PendingEditReq) {
		let (id, out, live, player_pos, has_token) = {
			let Some(client) = self.clients.get_mut(&req.conn_id) else {
				return;
			};
			let has_token = client.edit_tokens >= 1.0;
			if client.live && has_token {
				client.edit_tokens -= 1.0;
			}
			(
				client.id,
				client.out.clone(),
				client.live,
				client.player.pos,
				has_token,
			)
		};
		let reject = |reason: RejectReason| {
			send_frame(
				&out,
				&ServerMsg::EditRejected {
					client_seq: req.seq,
					reason,
				},
			);
		};
		if !live {
			return reject(RejectReason::NotLoaded);
		}
		if !has_token {
			return reject(RejectReason::RateLimited);
		}
		if !self.journal.healthy() {
			return reject(RejectReason::ServerError);
		}
		if !req.origin.is_finite() || !req.dir.is_finite() {
			return reject(RejectReason::OutOfReach);
		}
		let dir = req.dir.normalize_or_zero();
		if dir == Vec3::ZERO {
			return reject(RejectReason::OutOfReach);
		}
		if req.origin.distance(player_pos) > EDIT_POS_TOLERANCE {
			return reject(RejectReason::OutOfReach);
		}
		let store = &self.store;
		let hit = digblock_core::raycast::raycast_interact(req.origin, dir, REACH, &|x, y, z| {
			store.cell_at(x, y, z)
		});
		let Some(hit) = hit else {
			return reject(RejectReason::OutOfReach);
		};
		if (hit.cell.0, hit.cell.1, hit.cell.2) != req.cell {
			return reject(RejectReason::Occluded);
		}
		if !hit.block.is_door() {
			return reject(RejectReason::Occluded);
		}
		use digblock_core::chunk::{cell_id, cell_state};
		use digblock_core::shapes::{door_partner, door_toggled};
		let cur = hit.cell_value;
		let partner = door_partner(hit.cell, cell_state(cur));
		let mut writes = vec![(hit.cell, cur, door_toggled(cur))];
		if let Some(pc) = self.store.cell_at(partner.0, partner.1, partner.2) {
			if BlockType::from_id(cell_id(pc)).is_door() {
				writes.push((partner, pc, door_toggled(pc)));
			}
		}
		// A door inside a protected region is furniture, not a mechanism.
		let lock = self.spawn_lock();
		if writes
			.iter()
			.any(|&(c, _, _)| digblock_core::protect::immutable(c.0, c.1, c.2, lock.as_ref()))
		{
			return reject(RejectReason::Protected);
		}
		// Closing must not trap any live player in the leaf.
		let closing = cell_state(cur) & 0b0100 != 0;
		if closing {
			for c in self.clients.values() {
				if !c.live {
					continue;
				}
				for &(cell, _, val) in &writes {
					if digblock_core::player::placement_blocked(c.player.pos, cell, val) {
						return reject(RejectReason::InsidePlayer);
					}
				}
			}
		}
		let mut first = true;
		for &(cell, _, val) in &writes {
			if let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) {
				let seq = if first { req.seq } else { 0 };
				self.apply_write(cell, val, prev, id, seq, false);
			}
			first = false;
		}
	}

	/// Alchemy convert/craft: validate against the core tables, apply the
	/// shared `apply_alchemy` rule, persist, and answer — success or
	/// failure — with the authoritative `InventoryFull` rebase.
	fn process_alchemy(&mut self, conn_id: u64, op: digblock_net::messages::AlchemyOp) {
		use digblock_core::inventory::{apply_alchemy, AlchemyOp as CoreOp};
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		// Shares the edit token bucket; over-budget ops drop silently
		// (the client locally gates affordability, so honest clients
		// never burst here).
		if client.edit_tokens < 1.0 {
			return;
		}
		client.edit_tokens -= 1.0;
		let core_op = match op {
			digblock_net::messages::AlchemyOp::Convert { block, count } => {
				BlockType::try_from_id(block).map(|b| CoreOp::Convert { block: b, count })
			}
			digblock_net::messages::AlchemyOp::Craft { item, count } => {
				BlockType::try_from_id(item).map(|b| CoreOp::Craft { item: b, count })
			}
		};
		if let Some(core_op) = core_op {
			if apply_alchemy(&mut client.inventory, core_op) {
				// Mirror the client's auto-assign of freshly crafted items
				// (it reconciles on the InventoryFull rebase below).
				digblock_core::inv_grid::reconcile_layout(
					&client.inventory,
					&mut client.hotbar,
					&mut client.grid,
				);
			}
		}
		if let Some(hash) = client.identity {
			self.player_store.put(hash, client.record());
		}
		let counts = client.inventory.to_wire();
		send_frame(&client.out, &ServerMsg::InventoryFull { counts });
	}

	/// A column just gained its FIRST edit: enroll every in-radius client
	/// BEFORE the broadcast, so the `EditApplied` itself carries the data
	/// and no JournalColumn resend is ever needed for it. This is what
	/// lets the steady-state residency sync skip journal scans entirely.
	fn enroll_new_column(&mut self, edit: BlockEdit) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		for c in self.clients.values_mut() {
			if in_radius(col, c.center, c.draw_dist) {
				c.synced_columns.insert(col);
			}
		}
	}

	pub(super) fn broadcast_edit(
		&self,
		seq: u64,
		by: PlayerId,
		client_seq: u32,
		edit: BlockEdit,
		prev: u16,
	) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		let msg = ServerMsg::EditApplied {
			seq,
			tick: self.tick,
			by,
			client_seq,
			edit,
			prev,
		};
		for c in self.clients.values() {
			if c.synced_columns.contains(&col) || c.id == by {
				client_send(c, &msg);
			}
		}
	}

	pub(super) fn step_fluid(&mut self) {
		// Unhealthy journal: stop mutating the world durably — fluid
		// writes are journaled edits like any other.
		if !self.journal.healthy() {
			return;
		}
		self.flow_accum = (self.flow_accum + TICK).min(1.0);
		let lock = self.spawn_lock();
		while self.flow_accum >= FLOW_STEP_SECS {
			self.flow_accum -= FLOW_STEP_SECS;
			let mut edits = Vec::new();
			let mut world = StoreWorld {
				store: &mut self.store,
				edits: &mut edits,
				lock,
			};
			self.fluid.step(&mut world, FLOW_BUDGET);
			for (edit, prev) in edits {
				let (seq, new_column) = self.journal.append(edit, SERVER_ID);
				if new_column {
					self.enroll_new_column(edit);
				}
				self.broadcast_edit(seq, SERVER_ID, 0, edit, prev);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::super::harness::*;
	use super::super::{Sim, SimEvent};
	use digblock_core::block::BlockType;
	use digblock_core::raycast::{raycast, REACH};
	use digblock_net::messages::{ClientMsg, EditKind, RejectReason, ServerMsg};
	use glam::Vec3;

	#[test]
	fn edits_validate_apply_and_journal() {
		let mut sim = flat_sim("edits");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;

		// The ground block straight down is in reach.
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
			.expect("ground under spawn");

		let dig = |seq: u32, origin: Vec3, cell: (i32, i32, i32)| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 100,
				origin: origin.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [cell.0, cell.1, cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		};

		// 1. Wrong cell → Occluded.
		sim.tick_once(vec![dig(1, pos, (hit.cell.0 + 1, hit.cell.1, hit.cell.2))]);
		// 2. Origin 3 blocks off → OutOfReach.
		sim.tick_once(vec![dig(2, pos + Vec3::new(3.0, 0.0, 0.0), hit.cell)]);
		// 3. Honest dig with the full timed hold (stone: 90 ticks) → applied.
		let mut t = 0;
		let dug = dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
		assert_eq!(dug, hit.cell);
		c.drain();

		let rejected: Vec<(u32, RejectReason)> = c
			.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::EditRejected { client_seq, reason } => Some((*client_seq, *reason)),
				_ => None,
			})
			.collect();
		assert!(
			rejected.contains(&(1, RejectReason::Occluded)),
			"{rejected:?}"
		);
		assert!(
			rejected.contains(&(2, RejectReason::OutOfReach)),
			"{rejected:?}"
		);
		let applied = c.msgs.iter().find_map(|m| match m {
			ServerMsg::EditApplied {
				client_seq, edit, ..
			} if *client_seq == 3 => Some(*edit),
			_ => None,
		});
		let edit = applied.expect("dig 3 applied");
		assert_eq!((edit.x, edit.y, edit.z), hit.cell);
		assert_eq!(edit.block, BlockType::Air as u16);
		assert_eq!(
			sim.test_block_at(edit.x, edit.y, edit.z),
			Some(BlockType::Air)
		);
		assert!(sim
			.journal
			.index
			.column(edit.x.div_euclid(16), edit.z.div_euclid(16))
			.is_some_and(
				|col| col.get(&(edit.x, edit.y, edit.z)) == Some(&(BlockType::Air as u16))
			));
	}

	/// The world floor (y = BEDROCK_Y) is indestructible: DigStart
	/// records no hold there, so even a full-length honest hold ends in
	/// DigIncomplete and the block survives. World: a single stone layer
	/// at y = 0 with the player standing on it, so the floor is the ray
	/// target — no digging down through 126 blocks first.
	#[test]
	fn bedrock_floor_refuses_digs() {
		use digblock_core::block::BEDROCK_Y;
		use digblock_core::chunk::{ChunkData, AIR_CHUNK, WORLD_VCHUNK};
		use std::sync::Arc;

		let source: crate::store::ColumnSource = Arc::new(|_cx, _cz| {
			let mut floor = ChunkData::default();
			for z in 0..16 {
				for x in 0..16 {
					floor.set(x, 0, z, BlockType::Stone);
				}
			}
			let floor = Arc::new(floor);
			(0..WORLD_VCHUNK as usize)
				.map(|cy| {
					if cy == 0 {
						floor.clone()
					} else {
						AIR_CHUNK.clone()
					}
				})
				.collect()
		});
		let spawn = Vec3::new(8.5, BEDROCK_Y as f32 + 2.5, 8.5);
		let dir = temp_dir("bedrock");
		let mut sim = super::super::Sim::new_with_source(SEED, &dir, 1, 2, source, spawn).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let mut t = 0;
		let cell = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		assert_eq!(cell.1, BEDROCK_Y, "the ray targeted the floor layer");
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete,
				}
			)),
			"{:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Stone),
			"floor block survived"
		);
	}

	/// The reserved state nibble travels the wire but nothing consumes
	/// it yet: placements carrying state bits are BadBlock, not silently
	/// stripped — the rejection is the contract until states ship.
	#[test]
	fn place_with_state_bits_is_bad_block() {
		let mut sim = flat_sim("statebits");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place {
					block: (BlockType::Dirt as u16) | (1 << digblock_core::chunk::STATE_SHIFT),
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::BadBlock,
					..
				}
			)),
			"{:?}",
			c.msgs
		);
	}

	#[test]
	fn unhealthy_journal_rejects_edits_with_server_error() {
		let mut sim = flat_sim("jhealth");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();

		sim.journal.force_write_failure = true;
		// Each accepted timed dig hits one failing append; after three the
		// journal flips unhealthy and the next request is refused.
		let mut t = 0;
		for seq in 1..=3u32 {
			dig_hold(&mut sim, 1, down, 90, seq, EditKind::Dig, &mut t);
		}
		// The dig target changes as blocks vanish, so also fire a plain
		// guaranteed-processed request once unhealthy.
		assert!(!sim.journal.healthy(), "injected failures flipped health");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 9,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [0, 0, 0],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::ServerError,
					..
				}
			)),
			"unhealthy journal surfaces as ServerError: {:?}",
			c.msgs
		);
	}

	/// An instant dig (no DigStart) and an under-held dig both fail the
	/// timing gate; the honest hold at the grace boundary is accepted.
	#[test]
	fn dig_timing_gate() {
		let mut sim = flat_sim("digtime");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		// No DigStart at all.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		// Held for only 10 ticks (stone needs 90 − 16 grace).
		let mut t = 0;
		dig_hold(&mut sim, 1, down, 10, 2, EditKind::Dig, &mut t);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigCancel,
		}]);
		// Honest hold: accepted (dig_hold holds the full hardness).
		let dug = dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
		c.drain();

		for early in [1u32, 2] {
			assert!(
				c.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::EditRejected {
						client_seq,
						reason: RejectReason::DigIncomplete
					} if *client_seq == early
				)),
				"seq {early} should be DigIncomplete: {:?}",
				c.msgs
			);
		}
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq: 3, edit, .. }
					if (edit.x, edit.y, edit.z) == dug
			)),
			"{:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Stone),
			1,
			"dig credited the drop"
		);
	}

	/// Looking away for a SUSTAINED stretch (past DIG_OFF_TARGET_RESET)
	/// abandons the server's record entirely.
	#[test]
	fn look_away_mid_hold_resets() {
		let mut sim = flat_sim("lookaway");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		// 30 on target, 45 at the horizon (> the 30-tick abandon), then
		// back on target well past the would-be completion time: the
		// hold is gone, the banked 30 ticks with it.
		for _ in 0..30 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		for _ in 0..45 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, Vec3::Z)], 1)]);
		}
		assert!(
			sim.clients.get(&1).unwrap().digging.is_none(),
			"sustained miss abandoned the hold"
		);
		for _ in 0..90 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete
				}
			)),
			"look-away reset the hold: {:?}",
			c.msgs
		);
	}

	/// A brief aim miss (over the old 8-tick reset, under the 30-tick
	/// abandon) PAUSES the hold: the missed ticks don't count toward
	/// completion, but the banked progress survives and the dig finishes
	/// once enough verified ticks accumulate.
	#[test]
	fn aim_miss_pauses_hold_instead_of_resetting() {
		let mut sim = flat_sim("digpause");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		// 40 verified ticks, then 15 off target: the wall clock says 55
		// but only ~40 count — completing now is premature (this also
		// pins that misses are a pause, not elapsed time).
		for _ in 0..40 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		for _ in 0..15 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, Vec3::Z)], 1)]);
		}
		let dig = |seq: u32| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		};
		sim.tick_once(vec![dig(1)]);
		// Back on target: the banked 40 survive the miss stretch (which
		// would have reset the hold under consecutive-miss-8 semantics),
		// so ~50 more verified ticks complete the stone (needs ≥ 74).
		for _ in 0..50 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.tick_once(vec![dig(2)]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete
				}
			)),
			"paused ticks don't count toward completion: {:?}",
			c.msgs
		);
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq: 2, edit, .. }
					if (edit.x, edit.y, edit.z) == hit.cell
			)),
			"hold survived the brief miss and completed: {:?}",
			c.msgs
		);
	}

	/// Regression: back-to-back digs put "complete block A" and "start
	/// block B" in the SAME tick drain. Wire order must hold — the new
	/// DigStart must not clobber the hold A's completion validates
	/// against (it did, and honest rapid digging bounced blocks back).
	#[test]
	fn same_tick_complete_and_restart_keeps_wire_order() {
		let mut sim = flat_sim("digrace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		for _ in 0..95 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		// One tick carries both the completing EditRequest for A and the
		// DigStart for the block beneath it (what a client does 16 ms
		// after finishing A).
		let below = (hit.cell.0, hit.cell.1 - 1, hit.cell.2);
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq: 1,
					tick: 0,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Dig,
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [0, 0, 0],
					slot: 0,
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [below.0, below.1, below.2],
				},
			},
		]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq: 1, edit, .. }
					if (edit.x, edit.y, edit.z) == hit.cell
			)),
			"completion processed before the same-tick restart: {:?}",
			c.msgs
		);
		let digging = sim
			.clients
			.get(&1)
			.unwrap()
			.digging
			.as_ref()
			.map(|d| (d.cell, d.cell_value));
		assert_eq!(
			digging,
			Some((below, BlockType::Stone as u16)),
			"the new hold took over after the completion"
		);
	}

	/// The cell's content changing mid-hold resets the record — a
	/// collaborator can't swap a cheap block in under a running timer.
	#[test]
	fn block_swap_mid_hold_resets() {
		let mut sim = flat_sim("swap");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		let mut t = 0;
		t += 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		for _ in 0..30 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		// The target vanishes out from under the hold (authoritative
		// change by "someone else"), then reappears.
		sim.store
			.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Air);
		for _ in 0..12 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.store
			.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Stone);
		for _ in 0..60 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::DigIncomplete
				}
			)),
			"content swap reset the hold: {:?}",
			c.msgs
		);
	}

	/// Dig → place round trip: the credit funds exactly one placement;
	/// the second placement is NoStock and carries the repair snapshot.
	#[test]
	fn place_consumes_stock_then_no_stock() {
		let mut sim = flat_sim("stock");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let mut t = 0;
		dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		assert_eq!(sim.test_inventory_count(1, BlockType::Stone), 1);

		// Place onto ground a few blocks out — outside our own capsule.
		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let place = |sim: &mut super::super::Sim, seq: u32| {
			let hit =
				raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground out");
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 0,
					origin: pos.to_array(),
					dir: out.to_array(),
					kind: EditKind::Place {
						block: BlockType::Stone as u16,
					},
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
					slot: 0,
				},
			}]);
		};
		place(&mut sim, 2);
		assert_eq!(sim.test_inventory_count(1, BlockType::Stone), 0);
		place(&mut sim, 3);
		c.drain();
		assert!(
			c.msgs.iter().any(
				|m| matches!(m, ServerMsg::EditApplied { client_seq: 2, edit, .. } if edit.block == BlockType::Stone as u16)
			),
			"funded placement applied: {:?}",
			c.msgs
		);
		let no_stock_at = c
			.msgs
			.iter()
			.position(|m| {
				matches!(
					m,
					ServerMsg::EditRejected {
						client_seq: 3,
						reason: RejectReason::NoStock
					}
				)
			})
			.expect("unfunded placement refused");
		assert!(
			matches!(
				c.msgs.get(no_stock_at + 1),
				Some(ServerMsg::InventoryFull { .. })
			),
			"NoStock is followed by the repair snapshot: {:?}",
			c.msgs
		);
	}

	/// Water tool round trip: place a source (funded), collect it back
	/// with the fluid ray + hold, and a Collect aimed at a flow cell
	/// can't name the flow (the ray passes it) — Occluded.
	#[test]
	fn collect_water_end_to_end() {
		let mut sim = flat_sim("collect");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.clients
			.get_mut(&1)
			.unwrap()
			.inventory
			.add(BlockType::Water, 1);

		// Place the source on the ground a few blocks out.
		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let ground = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place {
					block: BlockType::Water as u16,
				},
				cell: [ground.cell.0, ground.cell.1, ground.cell.2],
				normal: [
					ground.normal.0 as i8,
					ground.normal.1 as i8,
					ground.normal.2 as i8,
				],
				// Water places through the tool's slot (pinned last).
				slot: digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1,
			},
		}]);
		assert_eq!(sim.test_inventory_count(1, BlockType::Water), 0);
		let source = (
			ground.cell.0 + ground.normal.0,
			ground.cell.1 + ground.normal.1,
			ground.cell.2 + ground.normal.2,
		);
		assert_eq!(
			sim.test_block_at(source.0, source.1, source.2),
			Some(BlockType::Water)
		);
		// Let the fluid spread so flow cells exist around the source.
		for _ in 0..40 {
			sim.tick_once(vec![]);
		}

		// Collect naming a flow cell: the fluid ray never stops there.
		let flow_cell = (source.0 + 1, source.1, source.2);
		assert!(sim
			.test_block_at(flow_cell.0, flow_cell.1, flow_cell.2)
			.is_some_and(|b| b.is_water() && !b.is_fluid_source()));
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 2,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Collect,
				cell: [flow_cell.0, flow_cell.1, flow_cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);

		// Collect the source with the real hold (water: 20 ticks).
		let toward = (Vec3::new(
			source.0 as f32 + 0.5,
			source.1 as f32 + 0.5,
			source.2 as f32 + 0.5,
		) - pos)
			.normalize();
		let mut t = 100;
		let collected = dig_hold(&mut sim, 1, toward, 20, 3, EditKind::Collect, &mut t);
		assert_eq!(collected, source);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 2,
					reason: RejectReason::Occluded
				}
			)),
			"flow cell is not collectible: {:?}",
			c.msgs
		);
		assert!(
			c.msgs.iter().any(
				|m| matches!(m, ServerMsg::EditApplied { client_seq: 3, edit, .. } if edit.block == BlockType::Air as u16)
			),
			"source collected: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Water),
			1,
			"water credited back"
		);
		// The scooped pool drains (lone source's spread, no regen pair).
		for _ in 0..600 {
			sim.tick_once(vec![]);
		}
		assert!(sim
			.test_block_at(flow_cell.0, flow_cell.1, flow_cell.2)
			.is_some_and(|b| !b.is_water()));
	}

	/// The bucket carries one charge, and the server owns it: an empty
	/// one can't pour, a full one can't scoop. The second half matters
	/// most — a refused scoop must leave the spring standing, not consume
	/// it into a bucket that had no room.
	#[test]
	fn the_bucket_holds_exactly_one_charge() {
		let mut sim = flat_sim("charge");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let tool_slot = digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1;
		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let ground = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		let pour = |seq: u32| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place {
					block: BlockType::Water as u16,
				},
				cell: [ground.cell.0, ground.cell.1, ground.cell.2],
				normal: [
					ground.normal.0 as i8,
					ground.normal.1 as i8,
					ground.normal.2 as i8,
				],
				slot: tool_slot,
			},
		};

		// An empty bucket pours nothing.
		sim.tick_once(vec![pour(1)]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::NoStock
				}
			)),
			"an empty bucket poured: {:?}",
			c.msgs
		);

		// Charge it and pour for real.
		sim.clients
			.get_mut(&1)
			.unwrap()
			.inventory
			.add(BlockType::Water, 1);
		sim.tick_once(vec![pour(2)]);
		let source = (
			ground.cell.0 + ground.normal.0,
			ground.cell.1 + ground.normal.1,
			ground.cell.2 + ground.normal.2,
		);
		assert_eq!(
			sim.test_block_at(source.0, source.1, source.2),
			Some(BlockType::Water)
		);
		assert!(
			!sim.clients[&1].inventory.bucket_full(),
			"the pour emptied it"
		);

		// Charge it again by hand and aim the scoop at the source we just
		// poured: a full bucket has nowhere to put it.
		sim.clients
			.get_mut(&1)
			.unwrap()
			.inventory
			.add(BlockType::Water, 1);
		let toward = (Vec3::new(
			source.0 as f32 + 0.5,
			source.1 as f32 + 0.5,
			source.2 as f32 + 0.5,
		) - pos)
			.normalize();
		let mut t = 100;
		dig_hold(&mut sim, 1, toward, 20, 3, EditKind::Collect, &mut t);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 3,
					reason: RejectReason::BucketFull
				}
			)),
			"a charged bucket scooped again: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_block_at(source.0, source.1, source.2),
			Some(BlockType::Water),
			"the refused scoop must not have consumed the source"
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::Water), 1);
	}

	/// Two placed sources one apart: the middle regenerates into a source
	/// server-side and broadcasts as an ordinary SERVER_ID edit.
	#[test]
	fn regen_broadcasts_as_server_edit() {
		let mut sim = flat_sim("regen");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let y = 127; // first air layer over the flat stone ground
		assert!(sim.store.set_block(6, y, 8, BlockType::Water).is_some());
		assert!(sim.store.set_block(8, y, 8, BlockType::Water).is_some());
		sim.fluid.schedule_around(6, y, 8);
		sim.fluid.schedule_around(8, y, 8);
		let mut regenerated = false;
		for _ in 0..300 {
			sim.tick_once(vec![]);
			if sim.test_block_at(7, y, 8) == Some(BlockType::Water) {
				regenerated = true;
				break;
			}
		}
		assert!(regenerated, "middle cell became a source");
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { by, edit, .. }
					if *by == digblock_net::protocol::SERVER_ID
						&& (edit.x, edit.y, edit.z) == (7, y, 8)
						&& edit.block == BlockType::Water as u16
			)),
			"regen broadcast as a SERVER_ID edit: {:?}",
			c.msgs
		);
	}

	/// Same-tick DigStarts resolve in wire order — the LAST start wins,
	/// exactly like the client's retarget stream. The old first-wins
	/// per-tick cap silently dropped the restart half of a cancel +
	/// restart pair bunched into one tick drain, leaving an honest hold
	/// with no server record (guaranteed DigIncomplete on completion).
	#[test]
	fn same_tick_dig_starts_resolve_in_wire_order() {
		let mut sim = flat_sim("digflood");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		let other = (hit.cell.0, hit.cell.1 - 1, hit.cell.2);
		let start = |cell: (i32, i32, i32)| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [cell.0, cell.1, cell.2],
			},
		};
		let cancel = || SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigCancel,
		};
		// A retarget flick bunched into one server tick: start A, then
		// cancel + restart on B — B's hold must stand.
		sim.tick_once(vec![start(hit.cell), cancel(), start(other)]);
		let digging = |sim: &super::super::Sim| {
			sim.clients
				.get(&1)
				.unwrap()
				.digging
				.as_ref()
				.map(|d| d.cell)
		};
		assert_eq!(digging(&sim), Some(other), "last start wins the tick");
		// And the next tick's start replaces it as usual.
		sim.tick_once(vec![start(hit.cell)]);
		assert_eq!(digging(&sim), Some(hit.cell));
	}

	#[test]
	fn place_into_player_and_rate_limit() {
		let mut sim = flat_sim("place");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

		// Placing on the ground's top face targets our own body cell.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 100,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place {
					block: BlockType::Dirt as u16,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::InsidePlayer
				}
			)),
			"{:?}",
			c.msgs
		);

		// Hammer digs: the token bucket runs dry.
		let digs: Vec<SimEvent> = (10..30)
			.map(|seq| SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 100,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Dig,
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [0, 0, 0],
					slot: 0,
				},
			})
			.collect();
		sim.tick_once(digs);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::RateLimited,
					..
				}
			)),
			"rate limiter engaged"
		);
	}

	// ---- sub-block shapes (v8) ------------------------------------------

	use digblock_core::chunk::{cell_state, make_cell, STATE_SHIFT};
	use digblock_core::shapes::{door_state, stair_state, Facing};

	/// Aim at the ground and place `cell_value`; returns (support cell,
	/// target cell). The caller grants stock first.
	fn place_cell(sim: &mut Sim, c: &mut TestClient, seq: u32, cell_value: u16) -> (i32, i32, i32) {
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place { block: cell_value },
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot: 0,
			},
		}]);
		c.drain();
		(
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		)
	}

	#[test]
	fn place_stair_with_facing_applies_full_cell() {
		let mut sim = flat_sim("stairplace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::StoneStairs, 1);
		let want = make_cell(BlockType::StoneStairs, stair_state(Facing::NegZ));
		let target = place_cell(&mut sim, &mut c, 1, want);
		assert_eq!(
			sim.test_cell_at(target.0, target.1, target.2),
			Some(want),
			"state nibble survives to the store"
		);
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { edit, .. } if edit.block == want
			)),
			"broadcast carries the full cell: {:?}",
			c.msgs
		);
	}

	#[test]
	fn stateful_place_gates() {
		let mut sim = flat_sim("stategates");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodStairs, 5);
		sim.test_grant(1, BlockType::WoodDoor, 5);
		let bad = [
			// Stateless block with state bits: still BadBlock.
			(BlockType::Dirt as u16) | (1 << STATE_SHIFT),
			// Stair with the non-facing state bits set.
			make_cell(BlockType::WoodStairs, 0b0100),
			// Door requests must be lower+closed — server derives upper.
			make_cell(BlockType::WoodDoor, door_state(Facing::PosX, true, false)),
			make_cell(BlockType::WoodDoor, door_state(Facing::PosX, false, true)),
			// Spare bits 10-11.
			(BlockType::WoodStairs as u16) | (1 << 10),
		];
		for (i, cell) in bad.into_iter().enumerate() {
			place_cell(&mut sim, &mut c, 10 + i as u32, cell);
			assert!(
				c.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::EditRejected {
						client_seq,
						reason: RejectReason::BadBlock,
					} if *client_seq == 10 + i as u32
				)),
				"cell {cell:#06x} refused: {:?}",
				c.msgs
			);
		}
	}

	// ---- stackable slabs ------------------------------------------------

	/// Aim from the eye at `point` and place `cell_value`, declaring
	/// `slot`. The shared angled-down ray enters a grown slab through its
	/// SIDE face (no grow), so slab tests aim at the top face directly.
	fn place_aimed(
		sim: &mut Sim,
		c: &mut TestClient,
		seq: u32,
		cell_value: u16,
		point: Vec3,
		slot: u8,
	) {
		let pos = sim.test_player(1).unwrap().pos;
		let dir = (point - pos).normalize();
		let hit = raycast(pos, dir, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: dir.to_array(),
				kind: EditKind::Place { block: cell_value },
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot,
			},
		}]);
		c.drain();
	}

	/// Center of a cell's top surface at `height` twelfths, a hair inside
	/// the box so the aimed ray enters through the TOP face.
	fn top_point(cell: (i32, i32, i32), height_twelfths: u8) -> Vec3 {
		Vec3::new(
			cell.0 as f32 + 0.5,
			cell.1 as f32 + height_twelfths as f32 / 12.0 - 0.03,
			cell.2 as f32 + 0.5,
		)
	}

	/// Three top-face places: fresh 1/3, grow to 2/3 IN PLACE, grow to
	/// the plain base block — one slab debit each, nothing written above.
	#[test]
	fn slab_top_place_grows_to_base_block_with_per_step_debits() {
		let mut sim = flat_sim("slabgrow");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodSlab, 3);
		let slab0 = BlockType::WoodSlab as u16;

		let cell = place_cell(&mut sim, &mut c, 1, slab0);
		assert_eq!(sim.test_cell_at(cell.0, cell.1, cell.2), Some(slab0));
		assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 2);

		// The ray now stops in the slab itself; place_cell returns
		// hit.cell + normal (the cell above) — the write must land in
		// the slab cell, not there.
		let above = place_cell(&mut sim, &mut c, 2, slab0);
		assert_eq!(above, (cell.0, cell.1 + 1, cell.2), "ray hit the slab top");
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(make_cell(BlockType::WoodSlab, 1)),
			"grew in place to 2/3"
		);
		assert_eq!(
			sim.test_block_at(above.0, above.1, above.2),
			Some(BlockType::Air),
			"nothing written above"
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 1);

		place_aimed(&mut sim, &mut c, 3, slab0, top_point(cell, 8), 0);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(BlockType::Wood as u16),
			"third grow completes into the plain base block"
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 0);
		// The full cube is ordinary Wood now: no fourth grow — placing
		// on its top writes a fresh 1/3 in the cell above.
		sim.test_grant(1, BlockType::WoodSlab, 1);
		place_aimed(&mut sim, &mut c, 4, slab0, top_point(cell, 12), 0);
		assert_eq!(sim.test_cell_at(above.0, above.1, above.2), Some(slab0));
	}

	/// A pre-grown 2/3 request is BadBlock (one item can't buy two
	/// thirds), and a cross-material slab on a slab top does NOT grow.
	#[test]
	fn slab_grow_gates() {
		let mut sim = flat_sim("slabgates");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodSlab, 1);
		sim.test_grant(1, BlockType::StoneSlab, 1);

		place_cell(&mut sim, &mut c, 1, make_cell(BlockType::WoodSlab, 1));
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::BadBlock,
				}
			)),
			"pre-grown state refused: {:?}",
			c.msgs
		);

		let cell = place_cell(&mut sim, &mut c, 2, BlockType::WoodSlab as u16);
		// Stone slab aimed at the wood slab's top: fresh 1/3 above, no
		// grow. Declare the slot the stone slab actually landed in.
		let stone_slot = sim.clients[&1]
			.hotbar
			.slots()
			.iter()
			.position(|s| {
				*s == Some(digblock_core::hotbar::HotbarItem::Block(
					BlockType::StoneSlab,
				))
			})
			.expect("stone slab on the bar") as u8;
		let above = (cell.0, cell.1 + 1, cell.2);
		place_aimed(
			&mut sim,
			&mut c,
			3,
			BlockType::StoneSlab as u16,
			top_point(cell, 4),
			stone_slot,
		);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(BlockType::WoodSlab as u16),
			"wood slab untouched"
		);
		assert_eq!(
			sim.test_cell_at(above.0, above.1, above.2),
			Some(BlockType::StoneSlab as u16),
			"stone slab placed fresh above"
		);
	}

	/// Digging shrinks one third per completed hold (2/3 → 1/3 → Air),
	/// crediting one slab item each time.
	#[test]
	fn slab_dig_shrinks_by_thirds() {
		let mut sim = flat_sim("slabshrink");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodSlab, 2);
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();

		let cell = place_cell(&mut sim, &mut c, 1, BlockType::WoodSlab as u16);
		place_cell(&mut sim, &mut c, 2, BlockType::WoodSlab as u16);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(make_cell(BlockType::WoodSlab, 1))
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 0);

		// WoodSlab hardness is 20 ticks per third.
		let mut t = 0;
		let dug = dig_hold(&mut sim, 1, down, 25, 3, EditKind::Dig, &mut t);
		assert_eq!(dug, cell);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(BlockType::WoodSlab as u16),
			"2/3 shrank to 1/3, not Air"
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 1);

		let dug = dig_hold(&mut sim, 1, down, 25, 4, EditKind::Dig, &mut t);
		assert_eq!(dug, cell);
		assert_eq!(
			sim.test_block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Air),
			"1/3 dug away"
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 2);
	}

	/// A grow landing mid-hold changes the cell's content, which resets
	/// the hold (verify_digs compares FULL cells) — a banked 1/3-priced
	/// timer can't complete against the taller fill.
	#[test]
	fn slab_grow_mid_hold_resets_dig_timer() {
		let mut sim = flat_sim("slabmidhold");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodSlab, 2);
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();

		let cell = place_cell(&mut sim, &mut c, 1, BlockType::WoodSlab as u16);
		// Bank a complete hold on the 1/3 slab, but DON'T send the edit yet.
		let pos = sim.test_player(1).unwrap().pos;
		let mut t = 1;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [cell.0, cell.1, cell.2],
				},
			},
			batch(vec![aim(t, down)], 1),
		]);
		for _ in 0..25 {
			t += 1;
			sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
		}
		// Grow the cell under the hold, then try to complete the dig.
		place_cell(&mut sim, &mut c, 2, BlockType::WoodSlab as u16);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 3,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [cell.0, cell.1, cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 3,
					reason: RejectReason::DigIncomplete,
				}
			)),
			"banked hold reset by the grow: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(make_cell(BlockType::WoodSlab, 1)),
			"cell still the grown 2/3"
		);
	}

	#[test]
	fn door_place_writes_both_halves_one_debit() {
		let mut sim = flat_sim("doorplace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodDoor, 1);
		let lower_cell = make_cell(BlockType::WoodDoor, door_state(Facing::PosZ, false, false));
		let target = place_cell(&mut sim, &mut c, 1, lower_cell);
		let upper = (target.0, target.1 + 1, target.2);
		assert_eq!(
			sim.test_cell_at(target.0, target.1, target.2),
			Some(lower_cell)
		);
		assert_eq!(
			sim.test_cell_at(upper.0, upper.1, upper.2),
			Some(make_cell(
				BlockType::WoodDoor,
				door_state(Facing::PosZ, false, true)
			)),
			"server derived the upper half"
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::WoodDoor),
			0,
			"one debit"
		);
		// Two broadcasts: the lower carries our seq, the upper seq 0.
		let applied: Vec<_> = c
			.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::EditApplied {
					client_seq, edit, ..
				} => Some((*client_seq, (edit.x, edit.y, edit.z))),
				_ => None,
			})
			.collect();
		assert!(applied.contains(&(1, target)), "{applied:?}");
		assert!(applied.contains(&(0, upper)), "{applied:?}");
	}

	#[test]
	fn door_toggle_flips_both_halves_and_dig_clears_both() {
		let mut sim = flat_sim("doortoggle");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::WoodDoor, 1);
		let lower_cell = make_cell(BlockType::WoodDoor, door_state(Facing::PosZ, false, false));
		let target = place_cell(&mut sim, &mut c, 1, lower_cell);
		let upper = (target.0, target.1 + 1, target.2);

		// Toggle open: aim at the lower half.
		let pos = sim.test_player(1).unwrap().pos;
		let dir = (Vec3::new(
			target.0 as f32 + 0.5,
			target.1 as f32 + 0.5,
			target.2 as f32 + 0.05,
		) - pos)
			.normalize();
		let hit = digblock_core::raycast::raycast_interact(pos, dir, REACH, &|x, y, z| {
			sim.test_cell_at(x, y, z)
		})
		.expect("door under the interact ray");
		assert_eq!(hit.cell, target);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 2,
				tick: 0,
				origin: pos.to_array(),
				dir: dir.to_array(),
				kind: EditKind::Interact,
				cell: [target.0, target.1, target.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		c.drain();
		let lower_now = sim.test_cell_at(target.0, target.1, target.2).unwrap();
		let upper_now = sim.test_cell_at(upper.0, upper.1, upper.2).unwrap();
		assert!(cell_state(lower_now) & 0b0100 != 0, "lower open");
		assert!(cell_state(upper_now) & 0b0100 != 0, "upper open");

		// An OPEN door is transparent to the dig ray, so close it first —
		// clicking the SWUNG leaf, which now hugs a different cell face —
		// then hold-dig the lower half: both cells clear, one credit.
		let leaf_box = digblock_core::shapes::interaction_boxes(lower_now)[0];
		let (leaf_lo, leaf_hi) = leaf_box.aabb(target);
		let dir2 = ((leaf_lo + leaf_hi) * 0.5 - pos).normalize();
		let hit2 = digblock_core::raycast::raycast_interact(pos, dir2, REACH, &|x, y, z| {
			sim.test_cell_at(x, y, z)
		})
		.expect("open leaf under the interact ray");
		assert_eq!(hit2.cell, target, "swung leaf still clickable");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 3,
				tick: 0,
				origin: pos.to_array(),
				dir: dir2.to_array(),
				kind: EditKind::Interact,
				cell: [target.0, target.1, target.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			cell_state(sim.test_cell_at(target.0, target.1, target.2).unwrap()) & 0b0100 == 0,
			"closed again"
		);
		let mut t = 100;
		let hold = BlockType::WoodDoor.hardness_ticks().unwrap() as u32 + 6;
		let dug = dig_hold(&mut sim, 1, dir, hold, 4, EditKind::Dig, &mut t);
		assert_eq!(dug, target);
		c.drain();
		assert_eq!(sim.test_cell_at(target.0, target.1, target.2), Some(0));
		assert_eq!(
			sim.test_cell_at(upper.0, upper.1, upper.2),
			Some(0),
			"partner cleared too"
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::WoodDoor),
			1,
			"one credit for the pair"
		);
	}

	#[test]
	fn alchemy_convert_craft_and_same_tick_place() {
		use digblock_net::messages::AlchemyOp;
		let mut sim = flat_sim("alchemy");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::Stone, 1);

		// Convert + craft + place, all queued in ONE tick: wire order
		// must debit before the place's stock gate runs.
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		let stair = make_cell(BlockType::StoneStairs, stair_state(Facing::PosX));
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Alchemy {
					op: AlchemyOp::Convert {
						block: BlockType::Stone as u16,
						count: 1,
					},
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Alchemy {
					op: AlchemyOp::Craft {
						item: BlockType::StoneStairs as u16,
						count: 1,
					},
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq: 1,
					tick: 0,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Place { block: stair },
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
					// Converting spent every Stone, so its slot was
					// released in the same reconcile that filed the
					// crafted stairs — they land in slot 0. The same
					// derivation an honest client runs.
					slot: 0,
				},
			},
		]);
		c.drain();
		let target = (
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		);
		assert_eq!(
			sim.test_cell_at(target.0, target.1, target.2),
			Some(stair),
			"craft-then-place landed in one tick: {:?}",
			c.msgs
		);
		// 27 sub-stone minted, 18 spent.
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts }
					if counts.contains(&(0x8001, 9))
			)),
			"sub-stone remainder in the rebase: {:?}",
			c.msgs
		);

		// Invalid ops still answer with the authoritative rebase.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Alchemy {
				op: AlchemyOp::Craft {
					item: BlockType::StoneStairs as u16,
					count: 999,
				},
			},
		}]);
		let before = sim.test_inventory_count(1, BlockType::StoneStairs);
		c.drain();
		assert_eq!(sim.test_inventory_count(1, BlockType::StoneStairs), before);
		assert!(
			c.msgs
				.iter()
				.rev()
				.any(|m| matches!(m, ServerMsg::InventoryFull { .. })),
			"failed op still rebases"
		);
	}

	/// Slot enforcement: a place must use the item in its declared slot.
	/// Declaring another slot rejects with WrongSlot and mutates nothing;
	/// the honest slot lands. A Layout update in the same tick applies in
	/// wire order, so assign-then-place works.
	#[test]
	fn wrong_slot_place_rejected_and_layout_applies_in_order() {
		let mut sim = flat_sim("wrongslot");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		// Dirt → bar slot 0, Stone → bar slot 1 (grant reconciles).
		sim.test_grant(1, BlockType::Dirt, 5);
		sim.test_grant(1, BlockType::Stone, 5);
		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		let place = |seq: u32, block: BlockType, slot: u8| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place {
					block: block as u16,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot,
			},
		};
		let target = (
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		);

		// Dirt declared from Stone's slot: rejected, nothing changes.
		sim.tick_once(vec![place(1, BlockType::Dirt, 1)]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::WrongSlot
				}
			)),
			"{:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_block_at(target.0, target.1, target.2),
			Some(BlockType::Air),
			"rejected place never lands"
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::Dirt), 5, "no debit");
		// An out-of-range slot is a wrong slot too.
		sim.tick_once(vec![place(2, BlockType::Dirt, 200)]);
		c.drain();
		assert!(c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 2,
				reason: RejectReason::WrongSlot
			}
		)));

		// Same tick: move Dirt to slot 4 via Layout, then place from 4 —
		// wire order means the arrangement lands first.
		use digblock_core::hotbar::{WIRE_EMPTY, WIRE_TOOL};
		let mut hotbar = vec![WIRE_EMPTY; digblock_core::hotbar::HOTBAR_SLOTS];
		hotbar[4] = BlockType::Dirt as u16;
		hotbar[1] = BlockType::Stone as u16;
		hotbar[8] = WIRE_TOOL;
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Layout {
					hotbar,
					grid: vec![WIRE_EMPTY; digblock_core::inv_grid::GRID_SLOTS],
				},
			},
			place(3, BlockType::Dirt, 4),
		]);
		c.drain();
		assert_eq!(
			sim.test_block_at(target.0, target.1, target.2),
			Some(BlockType::Dirt),
			"assign-then-place in one tick lands: {:?}",
			c.msgs
		);
		assert_eq!(sim.test_inventory_count(1, BlockType::Dirt), 4);
	}

	/// The held item rides snapshots: the selected slot's item (hotbar
	/// wire encoding) appears on the player's snapshot entry and tracks
	/// slot switches.
	#[test]
	fn held_item_rides_snapshots() {
		use digblock_core::hotbar::{HOTBAR_SLOTS, WIRE_TOOL};
		let mut sim = flat_sim("held");
		let mut c1 = connect(&mut sim, 1);
		let mut c2 = connect(&mut sim, 2);
		wait_live(&mut sim, 1);
		wait_live(&mut sim, 2);
		sim.test_grant(1, BlockType::Dirt, 1); // → bar slot 0

		// Player 1 selects slot 0 (Dirt).
		let mut input0 = aim(1, Vec3::NEG_Y);
		input0.active_slot = 0;
		sim.tick_once(vec![batch(vec![input0], 1)]);
		for _ in 0..6 {
			sim.tick_once(vec![]);
		}
		c2.drain();
		let held_of = |snaps: &[digblock_net::messages::Snapshot], id: u8| {
			snaps
				.iter()
				.rev()
				.find_map(|s| s.players.iter().find(|p| p.id == id).map(|p| p.held))
		};
		assert_eq!(
			held_of(&c2.snaps, 0),
			Some(BlockType::Dirt as u16),
			"player 1 shows the dirt in hand"
		);

		// Switch to the tool slot: the broadcast follows.
		let mut input1 = aim(2, Vec3::NEG_Y);
		input1.active_slot = HOTBAR_SLOTS as u8 - 1;
		sim.tick_once(vec![batch(vec![input1], 1)]);
		for _ in 0..6 {
			sim.tick_once(vec![]);
		}
		c2.snaps.clear();
		c2.drain();
		assert_eq!(
			held_of(&c2.snaps, 0),
			Some(WIRE_TOOL),
			"slot switch reaches remotes"
		);
		let _ = &mut c1;
	}

	/// A locked spawn refuses digs inside its region and the cell is
	/// never modified. The refusal itself arrives at DigStart (see
	/// `a_protected_dig_is_refused_at_dig_start`); a client that skips
	/// DigStart and sends the completing edit straight off has no hold on
	/// record, so it fails the timing gate instead — refused either way.
	#[test]
	fn locked_spawn_refuses_a_dig() {
		let mut sim = flat_sim("lockdig");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.spawn_store.set(Some(crate::spawn_store::SpawnLock {
			eye: FLAT_SPAWN.to_array(),
		}));
		let down = Vec3::new(0.0, -1.0, 0.0);
		let mut t = 0;
		let cell = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::DigRefused {
					reason: RejectReason::Protected,
					..
				}
			)),
			"protected dig should be refused: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(BlockType::Stone as u16),
			"the protected cell was modified anyway"
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Stone),
			0,
			"no credit"
		);
	}

	/// Placing into a locked region is refused too, and reports
	/// `Protected` rather than some incidental reason — the client picks
	/// its wording ("can't build" vs "won't break") from this.
	#[test]
	fn locked_spawn_refuses_a_place() {
		let mut sim = flat_sim("lockplace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.test_grant(1, BlockType::Stone, 4);
		// Lock a region offset from the player so the target is inside it
		// but the player is not standing in the way of their own place.
		let eye = FLAT_SPAWN + Vec3::new(4.0, 0.0, 0.0);
		sim.spawn_store.set(Some(crate::spawn_store::SpawnLock {
			eye: eye.to_array(),
		}));
		let region = sim.spawn_lock().expect("locked");

		let pos = sim.test_player(1).unwrap().pos;
		let out = Vec3::new(3.0, -2.0, 0.0).normalize();
		let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground");
		let target = (
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		);
		assert!(
			region.contains(target.0, target.1, target.2),
			"test aims outside the locked region: {target:?}"
		);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place {
					block: BlockType::Stone as u16,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot: 0,
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::Protected,
					..
				}
			)),
			"protected place should be refused as Protected: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Stone),
			4,
			"a refused place must not debit stock"
		);
	}

	/// Unlocking gives the cells back — the lock is a toggle, not a
	/// permanent property of the terrain.
	#[test]
	fn unlocking_restores_digging() {
		let mut sim = flat_sim("lockundig");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.spawn_store.set(Some(crate::spawn_store::SpawnLock {
			eye: FLAT_SPAWN.to_array(),
		}));
		sim.spawn_store.set(None);
		let down = Vec3::new(0.0, -1.0, 0.0);
		let mut t = 0;
		let cell = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		c.drain();
		assert!(
			!c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::Protected,
					..
				}
			)),
			"dig still refused after unlocking: {:?}",
			c.msgs
		);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(BlockType::Air as u16),
			"the dig did not land after unlocking"
		);
	}

	/// Water placed OUTSIDE a locked region must not flow into it. This is
	/// the case the write-refusal covers that a schedule filter would not:
	/// the source is legal, only its destination is protected.
	#[test]
	fn water_cannot_flow_into_a_locked_region() {
		let mut sim = flat_sim("lockflow");
		// Held, not dropped: dropping the client closes its channel, the
		// server drops the player, and residency evicts the whole world.
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let spawn = FLAT_SPAWN;
		let sz = spawn.z.floor() as i32;
		let h = spawn.y.floor() as i32 - 2;
		sim.spawn_store.set(Some(crate::spawn_store::SpawnLock {
			eye: spawn.to_array(),
		}));
		let region = sim.spawn_lock().expect("locked");

		// A source one column outside the region, level with the capsule,
		// with a clear run straight at it.
		let (wx, wy, wz) = (region.x1 + 1, h + 1, sz);
		assert!(!region.contains(wx, wy, wz), "source must start outside");
		assert!(sim.store.set_block(wx, wy, wz, BlockType::Water).is_some());
		sim.fluid.schedule_around(wx, wy, wz);
		// Plenty of flow steps for water to spread its full 3-block reach.
		for _ in 0..240 {
			sim.tick_once(vec![]);
		}

		for (x, y, z) in region.cells() {
			let cell = sim.test_cell_at(x, y, z);
			assert!(
				!cell.is_some_and(
					|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_water()
				),
				"water reached protected cell ({x},{y},{z})"
			);
		}
		// And the source itself did flow somewhere — otherwise this test
		// would pass on a fluid sim that simply never ran.
		assert!(
			sim.test_cell_at(region.x1 + 2, h + 1, sz)
				.is_some_and(|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_water()),
			"fluid never spread at all; the test proves nothing"
		);
	}

	/// The refusal lands when the hold STARTS, not when it completes.
	/// Before this, a player held the button through a stone block's full
	/// dig time and only then learned it was never going to break.
	#[test]
	fn a_protected_dig_is_refused_at_dig_start() {
		let mut sim = flat_sim("lockdigstart");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.spawn_store.set(Some(crate::spawn_store::SpawnLock {
			eye: FLAT_SPAWN.to_array(),
		}));
		// The block underfoot: inside the locked region and in reach.
		let cell = (
			FLAT_SPAWN.x.floor() as i32,
			FLAT_SPAWN.y.floor() as i32 - 2,
			FLAT_SPAWN.z.floor() as i32,
		);
		assert!(sim
			.spawn_lock()
			.expect("locked")
			.contains(cell.0, cell.1, cell.2));

		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [cell.0, cell.1, cell.2],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::DigRefused {
					reason: RejectReason::Protected,
					..
				}
			)),
			"DigStart on a protected cell should be refused immediately: {:?}",
			c.msgs
		);
		// And no hold was recorded, so the timer never runs.
		assert!(
			sim.test_digging(1).is_none(),
			"a refused DigStart still opened a hold"
		);
	}

	/// An ordinary undiggable target stays silent — the client's own
	/// `diggable` check already agrees, so answering would be chatter on
	/// every mistimed swing.
	#[test]
	fn an_ordinary_dig_start_is_not_answered() {
		let mut sim = flat_sim("digstartquiet");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		// Air well above the ground: undiggable, but not protected.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [8, FLAT_GROUND_TOP as i32 + 40, 8],
			},
		}]);
		c.drain();
		assert!(
			!c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::DigRefused { .. })),
			"undiggable-but-unprotected should stay silent: {:?}",
			c.msgs
		);
	}
}
