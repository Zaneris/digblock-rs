//! The Causeway's one frame-rate-sensitive contract: the road is LEVEL,
//! at every display rate. `Trail::wants` answers for the tick that lost
//! support and for no other, so the plank has to be asked for inside the
//! tick loop — asked once a frame instead, a frame spanning two ticks
//! hands `causeway::step_cell` feet that have already sagged under the
//! top they walked off, and the answer moves a whole cell down.
//!
//! Driven through the real `Engine::step` at several ticks per frame,
//! because the call SITE is the thing under test: no core-level fixture
//! can pin it, since a fixture that asks per frame is the bug.

use super::*;
use digblock_core::causeway::TRAIL;
use digblock_core::player::{EYE_TO_FEET, TICK};
use digblock_platform::{InputEvent, Key};

fn fresh() -> Engine {
	let mut config = EngineConfig::desktop();
	config.interactive = true;
	config.workers = 0;
	config.seed = Some(3.0);
	Engine::new(config)
}

/// Stream the world about spawn in until nothing is in flight — a road
/// lays nothing over a column the pipeline has no answer for.
fn settle(e: &mut Engine) {
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = e.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
}

/// Walk a standing causeway for `frames` frames of `per_frame` ticks
/// each, sampling the standing trail each frame — which is every plank
/// laid, since a road lays them slower than `TRAIL` per sample at any of
/// the rates here.
///
/// The caster starts well above the terrain so the road is the only
/// thing under them: the first plank catches the drop, and from there
/// every step is the cliff walk — off the edge of the plank they stand
/// on, into air.
fn walk(per_frame: u32, frames: usize) -> (Vec<(i32, i32, i32)>, f32) {
	let mut e = fresh();
	settle(&mut e);
	// High enough that the terrain is nowhere near the road, so a plank
	// at the wrong height is the road's own doing.
	let start = e.player.pos + Vec3::Y * 12.0;
	e.player.set_state(digblock_core::player::PlayerState {
		pos: start,
		vel: Vec3::ZERO,
	});
	e.causeway_left = 30.0;
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: true,
	});
	let mut laid: Vec<(i32, i32, i32)> = Vec::new();
	for _ in 0..frames {
		e.step(std::time::Duration::from_secs_f32(TICK * per_frame as f32));
		for c in e.causeway.cells() {
			if !laid.contains(&c) {
				laid.push(c);
			}
		}
	}
	// The fixture's own premise, and nothing about the road: a walk that
	// laid a trail's worth of planks is a walk that happened.
	assert!(
		laid.len() > TRAIL,
		"{per_frame} ticks/frame: only {} planks — the walk never turned the trail over",
		laid.len()
	);
	(laid, e.player.pos.y - EYE_TO_FEET)
}

/// The road stays level however many ticks a frame spans. Counted and
/// compared across rates rather than spot-checked at one, because the
/// rate at which it held was the whole of the old bug: one tick per
/// frame is the ONLY rate the per-frame question happened to answer
/// correctly, and 60fps is the one every capture path runs at.
#[test]
fn a_road_is_level_at_every_frame_rate() {
	let mut heights = Vec::new();
	for per_frame in [1, 2, 3, 4] {
		let (laid, feet) = walk(per_frame, 400 / per_frame as usize);
		let level = laid[0].1;
		assert!(
			laid.iter().all(|c| c.1 == level),
			"{per_frame} ticks/frame: the road is a staircase, not a road: {:?}",
			laid.iter().map(|c| c.1).collect::<Vec<_>>()
		);
		// The caster is still standing on it, and it went somewhere: a
		// road laid in one column is a caster who never walked, which
		// every rate would pass. Half a block, because the failure this
		// guards against is a WHOLE one — a road that stepped down while
		// the caster kept up with it would pass a levelness check on the
		// planks alone only by leaving them all behind.
		assert!(
			(feet - (level as f32 + 1.0)).abs() < 0.5,
			"{per_frame} ticks/frame: the caster ended at {feet}, not on the road at {level}"
		);
		assert!(
			laid.iter().any(|c| (c.0, c.2) != (laid[0].0, laid[0].2)),
			"{per_frame} ticks/frame: every plank in one column"
		);
		heights.push(level);
	}
	assert!(
		heights.windows(2).all(|w| w[0] == w[1]),
		"the rates disagree about where the road stands: {heights:?}"
	);
}
