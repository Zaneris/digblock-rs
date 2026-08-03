//! Inland lakes: sparse footprints from a thresholded low-frequency
//! field (warped coords, so shores wander), restricted to flat eroded
//! inland regions. The bowl is carved by min() — terrain is never raised
//! — below a locally flat, coarsely quantized water level derived from
//! the detail-free surface, so the whole river machinery (`holds_water`,
//! falls, source fill, `build_column` bounds) applies unchanged. Where
//! the footprint is active the river carve is suppressed entirely: lake
//! beats river.

use super::terrain::{smoothstep, soft_ceiling, TerrainParams, SEA_LEVEL};
use crate::simplex::fbm2;

pub struct LakeParams {
	pub freq: f64,
	pub seed_off: f64,
	/// Footprint threshold: field values above this are lake. High, so
	/// footprints stay sparse (2-octave fBm rarely exceeds ~0.55).
	pub thresh: f64,
	/// Field-units ramp from the shoreline to full depth.
	pub shore: f64,
	/// Max bed depth below the water level (blocks).
	pub depth: f64,
	/// Water-level quantum — coarse, so a level step inside one footprint
	/// (a terraced twin-lake with a fall line) stays rare.
	pub step: f64,
	/// The level sits this far below the smooth reference surface, so
	/// natural terrain around the footprint stands above the water.
	pub margin: f64,
}

/// The lake's effect on a column, or None where the footprint is
/// inactive: (final height, water surface). Same contract as
/// `rivers::carve`, but optional so the caller knows when to let rivers
/// run instead.
#[allow(clippy::too_many_arguments)]
pub(super) fn carve(
	p: &TerrainParams,
	wx: f64,
	wz: f64,
	seed: f64,
	base: f64,
	rough: f64,
	erosion: f64,
	terrace_w: f64,
	h_ridge: f64,
	h_pre: f64,
) -> Option<(f64, f64)> {
	let lp = &p.lake;
	let sea = f64::from(SEA_LEVEL);
	// Region gate: inland, genuinely planed flat (the steppe erosion
	// band, where detail relief is fractional), non-mountainous, and NOT
	// terraced — a footprint edge landing on a tread riser would perch
	// water against an un-covered multi-block step. Also the depth scale
	// — lakes fade to dry depressions at the edges.
	let g = smoothstep((base - sea - 4.0) / 10.0)
		* smoothstep((erosion - 0.6) / 0.25)
		* (1.0 - smoothstep((rough - 0.1) / 0.2))
		* (1.0 - smoothstep((terrace_w - 0.02) / 0.08));
	if g <= 0.0 {
		return None;
	}
	let lv = fbm2(wx, wz, lp.freq, 2, seed + lp.seed_off);
	if lv <= lp.thresh {
		return None;
	}
	// River exclusion with a buffer: inside twice the river's channel
	// half-width the lake stands aside entirely, so a channel crossing
	// the footprint keeps its natural banks and lake water never abuts
	// a river trench (the wall the containment test hunts).
	let rv = fbm2(wx, wz, p.river.freq, 2, seed + p.river.seed_off);
	if rv.abs() < p.river.w * (1.0 - 0.6 * rough) * (1.0 + 0.5 * erosion) * 2.0 {
		return None;
	}
	// 0 at the shoreline → 1 in the interior.
	let t = smoothstep((lv - lp.thresh) / lp.shore);
	// Locally flat level from the detail-free surface, quantized coarse.
	let ws_smooth = soft_ceiling(h_ridge) - lp.margin - sea;
	let ws = sea + ((ws_smooth / lp.step).floor() * lp.step).max(0.0);
	// Bowl by min-carve: a dry rim ring at ws+1 near the shore, a bed up
	// to `depth` below the level in the interior. Water fills only where
	// the bowl actually bites below natural terrain that stood at or
	// above the level — never over lower ground at a region-gate fade —
	// so shoreline exposure stays within a bank's height. Non-bowl
	// footprint columns keep their terrain but still suppress rivers.
	let target = ws + 1.0 - (1.0 + lp.depth * t) * g;
	if target >= h_pre || h_pre + 1.0 < ws {
		return Some((h_pre.max(1.0), sea));
	}
	let h = target.max(1.0);
	let water = if ws > h { ws } else { sea };
	Some((h, water))
}
