//! Edit validation (re-raycast from the claimed origin) and the fluid
//! step, which turns settled flow into ordinary journaled edits.

use super::{Sim, client_send, in_radius, send_frame};
use crate::store::StoreWorld;
use digblock_core::block::BlockType;
use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
use digblock_core::player::TICK;
use digblock_core::protect::Region;
use digblock_core::raycast::{REACH, raycast, raycast_fluid};
use digblock_net::messages::{BlockEdit, EditKind, RejectReason, ServerMsg};
use digblock_net::protocol::{
	CHAT_BURST, CHAT_RATE_PER_SEC, DIG_OFF_TARGET_RESET, EDIT_BURST, EDIT_POS_TOLERANCE,
	EDIT_RATE_PER_SEC, NO_CLIENT_SEQ, PlayerId, SERVER_ID, TICK_RATE, dig_grace,
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

/// One queued client action, processed in WIRE ORDER within the tick. DigStart/DigCancel must ride
/// the same queue as edit requests: back-to-back digs put "complete block A" and "start block B"
/// in the same tick drain, and applying the start out of order would clobber the hold A's
/// completion is about to be validated against (an honest dig would bounce with DigIncomplete).
pub(super) enum ClientAction {
	Edit(PendingEditReq),
	DigStart {
		conn_id: u64,
		cell: (i32, i32, i32),
		slot: u8,
	},
	DigCancel {
		conn_id: u64,
	},
	/// Alchemy rides the same queue: a craft-then-place must debit before the place's stock gate.
	Alchemy {
		conn_id: u64,
		op: digblock_net::messages::AlchemyOp,
	},
	/// Layout rides the same queue: assign-to-bar-then-place lands before the place's slot gate.
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
	/// a pause, not progress and not a reset. This is the banked PEAK:
	/// the decay below is derived from it rather than written back into
	/// it, so both ends compute one number from the same two counts.
	pub(super) progress: u32,
	/// Consecutive ticks the server's own aim ray missed the cell.
	pub(super) off_target: u32,
	/// Ticks since the hold stopped advancing — zero while it is live.
	/// A released hold (or one that lost aim for good) bleeds down
	/// through `skills::dig_decayed` instead of vanishing, and the
	/// client runs the identical decay over the identical interval.
	pub(super) idle: u32,
}

impl DigState {
	/// Banked progress after the decay so far — what the completion gate
	/// charges and what a resumed hold inherits.
	pub(super) fn banked(&self, required: u32) -> u32 {
		digblock_core::skills::dig_decayed(self.progress, required, self.idle)
	}
}

impl Sim {
	/// Aim-tracking for in-progress holds: every tick, re-raycast each digging client from the
	/// server's OWN simulated position and look. An on-target tick advances `progress`; a missed
	/// one merely doesn't, the server's view of the aim lagging the client's. Only a sustained miss
	/// — [`DIG_OFF_TARGET_RESET`] consecutive ticks — or the cell's content changing abandons the
	/// hold. This is the anti-cheat: the only client input is the cell. Do NOT optimize it away.
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
			// A released hold has no aim to verify and banks nothing — it
			// only bleeds. Skipping the ray here is also what keeps this
			// loop's cost tied to who is actually digging.
			if state.idle > 0 {
				state.idle = (state.idle + 1).min(digblock_core::skills::DIG_DECAY_TICKS);
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
			// Reach slack: this ray starts at the server's eye, which may trail
			// the client's by up to the claimed-origin tolerance — a target the
			// client legitimately picked at max reach would otherwise miss every
			// verify tick and never bank progress. The completing edit still
			// enforces exact REACH from its (tolerance-checked) claimed origin.
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
					// Sustained loss of aim ends the hold but no longer
					// discards it: the client gave this cell up when it
					// lost aim and has been decaying since. Entering the
					// decay here instead lands the two ends at most one
					// `DIG_OFF_TARGET_RESET` apart, and errs in the
					// server's favour — it decays LATER, so an honest
					// completion is never short.
					state.idle = 1;
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
			// The road's own bucket — see `CAUSEWAY_RATE_PER_SEC` for why
			// it cannot share the edit one.
			client.causeway_tokens = (client.causeway_tokens
				+ digblock_net::protocol::CAUSEWAY_RATE_PER_SEC as f32 / TICK_RATE as f32)
				.min(digblock_net::protocol::CAUSEWAY_BURST);
			// The spellbook's bucket. Refilled here with the others; spent
			// in `handle_cast`, which is where a cast's real cost — the
			// Ward's sphere derivation — is about to be paid.
			client.cast_tokens = (client.cast_tokens
				+ super::spawn::CAST_RATE_PER_SEC as f32 / TICK_RATE as f32)
				.min(super::spawn::CAST_BURST);
			// The bench bucket banks TICKS — one per tick — and an op
			// spends its own pace out of it, so the ceiling tracks the
			// honest client's drip whatever it is working on. Capacity is
			// a short burst of whole blocks, or one costliest op where
			// that is larger — see `bench_burst_ticks`.
			client.bench_tokens =
				(client.bench_tokens + 1.0).min(digblock_core::alchemy::bench_burst_ticks(
					client.skills.level(digblock_core::skills::Skill::Alchemy),
				));
		}
		let queue = std::mem::take(&mut self.action_queue);
		for action in queue {
			match action {
				ClientAction::Edit(req) if req.kind == EditKind::Interact => {
					self.process_interact(req)
				}
				ClientAction::Edit(req) => self.process_one_edit(req),
				ClientAction::DigStart {
					conn_id,
					cell,
					slot,
				} => self.handle_dig_start(conn_id, cell, slot),
				ClientAction::DigCancel { conn_id } => {
					// The button came up. Stop banking, but start the same
					// decay the client started when it sent this — a hold
					// resumed inside the window is worth what is left of it.
					if let Some(client) = self.clients.get_mut(&conn_id)
						&& let Some(state) = &mut client.digging
					{
						state.idle = state.idle.max(1);
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

	/// May a placement write this cell? It must be RESIDENT (an absent
	/// column answers `None`, and building into ungenerated world would let
	/// the write be overwritten the moment the column loads) and hold a
	/// block `is_replaceable` accepts. Shared authority with the client's
	/// prediction, which asks the same core predicate.
	fn cell_is_replaceable(&self, cell: (i32, i32, i32)) -> bool {
		self.store
			.cell_at(cell.0, cell.1, cell.2)
			.is_some_and(|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_replaceable())
	}

	/// Adopt a client's item arrangement: sanitizing decode (hostile input
	/// yields a valid bar/grid), reconcile against the authoritative
	/// counts, write through to the store. The client owns arrangement;
	/// the server owns what it can afford to use.
	fn process_layout(&mut self, conn_id: u64, hotbar: &[u16], grid: &[u16]) {
		use digblock_core::hotbar::{HOTBAR_SLOTS, Hotbar, WIRE_EMPTY};
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		let mut bar_wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		for (d, s) in bar_wire.iter_mut().zip(hotbar.iter()) {
			*d = *s;
		}
		// The wire carries the arrangement, never the selection — that
		// rides `InputTick.active_slot` and the slot every edit and every
		// hold declares — so a rearrangement must not silently move the
		// hand back to slot one, which is what a bare `from_wire` does.
		let selected = client.hotbar.selected_index();
		client.hotbar = Hotbar::from_wire(&bar_wire);
		client.hotbar.set_selected(selected);
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
		use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem};
		// Copy what we need out of the client entry so the &mut borrow
		// ends before the all-players InsidePlayer scan below.
		let (id, out, live, player_pos, has_token, dig_state, slot_item, active_tool) = {
			let Some(client) = self.clients.get_mut(&req.conn_id) else {
				return;
			};
			// A causeway plank spends its OWN bucket (below), never this
			// one: sharing would let a running player's road starve their
			// digging, and a starved plank is a fall.
			let plank = req.kind == EditKind::CausewayStep;
			let has_token = plank || client.edit_tokens >= 1.0;
			if client.live && has_token && !plank {
				client.edit_tokens -= 1.0;
			}
			// The declared slot's item (None for an out-of-range slot). An edit is
			// also evidence of selection — mirror it so the held-item broadcast
			// tracks a click that outran the jitter-buffered input path, and so
			// `tool_in_hand` below answers for the hand this edit was made with.
			let slot_item = ((req.slot as usize) < HOTBAR_SLOTS).then(|| {
				client.hotbar.set_selected(req.slot as usize);
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
					.map(|d| (d.cell, d.cell_value, d.progress, d.idle)),
				slot_item,
				client.hotbar.tool_in_hand(),
			)
		};
		// Is this dig a field EXTRACTION (SKILLS-DESIGN §5.3)? Resolved
		// here, from the server's own world and the client's own level and
		// tool — the same `ore::dig_mode` the client predicted with, so
		// neither end can decide alone what an action was.
		let extracting = match self.ore_dig_mode(req.conn_id, req.cell) {
			digblock_core::worldgen::nodes::DigMode::Extract(ore) => Some(ore),
			_ => None,
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

		// A causeway plank takes none of what follows — no ray, no reach,
		// no slot, no stock. Its bound is the server's own copy of the
		// caster's feet, and the trail it joins retires its own tail.
		if req.kind == EditKind::CausewayStep {
			if let Err(reason) = self.causeway_step(req.conn_id, req.cell, req.seq) {
				reject(reason);
			}
			return;
		}

		let arc = matches!(req.kind, EditKind::PlaceArc { .. });
		let place_cell = match req.kind {
			EditKind::Dig | EditKind::Collect => None,
			EditKind::Interact => unreachable!("routed to process_interact"),
			EditKind::CausewayStep => unreachable!("returned above"),
			EditKind::Place { block } | EditKind::PlaceArc { block } => {
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
				// Slab requests must be the canonical floor third with no
				// state bits set: the server derives grown values itself,
				// so a client can never conjure a taller fill (more
				// material) from one item, and it derives the ceiling
				// anchor from the normal it verified rather than taking
				// the client's word.
				if b.is_slab() && digblock_core::chunk::cell_state(block) != 0 {
					return reject(RejectReason::BadBlock);
				}
				// The arc multiplies a PLAIN placement only: slabs grow,
				// doors pair and water pours — each its own machinery the
				// line must not multiply.
				if arc && (b.is_slab() || b.is_door() || b.is_fluid_source()) {
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
		let residual = req.origin.distance(player_pos);
		self.note_origin_residual(req.conn_id, residual, residual > EDIT_POS_TOLERANCE);
		if residual > EDIT_POS_TOLERANCE {
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
			// How many verified ticks the completion was missing, for the
			// instrumentation: it prices how much of `dig_grace` an honest hold on
			// this link still had left over. Only meaningful when the hold is on the
			// right cell — a mismatch is a different failure, and reporting it as a
			// shortfall would read as the grace being too small.
			let mut short = 0u32;
			let mining_level = self
				.clients
				.get(&req.conn_id)
				.map_or(1, |c| c.skills.level(digblock_core::skills::Skill::Mining));
			let satisfied = dig_state.is_some_and(|(cell, cell_value, progress, idle)| {
				// An extraction is timed as the ORE, which means BOTH halves come
				// from the ore: the block (for its `DigFamily`) and the hardness.
				// Taking the family from the ore and the hardness from the cell left
				// the server asking for Stone's hold while the client's bar ran on
				// Aetherock's — several times apart, far outside `dig_grace`.
				let timed = digblock_core::worldgen::nodes::dig_block(
					self.ore_dig_mode(req.conn_id, req.cell),
					hit.block,
				);
				cell == req.cell
					&& cell_value == hit.cell_value
					&& timed.dig_hardness(req.cell.1).is_some_and(|h| {
						// Family-scaled hold time (SIM v12), plus the pick's
						// level-only multiplier (§5.6): the same
						// `dig_required_with` the client's own timer runs.
						let need = digblock_core::skills::dig_required_with(
							timed,
							h,
							mining_level,
							active_tool,
						);
						// A hold that has been bleeding since its release is
						// worth what the decay left it, never what it peaked
						// at — the client's bar shows the same number.
						let banked = digblock_core::skills::dig_decayed(progress, need, idle);
						let have = banked + dig_grace(need);
						short = need.saturating_sub(have);
						have >= need
					})
			});
			if satisfied || short > 0 {
				self.note_dig_gate(req.conn_id, short);
			}
			if !satisfied {
				return reject(RejectReason::DigIncomplete);
			}
		}

		// The full (cell, value) list this action writes: one for a plain
		// dig/place, two for a door (place derives the upper half; digging
		// either half clears both).
		let air = BlockType::Air as u16;
		let mut writes: Vec<((i32, i32, i32), u16)> = Vec::with_capacity(2);
		// How many BLOCKS this one action breaks: one, or the whole plane
		// with the Resonant Pick. A door's two cells are one block and
		// never increment this — the credit and the XP are per block, not
		// per cell, which is what `begin_multi`'s single `inv_delta` means.
		let mut broken = 1u32;
		let place_b = place_cell.map(|v| BlockType::from_id(digblock_core::chunk::cell_id(v)));
		match place_cell {
			None => {
				// Slabs shrink one third per completed dig; everything
				// else clears. Collect never stops in a slab (fluid ray).
				let new_val = if hit.block.is_slab() {
					digblock_core::shapes::slab_shrunk(hit.cell_value)
				} else if let Some(ore) = extracting {
					// The vein is DRAWN out, not dug out: the cell becomes
					// the tier's own tapped face, so mining ore never
					// excavates and the mountainside keeps its shape.
					// Writing a different id (rather than the same Stone
					// back) is also what makes the edit unambiguously real
					// to the journal, LWW replay and mesh invalidation.
					digblock_core::worldgen::nodes::spent_block(ore)
						.expect("an Extract names one of the four tiers") as u16
				} else {
					air
				};
				writes.push((req.cell, new_val));
				// The Resonant Pick's plane (SKILLS-DESIGN §5.6), derived
				// from the SERVER's raycast and its own world — never from
				// anything the client chose. Extras clear outright, and
				// `pick::breaks_as_extra` is what makes that safe: only the
				// aimed cell can be a slab shrink, a door pair or a tapped
				// vein, because nothing else is allowed into the group.
				for c in self.pick_plane(req.conn_id, &hit, req.dir.to_array()) {
					writes.push((c, air));
					broken += 1;
				}
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
				// A slab onto a same-material slab's OPEN face (up for a
				// floor slab, down for a ceiling one) grows the hit cell in
				// place — the server derives the grown value from its own
				// world, mirroring the client's prediction exactly.
				let grow = place_b.is_some_and(|b| b.is_slab())
					&& digblock_core::chunk::cell_id(hit.cell_value)
						== digblock_core::chunk::cell_id(v)
					&& hit.normal == digblock_core::shapes::slab_grow_normal(hit.cell_value);
				if grow {
					writes.push((req.cell, digblock_core::shapes::slab_grown(hit.cell_value)));
				} else {
					let target = (
						req.cell.0 + hit.normal.0,
						req.cell.1 + hit.normal.1,
						req.cell.2 + hit.normal.2,
					);
					// The target itself must be loaded and replaceable. This
					// gate is newer than the door's below and was its missing
					// twin: with only the derived upper half checked, a
					// placement could overwrite an OPEN door outright (the
					// plain ray passes through its empty collision boxes, so
					// aiming at the floor inside a doorway targets the door
					// cell) — no drop, no dig timer, orphaned upper half.
					if !self.cell_is_replaceable(target) {
						return reject(RejectReason::Occluded);
					}
					// A slab built against a ceiling hangs from it. Derived
					// here from the verified normal, exactly as the client
					// predicted it — the request itself always carries the
					// canonical floor slab.
					let placed = if place_b.is_some_and(|b| b.is_slab()) {
						v | (u16::from(digblock_core::shapes::slab_place_state(hit.normal))
							<< digblock_core::chunk::STATE_SHIFT)
					} else {
						v
					};
					writes.push((target, placed));
					if arc {
						// The Mason's Arc: the full line along the verified
						// normal, all or nothing — a partial line would
						// leave the two ends deriving different sets.
						let arcana = self
							.clients
							.get(&req.conn_id)
							.map_or(1, |c| c.skills.level(digblock_core::skills::Skill::Arcana));
						let n = digblock_core::unlock::arc_cells(arcana) as i32;
						for i in 2..=n {
							let t = (
								req.cell.0 + hit.normal.0 * i,
								req.cell.1 + hit.normal.1 * i,
								req.cell.2 + hit.normal.2 * i,
							);
							if !self.cell_is_replaceable(t) {
								return reject(RejectReason::Occluded);
							}
							writes.push((t, placed));
						}
					}
					if place_b.is_some_and(|b| b.is_door()) {
						// The derived upper half, same rule.
						let upper = (target.0, target.1 + 1, target.2);
						if !self.cell_is_replaceable(upper) {
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

		// Protected cells: the world floor, and the arrival regions. Checked
		// against the assembled `writes` so every path is covered at once — a
		// plain dig, a place landing on the neighbour cell, a door's derived upper
		// half, a slab grow. Honest clients predict this; an admin's are exempt
		// (`requester_regions`).
		let regions = self.requester_regions(req.conn_id);
		if writes
			.iter()
			.any(|&(c, _)| digblock_core::protect::immutable(c.0, c.1, c.2, &regions))
		{
			return reject(RejectReason::Protected);
		}

		// Slot enforcement: the edit must use the item actually in its declared
		// hotbar slot (the arrangement arrived in-order on the same stream, so
		// honest clients can never trip this). Digs are tool-free and exempt. Sits
		// with the stock gate, after every world-shaped rejection, so rejection
		// precedence is unchanged for pre-v10-shaped mistakes.
		let slot_ok = match req.kind {
			EditKind::Dig => true,
			EditKind::CausewayStep => unreachable!("returned above"),
			// The tool slot must hold the BUCKET — with §6's tool axis the
			// slot is one of a roster, and only the bucket scoops. The
			// Layout carrying a cycle precedes this edit on the reliable
			// stream, so honest clients can't race it.
			EditKind::Collect => {
				slot_item == Some(Some(HotbarItem::WaterTool)) && active_tool == BlockType::Bucket
			}
			EditKind::Place { .. } | EditKind::PlaceArc { .. } => match slot_item {
				Some(Some(HotbarItem::Block(b))) => place_b == Some(b),
				// Pouring the bucket. Water is the ONLY thing the tool places,
				// and the tool is the only thing that places water:
				// `reconcile_layout` refuses to leave water in a block slot, so
				// the arm above can no longer match it. The stock gate below
				// enforces the charge — an empty bucket's pour bounces as NoStock.
				Some(Some(HotbarItem::WaterTool)) => {
					place_b == Some(BlockType::Water) && active_tool == BlockType::Bucket
				}
				_ => false,
			},
			EditKind::Interact => unreachable!("routed to process_interact"),
		};
		if !slot_ok {
			reject(RejectReason::WrongSlot);
			// The same drift-repair backstop as NoStock: a count drift
			// files the item in different slots on the two ends once the
			// layouts reconcile apart, so without the rebase the mismatch
			// repeats on every retry and the drift never heals.
			if let Some(client) = self.clients.get(&req.conn_id) {
				send_frame(
					&out,
					&ServerMsg::InventoryFull {
						counts: client.inventory.to_wire(),
					},
				);
			}
			return;
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
		// One debit per BLOCK: a door's derived half is free, an arc's
		// line costs its length.
		let debit = if arc { writes.len() as u32 } else { 1 };
		if let Some(b) = place_b
			&& !client.inventory.try_take(b, debit)
		{
			let inv = client.inventory.to_wire();
			reject(RejectReason::NoStock);
			// Drift-repair backstop: the client predicted stock it
			// doesn't have — hand it the authoritative base.
			send_frame(&out, &ServerMsg::InventoryFull { counts: inv });
			return;
		}

		let (first_cell, first_val) = writes[0];
		// Whether this coordinate had ever been written, read BEFORE the
		// write for the reason the client reads it before its own: it is
		// the provenance half of `dig_credit`, and this dig is about to
		// become an edit itself. Journal-backed, so `apply_write` at the
		// bottom is what would flip it.
		let was_edited = self.cell_edited(first_cell.0, first_cell.1, first_cell.2);
		let Some(prev_cell) =
			self.store
				.set_cell(first_cell.0, first_cell.1, first_cell.2, first_val)
		else {
			// Refund the debit — the world never changed.
			if let Some(b) = place_b
				&& let Some(client) = self.clients.get_mut(&req.conn_id)
			{
				client.inventory.add(b, debit);
			}
			return reject(RejectReason::NotLoaded);
		};
		let prev = BlockType::from_id(digblock_core::chunk::cell_id(prev_cell));

		// Read off the store BEFORE the client borrow below, which holds
		// `self` for the rest of the block: a field draw's essence is
		// partly derived from the world (`ore::traces_aether`).
		let seed = self.store.seed();
		// Dig/collect credit comes from the journaled truth (the actual
		// replaced cell), not the raycast hit. One credit per door pair.
		let client = self
			.clients
			.get_mut(&req.conn_id)
			.expect("checked live above");
		// The levels crossed by this award, announced once the client
		// borrow above has ended (the broadcast walks every client). A
		// LIST: one action can level both skills at once.
		let mut crossed = Vec::new();
		if place_cell.is_none() {
			// An extraction credits ESSENCE, never a block: the siphon draws
			// the seam out where it stands, so no node is ever an item and the
			// cell's own drop is beside the point. What the player can carry
			// away is the spent rock now standing in the cell, on a later
			// ordinary dig.
			//
			// Un-predicted, deliberately: the essence is a SPLIT, and a
			// `PendingEdit` carries one `inv_delta`. Rather than widen the wire
			// for the one tier that pays in two, this takes the path
			// `ClientMsg::Alchemy` already takes. The CELL is still predicted,
			// so the vein turns spent under the player's hand with no latency.
			if let Some(ore) = extracting {
				let mining = client.skills.level(digblock_core::skills::Skill::Mining);
				let alchemy = client.skills.level(digblock_core::skills::Skill::Alchemy);
				for (e, n) in
					digblock_core::alchemy::field_yield(ore, alchemy, mining, req.cell, seed)
				{
					client.inventory.add_sub(e, n);
				}
				send_frame(
					&client.out,
					&ServerMsg::InventoryFull {
						counts: client.inventory.to_wire(),
					},
				);
			} else if let Some(drop) = prev.dig_credit(
				was_edited,
				client.skills.level(digblock_core::skills::Skill::Alchemy),
			) {
				// `dig_credit`, not the block's own natural drop: the
				// world's rock crumbles and a mason's wall comes back whole,
				// which is what keeps clean stone craftable without opening
				// a craft-and-extract loop. The client predicted this from
				// its own mirror of the same journal.
				client.inventory.add(drop, broken);
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
			// XP from the journaled truth (`prev`), the same authority as the
			// credit, and from the cell's own depth (§5.2). The frame carries the
			// new TOTAL, so a lost or reordered frame can never double-pay. A field
			// extraction pays BOTH skills (§5.4) from ONE call — the offline arm
			// makes the same one, so the two economies cannot drift.
			let (mining_xp, alch_xp) =
				digblock_core::skills::dig_award(prev, extracting, broken, req.cell.1);
			for (skill, xp) in [
				(digblock_core::skills::Skill::Mining, mining_xp),
				(digblock_core::skills::Skill::Alchemy, alch_xp),
			] {
				if xp == 0 {
					continue;
				}
				let before = client.skills.level(skill);
				let total = client.skills.add_xp(skill, xp);
				send_frame(
					&client.out,
					&ServerMsg::XpGain {
						skill: skill as u8,
						total,
					},
				);
				let after = client.skills.level(skill);
				if after > before {
					// PUSHED, not assigned. A field extraction pays both
					// skills in one action, so a player sitting under both
					// thresholds crosses both — and overwriting dropped the
					// Mining one on the floor: no LevelUp broadcast, no
					// confetti over their head for anyone else, no feed
					// line. The single-award version could not reach this.
					crossed.push((skill, after));
				}
			}
			client.digging = None;
		}
		if let Some(hash) = client.identity {
			self.player_store.put(hash, client.record());
		}
		for (skill, level) in crossed {
			self.announce_level_up(id, skill, level);
		}

		// Journal + broadcast the first write under the client's seq, the
		// companion (a door's other half) under `NO_CLIENT_SEQ` — the
		// in-order stream confirms the pair before the companion applies
		// idempotently over the client's identical prediction.
		self.apply_write(first_cell, first_val, prev_cell, id, req.seq, false);
		for &(cell, val) in &writes[1..] {
			if let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) {
				self.apply_write(cell, val, prev, id, NO_CLIENT_SEQ, false);
			}
		}

		// A sealing cover starves the grass beneath it back to dirt
		// (`grass::smothered_by`). Applied in the world-job sweep's style
		// — skip-on-protected, skip-on-nonresident, SERVER_ID — and only
		// AFTER the action has landed, never as part of it: a smother
		// must not be able to reject a place, and the client deliberately
		// does not predict it, so this broadcast is its one delivery.
		if place_cell.is_some() {
			let regions = self.protected_regions();
			for &(cell, val) in &writes {
				let below = (cell.0, cell.1 - 1, cell.2);
				if digblock_core::protect::immutable(below.0, below.1, below.2, &regions) {
					continue;
				}
				let Some(dirt) = self
					.store
					.cell_at(below.0, below.1, below.2)
					.and_then(|cur| digblock_core::worldjob::grass::smothered_by(val, cur))
				else {
					continue;
				};
				let Some(prev) = self.store.set_cell(below.0, below.1, below.2, dirt) else {
					continue;
				};
				self.apply_write(below, dirt, prev, SERVER_ID, NO_CLIENT_SEQ, true);
			}
		}
	}

	/// Journal, fluid-schedule, and broadcast one landed world write (the
	/// store mutation already happened). `already_set` is a lie-guard for
	/// the doc only — callers pass the actual prev they observed.
	pub(super) fn apply_write(
		&mut self,
		cell: (i32, i32, i32),
		val: u16,
		prev_cell: u16,
		by: PlayerId,
		client_seq: u32,
		_already_set: bool,
	) {
		let (seq, edit) = self.journal_write(cell, val, by);
		self.broadcast_edit(seq, by, client_seq, edit, prev_cell);
	}

	/// The durable half of [`Self::apply_write`], for cells every client DERIVES rather than being
	/// told: journal, enroll, schedule fluid — no `EditApplied`. The Ward owns this. Both ends draw
	/// the same sphere from the same head and radius (`ward::plan`), so a per-cell broadcast would
	/// be redundant, and a fan-out of hundreds of frames per cast against an outbound queue whose
	/// refusal DROPS the peer. The journal still takes every cell, which is what a joiner reads.
	pub(super) fn journal_write(
		&mut self,
		cell: (i32, i32, i32),
		val: u16,
		by: PlayerId,
	) -> (u64, BlockEdit) {
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
		(seq, edit)
	}

	/// A use/interact click: re-raycast and toggle the hit door pair.
	/// Same preamble gates as edits; `normal` is not required to match (a
	/// swung open leaf has no meaningful entry face).
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
		let residual = req.origin.distance(player_pos);
		self.note_origin_residual(req.conn_id, residual, residual > EDIT_POS_TOLERANCE);
		if residual > EDIT_POS_TOLERANCE {
			return reject(RejectReason::OutOfReach);
		}
		let store = &self.store;
		let hit = raycast(req.origin, dir, REACH, &|x, y, z| store.cell_at(x, y, z));
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
		if let Some(pc) = self.store.cell_at(partner.0, partner.1, partner.2)
			&& BlockType::from_id(cell_id(pc)).is_door()
		{
			writes.push((partner, pc, door_toggled(pc)));
		}
		// A door inside a protected region is furniture, not a mechanism
		// — except under an admin's hand.
		let regions = self.requester_regions(req.conn_id);
		if writes
			.iter()
			.any(|&(c, _, _)| digblock_core::protect::immutable(c.0, c.1, c.2, &regions))
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
				let seq = if first { req.seq } else { NO_CLIENT_SEQ };
				self.apply_write(cell, val, prev, id, seq, false);
			}
			first = false;
		}
	}

	/// Alchemy craft: validate against the core tables, apply the shared `apply_alchemy` rule,
	/// persist, and answer — success or failure — with the authoritative `InventoryFull` rebase.
	/// The wire's legacy `Convert` decodes and is REFUSED here: a frame kept decodable is how an
	/// append-only wire retires a verb. Every craft also draws from the per-player BENCH bucket,
	/// charged per item; the gate lives HERE, not in `apply_alchemy`, since pacing is anti-cheat.
	fn process_alchemy(&mut self, conn_id: u64, op: digblock_net::messages::AlchemyOp) {
		use digblock_core::inventory::{AlchemyOp as CoreOp, apply_alchemy};
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
			// The retired verb: refused, but still answered with the
			// rebase below, so an out-of-date client at least resyncs.
			digblock_net::messages::AlchemyOp::Convert { .. } => None,
			digblock_net::messages::AlchemyOp::Craft { item, count } => {
				BlockType::try_from_id(item).map(|b| CoreOp::Craft { item: b, count })
			}
		};
		let level = client.skills.level(digblock_core::skills::Skill::Alchemy);
		// The bench bucket, charged per ITEM BEFORE the economy runs — an op it
		// can't cover must not debit anything. The bucket is in ticks and a craft
		// costs its own pace, so neither a bulk `count` nor a stream of cheap ops
		// can sidestep the drip: a craft used to answer instantly, and the
		// transmute tab's CTRL multiplier paid a hundred crafts' XP in a frame.
		let core_op = core_op.filter(|op| {
			let CoreOp::Craft { item, count } = *op;
			// Saturating, because `count` is an unvalidated wire u32 and this
			// multiply runs BEFORE `apply_alchemy`'s own range check. Debug builds
			// have overflow checks on and `just server` builds debug, so a crafted
			// `count` near the type's ceiling panicked the sim thread. A saturated
			// cost is unaffordable, which is the right answer anyway.
			let cost =
				digblock_core::alchemy::craft_pace_ticks(item, level).saturating_mul(count) as f32;
			if client.bench_tokens < cost {
				return false;
			}
			client.bench_tokens -= cost;
			true
		});
		// As in the dig path: the crossing is announced after the client
		// borrow ends, since the broadcast walks every client.
		let mut crossed = None;
		if let Some(core_op) = core_op
			&& apply_alchemy(&mut client.inventory, core_op, level)
		{
			// Mirror the client's auto-assign of freshly crafted items
			// (it reconciles on the InventoryFull rebase below).
			digblock_core::inv_grid::reconcile_layout(
				&client.inventory,
				&mut client.hotbar,
				&mut client.grid,
			);
			// Alchemy XP for the validated op, as a new TOTAL. Sent
			// alongside the InventoryFull answer on the same stream.
			let xp = digblock_core::skills::alchemy_xp(&core_op);
			if xp > 0 {
				let total = client
					.skills
					.add_xp(digblock_core::skills::Skill::Alchemy, xp);
				send_frame(
					&client.out,
					&ServerMsg::XpGain {
						skill: digblock_core::skills::Skill::Alchemy as u8,
						total,
					},
				);
				let after = client.skills.level(digblock_core::skills::Skill::Alchemy);
				if after > level {
					crossed = Some(after);
				}
			}
		}
		let id = client.id;
		if let Some(hash) = client.identity {
			self.player_store.put(hash, client.record());
		}
		let counts = client.inventory.to_wire();
		send_frame(&client.out, &ServerMsg::InventoryFull { counts });
		if let Some(level) = crossed {
			self.announce_level_up(id, digblock_core::skills::Skill::Alchemy, level);
		}
	}

	/// A column just gained its FIRST edit: enroll every in-radius client
	/// BEFORE the broadcast, so the `EditApplied` itself carries the data
	/// and no JournalColumn resend is ever needed for it. This is what
	/// lets the steady-state residency sync skip journal scans entirely.
	pub(super) fn enroll_new_column(&mut self, edit: BlockEdit) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		for c in self.clients.values_mut() {
			if in_radius(col, c.residency.center, c.residency.draw_dist) {
				c.residency.synced_columns.insert(col);
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
		// Encoded once for the whole fan-out: fluid alone lands a whole
		// `FLOW_BUDGET` of edits every flow step, each going to every
		// client synced to the column.
		self.broadcast_where(&msg, |c| {
			c.residency.synced_columns.contains(&col) || c.id == by
		});
	}

	pub(super) fn step_fluid(&mut self) {
		// Unhealthy journal: stop mutating the world durably — fluid
		// writes are journaled edits like any other.
		if !self.journal.healthy() {
			return;
		}
		self.flow_accum = (self.flow_accum + TICK).min(1.0);
		let locks = self.protected_regions();
		while self.flow_accum >= FLOW_STEP_SECS {
			self.flow_accum -= FLOW_STEP_SECS;
			let mut edits = Vec::new();
			let mut world = StoreWorld {
				store: &mut self.store,
				edits: &mut edits,
				locks: locks.clone(),
			};
			self.fluid.step(&mut world, FLOW_BUDGET);
			for (edit, prev) in edits {
				let (seq, new_column) = self.journal.append(edit, SERVER_ID);
				if new_column {
					self.enroll_new_column(edit);
				}
				self.broadcast_edit(seq, SERVER_ID, NO_CLIENT_SEQ, edit, prev);
			}
		}
	}

	/// Record a hold-to-dig start (called from the ordered queue drain): capture the cell's CURRENT
	/// block — the client never gets to claim what it is digging. Within a tick the LAST start
	/// wins, like the client's retarget stream; first-wins silently dropped the restart of a
	/// cancel+restart pair bunched into one drain. Floods still can't spin the timer: a start on a
	/// new cell zeroes progress, and one on the same cell can only RE-READ what the decay left.
	fn handle_dig_start(&mut self, conn_id: u64, cell: (i32, i32, i32), slot: u8) {
		// Before the client borrow: `requester_regions` reads all of `self`.
		let regions = self.requester_regions(conn_id);
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		// The hand this hold is being made with, mirrored onto the bar
		// before any tool question is asked of it. It travels with the
		// start for the reason `EditRequest.slot` travels with an edit:
		// the selection otherwise arrives on the jitter-buffered input
		// path, and a player who picks up the siphon and holds on a vein
		// in the same breath would meet a `NeedsTool` refusal made
		// against the hand they had a moment ago.
		client.hotbar.set_selected(slot as usize);
		// Refuse a protected cell up front. The completing edit would
		// reject it anyway, but only after the player held the button
		// through the block's full dig time for nothing — the refusal is
		// only useful if it arrives when the hold starts.
		if digblock_core::protect::immutable(cell.0, cell.1, cell.2, &regions) {
			client.digging = None;
			client_send(
				client,
				&ServerMsg::DigRefused {
					cell: [cell.0, cell.1, cell.2],
					reason: RejectReason::Protected,
				},
			);
			return;
		}
		let cell_value = self.store.cell_at(cell.0, cell.1, cell.2);
		// The skill gates (§5.1's strength wall, v13's craft gate), refused
		// at the hold's start for the same reason protection is: the
		// refusal is useless after the player held the button through a
		// full dig timer. Honest clients predict it and never send this.
		if let Some(v) = cell_value {
			let b = digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v));
			// One question, the same one the client predicted with; the
			// wire reason is all this end adds.
			if let Some(req) = digblock_core::unlock::dig_blocked(b, &client.skills) {
				use digblock_core::unlock::Kind;
				client.digging = None;
				client_send(
					client,
					&ServerMsg::DigRefused {
						cell: [cell.0, cell.1, cell.2],
						reason: match req.kind {
							Kind::Strength => RejectReason::SkillGated,
							Kind::Recipe => RejectReason::NotUnlocked,
							Kind::Tool => RejectReason::NeedsTool,
						},
					},
				);
				return;
			}
		}
		// A revealed vein with the wrong tool refuses here too, for
		// exactly the reason the skill gates do — a refusal that arrives
		// after a two-second hold is not a refusal, it is a punishment.
		// Below the reveal this never fires: that player's world has no
		// vein in it, so the cell digs as the stone it appears to be.
		if self.ore_dig_mode(conn_id, cell) == digblock_core::worldgen::nodes::DigMode::NeedsSiphon
		{
			if let Some(client) = self.clients.get_mut(&conn_id) {
				client.digging = None;
				client_send(
					client,
					&ServerMsg::DigRefused {
						cell: [cell.0, cell.1, cell.2],
						reason: RejectReason::NeedsTool,
					},
				);
			}
			return;
		}
		// The resume's `required` has to be the SAME number the completion gate
		// will check against, or the decay clamps a resumed hold at one figure
		// while the client's bar bleeds from another. That means the ore, not the
		// stone the cell still holds. Resolved before the client borrow, since it
		// reads the store.
		let mode = self.ore_dig_mode(conn_id, cell);
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		// Re-opening a hold on the SAME unchanged cell resumes it at whatever the
		// decay has left, the number the client's own bar shows; everything else
		// starts at zero. `off_target` is carried across rather than cleared — it
		// is what still limits a start FLOOD: a client spamming DigStart to freeze
		// the decay while looking elsewhere trips `DIG_OFF_TARGET_RESET` anyway.
		let level = client.skills.level(digblock_core::skills::Skill::Mining);
		let tool = client.hotbar.tool_in_hand();
		let resumed = client
			.digging
			.as_ref()
			.filter(|d| d.cell == cell && Some(d.cell_value) == cell_value)
			.map(|d| {
				let b = digblock_core::worldgen::nodes::dig_block(
					mode,
					digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(
						d.cell_value,
					)),
				);
				let need = b.dig_hardness(cell.1).map_or(0, |h| {
					digblock_core::skills::dig_required_with(b, h, level, tool)
				});
				(d.banked(need), d.off_target)
			});
		client.digging = match cell_value {
			Some(v)
				if digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v))
					.dig_hardness(cell.1)
					.is_some() =>
			{
				let (progress, off_target) = resumed.unwrap_or((0, 0));
				Some(DigState {
					cell,
					cell_value: v,
					progress,
					off_target,
					idle: 0,
				})
			}
			// Undiggable/unloaded target: no hold. The completing edit
			// will fail its DigIncomplete check.
			_ => None,
		};
	}

	/// The regions THIS requester's action must respect: every arrival region for a player, none
	/// for an admin — the operator who moves spawn and stamps the sites must also be able to repair
	/// them. Only player-authored actions (edits, holds, door toggles, casts) ask this; the sim's
	/// own writers — fluid, the sweeps, the smother — always answer to [`Self::protected_regions`],
	/// or an admin merely being ONLINE would let water eat the spawn pad.
	pub(super) fn requester_regions(&self, conn_id: u64) -> Vec<Region> {
		let admin = self
			.clients
			.get(&conn_id)
			.is_some_and(|c| self.is_admin(c.account));
		if admin {
			Vec::new()
		} else {
			self.protected_regions()
		}
	}

	/// Every protected arrival region — the spawn lock's sphere and the
	/// resolved waystone pads. Owned on purpose: every caller hoists it
	/// before taking a mutable client borrow.
	pub(super) fn protected_regions(&self) -> Vec<Region> {
		self.spawn_store
			.lock()
			.map(|l| digblock_core::spawn::lock_region(Vec3::from_array(l.eye)))
			.into_iter()
			.chain(
				self.waystone_store
					.sites()
					.iter()
					.map(|&(_, eye)| digblock_core::spawn::lock_region(Vec3::from_array(eye))),
			)
			.collect()
	}
}

#[cfg(test)]
mod tests;
