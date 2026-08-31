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
	SelectSlot,
	Place,
	OpenInventory,
	OpenAlchemy,
}

impl StepId {
	/// Test sweeps only; shipping code walks STEPS.
	#[cfg(test)]
	pub(crate) const ALL: [StepId; 8] = [
		StepId::Move,
		StepId::Look,
		StepId::Jump,
		StepId::Dig,
		StepId::SelectSlot,
		StepId::Place,
		StepId::OpenInventory,
		StepId::OpenAlchemy,
	];

	/// The knob spelling (`DIG_TUTORIAL=<step>`); None = unknown, and the
	/// knob layer turns that into a loud exit, never a default.
	pub fn parse(s: &str) -> Option<StepId> {
		Some(match s {
			"move" => StepId::Move,
			"look" => StepId::Look,
			"jump" => StepId::Jump,
			"dig" => StepId::Dig,
			"slot" => StepId::SelectSlot,
			"place" => StepId::Place,
			"inventory" => StepId::OpenInventory,
			"alchemy" => StepId::OpenAlchemy,
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
	HotbarSelected(u8),
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
pub(crate) const STEPS: [Step; 8] = [
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
		id: StepId::OpenInventory,
		trigger: Trigger::ScreenOpened(ScreenKind::Inventory),
		anchor: None,
		touch_anchor: Some(AnchorId::CornerMenu),
	},
	Step {
		id: StepId::OpenAlchemy,
		trigger: Trigger::ScreenOpened(ScreenKind::Alchemy),
		anchor: None,
		touch_anchor: Some(AnchorId::CornerMenu),
	},
];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
	#[default]
	Inactive,
	Running {
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
}

/// What advancing produced, for the engine to react to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Advance {
	Step(StepId),
	Finished,
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

	/// Dev knob: jump straight to one step.
	pub(crate) fn start_at(&mut self, id: StepId) {
		let step = STEPS.iter().position(|s| s.id == id).expect("id in STEPS") as u8;
		self.state = State::Running { step };
		self.progress = 0.0;
	}

	pub(crate) fn is_running(&self) -> bool {
		matches!(self.state, State::Running { .. })
	}

	/// Abandon the walkthrough; completion persists exactly as finishing.
	pub(crate) fn skip(&mut self) -> Option<Advance> {
		if !self.is_running() {
			return None;
		}
		self.state = State::Done;
		Some(Advance::Finished)
	}

	/// A gameplay moment happened; advance if it is what the current step
	/// asked for. Fires each step's Advance exactly once.
	pub(crate) fn observe(&mut self, ev: &GameEvent) -> Option<Advance> {
		let State::Running { step } = self.state else {
			return None;
		};
		let hit = match (STEPS[step as usize].trigger, ev) {
			(Trigger::Dug, GameEvent::DigFinished { .. }) => true,
			(Trigger::Placed, GameEvent::Placed { .. }) => true,
			(Trigger::ScreenOpened(k), GameEvent::Ui(UiEvent::ScreenOpen(o))) => k == *o,
			(Trigger::HotbarSelected(n), GameEvent::Ui(UiEvent::HotbarSelect(i))) => n == *i,
			_ => false,
		};
		hit.then(|| self.advance())
	}

	/// One frame of the continuous verbs; also ages the flourish.
	pub(crate) fn poll(&mut self, p: &Poll, dt: f32) -> Option<Advance> {
		self.celebrate = (self.celebrate - dt).max(0.0);
		let turned = match self.prev_look {
			Some((y, pi)) => (p.yaw_deg - y).abs() + (p.pitch_deg - pi).abs(),
			None => 0.0,
		};
		self.prev_look = Some((p.yaw_deg, p.pitch_deg));
		let State::Running { step } = self.state else {
			return None;
		};
		let hit = match STEPS[step as usize].trigger {
			Trigger::Moved { blocks } => {
				self.progress += p.moved_blocks;
				self.progress >= blocks
			}
			Trigger::Looked { degrees } => {
				self.progress += turned;
				self.progress >= degrees
			}
			Trigger::Jumped => p.jumped,
			_ => false,
		};
		hit.then(|| self.advance())
	}

	/// What to show, or None while inactive/done. `touch` picks the
	/// finger anchors over the keyboard ones.
	pub(crate) fn view(&self, touch: bool) -> Option<TutorialView> {
		let State::Running { step } = self.state else {
			return None;
		};
		let s = &STEPS[step as usize];
		Some(TutorialView {
			step: s.id,
			index: step,
			total: STEPS.len() as u8,
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
		let State::Running { step } = self.state else {
			unreachable!("advance is only reached from Running");
		};
		let done = STEPS[step as usize].id;
		self.progress = 0.0;
		self.celebrate = CELEBRATE_SECS;
		if step as usize + 1 >= STEPS.len() {
			self.state = State::Done;
			Advance::Finished
		} else {
			self.state = State::Running { step: step + 1 };
			Advance::Step(done)
		}
	}
}

#[cfg(test)]
mod tests;
