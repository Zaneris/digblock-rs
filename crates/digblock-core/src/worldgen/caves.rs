//! Caves: the intersection of two independent 3D-noise zero bands. One
//! band alone carves space-filling sheets; requiring both fields near
//! zero reduces the void to worm-like tunnel curves (and the carved
//! volume roughly quadratically). Land columns only (no ocean-floor
//! caves → no water walls at carved openings), never the bedrock layer
//! at y=0. Frequency sets tunnel spacing (zero bands sit ~half a
//! wavelength apart); threshold/frequency sets bore width in blocks — so
//! lower frequency + higher threshold = fewer, larger caves.

use crate::simplex::noise3;

const CAVE_FREQ: f64 = 0.02;
const CAVE_THRESHOLD: f64 = 0.15;
/// Karst anisotropy: band 1's y-frequency is compressed ~2.2x, so its
/// zero surfaces extend in x/z and stack in y — galleries prefer
/// near-horizontal levels connected where band 2 wanders, like real
/// cave systems following bedding planes. Its threshold is raised to
/// give the vertically-thinner galleries back their headroom.
const CAVE_FREQ_Y: f64 = 0.045;
const CAVE_THRESHOLD_1: f64 = 0.18;
const CAVE_SEED_OFF: f64 = 16193.0;
const CAVE_SEED_OFF2: f64 = 27449.0;
pub(super) const CAVE_FLOOR: i32 = 1;

/// Whether the cave field carves this cell (pure; decoration needs it to
/// judge "surface not carved" without building the chunk). Short-circuits
/// so most cells pay for one noise evaluation, not two.
pub fn carved(x: i32, y: i32, z: i32, seed: f64) -> bool {
	let band = |off: f64, freq_y: f64, threshold: f64| {
		let o = seed + off;
		let n = noise3(
			o + f64::from(x) * CAVE_FREQ,
			o + f64::from(y) * freq_y,
			o + f64::from(z) * CAVE_FREQ,
		);
		n.abs() < threshold
	};
	// Anisotropic gallery band first (cheapest reject), isotropic second.
	band(CAVE_SEED_OFF, CAVE_FREQ_Y, CAVE_THRESHOLD_1)
		&& band(CAVE_SEED_OFF2, CAVE_FREQ, CAVE_THRESHOLD)
}
