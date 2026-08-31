//! Inventory, alchemy and XP. In multiplayer the server owns all
//! three and these apply its answer; the offline arms award through
//! the same core functions, never through the shared prediction path.

use super::*;

impl Engine {
	/// Apply one inventory-screen drop to the hotbar/grid pair, then
	/// reconcile so a moved type never shows in two places. The per-frame
	/// layout sync mirrors the result to the server before the next edit.
	pub(super) fn apply_inv_action(&mut self, act: InvAction) {
		use digblock_core::hotbar::HotbarItem;
		// The drop's destination, for the cue — identity rides the event.
		let dest = match &act {
			InvAction::Assign { slot, .. } | InvAction::ClearBar { slot } => {
				crate::ui::InvId::Bar(*slot as u8)
			}
			InvAction::SwapBar { b, .. } => crate::ui::InvId::Bar(*b as u8),
			InvAction::PlaceGrid { grid_slot, .. } => crate::ui::InvId::Grid(*grid_slot as u16),
			InvAction::MoveGrid { to, .. } => crate::ui::InvId::Grid(*to as u16),
		};
		self.react(crate::events::GameEvent::Ui(
			crate::events::UiEvent::SlotDrop(dest),
		));
		match act {
			InvAction::Assign { slot, block } => {
				let _ = self.hotbar.assign(slot, block);
			}
			InvAction::SwapBar { a, b } => self.hotbar.swap(a, b),
			InvAction::PlaceGrid {
				bar_slot,
				grid_slot,
			} => {
				if let Some(Some(HotbarItem::Block(b))) = self.hotbar.slots().get(bar_slot).copied()
				{
					let occupant = self.inv_grid.get(grid_slot);
					if self.hotbar.clear_slot(bar_slot) {
						self.inv_grid.place(grid_slot, b);
						// The displaced occupant takes the vacated bar slot.
						if let Some(x) = occupant {
							let _ = self.hotbar.assign(bar_slot, x);
						}
					}
				}
			}
			InvAction::MoveGrid { from, to } => self.inv_grid.swap(from, to),
			InvAction::ClearBar { slot } => {
				if let Some(Some(HotbarItem::Block(b))) = self.hotbar.slots().get(slot).copied()
					&& self.hotbar.clear_slot(slot)
				{
					self.inv_grid.file(b);
				}
			}
		}
		digblock_core::inv_grid::reconcile_layout(
			&self.inventory,
			&mut self.hotbar,
			&mut self.inv_grid,
		);
	}

	/// Apply an alchemy action: offline it IS the authority (the same
	/// `apply_alchemy` rule the server runs); online it goes to the
	/// server un-predicted — the `InventoryFull` echo is the rebase. The
	/// screen already clamped to affordable counts, so online sends are
	/// never no-ops.
	pub(super) fn apply_alchemy_action(&mut self, act: crate::ui::screens::alchemy::AlchAction) {
		use crate::ui::screens::alchemy::AlchAction;
		use digblock_core::inventory::{AlchemyOp, apply_alchemy};
		let AlchAction::Craft { item, count } = act;
		let core_op = AlchemyOp::Craft { item, count };
		let wire_op = digblock_net::messages::AlchemyOp::Craft {
			item: item as u16,
			count,
		};
		self.react(crate::events::GameEvent::Ui(crate::events::UiEvent::Craft));
		match self.session.client.as_mut() {
			Some(net) if net.ready() => net.send_alchemy(wire_op),
			_ => {
				if apply_alchemy(&mut self.inventory, core_op, &self.skills) {
					self.hotbar.auto_assign(item);
					self.award_xp_offline(
						digblock_core::skills::Skill::Alchemy,
						digblock_core::skills::alchemy_xp(&core_op),
					);
					// The craft that matters here is the siphon: making it is
					// what reveals the surface nodes, so the pipeline learns
					// about the pack's new tool the moment it exists. A no-op
					// for every other craft (no gate crossed, no re-mesh).
					// Under the walkthrough's craft step the remeshes are
					// DEFERRED: the cinematic lands each subject's pop-in
					// mid-shot instead of letting the heap race the camera.
					if self
						.tutorial
						.view(false)
						.is_some_and(|v| v.step == crate::tutorial::StepId::CraftSiphon)
					{
						self.defer_node_reveal();
					} else {
						self.sync_node_reveal(true);
					}
				}
			}
		}
	}

	/// Raise or refresh the toast for one label: a second award inside
	/// the window adds to the standing toast rather than stacking a
	/// twin, for XP and essence alike.
	pub(super) fn toast(&mut self, label: crate::ui::hud::ToastLabel, amount: u64) {
		if amount == 0 {
			return;
		}
		match self.vfx.toasts.iter_mut().find(|t| t.label == label) {
			Some(t) => {
				t.amount += amount;
				t.ttl = crate::ui::hud::TOAST_TTL;
			}
			None => self.vfx.toasts.push(crate::ui::hud::Toast {
				label,
				amount,
				ttl: crate::ui::hud::TOAST_TTL,
			}),
		}
	}

	/// Adopt the server's authoritative essence counts and toast every one that grew since the
	/// last base. Only a draw (or an operator's `/give`) can grow one — no craft creates essence —
	/// so a rise IS a find, and the client never has to be told the split it was not allowed to
	/// predict. The first base after a join is adopted silently: that is state arriving, not an
	/// award.
	pub(super) fn adopt_auth_essence(&mut self, base: &digblock_core::inventory::Inventory) {
		use digblock_core::block::Essence;
		let now: [u32; Essence::COUNT] = Essence::ALL.map(|e| base.sub_count(e));
		if let Some(prev) = self.auth_essence {
			for (i, e) in Essence::ALL.into_iter().enumerate() {
				self.toast(
					crate::ui::hud::ToastLabel::Essence(e),
					u64::from(now[i].saturating_sub(prev[i])),
				);
			}
		}
		self.auth_essence = Some(now);
	}

	/// The bench's opening invitation, asked wherever the pack GROWS (the
	/// dig credit, the server's inventory rebase): the first time it
	/// holds a siphon's whole price and no siphon, the siphon walkthrough
	/// chapter arms and does the talking. Price and gate are read from
	/// the tables, so a retune moves the nudge with it. Once per world
	/// session — owning one silences it for good.
	pub(super) fn nudge_siphon(&mut self) {
		use digblock_core::block::BlockType;
		let siphon = BlockType::EssenceSiphon;
		if self.siphon_nudged
			|| self.inventory.count(siphon) > 0
			|| !digblock_core::unlock::can_craft(siphon, &self.skills)
		{
			return;
		}
		let priced = digblock_core::alchemy::craft_feedstock(siphon)
			.iter()
			.all(|&(b, n)| self.inventory.count(b) >= n);
		if !priced {
			return;
		}
		self.siphon_nudged = true;
		self.arm_siphon_tutorial();
	}

	/// Adopt a new authoritative XP total for one skill: coalesce the delta into a toast, then
	/// word and sound a level-up. Totals only grow, so a replayed or reordered frame is a no-op.
	/// Both award paths land here — the server's `XpGain` online, and [`Self::award_xp_offline`]
	/// offline — so the presentation can't drift between the two.
	pub(super) fn apply_xp_total(&mut self, skill: digblock_core::skills::Skill, total: u64) {
		let old = self.skills.xp(skill);
		if total <= old {
			return;
		}
		self.skills.set_total(skill, total);
		self.toast(crate::ui::hud::ToastLabel::Xp(skill), total - old);
		let (old_level, new_level) = (
			digblock_core::skills::level_for_xp(old),
			digblock_core::skills::level_for_xp(total),
		);
		if new_level > old_level {
			self.push_feed(crate::voice::Msg::LevelUp {
				skill,
				level: new_level,
			});
			// Name what the rung actually opened ("you can now dig wood"),
			// derived from the strength-wall table — every level crossed,
			// so a coalesced multi-level award skips nothing.
			if skill == digblock_core::skills::Skill::Mining {
				for l in old_level + 1..=new_level {
					if let Some(f) = digblock_core::unlock::family_opened(l) {
						self.push_feed(crate::voice::Msg::NowDiggable(f));
					}
				}
			}
			// A level that OPENS something speaks with the bigger voice —
			// derived from the gate tables, so the two sounds can't drift
			// from what the level actually did. A multi-level jump (a big
			// offline award, or a coalesced total) asks about every level
			// crossed, not just the one landed on.
			let unlocked =
				(old_level + 1..=new_level).any(|l| digblock_core::unlock::opens(skill, l));
			self.react(crate::events::GameEvent::LeveledUp { unlocked });
			// Your own confetti, over your own head — first-person, that
			// is paper falling past the camera. Everyone ELSE gets theirs
			// from the server's `LevelUp` broadcast.
			let eye = self.player.pos;
			self.throw_confetti(eye);
			self.sync_node_reveal(true);
		}
	}

	/// The scripted half of [`Self::sync_node_reveal`]: flip the stamp
	/// and speak, but queue NO remesh — the siphon cinematic remeshes
	/// each subject mid-shot and sweeps the rest when the camera comes
	/// home ([`Self::finish_reveal`]); if no shot materializes, the
	/// cinematic starter falls straight back to the full sweep.
	pub(super) fn defer_node_reveal(&mut self) {
		let levels = digblock_core::worldgen::nodes::Levels::of(&self.skills, &self.inventory);
		let was = self.pipeline.levels();
		if !self.pipeline.set_levels(levels) {
			return;
		}
		self.reveal_finale = true;
		if levels.siphon && !was.siphon {
			self.push_feed(crate::voice::Msg::SourcesRevealed);
		}
	}

	/// The staged reveal's catch-all: whatever was withheld from the
	/// resident chunks sweeps in now. Harmless when nothing is owed.
	pub(super) fn finish_reveal(&mut self) {
		if std::mem::take(&mut self.reveal_finale) {
			self.reveal_cues.clear();
			self.pipeline.remesh_all();
		}
	}

	/// Hand the pipeline the gates its node stamp reveals at, and sweep every resident chunk when
	/// that crosses one (§5.3 — a Mining threshold for the ore tiers, the siphon bit for the
	/// surface nodes). The sweep is `remesh_all`, which only ENQUEUES — the mesh heap drains
	/// under the ordinary `STREAM_BUDGET_MS`, so the finds fade into the world around you instead
	/// of hitching the frame that opened them.
	pub(super) fn sync_node_reveal(&mut self, announce: bool) {
		let levels = digblock_core::worldgen::nodes::Levels::of(&self.skills, &self.inventory);
		let was = self.pipeline.levels();
		if !self.pipeline.set_levels(levels) {
			return;
		}
		self.pipeline.remesh_all();
		// Only a fresh gain says so. Entering a world, or a join sync
		// handing over a Mining 60 or a siphon you already had, crosses
		// the same gates against a fresh pipeline and would announce a
		// discovery you made an hour ago.
		if announce {
			self.push_feed(if levels.siphon && !was.siphon {
				crate::voice::Msg::SourcesRevealed
			} else {
				crate::voice::Msg::VeinsRevealed
			});
		}
	}

	/// Throw a confetti burst from a player's head. `eye` is the eye position (the authoritative
	/// `Player::pos` locally, the interpolated sample for a remote), and the burst sits just above
	/// it so it erupts over the head rather than out of the face. Over
	/// [`crate::confetti::MAX_BURSTS`] the oldest is dropped: a celebration that can't be drawn is
	/// better lost than the newest one, which is the one somebody is watching.
	pub(super) fn throw_confetti(&mut self, eye: Vec3) {
		const HEAD_CLEARANCE: f32 = 0.55;
		self.vfx.confetti_seq = self.vfx.confetti_seq.wrapping_add(1);
		if self.vfx.confetti.len() >= crate::confetti::MAX_BURSTS {
			self.vfx.confetti.remove(0);
		}
		self.vfx.confetti.push(crate::confetti::instance(
			[eye.x, eye.y + HEAD_CLEARANCE, eye.z],
			0.0,
			// A spread-out seed: consecutive bursts must not look like
			// the same paper thrown twice.
			self.vfx.confetti_seq as f32 * 7.919,
		));
	}

	/// Offline XP award, through the same core award tables the server
	/// runs. Online this is never called — the server's `XpGain` is the
	/// only award path (XP is not client-predicted).
	pub(super) fn award_xp_offline(&mut self, skill: digblock_core::skills::Skill, amount: u64) {
		if amount > 0 {
			let total = self.skills.xp(skill).saturating_add(amount);
			self.apply_xp_total(skill, total);
		}
	}

	/// Undo one pending edit's predicted inventory delta (rejection or
	/// authoritative supersede).
	pub(super) fn revert_inv_delta(&mut self, delta: Option<(BlockType, i32)>) {
		match delta {
			Some((b, n)) if n > 0 => {
				// A credited dig didn't happen: take the credit back.
				let _ = self.inventory.try_take(b, n as u32);
			}
			Some((b, n)) if n < 0 => {
				// A debited place didn't happen: refund.
				self.inventory.add(b, (-n) as u32);
			}
			_ => {}
		}
	}
}
