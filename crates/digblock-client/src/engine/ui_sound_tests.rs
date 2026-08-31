//! The UI sound layer, driven headless through a recording sink: the
//! one-comparison open/close cues, the seeded-snapshot silence at
//! startup, and the remote-edit cue budget.

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

/// An [`crate::audio::AudioSink`] that records — the trait is
/// deliberately not `Send`, so an `Rc` is exactly in contract.
struct RecSink(Rc<RefCell<Vec<crate::audio::AudioCmd>>>);

impl crate::audio::AudioSink for RecSink {
	fn send(&mut self, cmd: crate::audio::AudioCmd) {
		self.0.borrow_mut().push(cmd);
	}
}

fn engine_with_sink() -> (Engine, Rc<RefCell<Vec<crate::audio::AudioCmd>>>) {
	let mut config = EngineConfig::desktop();
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	let log = Rc::new(RefCell::new(Vec::new()));
	engine.set_audio(Box::new(RecSink(log.clone())));
	(engine, log)
}

/// The one-shots recorded since `log` was last drained.
fn sfx_of(log: &Rc<RefCell<Vec<crate::audio::AudioCmd>>>) -> Vec<crate::audio::Sfx> {
	log.borrow_mut()
		.drain(..)
		.filter_map(|c| match c {
			crate::audio::AudioCmd::Sfx { sound, .. } => Some(sound),
			_ => None,
		})
		.collect()
}

#[test]
fn screens_chirp_on_the_delta_and_never_at_startup() {
	let (mut engine, log) = engine_with_sink();
	// First frames seed the snapshot: whatever surfaces the session
	// starts on (the title menu is open here) must not chirp.
	engine.update(Duration::from_millis(16));
	engine.update(Duration::from_millis(16));
	let start = sfx_of(&log);
	assert!(
		!start
			.iter()
			.any(|s| matches!(s, crate::audio::Sfx::UiOpen | crate::audio::Sfx::UiClose)),
		"startup chirped: {start:?}"
	);
	// Opening the inventory is one UiOpen; closing it one UiClose.
	engine.ui.inv_screen.open();
	engine.update(Duration::from_millis(16));
	let opened = sfx_of(&log);
	assert_eq!(
		opened
			.iter()
			.filter(|s| **s == crate::audio::Sfx::UiOpen)
			.count(),
		1,
		"{opened:?}"
	);
	engine.ui.inv_screen.close();
	engine.update(Duration::from_millis(16));
	let closed = sfx_of(&log);
	assert_eq!(
		closed
			.iter()
			.filter(|s| **s == crate::audio::Sfx::UiClose)
			.count(),
		1,
		"{closed:?}"
	);
}

#[test]
fn hotbar_scroll_spam_coalesces_to_one_cue_per_frame() {
	let (mut engine, log) = engine_with_sink();
	engine.update(Duration::from_millis(16));
	sfx_of(&log);
	// Three selection moves inside one frame: the delta compare sees
	// only where it LANDED.
	engine.hotbar.set_selected(1);
	engine.hotbar.set_selected(2);
	engine.hotbar.set_selected(3);
	engine.update(Duration::from_millis(16));
	let cues = sfx_of(&log);
	assert_eq!(
		cues.iter()
			.filter(|s| **s == crate::audio::Sfx::UiClick)
			.count(),
		1,
		"{cues:?}"
	);
}

#[test]
fn a_remote_edit_storm_collapses_to_a_few_cues() {
	let (mut engine, log) = engine_with_sink();
	engine.update(Duration::from_millis(16));
	sfx_of(&log);
	// Ten broadcast edits land in one frame; the budget keeps a few.
	for i in 0..10 {
		engine.react(crate::events::GameEvent::RemoteEdit {
			cell: (i, 5, 0),
			block: BlockType::Lumber,
			prev: Some(BlockType::Air),
		});
	}
	let cues = sfx_of(&log);
	let places = cues
		.iter()
		.filter(|s| **s == crate::audio::Sfx::Place)
		.count();
	assert!(places > 0 && places <= 3, "{places} cues from 10 edits");
	// The next frame's budget is fresh: one more edit still speaks.
	engine.update(Duration::from_millis(16));
	sfx_of(&log);
	engine.react(crate::events::GameEvent::RemoteEdit {
		cell: (0, 6, 0),
		block: BlockType::Lumber,
		prev: Some(BlockType::Air),
	});
	assert_eq!(
		sfx_of(&log)
			.iter()
			.filter(|s| **s == crate::audio::Sfx::Place)
			.count(),
		1
	);
}
