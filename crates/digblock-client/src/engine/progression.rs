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
		match self.session.client.as_mut() {
			Some(net) if net.ready() => net.send_alchemy(wire_op),
			_ => {
				let level = self.skills.level(digblock_core::skills::Skill::Alchemy);
				if apply_alchemy(&mut self.inventory, core_op, level) {
					self.hotbar.auto_assign(item);
					self.award_xp_offline(
						digblock_core::skills::Skill::Alchemy,
						digblock_core::skills::alchemy_xp(&core_op),
					);
				}
			}
		}
	}

	/// Adopt a new authoritative XP total for one skill: coalesce the
	/// delta into a toast, and word + sound a level-up. Totals only
	/// grow, so a replayed or reordered frame is a no-op. Both award
	/// paths land here — the server's `XpGain` online, and
	/// [`Self::award_xp_offline`] offline — so the presentation can't
	/// drift between the two.
	pub(super) fn apply_xp_total(&mut self, skill: digblock_core::skills::Skill, total: u64) {
		let old = self.skills.xp(skill);
		if total <= old {
			return;
		}
		self.skills.set_total(skill, total);
		let delta = total - old;
		match self.vfx.xp_toasts.iter_mut().find(|t| t.skill == skill) {
			Some(t) => {
				t.amount += delta;
				t.ttl = crate::ui::hud::XP_TOAST_TTL;
			}
			None => self.vfx.xp_toasts.push(crate::ui::hud::XpToast {
				skill,
				amount: delta,
				ttl: crate::ui::hud::XP_TOAST_TTL,
			}),
		}
		let (old_level, new_level) = (
			digblock_core::skills::level_for_xp(old),
			digblock_core::skills::level_for_xp(total),
		);
		if new_level > old_level {
			self.push_feed(crate::voice::Msg::LevelUp {
				skill,
				level: new_level,
			});
			// A level that OPENS something speaks with the bigger voice —
			// derived from the gate tables, so the two sounds can't drift
			// from what the level actually did. A multi-level jump (a big
			// offline award, or a coalesced total) asks about every level
			// crossed, not just the one landed on.
			let unlocked =
				(old_level + 1..=new_level).any(|l| digblock_core::unlock::opens(skill, l));
			self.emit(crate::audio::AudioCmd::Sfx {
				sound: crate::audio::level_sfx(unlocked),
				pos: None,
				gain: crate::audio::LEVEL_GAIN,
			});
			// Your own confetti, over your own head — first-person, that
			// is paper falling past the camera. Everyone ELSE gets theirs
			// from the server's `LevelUp` broadcast.
			let eye = self.player.pos;
			self.throw_confetti(eye);
			self.sync_node_reveal(true);
		}
	}

	/// Hand the pipeline the skill levels its node stamp reveals at, and
	/// sweep every resident chunk when that crosses a threshold on either
	/// axis (§5.3 — Mining for the ore tiers, Alchemy for the surface
	/// nodes).
	///
	/// The sweep is `remesh_all`, which only ENQUEUES — the mesh heap
	/// drains under the ordinary `STREAM_BUDGET_MS`, so the veins fade in
	/// around you over a second or two instead of hitching the frame that
	/// levelled you up.
	pub(super) fn sync_node_reveal(&mut self, announce: bool) {
		let levels = digblock_core::worldgen::nodes::Levels::of(&self.skills);
		if !self.pipeline.set_levels(levels) {
			return;
		}
		self.pipeline.remesh_all();
		// Only a LEVEL-UP says so. Entering a world, or a join sync
		// handing over a Mining 60 you already had, crosses the same
		// thresholds against a fresh pipeline and would announce a
		// discovery you made an hour ago.
		if announce {
			self.push_feed(crate::voice::Msg::VeinsRevealed);
		}
	}

	/// Throw a confetti burst from a player's head. `eye` is the eye
	/// position (the authoritative `Player::pos` locally, the
	/// interpolated sample for a remote), and the burst sits just above
	/// it so it erupts over the head rather than out of the face.
	///
	/// Over [`crate::confetti::MAX_BURSTS`] the oldest is dropped: a
	/// celebration that can't be drawn is better lost than the newest
	/// one, which is the one somebody is watching.
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
