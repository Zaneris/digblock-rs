use super::*;
use crate::audio::sound_for;

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
