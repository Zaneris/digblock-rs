use super::*;
use crate::audio::{Sfx, SoundSpec, sound_for};

/// Every row of the sound table is sane: gains authored in (0, 1], and a
/// positional spec always has a world anchor to be positional AT.
#[test]
fn every_spec_has_a_sane_gain_and_an_anchor_when_positional() {
	for ev in GameEvent::samples() {
		let Some(spec) = sound_for(&ev) else { continue };
		assert!(
			spec.gain > 0.0 && spec.gain <= 1.0,
			"{ev:?} gain {}",
			spec.gain
		);
		if spec.positional {
			assert!(ev.pos().is_some(), "{ev:?} positional without a pos");
		}
	}
}

/// The migration to the table preserved every call site's voice: these
/// pin the exact (sfx, gain, positional) triples the nine inline emits
/// used to build by hand.
#[test]
fn the_table_speaks_with_the_call_sites_old_voices() {
	use BlockType as B;
	let cell = (1, 2, 3);
	let spec = |sfx, gain, positional| {
		Some(SoundSpec {
			sfx,
			gain,
			positional,
		})
	};
	let dug = GameEvent::DigFinished {
		cell,
		block: B::Stone,
		extracted: false,
		scooped: false,
	};
	assert_eq!(sound_for(&dug), spec(Sfx::BreakStone, 1.0, true));
	let drawn = GameEvent::DigFinished {
		cell,
		block: B::Emberore,
		extracted: true,
		scooped: false,
	};
	assert_eq!(sound_for(&drawn), spec(Sfx::SiphonTake, 0.9, true));
	let scooped = GameEvent::DigFinished {
		cell,
		block: B::Water,
		extracted: false,
		scooped: true,
	};
	assert_eq!(sound_for(&scooped), spec(Sfx::Splash, 0.5, true));
	let poured = GameEvent::Placed { cell, poured: true };
	assert_eq!(sound_for(&poured), spec(Sfx::Splash, 0.6, true));
	// An OWN teleport arrival and sphere seal land in your ears: flat.
	let own_in = GameEvent::TeleportArrived {
		to: [0.0; 3],
		own: true,
	};
	assert_eq!(sound_for(&own_in), spec(Sfx::TeleportIn, 0.7, false));
	let own_seal = GameEvent::SphereSealed {
		head: cell,
		own: true,
	};
	assert_eq!(sound_for(&own_seal), spec(Sfx::SphereClose, 0.8, false));
	// A remote's dig breaks as what fell; water and conjured stay silent.
	let broke = GameEvent::RemoteEdit {
		cell,
		block: B::Air,
		prev: Some(B::Stone),
	};
	assert_eq!(sound_for(&broke), spec(Sfx::BreakStone, 0.5, true));
	let water = GameEvent::RemoteEdit {
		cell,
		block: B::Water,
		prev: None,
	};
	assert_eq!(sound_for(&water), None);
	assert_eq!(
		sound_for(&GameEvent::LeveledUp { unlocked: false }),
		spec(Sfx::LevelUp, crate::audio::LEVEL_GAIN, false)
	);
}
