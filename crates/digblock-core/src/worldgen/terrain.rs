//! The continuous per-column terrain fields: two low-frequency control
//! fields shape the world into coherent regions (see the crate docs on
//! worldgen). Everything is a pure function of `(x, z, seed)` — and of
//! the [`TerrainParams`] knob surface, so a future "world type" can vary
//! the tuning without touching the math.

use super::lakes::{self, LakeParams};
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
	/// Climate: a broad temperature field shifts the stone/snow lines and
	/// picks warm/cold surface kinds; a mid-scale vegetation field decides
	/// where forests concentrate. Both are classification inputs only —
	/// they never move terrain.
	pub temp_freq: f64,
	pub temp_seed_off: f64,
	pub veg_freq: f64,
	pub veg_seed_off: f64,
	/// Terracing: in strongly eroded mid-altitude regions gated by a
	/// sparse regional mask, the pre-detail height quantizes into treads
	/// and risers — stepped mesa country.
	pub terrace_freq: f64,
	pub terrace_seed_off: f64,
	/// Small-scale edge dither so tread edges wander (±2 blocks).
	pub terrace_edge_freq: f64,
	pub terrace_edge_off: f64,
	pub terrace_step: f64,
	pub river: RiverParams,
	pub lake: LakeParams,
	/// Base elevation by continentalness: piecewise-linear anchors from
	/// deep ocean floor through a continental slope and a shallow coastal
	/// shelf to highland base. Monotonic in `c`.
	pub base_spline: [(f64, f64); 7],
}

impl TerrainParams {
	// Region wavelengths are tuned so distinct terrain archetypes sit
	// farther apart than one draw distance (~640 blocks across): mountains
	// belong to ~740-block belts, eroded flats to ~2500-block provinces,
	// continents to ~1333-block masses — you walk between regions instead
	// of seeing them all in one frame. Local character (detail, ridge
	// forms, channel width) keeps its original scale.
	pub const DEFAULT: TerrainParams = TerrainParams {
		continent_freq: 0.00075,
		rough_freq: 0.00135,
		detail_freq: 0.015,
		ridge_freq: 0.004,
		plains_amp: 6.0,
		mountain_amp: 90.0,
		rough_seed_off: 4079.0,
		detail_seed_off: 8161.0,
		ridge_seed_off: 12281.0,
		warp_freq: 0.0004,
		warp_amp: 320.0,
		warp_x_off: 31337.0,
		warp_z_off: 44777.0,
		erosion_freq: 0.0004,
		erosion_seed_off: 52561.0,
		orient_freq: 0.0002,
		orient_seed_off: 61553.0,
		ridge_stretch: 0.35,
		temp_freq: 0.0005,
		temp_seed_off: 97001.0,
		veg_freq: 0.0025,
		veg_seed_off: 99991.0,
		terrace_freq: 0.0007,
		terrace_seed_off: 97501.0,
		terrace_edge_freq: 0.03,
		terrace_edge_off: 103913.0,
		terrace_step: 6.0,
		river: RiverParams {
			freq: 0.0006,
			seed_off: 83777.0,
			// Physical channel width scales with w/freq: w shrinks with
			// freq so wider spacing doesn't also widen every river.
			w: 0.030,
			depth: 5.0,
			pool_step: 4.0,
		},
		lake: LakeParams {
			freq: 0.0011,
			seed_off: 92173.0,
			thresh: 0.42,
			shore: 0.10,
			depth: 4.5,
			step: 8.0,
			margin: 3.0,
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
	/// Regional temperature, [-1, 1]: shifts the stone/snow lines and
	/// selects warm/cold surface kinds in classification.
	pub(super) temp: f64,
	/// Vegetation/lushness, [-1, 1]: where forests (and marshes) gather.
	pub(super) veg: f64,
	/// This column's standing water (if any) is a lake, not a river —
	/// classification tells the two beds apart by this flag.
	pub(super) lake: bool,
}

/// The terrace tread transform: heights snap toward flat treads of
/// `step` blocks joined by short risers (70% tread, 30% riser).
/// Continuous and non-decreasing in `h`; output stays within
/// `step + |edge|` of the input.
pub(super) fn tread(step: f64, h: f64, edge: f64) -> f64 {
	let k = (h + edge) / step;
	(k.floor() + smoothstep((k.fract() - 0.7) / 0.3)) * step
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
	// Terracing: strongly eroded mid-altitude country, gated by a sparse
	// regional mask, quantizes toward treads. `tw` blends smoothly to 0
	// at every gate edge, so terraced country never pops in.
	let tz = smoothstep((erosion - 0.45) / 0.3)
		* smoothstep((h_ridge - 138.0) / 8.0)
		* (1.0 - smoothstep((h_ridge - 195.0) / 12.0));
	let mut tw = 0.0;
	let mut h_t = h_ridge;
	if tz > 0.0 {
		let tm =
			smoothstep((fbm2(xf, zf, p.terrace_freq, 2, seed + p.terrace_seed_off) - 0.1) / 0.3);
		tw = tz * tm;
		if tw > 0.0 {
			let edge = fbm2(xf, zf, p.terrace_edge_freq, 2, seed + p.terrace_edge_off);
			h_t += tw * (tread(p.terrace_step, h_ridge, edge * 2.0) - h_ridge);
		}
	}
	// Altitude-weighted detail (Musgrave-style heterogeneous fractal):
	// lowlands read calm, high ridges craggy, eroded regions calmer still.
	// The weighting is V-shaped around sea level — relief also returns
	// with depth, so the seafloor rolls instead of planing flat.
	let alt =
		((h_t - f64::from(SEA_LEVEL)) / (CEILING_KNEE - f64::from(SEA_LEVEL))).clamp(0.0, 1.0);
	let depth = ((f64::from(SEA_LEVEL) - h_t) / 40.0).clamp(0.0, 1.0);
	let v = alt.max(0.6 * depth);
	let base_amp = p.plains_amp * (1.0 - 0.6 * erosion) * (0.5 + 2.5 * v);
	// Steppe gate: strongly eroded, non-mountainous, near-base-altitude
	// columns collapse toward a 0.25-block detail floor — genuinely flat
	// plains instead of the ~1.2-block minimum the product above allows.
	// Any factor at 0 leaves base_amp untouched, so mountains, plateaus,
	// and the seafloor keep their relief.
	let flat = smoothstep((erosion - 0.55) / 0.35)
		* (1.0 - smoothstep(rough / 0.20))
		* (1.0 - smoothstep((v - 0.08) / 0.22));
	// Treads also damp detail, so mesa tops read planed.
	let detail_amp = (base_amp + (0.25 - base_amp) * flat) * (1.0 - 0.7 * tw);
	let h_pre = soft_ceiling(h_t + gained(d, 2.0) * detail_amp).max(1.0);

	let temp = gained(fbm2(xf, zf, p.temp_freq, 2, seed + p.temp_seed_off), 2.2);
	let veg = gained(fbm2(xf, zf, p.veg_freq, 2, seed + p.veg_seed_off), 2.0);

	// Lake beats river: where the lake footprint is active, the river
	// carve is suppressed entirely so the two never fight over a column.
	let (h, water, lake) = match lakes::carve(p, wx, wz, seed, base, rough, erosion, tw, h_t, h_pre)
	{
		Some((h, w)) => (h, w, w > h),
		None => {
			let (h, w) = rivers::carve(p, wx, wz, seed, base, rough, erosion, h_t, h_pre);
			(h, w, false)
		}
	};
	Fields {
		h,
		h_pre,
		water,
		temp,
		veg,
		lake,
	}
}
