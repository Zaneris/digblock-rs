//! Caves: the intersection of two independent 3D-noise zero bands. One
//! band alone carves space-filling sheets; requiring both fields near
//! zero reduces the void to worm-like tunnel curves (and the carved
//! volume roughly quadratically). Land columns only (no ocean-floor
//! caves → no water walls at carved openings), never the bedrock course
//! at the world floor. Frequency sets tunnel spacing (zero bands sit ~half a
//! wavelength apart); threshold/frequency sets bore width in blocks — so
//! lower frequency + higher threshold = fewer, larger caves.

use crate::simplex::{Noise3Column, noise3};

const CAVE_FREQ: f64 = 0.02;
const CAVE_THRESHOLD: f64 = 0.15;
/// Karst anisotropy: the gallery band's y-frequency is compressed against its horizontal one, so
/// its zero surfaces extend in x/z and stack in y — galleries prefer near-horizontal levels
/// connected where the worm band wanders, like real cave systems following bedding planes. Its
/// threshold is raised to give the vertically-thinner galleries back their headroom.
const CAVE_FREQ_Y: f64 = 0.045;
const CAVE_THRESHOLD_1: f64 = 0.18;
const CAVE_SEED_OFF: f64 = 16193.0;
const CAVE_SEED_OFF2: f64 = 27449.0;
pub(super) const CAVE_FLOOR: i32 = 1;

/// One band's vertical walk of a single column: the lattice-caching
/// noise walker plus the band's domain offset, hoisted out of the y loop.
struct Band {
	n: Noise3Column,
	o: f64,
}

impl Band {
	fn new(x: i32, z: i32, seed: f64, off: f64) -> Self {
		let o = seed + off;
		Self {
			n: Noise3Column::new(o + f64::from(x) * CAVE_FREQ, o + f64::from(z) * CAVE_FREQ),
			o,
		}
	}

	#[inline]
	fn at(&mut self, y: i32, freq_y: f64) -> f64 {
		self.n.at(self.o + f64::from(y) * freq_y)
	}
}

/// The cave field of one column, walked in y. Column-constant work (the
/// x/z domain offsets) and lattice-cell-constant work (corner gradients,
/// every x/z term of the corner sum) are computed once per run instead of
/// once per cell. Every bit it reports is bit-identical to the free
/// `carved` below, at any y, in any order.
pub(super) struct CaveColumn {
	gallery: Band,
	worm: Band,
}

impl CaveColumn {
	pub(super) fn new(x: i32, z: i32, seed: f64) -> Self {
		Self {
			gallery: Band::new(x, z, seed, CAVE_SEED_OFF),
			worm: Band::new(x, z, seed, CAVE_SEED_OFF2),
		}
	}

	/// Carve bits of the inclusive run `lo..=hi` (no longer than the returned bitmask), bit `n` =
	/// `lo + n`. The anisotropic gallery band goes first (cheapest reject) and the isotropic one
	/// only where it passed, so most cells still cost one noise evaluation — but sweeping the
	/// gallery across the whole run first leaves those evaluations independent, where a per-cell
	/// short-circuit puts a coin-flip branch between them that the predictor cannot learn.
	pub(super) fn carve_run(&mut self, lo: i32, hi: i32) -> u32 {
		debug_assert!(hi - lo < 32, "run fits the mask");
		let mut gallery = 0u32;
		for (n, y) in (lo..=hi).enumerate() {
			let open = self.gallery.at(y, CAVE_FREQ_Y).abs() < CAVE_THRESHOLD_1;
			gallery |= u32::from(open) << n;
		}
		let mut out = 0u32;
		while gallery != 0 {
			let n = gallery.trailing_zeros();
			gallery &= gallery - 1;
			let open = self.worm.at(lo + n as i32, CAVE_FREQ).abs() < CAVE_THRESHOLD;
			out |= u32::from(open) << n;
		}
		out
	}
}

/// Whether the cave field carves this cell (pure; decoration needs it to
/// judge "surface not carved" without building the chunk). One-shot
/// probes skip `CaveColumn` — priming its per-cell cache costs more than
/// the two evaluations it would save.
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
	band(CAVE_SEED_OFF, CAVE_FREQ_Y, CAVE_THRESHOLD_1)
		&& band(CAVE_SEED_OFF2, CAVE_FREQ, CAVE_THRESHOLD)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The pre-optimization carve: the two-band test over the transcribed
	/// reference noise. A mismatch against this is a worldgen output
	/// change, which stales every server volume and client save.
	fn reference_carved(x: i32, y: i32, z: i32, seed: f64) -> bool {
		let band = |off: f64, freq_y: f64, threshold: f64| {
			let o = seed + off;
			let n = crate::simplex::reference::noise3(
				o + f64::from(x) * CAVE_FREQ,
				o + f64::from(y) * freq_y,
				o + f64::from(z) * CAVE_FREQ,
			);
			n.abs() < threshold
		};
		band(CAVE_SEED_OFF, CAVE_FREQ_Y, CAVE_THRESHOLD_1)
			&& band(CAVE_SEED_OFF2, CAVE_FREQ, CAVE_THRESHOLD)
	}

	/// All three carve paths — the one-shot probe, the column walk, and
	/// the run mask the chunk fill uses — agree with the reference on
	/// every cell of a spread of columns across several seeds, and the
	/// caves are neither empty nor everywhere.
	#[test]
	fn carve_paths_match_the_reference() {
		let mut carved_cells = 0u32;
		let mut total = 0u32;
		for seed in [42.0f64, 1234.5678, -7.25, 99991.0] {
			for &(x, z) in &[(0, 0), (13, -29), (-401, 517), (4096, -4096), (7, 7)] {
				let mut col = CaveColumn::new(x, z, seed);
				for lo in (1..=240).step_by(16) {
					let hi = lo + 15;
					let run = col.carve_run(lo, hi);
					let mut probe = CaveColumn::new(x, z, seed);
					for (n, y) in (lo..=hi).enumerate() {
						let want = reference_carved(x, y, z, seed);
						assert_eq!(carved(x, y, z, seed), want, "one-shot at ({x},{y},{z})");
						let step = probe.carve_run(y, y) & 1 == 1;
						assert_eq!(step, want, "one-cell run at ({x},{y},{z})");
						assert_eq!(run >> n & 1 == 1, want, "run mask at ({x},{y},{z})");
						carved_cells += u32::from(want);
						total += 1;
					}
				}
			}
		}
		assert!(
			carved_cells > 0 && carved_cells < total,
			"{carved_cells}/{total}"
		);
	}

	/// The run mask is independent of how the column is walked: a fresh
	/// column, a reused one, and one primed by out-of-order probes all
	/// produce the same bits.
	#[test]
	fn run_mask_ignores_walk_order() {
		let seed = 1234.5678;
		for &(x, z) in &[(0, 0), (-77, 31)] {
			let mut fresh = CaveColumn::new(x, z, seed);
			let mut jumbled = CaveColumn::new(x, z, seed);
			for y in [200, 3, 199, 61, 4] {
				jumbled.carve_run(y, y);
			}
			for lo in (1..=240).step_by(16) {
				let a = CaveColumn::new(x, z, seed).carve_run(lo, lo + 15);
				assert_eq!(fresh.carve_run(lo, lo + 15), a, "reused column at {lo}");
				assert_eq!(jumbled.carve_run(lo, lo + 15), a, "primed column at {lo}");
			}
		}
	}
}
