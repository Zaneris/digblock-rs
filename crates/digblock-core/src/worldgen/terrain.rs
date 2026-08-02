//! The continuous per-column terrain fields: two low-frequency control
//! fields shape the world into coherent regions (see the crate docs on
//! worldgen). Everything is a pure function of `(x, z, seed)` — and of
//! the [`TerrainParams`] knob surface, so a future "world type" can vary
//! the tuning without touching the math.

use super::rivers::{self, RiverParams};
use crate::simplex::fbm2;

/// Sea sits at mid-world: oceans below, up to ~112 blocks of relief above.
pub const SEA_LEVEL: i32 = 128;
/// Asymptotic terrain ceiling: heights above CEILING_KNEE are compressed
/// toward this limit but never reach it, so the tallest peaks round off
/// instead of clamping into flat tops.
pub const MAX_TERRAIN: f64 = 240.0;
/// Where the soft ceiling starts to bite.
pub(super) const CEILING_KNEE: f64 = 200.0;

/// The terrain tuning surface, gathered so it can vary per world later.
/// Frequencies are per-block; amplitudes in blocks. Field seed offsets
/// are domain offsets so the fields decorrelate. `DEFAULT` carries the
/// exact literals the world has always used — construction from the same
/// values keeps every f64 operation bit-identical.
pub struct TerrainParams {
	pub continent_freq: f64,
	pub rough_freq: f64,
	pub detail_freq: f64,
	/// Mountain forms live on their own broad field — far lower frequency
	/// than the detail fBm, so 90 blocks of amplitude spreads over
	/// hundreds of blocks of run instead of spiking.
	pub ridge_freq: f64,
	pub plains_amp: f64,
	/// Extra ridged amplitude in fully mountainous regions.
	pub mountain_amp: f64,
	pub rough_seed_off: f64,
	pub detail_seed_off: f64,
	pub ridge_seed_off: f64,
	/// Domain warp: the continent and ridge fields sample warped
	/// coordinates so coastlines and ridgelines wander and finger instead
	/// of tracing the smooth blobs plain fBm produces. Amplitude is ~25%
	/// of the continent wavelength.
	pub warp_freq: f64,
	pub warp_amp: f64,
	pub warp_x_off: f64,
	pub warp_z_off: f64,
	/// Erosion: where high, relief is planed down — wide flat valleys and
	/// plateaus appear even inland.
	pub erosion_freq: f64,
	pub erosion_seed_off: f64,
	/// Mountain-range orientation: the ridge domain is rotated by a slowly
	/// varying angle and stretched along the rotated axis, so ranges form
	/// oriented belts instead of isotropic clusters.
	pub orient_freq: f64,
	pub orient_seed_off: f64,
	/// Along-range frequency multiplier: ~3x longer than wide.
	pub ridge_stretch: f64,
	pub river: RiverParams,
	/// Base elevation by continentalness: piecewise-linear anchors from
	/// deep ocean floor through a continental slope and a shallow coastal
	/// shelf to highland base. Monotonic in `c`.
	pub base_spline: [(f64, f64); 7],
}

impl TerrainParams {
	pub const DEFAULT: TerrainParams = TerrainParams {
		continent_freq: 0.0015,
		rough_freq: 0.004,
		detail_freq: 0.015,
		ridge_freq: 0.004,
		plains_amp: 6.0,
		mountain_amp: 90.0,
		rough_seed_off: 4079.0,
		detail_seed_off: 8161.0,
		ridge_seed_off: 12281.0,
		warp_freq: 0.0008,
		warp_amp: 160.0,
		warp_x_off: 31337.0,
		warp_z_off: 44777.0,
		erosion_freq: 0.0012,
		erosion_seed_off: 52561.0,
		orient_freq: 0.0004,
		orient_seed_off: 61553.0,
		ridge_stretch: 0.35,
		river: RiverParams {
			freq: 0.0009,
			seed_off: 83777.0,
			w: 0.045,
			depth: 5.0,
			pool_step: 4.0,
		},
		base_spline: [
			(-1.0, 60.0),
			(-0.55, 75.0),
			(-0.25, 105.0),
			(-0.10, 122.0),
			(0.0, 130.0),
			(0.3, 140.0),
			(1.0, 170.0),
		],
	};
}

pub(super) fn spline(p: &TerrainParams, c: f64) -> f64 {
	let first = p.base_spline[0];
	if c <= first.0 {
		return first.1;
	}
	for w in p.base_spline.windows(2) {
		let (x0, y0) = w[0];
		let (x1, y1) = w[1];
		if c <= x1 {
			return y0 + (y1 - y0) * (c - x0) / (x1 - x0);
		}
	}
	p.base_spline[p.base_spline.len() - 1].1
}

pub(super) fn smoothstep(t: f64) -> f64 {
	let t = t.clamp(0.0, 1.0);
	t * t * (3.0 - 2.0 * t)
}

/// Mountain gate: 0 at the coast, 1 well inland — amplitude fades to
/// plains level near shores so there are no cliffs at the beach.
fn inland(c: f64) -> f64 {
	smoothstep((c - 0.05) / 0.30)
}

/// Compress heights above CEILING_KNEE asymptotically toward MAX_TERRAIN:
/// tanh keeps the curve tangent to the identity at the knee and never
/// reaches the limit, so no peak ever clamps into a flat top.
pub(super) fn soft_ceiling(h: f64) -> f64 {
	if h <= CEILING_KNEE {
		h
	} else {
		let span = MAX_TERRAIN - CEILING_KNEE;
		CEILING_KNEE + span * ((h - CEILING_KNEE) / span).tanh()
	}
}

/// fBm concentrates near zero (octave sums rarely approach ±1); gain +
/// clamp spreads a field over its nominal [-1, 1] so spline anchors and
/// gates see their full input range.
pub(super) fn gained(v: f64, gain: f64) -> f64 {
	(v * gain).clamp(-1.0, 1.0)
}

/// The continuous per-column terrain fields, before block-grid
/// discretization and surface classification. Split from `classify` so
/// classification can sample neighbor heights (for slope) without
/// recursing: `fields`/`raw_height` never classify, `classify` never
/// evaluates noise beyond its own inputs.
#[derive(Clone, Copy, Default)]
pub(super) struct Fields {
	/// Surface height (f64, soft-ceilinged, ≥ 1; floor = the block y).
	pub(super) h: f64,
	/// Height before river carving — what slope classification reads, so
	/// river banks keep the surface of the terrain they cut through
	/// instead of registering as artificial cliffs.
	pub(super) h_pre: f64,
	/// Local water surface (f64): SEA_LEVEL, or the river level above it.
	pub(super) water: f64,
}

pub(super) fn fields(p: &TerrainParams, x: i32, z: i32, seed: f64) -> Fields {
	let (xf, zf) = (f64::from(x), f64::from(z));
	// Warped coordinates for the region-scale fields; roughness and
	// detail stay unwarped (their phase relative to the warped fields is
	// what varies, which is harmless — they are gates/relief, not shapes).
	let wx = xf + p.warp_amp * fbm2(xf, zf, p.warp_freq, 2, seed + p.warp_x_off);
	let wz = zf + p.warp_amp * fbm2(xf, zf, p.warp_freq, 2, seed + p.warp_z_off);
	// tanh instead of the hard gain-clamp: deep ocean and far inland keep
	// varying instead of saturating (a clamped c = −1 plateau rendered the
	// entire deep floor as a billiard table at the spline's first anchor).
	let c = (fbm2(wx, wz, p.continent_freq, 2, seed) * 2.2).tanh();
	let r = gained(fbm2(xf, zf, p.rough_freq, 2, seed + p.rough_seed_off), 2.2);
	let d = fbm2(xf, zf, p.detail_freq, 4, seed + p.detail_seed_off);
	// Ridge field on a rotated, stretched domain (continuous in theta, so
	// no seams — just a slow shear where the orientation drifts).
	let theta = std::f64::consts::PI * fbm2(xf, zf, p.orient_freq, 1, seed + p.orient_seed_off);
	let (st, ct) = theta.sin_cos();
	let (u, v) = (ct * wx + st * wz, -st * wx + ct * wz);
	let m = fbm2(
		u * p.ridge_stretch,
		v,
		p.ridge_freq,
		3,
		seed + p.ridge_seed_off,
	);

	let base = spline(p, c);
	// 0..1 mountainousness, gated to inland regions.
	let rough = smoothstep(((r + 1.0) * 0.5 - 0.55) / 0.3) * inland(c);
	// 0 = rugged, 1 = planed flat.
	let e = gained(
		fbm2(xf, zf, p.erosion_freq, 2, seed + p.erosion_seed_off),
		2.2,
	);
	let erosion = smoothstep((e + 0.3) / 0.9);
	// Ridged (|m|): ranges crease into valleys at zero crossings and only
	// ever rise above their base. The smoothstep flattens the derivative
	// at the valley end; the concave power remap pushes mid-values down —
	// mass settles into a broad base while the approach to a summit
	// steepens, so peaks read pointed instead of domed.
	let ridge = smoothstep(gained(m, 2.2).abs()).powf(1.6);
	let h_ridge = base + ridge * p.mountain_amp * rough * (1.0 - 0.75 * erosion);
	// Altitude-weighted detail (Musgrave-style heterogeneous fractal):
	// lowlands read calm, high ridges craggy, eroded regions calmer still.
	// The weighting is V-shaped around sea level — relief also returns
	// with depth, so the seafloor rolls instead of planing flat.
	let alt =
		((h_ridge - f64::from(SEA_LEVEL)) / (CEILING_KNEE - f64::from(SEA_LEVEL))).clamp(0.0, 1.0);
	let depth = ((f64::from(SEA_LEVEL) - h_ridge) / 40.0).clamp(0.0, 1.0);
	let detail_amp = p.plains_amp * (1.0 - 0.6 * erosion) * (0.5 + 2.5 * alt.max(0.6 * depth));
	let h_pre = soft_ceiling(h_ridge + gained(d, 2.0) * detail_amp).max(1.0);

	let (h, water) = rivers::carve(p, wx, wz, seed, base, rough, erosion, h_ridge, h_pre);
	Fields { h, h_pre, water }
}
