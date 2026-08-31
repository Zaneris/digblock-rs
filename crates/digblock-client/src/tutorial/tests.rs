use super::*;
use digblock_core::block::BlockType;

/// The event/poll pair that satisfies one step.
fn satisfy(t: &mut Tutorial, id: StepId) -> Option<Advance> {
	let cell = (0, 0, 0);
	match id {
		StepId::Move => t.poll(
			&Poll {
				moved_blocks: 6.0,
				..Poll::default()
			},
			0.016,
		),
		StepId::Look => t.poll(
			&Poll {
				yaw_deg: 400.0,
				..Poll::default()
			},
			0.016,
		),
		StepId::Jump => t.poll(
			&Poll {
				jumped: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::Dig => t.observe(&GameEvent::DigFinished {
			cell,
			block: BlockType::Dirt,
			extracted: false,
			scooped: false,
		}),
		StepId::SwitchSlot => t.observe(&GameEvent::Ui(UiEvent::HotbarSelect(2))),
		StepId::SelectSlot => t.observe(&GameEvent::Ui(UiEvent::HotbarSelect(0))),
		StepId::Place => t.observe(&GameEvent::Placed {
			cell,
			poured: false,
		}),
		StepId::OpenMenu => t.observe(&GameEvent::Ui(UiEvent::ScreenOpen(ScreenKind::Menu))),
		StepId::OpenInventory => {
			t.observe(&GameEvent::Ui(UiEvent::ScreenOpen(ScreenKind::Inventory)))
		}
		StepId::CloseInventory => {
			t.observe(&GameEvent::Ui(UiEvent::ScreenClose(ScreenKind::Inventory)))
		}
	}
}

/// Table invariants: unique ids, and the parse round-trips every one.
#[test]
fn the_table_is_sound_and_the_knob_names_every_step() {
	for (i, s) in STEPS.iter().enumerate() {
		assert_eq!(
			STEPS.iter().filter(|o| o.id == s.id).count(),
			1,
			"{:?} appears twice",
			s.id
		);
		assert_eq!(STEPS[i].id, StepId::ALL[i], "ALL order matches STEPS");
	}
	for id in StepId::ALL {
		let spelled = match id {
			StepId::Move => "move",
			StepId::Look => "look",
			StepId::Jump => "jump",
			StepId::Dig => "dig",
			StepId::SwitchSlot => "switch",
			StepId::SelectSlot => "slot",
			StepId::Place => "place",
			StepId::OpenMenu => "menu",
			StepId::OpenInventory => "inventory",
			StepId::CloseInventory => "close",
		};
		assert_eq!(StepId::parse(spelled), Some(id));
	}
	assert_eq!(StepId::parse("bogus"), None);
}

/// The full walkthrough: each step advances exactly once on its own
/// satisfying input, wrong inputs advance nothing, and the last step
/// yields Finished.
#[test]
fn the_walkthrough_runs_start_to_finish() {
	let mut t = Tutorial::default();
	// Inactive: nothing moves it.
	assert_eq!(satisfy(&mut t, StepId::Dig), None);
	t.start();
	for (i, s) in STEPS.iter().enumerate() {
		let view = t.view(false).expect("running");
		assert_eq!(view.step, s.id);
		assert_eq!(view.index as usize, i);
		// A WRONG event moves nothing (dig is never the answer to a
		// non-dig step, and vice versa with the slot select).
		let wrong = if s.id == StepId::Dig {
			t.observe(&GameEvent::Ui(UiEvent::HotbarSelect(3)))
		} else {
			t.observe(&GameEvent::DigFinished {
				cell: (0, 0, 0),
				block: BlockType::Dirt,
				extracted: false,
				scooped: false,
			})
			.filter(|_| s.id != StepId::Dig)
		};
		assert_eq!(wrong, None, "{:?} advanced on the wrong input", s.id);
		let adv = satisfy(&mut t, s.id);
		if i + 1 == STEPS.len() {
			assert_eq!(adv, Some(Advance::Finished));
		} else {
			assert_eq!(adv, Some(Advance::Step(s.id)));
		}
	}
	assert!(!t.is_running());
	// Spent: nothing fires again.
	assert_eq!(satisfy(&mut t, StepId::Dig), None);
}

/// Accumulation crosses frames; a teleport-sized zero frame adds zero.
#[test]
fn walking_accumulates_across_frames() {
	let mut t = Tutorial::default();
	t.start();
	for _ in 0..9 {
		let adv = t.poll(
			&Poll {
				moved_blocks: 0.5,
				..Poll::default()
			},
			0.016,
		);
		assert_eq!(adv, None);
	}
	let p = t.view(false).unwrap().progress.unwrap();
	assert!((p - 0.9).abs() < 1e-4, "{p}");
	assert_eq!(
		t.poll(
			&Poll {
				moved_blocks: 0.6,
				..Poll::default()
			},
			0.016
		),
		Some(Advance::Step(StepId::Move))
	);
}

/// The look step diffs absolutes and never credits the seeding frame:
/// the very first sample of a session establishes the baseline, however
/// wild the angle it carries.
#[test]
fn the_first_look_sample_credits_nothing() {
	let mut t = Tutorial::default();
	t.start_at(StepId::Look);
	assert_eq!(
		t.poll(
			&Poll {
				yaw_deg: 1000.0,
				..Poll::default()
			},
			0.016
		),
		None
	);
	assert_eq!(t.view(false).unwrap().progress, Some(0.0));
	// The second sample diffs against the first: a real turn credits.
	t.poll(
		&Poll {
			yaw_deg: 1090.0,
			..Poll::default()
		},
		0.016,
	);
	assert_eq!(t.view(false).unwrap().progress, Some(0.5));
}

/// Skip from any step lands Done and reports Finished exactly once.
#[test]
fn skip_finishes_from_any_step() {
	for id in StepId::ALL {
		let mut t = Tutorial::default();
		t.start_at(id);
		assert_eq!(t.skip(), Some(Advance::Finished), "{id:?}");
		assert_eq!(t.skip(), None, "{id:?} skipped twice");
	}
}

/// Touch picks the touch anchor when one exists, else the keyboard's.
#[test]
fn touch_anchors_override_where_declared() {
	let mut t = Tutorial::default();
	t.start_at(StepId::Jump);
	assert_eq!(t.view(false).unwrap().anchor, None);
	assert_eq!(t.view(true).unwrap().anchor, Some(AnchorId::JumpButton));
	t.start_at(StepId::SelectSlot);
	let want = Some(AnchorId::Ui(crate::ui::InvId::Bar(0)));
	assert_eq!(t.view(false).unwrap().anchor, want);
	assert_eq!(t.view(true).unwrap().anchor, want, "falls back to anchor");
}
