//! Rivers: the zero band of a low-frequency field (on warped coords, so
//! channels meander) carves a U-profile bed below a locally flat water
//! level derived from the low+mid-frequency surface. Channels fade out
//! in strongly mountainous terrain, widen across eroded flats, and taper
//! to nothing at the coast so they meet the sea at grade. Not true
//! downhill flow — the level can step a block along the run — but the
//! bands read as connected rivers that end in the ocean.

use super::terrain::{smoothstep, soft_ceiling, TerrainParams, SEA_LEVEL};
use crate::simplex::fbm2;

pub struct RiverParams {
	pub freq: f64,
	pub seed_off: f64,
	/// Half-width of the channel in field units.
	pub w: f64,
	/// Max carve depth below the water surface (blocks).
	pub depth: f64,
	/// River level quantum: the water surface snaps to this many blocks,
	/// so a descending river forms flat pools joined by discrete falls
	/// instead of a per-block staircase of water sheets hugging the slope.
	pub pool_step: f64,
}

/// Apply the river channel to a column: returns (final height, water
/// surface). Fades toward mountains, widens over eroded flats, tapers to
/// zero depth at the coast (base at sea level → g = 0).
#[allow(clippy::too_many_arguments)]
pub(super) fn carve(
	p: &TerrainParams,
	wx: f64,
	wz: f64,
	seed: f64,
	base: f64,
	rough: f64,
	erosion: f64,
	h_ridge: f64,
	h_pre: f64,
) -> (f64, f64) {
	let rp = &p.river;
	let mut h_final = h_pre;
	let mut water = f64::from(SEA_LEVEL);
	let g = smoothstep((base - f64::from(SEA_LEVEL)) / 12.0);
	if g > 0.0 {
		let rv = fbm2(wx, wz, rp.freq, 2, seed + rp.seed_off);
		let w = rp.w * (1.0 - 0.6 * rough) * (1.0 + 0.5 * erosion);
		if rv.abs() < w {
			// U-profile: deepest at the band center. The water surface
			// derives from the detail-free height (locally flat across
			// the channel), snapped to pool_step quanta along it.
			let t = smoothstep(1.0 - rv.abs() / w);
			let ws_smooth = soft_ceiling(h_ridge) - 2.0 - f64::from(SEA_LEVEL);
			let ws =
				f64::from(SEA_LEVEL) + ((ws_smooth / rp.pool_step).floor() * rp.pool_step).max(0.0);
			let bed = ws - 1.0 - rp.depth * t * g;
			if bed < h_pre {
				h_final = bed.max(1.0);
			}
			// Fill to the river level even where the terrain already sat
			// below the bed — no dry holes mid-channel.
			if ws > h_final {
				water = ws;
			}
		}
	}
	(h_final, water)
}
