//! Gameplay moments as data. A significant instant (a dig lands, a
//! teleport fires, a level is gained) is described once as a
//! [`GameEvent`] and handed to `Engine::react`, the single fan-out
//! point that attaches its presentation — sound today via
//! [`crate::audio::sound_for`]; particles, toasts and camera feel hang
//! off the same match later. The event says WHAT happened with enough
//! payload to decide every reaction; it never says how it should sound,
//! which is the table's job. All-`Copy`, like the wire types.

use digblock_core::block::BlockType;

/// The surfaces a session opens and closes — identity for "open THIS
/// screen" sounds today and tutorial gates later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenKind {
	Menu,
	Inventory,
	Alchemy,
	Character,
	Hiscores,
	Spellbook,
	Chat,
}

/// A UI moment, carrying WHICH element where one exists — the same
/// identity a tutorial step gates on. All flat at the ears.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiEvent {
	/// A menu row activated (click or Enter).
	Click,
	ScreenOpen(ScreenKind),
	ScreenClose(ScreenKind),
	/// An inventory-screen drop landed on this slot.
	SlotDrop(crate::ui::InvId),
	/// The hotbar selection moved here (key, scroll, click or tap alike).
	HotbarSelect(u8),
	ChatSend,
	/// A transmute was ordered at the bench.
	Craft,
	/// A refusal the feed is about to word (see `audio::feed_sound`).
	Refused,
}

/// A gameplay-significant moment. Continuous state (dig-hold loops,
/// footstep cadence, the ambient bed) stays at its own call sites; these
/// are the discrete instants.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GameEvent {
	/// A dig hold ran out: the cell broke, drained, or was scooped.
	DigFinished {
		cell: (i32, i32, i32),
		block: BlockType,
		extracted: bool,
		scooped: bool,
	},
	/// A water-tool swing collected a source block.
	WaterScooped { cell: (i32, i32, i32) },
	/// A block landed (poured = a water source, which splashes).
	Placed { cell: (i32, i32, i32), poured: bool },
	/// A door leaf swung (never consumes stock, quieter than a place).
	DoorToggled { cell: (i32, i32, i32) },
	/// A teleport's departure collapse, at the jumper's old eye.
	TeleportDeparted { from: [f32; 3] },
	/// The arrival bloom. `own` = it lands in YOUR ears, heard flat.
	TeleportArrived { to: [f32; 3], own: bool },
	/// A glass sphere sealed over its caster's head.
	SphereSealed { head: (i32, i32, i32), own: bool },
	/// Another player's authoritative edit reached us. `block` = the new
	/// cell's block, `prev` = what stood there before it applied.
	RemoteEdit {
		cell: (i32, i32, i32),
		block: BlockType,
		prev: Option<BlockType>,
	},
	/// A skill crossed a level; `unlocked` = some level crossed opened
	/// something (see `digblock_core::unlock::opens`).
	LeveledUp { unlocked: bool },
	/// A UI moment (always at the ears; see [`UiEvent`]).
	Ui(UiEvent),
	/// A tutorial step completed (`step` names the one that finished).
	TutorialStepDone { step: crate::tutorial::StepId },
	/// The tutorial finished, or was skipped.
	TutorialDone,
}

/// Center of a cell — mirrors `engine::carried::cell_center`, private
/// there; three additions are cheaper than opening that module's seam.
fn center(cell: (i32, i32, i32)) -> [f32; 3] {
	[
		cell.0 as f32 + 0.5,
		cell.1 as f32 + 0.5,
		cell.2 as f32 + 0.5,
	]
}

impl GameEvent {
	/// The event's world anchor, or None for events with no place of
	/// their own. Whether a reaction USES it is the sound table's call
	/// (`SoundSpec::positional`), not the event's.
	pub fn pos(&self) -> Option<[f32; 3]> {
		match *self {
			GameEvent::DigFinished { cell, .. }
			| GameEvent::WaterScooped { cell }
			| GameEvent::Placed { cell, .. }
			| GameEvent::DoorToggled { cell }
			| GameEvent::RemoteEdit { cell, .. } => Some(center(cell)),
			GameEvent::TeleportDeparted { from } => Some(from),
			GameEvent::TeleportArrived { to, .. } => Some(to),
			GameEvent::SphereSealed { head, .. } => Some(center(head)),
			GameEvent::LeveledUp { .. }
			| GameEvent::Ui(_)
			| GameEvent::TutorialStepDone { .. }
			| GameEvent::TutorialDone => None,
		}
	}

	/// One sample per variant AND per branch the sound table takes, so a
	/// sweep over these exercises every row.
	#[cfg(test)]
	pub(crate) fn samples() -> Vec<GameEvent> {
		use BlockType as B;
		let cell = (1, 2, 3);
		vec![
			GameEvent::DigFinished {
				cell,
				block: B::Stone,
				extracted: false,
				scooped: false,
			},
			GameEvent::DigFinished {
				cell,
				block: B::Emberore,
				extracted: true,
				scooped: false,
			},
			GameEvent::DigFinished {
				cell,
				block: B::Water,
				extracted: false,
				scooped: true,
			},
			GameEvent::WaterScooped { cell },
			GameEvent::Placed {
				cell,
				poured: false,
			},
			GameEvent::Placed { cell, poured: true },
			GameEvent::DoorToggled { cell },
			GameEvent::TeleportDeparted {
				from: [1.0, 2.0, 3.0],
			},
			GameEvent::TeleportArrived {
				to: [1.0, 2.0, 3.0],
				own: true,
			},
			GameEvent::TeleportArrived {
				to: [1.0, 2.0, 3.0],
				own: false,
			},
			GameEvent::SphereSealed {
				head: cell,
				own: true,
			},
			GameEvent::SphereSealed {
				head: cell,
				own: false,
			},
			GameEvent::RemoteEdit {
				cell,
				block: B::Air,
				prev: Some(B::Stone),
			},
			GameEvent::RemoteEdit {
				cell,
				block: B::Lumber,
				prev: Some(B::Air),
			},
			GameEvent::RemoteEdit {
				cell,
				block: B::Water,
				prev: None,
			},
			GameEvent::LeveledUp { unlocked: false },
			GameEvent::LeveledUp { unlocked: true },
			GameEvent::Ui(UiEvent::Click),
			GameEvent::Ui(UiEvent::ScreenOpen(ScreenKind::Inventory)),
			GameEvent::Ui(UiEvent::ScreenClose(ScreenKind::Menu)),
			GameEvent::Ui(UiEvent::SlotDrop(crate::ui::InvId::Bar(0))),
			GameEvent::Ui(UiEvent::HotbarSelect(2)),
			GameEvent::Ui(UiEvent::ChatSend),
			GameEvent::Ui(UiEvent::Craft),
			GameEvent::Ui(UiEvent::Refused),
			GameEvent::TutorialStepDone {
				step: crate::tutorial::StepId::Move,
			},
			GameEvent::TutorialDone,
		]
	}
}

#[cfg(test)]
mod tests;
