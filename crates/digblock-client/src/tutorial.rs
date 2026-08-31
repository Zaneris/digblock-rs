//! The interactive tutorial: a const table of steps, each completed by
//! the player actually doing the thing, driven by the same events the
//! sound layer rides plus a per-frame poll for the continuous verbs
//! (walk, look, jump). Pure and headless: `observe`/`poll` consume data
//! and answer with an [`Advance`]; the engine owns when it starts (first
//! entry into an offline world), what it looks like (`ui::tutorial`),
//! and where completion persists (`Settings::show_tutorial`). Strictly
//! linear in v1; steps are NAMED (`StepId`), never numbered, so nothing
//! persisted or parsed can rot when one is inserted.

use crate::events::{GameEvent, ScreenKind, UiEvent};
use crate::ui::anchors::AnchorId;

/// Stable step names — `STEPS` order is the walkthrough; these are for
/// knobs, voice lines and tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepId {
	Move,
	Look,
	Jump,
	Dig,
	SwitchSlot,
	SelectSlot,
	Place,
	OpenMenu,
	OpenInventory,
	CloseInventory,
	// ---- the siphon chapter (SIPHON_STEPS) ----
	CraftSiphon,
	SelectSiphon,
}

impl StepId {
	/// Test sweeps only; shipping code walks the step tables.
	#[cfg(test)]
	pub(crate) const ALL: [StepId; 12] = [
		StepId::Move,
		StepId::Look,
		StepId::Jump,
		StepId::Dig,
		StepId::SwitchSlot,
		StepId::SelectSlot,
		StepId::Place,
		StepId::OpenMenu,
		StepId::OpenInventory,
		StepId::CloseInventory,
		StepId::CraftSiphon,
		StepId::SelectSiphon,
	];

	/// The knob spelling (`DIG_TUTORIAL=<step>`); None = unknown, and the
	/// knob layer turns that into a loud exit, never a default.
	pub fn parse(s: &str) -> Option<StepId> {
		Some(match s {
			"move" => StepId::Move,
			"look" => StepId::Look,
			"jump" => StepId::Jump,
			"dig" => StepId::Dig,
			"switch" => StepId::SwitchSlot,
			"slot" => StepId::SelectSlot,
			"place" => StepId::Place,
			"menu" => StepId::OpenMenu,
			"inventory" => StepId::OpenInventory,
			"close" => StepId::CloseInventory,
			"craft" => StepId::CraftSiphon,
			"siphon" => StepId::SelectSiphon,
			_ => return None,
		})
	}
}

/// `DIG_TUTORIAL`'s parsed value: force on, force off, or jump straight
/// to one step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TutorialOverride {
	On,
	Off,
	At(StepId),
}

/// What completes a step — enum data, no closures. The poll triggers
/// accumulate toward a goal; the event triggers match one moment.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Trigger {
	/// Accumulate horizontal blocks walked.
	Moved {
		blocks: f32,
	},
	/// Accumulate degrees of camera turn (|yaw| + |pitch|).
	Looked {
		degrees: f32,
	},
	Jumped,
	/// Any dig hold completed.
	Dug,
	/// Any block placed.
	Placed,
	ScreenOpened(ScreenKind),
	ScreenClosed(ScreenKind),
	/// Any hotbar change at all (the events only fire on a change, so a
	/// player parked on the slot the next step wants must leave it first).
	HotbarSwitched,
	HotbarSelected(u8),
	/// The pack holds an essence siphon — polled off the inventory, not
	/// the craft click, so the server's echo path lands it too.
	OwnsSiphon,
	/// The tool slot is selected with the siphon as the active tool.
	SiphonInHand,
}

pub(crate) struct Step {
	pub id: StepId,
	pub trigger: Trigger,
	/// Element to pulse while the step runs (keyboard context).
	pub anchor: Option<AnchorId>,
	/// Override when the touch HUD is up; None falls back to `anchor`.
	pub touch_anchor: Option<AnchorId>,
}

/// The walkthrough. Wording lives in `voice::en`, keyed by `id`.
pub(crate) const STEPS: [Step; 10] = [
	Step {
		id: StepId::Move,
		trigger: Trigger::Moved { blocks: 5.0 },
		anchor: None,
		touch_anchor: None,
	},
	Step {
		id: StepId::Look,
		trigger: Trigger::Looked { degrees: 180.0 },
		anchor: None,
		touch_anchor: None,
	},
	Step {
		id: StepId::Jump,
		trigger: Trigger::Jumped,
		anchor: None,
		touch_anchor: Some(AnchorId::JumpButton),
	},
	Step {
		id: StepId::Dig,
		trigger: Trigger::Dug,
		anchor: None,
		touch_anchor: None,
	},
	// The dig just landed its yield in the SELECTED slot, so "select your
	// first slot" would already be true: leave it first, then come back.
	Step {
		id: StepId::SwitchSlot,
		trigger: Trigger::HotbarSwitched,
		anchor: Some(AnchorId::Ui(crate::ui::InvId::Bar(1))),
		touch_anchor: None,
	},
	Step {
		id: StepId::SelectSlot,
		trigger: Trigger::HotbarSelected(0),
		anchor: Some(AnchorId::Ui(crate::ui::InvId::Bar(0))),
		touch_anchor: None,
	},
	Step {
		id: StepId::Place,
		trigger: Trigger::Placed,
		anchor: None,
		touch_anchor: Some(AnchorId::PlaceButton),
	},
	Step {
		id: StepId::OpenMenu,
		trigger: Trigger::ScreenOpened(ScreenKind::Menu),
		anchor: None,
		touch_anchor: Some(AnchorId::CornerMenu),
	},
	// Performed from inside the menu (its Inventory row, or the hotkey
	// reaching through); no anchor, the row is the menu's own geometry.
	Step {
		id: StepId::OpenInventory,
		trigger: Trigger::ScreenOpened(ScreenKind::Inventory),
		anchor: None,
		touch_anchor: None,
	},
	Step {
		id: StepId::CloseInventory,
		trigger: Trigger::ScreenClosed(ScreenKind::Inventory),
		anchor: None,
		touch_anchor: Some(AnchorId::CornerMenu),
	},
];

/// The siphon chapter, armed by the affordability nudge rather than at
/// world entry. Crafting one advances the first step (and the engine
/// cuts to the reveal cinematic); the second waits for the siphon to
/// actually be in hand, cycle included.
pub(crate) const SIPHON_STEPS: [Step; 2] = [
	Step {
		id: StepId::CraftSiphon,
		trigger: Trigger::OwnsSiphon,
		anchor: None,
		touch_anchor: None,
	},
	Step {
		id: StepId::SelectSiphon,
		trigger: Trigger::SiphonInHand,
		anchor: Some(AnchorId::Ui(crate::ui::InvId::Bar(
			(digblock_core::hotbar::HOTBAR_SLOTS - 1) as u8,
		))),
		touch_anchor: None,
	},
];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
	#[default]
	Inactive,
	Running {
		step: u8,
	},
	/// The siphon chapter (SIPHON_STEPS), armed by the nudge.
	Siphon {
		step: u8,
	},
	Done,
}

/// The running walkthrough. `Default` = inactive (server joins, bench).
#[derive(Default)]
pub(crate) struct Tutorial {
	state: State,
	/// Current step's accumulator (blocks for Moved, degrees for Looked).
	progress: f32,
	/// Seconds left of the step-done flourish on the panel.
	celebrate: f32,
	/// Last seen look angles; None seeds without crediting a turn.
	prev_look: Option<(f32, f32)>,
}

/// One frame of play, distilled for the poll triggers. Movement is the
/// frame's DELTA (a teleport resets its baseline and credits nothing);
/// look angles are absolutes the tutorial diffs itself.
#[derive(Clone, Copy, Default)]
pub(crate) struct Poll {
	pub moved_blocks: f32,
	pub yaw_deg: f32,
	pub pitch_deg: f32,
	pub jumped: bool,
	pub owns_siphon: bool,
	pub siphon_in_hand: bool,
}

/// What advancing produced, for the engine to react to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Advance {
	Step(StepId),
	Finished,
	/// The siphon chapter's own end: its wording differs, and the
	/// walkthrough's completion setting is not its business.
	SiphonFinished,
}

/// Everything presentation needs for the current step.
pub(crate) struct TutorialView {
	pub step: StepId,
	pub index: u8,
	pub total: u8,
	pub anchor: Option<AnchorId>,
	/// 0..=1 for the accumulating steps; None for one-moment steps.
	pub progress: Option<f32>,
	pub celebrating: bool,
}

/// Seconds the completed step flashes before the next one's text.
const CELEBRATE_SECS: f32 = 0.8;

impl Tutorial {
	pub(crate) fn start(&mut self) {
		self.state = State::Running { step: 0 };
		self.progress = 0.0;
	}

	/// Arm the siphon chapter (the nudge moment: the pack can afford
	/// one). A chapter already running keeps the floor.
	pub(crate) fn start_siphon(&mut self) {
		if self.is_running() {
			return;
		}
		self.state = State::Siphon { step: 0 };
		self.progress = 0.0;
	}

	/// Dev knob: jump straight to one step, in whichever table holds it.
	pub(crate) fn start_at(&mut self, id: StepId) {
		self.state = if let Some(i) = STEPS.iter().position(|s| s.id == id) {
			State::Running { step: i as u8 }
		} else {
			let i = SIPHON_STEPS
				.iter()
				.position(|s| s.id == id)
				.expect("id in a step table");
			State::Siphon { step: i as u8 }
		};
		self.progress = 0.0;
	}

	pub(crate) fn is_running(&self) -> bool {
		self.current().is_some()
	}

	/// The active chapter's table and step, or None at rest.
	fn current(&self) -> Option<(&'static [Step], u8)> {
		match self.state {
			State::Running { step } => Some((&STEPS[..], step)),
			State::Siphon { step } => Some((&SIPHON_STEPS[..], step)),
			State::Inactive | State::Done => None,
		}
	}

	/// Abandon the running chapter; completion lands exactly as finishing.
	pub(crate) fn skip(&mut self) -> Option<Advance> {
		let adv = match self.state {
			State::Running { .. } => Advance::Finished,
			State::Siphon { .. } => Advance::SiphonFinished,
			State::Inactive | State::Done => return None,
		};
		self.state = State::Done;
		Some(adv)
	}

	/// A gameplay moment happened; advance if it is what the current step
	/// asked for. Fires each step's Advance exactly once.
	pub(crate) fn observe(&mut self, ev: &GameEvent) -> Option<Advance> {
		let (steps, step) = self.current()?;
		let hit = match (steps[step as usize].trigger, ev) {
			(Trigger::Dug, GameEvent::DigFinished { .. }) => true,
			(Trigger::Placed, GameEvent::Placed { .. }) => true,
			(Trigger::ScreenOpened(k), GameEvent::Ui(UiEvent::ScreenOpen(o))) => k == *o,
			(Trigger::ScreenClosed(k), GameEvent::Ui(UiEvent::ScreenClose(o))) => k == *o,
			(Trigger::HotbarSwitched, GameEvent::Ui(UiEvent::HotbarSelect(_))) => true,
			(Trigger::HotbarSelected(n), GameEvent::Ui(UiEvent::HotbarSelect(i))) => n == *i,
			_ => false,
		};
		hit.then(|| self.advance())
	}

	/// Age only the step-done flourish: the frames where the poll verbs
	/// pause (open menu) but the panel may still be showing.
	pub(crate) fn tick_celebrate(&mut self, dt: f32) {
		self.celebrate = (self.celebrate - dt).max(0.0);
	}

	/// One frame of the continuous verbs; also ages the flourish.
	pub(crate) fn poll(&mut self, p: &Poll, dt: f32) -> Option<Advance> {
		self.celebrate = (self.celebrate - dt).max(0.0);
		let turned = match self.prev_look {
			Some((y, pi)) => (p.yaw_deg - y).abs() + (p.pitch_deg - pi).abs(),
			None => 0.0,
		};
		self.prev_look = Some((p.yaw_deg, p.pitch_deg));
		let (steps, step) = self.current()?;
		let hit = match steps[step as usize].trigger {
			Trigger::Moved { blocks } => {
				self.progress += p.moved_blocks;
				self.progress >= blocks
			}
			Trigger::Looked { degrees } => {
				self.progress += turned;
				self.progress >= degrees
			}
			Trigger::Jumped => p.jumped,
			Trigger::OwnsSiphon => p.owns_siphon,
			Trigger::SiphonInHand => p.siphon_in_hand,
			_ => false,
		};
		hit.then(|| self.advance())
	}

	/// What to show, or None while inactive/done. `touch` picks the
	/// finger anchors over the keyboard ones.
	pub(crate) fn view(&self, touch: bool) -> Option<TutorialView> {
		let (steps, step) = self.current()?;
		let s = &steps[step as usize];
		Some(TutorialView {
			step: s.id,
			index: step,
			total: steps.len() as u8,
			anchor: if touch {
				s.touch_anchor.or(s.anchor)
			} else {
				s.anchor
			},
			progress: match s.trigger {
				Trigger::Moved { blocks } => Some((self.progress / blocks).min(1.0)),
				Trigger::Looked { degrees } => Some((self.progress / degrees).min(1.0)),
				_ => None,
			},
			celebrating: self.celebrate > 0.0,
		})
	}

	fn advance(&mut self) -> Advance {
		let siphon = matches!(self.state, State::Siphon { .. });
		let Some((steps, step)) = self.current() else {
			unreachable!("advance is only reached from a running chapter");
		};
		let done = steps[step as usize].id;
		self.progress = 0.0;
		self.celebrate = CELEBRATE_SECS;
		if step as usize + 1 >= steps.len() {
			self.state = State::Done;
			if siphon {
				Advance::SiphonFinished
			} else {
				Advance::Finished
			}
		} else {
			let step = step + 1;
			self.state = if siphon {
				State::Siphon { step }
			} else {
				State::Running { step }
			};
			Advance::Step(done)
		}
	}
}

#[cfg(test)]
mod tests;
