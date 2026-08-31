//! The fan-out for gameplay moments: every discrete instant worth
//! presenting arrives here as one [`crate::events::GameEvent`], and this
//! match attaches everything it should do — sound today (the
//! `audio::sound_for` table); particles, toasts, camera feel and
//! tutorial advancement hang off the same point later. Call sites fire
//! the event and stay out of the presentation business.

use super::*;

/// One-shot cues a single frame may fire through `react`; the rest drop.
/// Loops and the cadence emitters never pass through here.
const ONESHOTS_PER_FRAME: u8 = 10;
/// Remote-edit cues per frame — a broadcast storm collapses to a few.
const REMOTE_EDIT_CUES_PER_FRAME: u8 = 3;

impl Engine {
	/// React to one gameplay moment. Free when nothing is attached (the
	/// sound path is one branch on a `None` sink).
	pub(super) fn react(&mut self, ev: crate::events::GameEvent) {
		// The tutorial watches everything, AHEAD of the frame budgets —
		// a storm may drop a cue, never a step.
		if let Some(adv) = self.tutorial.observe(&ev) {
			self.on_tutorial_advance(adv);
		}
		if matches!(ev, crate::events::GameEvent::RemoteEdit { .. }) {
			if self.audio.frame_remote_edits >= REMOTE_EDIT_CUES_PER_FRAME {
				return;
			}
			self.audio.frame_remote_edits += 1;
		}
		if self.audio.frame_oneshots >= ONESHOTS_PER_FRAME {
			return;
		}
		self.audio.frame_oneshots += 1;
		if let Some(s) = crate::audio::sound_for(&ev) {
			self.emit(crate::audio::AudioCmd::Sfx {
				sound: s.sfx,
				pos: if s.positional { ev.pos() } else { None },
				gain: s.gain,
			});
		}
		self.spawn_particles(&ev);
		// Camera feel, same table: the dig lands a small kick; an OWN
		// arrival breathes the lens out for a beat.
		match ev {
			crate::events::GameEvent::DigFinished { scooped: false, .. } => {
				self.vfx.camera_fx.kick(0.6);
				self.vfx.camera_fx.add_trauma(0.18);
			}
			crate::events::GameEvent::TeleportArrived { own: true, .. } => {
				self.vfx.camera_fx.pulse_fov(0.08);
			}
			_ => {}
		}
	}

	/// A tutorial step (or the whole walkthrough) just completed: word
	/// it, sound it, and persist completion through the settings path.
	pub(super) fn on_tutorial_advance(&mut self, adv: crate::tutorial::Advance) {
		match adv {
			crate::tutorial::Advance::Step(step) => {
				self.react(crate::events::GameEvent::TutorialStepDone { step });
			}
			crate::tutorial::Advance::Finished => {
				// Through the menu's settings, so the change persists via
				// the same drain/apply/store path a slider uses.
				self.ui.menu.set_show_tutorial(false);
				self.push_feed(crate::voice::Msg::TutorialDone);
				self.react(crate::events::GameEvent::TutorialDone);
			}
		}
	}

	/// Fire the open/close/select cues from ONE state-delta comparison,
	/// once per frame — covering every toggle path (keys, menu drains,
	/// focus rules, touch) with no scattered hooks, coalesced for free.
	pub(super) fn step_ui_sounds(&mut self) {
		use crate::events::{GameEvent, ScreenKind, UiEvent};
		let now = UiSnapshot {
			menu: self.ui.menu.is_open(),
			inv: self.ui.inv_screen.is_open(),
			alch: self.ui.alch_screen.is_open(),
			character: self.ui.char_screen.is_open(),
			hiscores: self.ui.hiscores.is_open(),
			spellbook: self.ui.spellbook.is_open(),
			chat: self.ui.chat.is_open(),
			hotbar: self.hotbar.selected_index(),
		};
		let Some(prev) = self.audio.ui_snapshot.replace(now) else {
			return;
		};
		let flags = [
			(prev.menu, now.menu, ScreenKind::Menu),
			(prev.inv, now.inv, ScreenKind::Inventory),
			(prev.alch, now.alch, ScreenKind::Alchemy),
			(prev.character, now.character, ScreenKind::Character),
			(prev.hiscores, now.hiscores, ScreenKind::Hiscores),
			(prev.spellbook, now.spellbook, ScreenKind::Spellbook),
			(prev.chat, now.chat, ScreenKind::Chat),
		];
		for (was, is, kind) in flags {
			if is != was {
				self.react(GameEvent::Ui(if is {
					UiEvent::ScreenOpen(kind)
				} else {
					UiEvent::ScreenClose(kind)
				}));
			}
		}
		if now.hotbar != prev.hotbar {
			self.react(GameEvent::Ui(UiEvent::HotbarSelect(now.hotbar as u8)));
		}
	}

	/// The particle half of the fan-out: which burst a moment throws.
	fn spawn_particles(&mut self, ev: &crate::events::GameEvent) {
		use crate::bursts::BurstKind;
		use crate::events::GameEvent as E;
		let center = |c| Vec3::from_array(cell_center(c));
		match *ev {
			E::DigFinished {
				cell,
				block,
				extracted,
				scooped,
			} => {
				if scooped {
					self.spawn_burst(BurstKind::Splash, center(cell), [0.3, 0.5, 0.8]);
				} else if extracted {
					// A drained seam releases motes, not rubble.
					self.spawn_burst(BurstKind::Sparkle, center(cell), [1.0, 0.9, 0.5]);
				} else {
					let color = self.block_tint(block);
					self.spawn_burst(BurstKind::Debris, center(cell), color);
				}
			}
			E::Placed { cell, poured: true } => {
				self.spawn_burst(BurstKind::Splash, center(cell), [0.3, 0.5, 0.8]);
			}
			// A remote's dig throws the same chips ours does; their
			// places and everything else stay particle-free.
			E::RemoteEdit {
				cell,
				block: BlockType::Air,
				prev: Some(p),
			} if !p.is_water() && p != BlockType::Air => {
				let color = self.block_tint(p);
				self.spawn_burst(BurstKind::Debris, center(cell), color);
			}
			_ => {}
		}
	}

	/// The mean albedo the renderer computed for a block, or plain gray
	/// headless (bench, tests) where no renderer exists.
	fn block_tint(&self, b: BlockType) -> [f32; 3] {
		self.renderer
			.as_ref()
			.map_or([0.5, 0.5, 0.5], |r| r.block_color(b))
	}

	/// Spawn one burst at a world point. Over the cap the oldest drops:
	/// a burst that can't draw is better lost than the newest one, which
	/// is the one somebody is watching (confetti's policy).
	pub(super) fn spawn_burst(
		&mut self,
		kind: crate::bursts::BurstKind,
		pos: Vec3,
		color: [f32; 3],
	) {
		self.vfx.burst_seq = self.vfx.burst_seq.wrapping_add(1);
		if self.vfx.bursts.len() >= crate::bursts::MAX_BURSTS {
			self.vfx.bursts.remove(0);
		}
		self.vfx.bursts.push(crate::bursts::instance(
			pos.to_array(),
			0.0,
			// The spread-out seed confetti uses: consecutive bursts must
			// not look like the same throw twice.
			self.vfx.burst_seq as f32 * 7.919,
			kind,
			1.0,
			color,
		));
	}
}
