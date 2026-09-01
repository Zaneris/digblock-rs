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
		StepId::OpenBench => t.poll(
			&Poll {
				bench_open: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::PickTools => t.poll(
			&Poll {
				tools_picked: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::PickSiphon => t.poll(
			&Poll {
				siphon_picked: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::CraftSiphon => t.poll(
			&Poll {
				owns_siphon: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::SelectSiphon => t.poll(
			&Poll {
				siphon_in_hand: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::OpenArcana => t.poll(
			&Poll {
				arcana_open: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::SlotWisp => t.poll(
			&Poll {
				wisp_slotted: true,
				..Poll::default()
			},
			0.016,
		),
		StepId::CloseArcana => {
			t.observe(&GameEvent::Ui(UiEvent::ScreenClose(ScreenKind::Spellbook)))
		}
		StepId::CastWisp => t.poll(
			&Poll {
				wisp_lit: true,
				..Poll::default()
			},
			0.016,
		),
	}
}

/// Table invariants: unique ids across BOTH chapters, `ALL` in table
/// order, and the parse round-trips every one.
#[test]
fn the_table_is_sound_and_the_knob_names_every_step() {
	let all: Vec<StepId> = STEPS
		.iter()
		.chain(SIPHON_STEPS.iter())
		.chain(WISP_STEPS.iter())
		.map(|s| s.id)
		.collect();
	for (i, id) in all.iter().enumerate() {
		assert_eq!(
			all.iter().filter(|o| *o == id).count(),
			1,
			"{id:?} appears twice"
		);
		assert_eq!(*id, StepId::ALL[i], "ALL order matches the tables");
	}
	assert_eq!(all.len(), StepId::ALL.len());
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
			StepId::OpenBench => "bench",
			StepId::PickTools => "tools",
			StepId::PickSiphon => "pick",
			StepId::CraftSiphon => "craft",
			StepId::SelectSiphon => "siphon",
			StepId::OpenArcana => "arcana",
			StepId::SlotWisp => "wispbar",
			StepId::CloseArcana => "closebook",
			StepId::CastWisp => "wisp",
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

/// Skip from any step lands Done and reports its chapter's finish
/// exactly once.
#[test]
fn skip_finishes_from_any_step() {
	for id in StepId::ALL {
		let mut t = Tutorial::default();
		t.start_at(id);
		let want = if SIPHON_STEPS.iter().any(|s| s.id == id) {
			Advance::SiphonFinished
		} else if WISP_STEPS.iter().any(|s| s.id == id) {
			Advance::WispFinished
		} else {
			Advance::Finished
		};
		assert_eq!(t.skip(), Some(want), "{id:?}");
		assert_eq!(t.skip(), None, "{id:?} skipped twice");
	}
}

/// The siphon chapter: armed by the nudge, walked by the pack and the
/// bar rather than clicks, and finishing with its own advance. A nudge
/// landing mid-walkthrough keeps the running chapter's floor.
#[test]
fn the_siphon_chapter_arms_and_walks() {
	let mut t = Tutorial::default();
	// Inactive: the poll conditions move nothing.
	assert_eq!(satisfy(&mut t, StepId::OpenBench), None);
	t.start_chapter(Chapter::Siphon);
	let view = t.view(false).expect("running");
	assert_eq!(view.step, StepId::OpenBench);
	assert_eq!(view.total, 5);
	// The wrong condition holds nothing.
	assert_eq!(satisfy(&mut t, StepId::SelectSiphon), None);
	for id in [
		StepId::OpenBench,
		StepId::PickTools,
		StepId::PickSiphon,
		StepId::CraftSiphon,
	] {
		assert_eq!(satisfy(&mut t, id), Some(Advance::Step(id)), "{id:?}");
	}
	assert_eq!(
		satisfy(&mut t, StepId::SelectSiphon),
		Some(Advance::SiphonFinished)
	);
	assert!(!t.is_running());
	// Mid-walkthrough, the nudge does not steal the floor.
	let mut t = Tutorial::default();
	t.start();
	t.start_chapter(Chapter::Siphon);
	assert_eq!(t.view(false).unwrap().step, StepId::Move);
}

/// The wisp chapter: armed by its own nudge, the book opened, the wisp
/// slotted (however it got there), the book closed by its event, and
/// the burning wisp finishing with its own advance.
#[test]
fn the_wisp_chapter_arms_and_walks() {
	let mut t = Tutorial::default();
	// Inactive: the poll conditions move nothing.
	assert_eq!(satisfy(&mut t, StepId::OpenArcana), None);
	t.start_chapter(Chapter::Wisp);
	let view = t.view(false).expect("running");
	assert_eq!(view.step, StepId::OpenArcana);
	assert_eq!(view.total, 4);
	// The wrong condition holds nothing.
	assert_eq!(satisfy(&mut t, StepId::CastWisp), None);
	for id in [StepId::OpenArcana, StepId::SlotWisp, StepId::CloseArcana] {
		assert_eq!(satisfy(&mut t, id), Some(Advance::Step(id)), "{id:?}");
	}
	assert_eq!(
		satisfy(&mut t, StepId::CastWisp),
		Some(Advance::WispFinished)
	);
	assert!(!t.is_running());
}

/// Nudges landing mid-walkthrough queue their chapters: the walkthrough
/// finishes as itself, and the queued chapters take the floor in
/// arrival order instead of being lost for the session.
#[test]
fn mid_walkthrough_nudges_queue_their_chapters() {
	let mut t = Tutorial::default();
	t.start();
	t.start_chapter(Chapter::Siphon);
	t.start_chapter(Chapter::Wisp);
	t.start_chapter(Chapter::Siphon); // a duplicate arms nothing twice
	for (i, s) in STEPS.iter().enumerate() {
		let adv = satisfy(&mut t, s.id);
		if i + 1 == STEPS.len() {
			assert_eq!(adv, Some(Advance::Finished));
		} else {
			assert_eq!(adv, Some(Advance::Step(s.id)));
		}
	}
	// The first queued chapter is already up, from its first step.
	assert_eq!(t.view(false).expect("running").step, StepId::OpenBench);
	for id in [
		StepId::OpenBench,
		StepId::PickTools,
		StepId::PickSiphon,
		StepId::CraftSiphon,
	] {
		assert_eq!(satisfy(&mut t, id), Some(Advance::Step(id)), "{id:?}");
	}
	assert_eq!(
		satisfy(&mut t, StepId::SelectSiphon),
		Some(Advance::SiphonFinished)
	);
	// And the second right behind it.
	assert_eq!(t.view(false).expect("running").step, StepId::OpenArcana);
}

/// Skipping drops the queued chapters too: the player said no more
/// tutoring, and nothing pops up behind the skip.
#[test]
fn skip_drops_the_queued_chapters() {
	let mut t = Tutorial::default();
	t.start();
	t.start_chapter(Chapter::Siphon);
	t.start_chapter(Chapter::Wisp);
	assert_eq!(t.skip(), Some(Advance::Finished));
	assert!(!t.is_running());
	assert_eq!(satisfy(&mut t, StepId::OpenBench), None);
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
