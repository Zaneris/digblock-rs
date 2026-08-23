//! Time-of-day atmosphere: light/ambient/sky colors sampled from a
//! keyframe table by sun elevation. Pure data, headless-testable; the
//! renderer feeds the sampled values straight into the frame uniforms.

/// Everything the shaders need that varies with time of day.
/// Colors are gamma-space RGB in [0, 1].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Atmosphere {
	/// Direct light color: warm sun by day, dim blue moonlight at night.
	pub light_color: [f32; 3],
	/// Sky-tinted ambient floor (what fully shadowed faces receive).
	pub ambient: [f32; 3],
	/// Sky color straight up.
	pub zenith: [f32; 3],
	/// Sky color at the horizon; doubles as the fog color.
	pub horizon: [f32; 3],
	/// 0 by day → 1 in deep night.
	pub star_intensity: f32,
	/// Sunrise/sunset horizon glow, peaks with the sun at the horizon.
	pub glow_strength: f32,
}

/// The brightest direct light the frame ever carries. Nothing tonemaps
/// downstream, so at this value ambient + direct on a fully lit top face
/// lands just above unity — about where the old flat lighting peaked —
/// and pushing past it blows the block textures out to neon.
pub const LIGHT_CEILING: f32 = 0.68;

/// Peak sunlight as a share of [`LIGHT_CEILING`]. Noon is tuned to sit
/// exactly on it, so this is a ceiling rather than a default.
pub const SUN_GAIN: f32 = 1.0;

/// Peak moonlight as a share of peak SUNLIGHT, so dimming the day dims
/// the night with it and the contrast that makes a night read as one
/// survives a retune. The caster lane fades it again by
/// [`crate::sun::MOON_STRENGTH`] before it reaches a face; this is the
/// colour half, which the probe field and the sun-DDA gates read alone.
pub const MOON_GAIN: f32 = 0.397;

const SUN_PEAK: f32 = LIGHT_CEILING * SUN_GAIN;
const MOON_PEAK: f32 = SUN_PEAK * MOON_GAIN;

/// A key's direct light, split so brightness and colour tune apart:
/// `level` is its share of the body's peak and `tint` its hue, brightest
/// channel at 1. A tint channel past 1 hands the brightness back to the
/// colour — `no_key_outshines_its_body` is what catches that.
const fn sunlight(level: f32, tint: [f32; 3]) -> [f32; 3] {
	scaled(SUN_PEAK * level, tint)
}

/// [`sunlight`]'s opposite number, for the keys below the horizon.
const fn moonlight(level: f32, tint: [f32; 3]) -> [f32; 3] {
	scaled(MOON_PEAK * level, tint)
}

const fn scaled(k: f32, tint: [f32; 3]) -> [f32; 3] {
	[k * tint[0], k * tint[1], k * tint[2]]
}

/// Keyframes over `e` = sin of sun altitude (+1 noon, 0 horizon,
/// −1 midnight), descending. Keys at `e >= 0` are lit by the sun and the
/// rest by the moon, the same split `Sun::caster` makes of the same `e`.
#[rustfmt::skip]
const KEYS: &[(f32, Atmosphere)] = &[
	(1.00, Atmosphere { // noon
		light_color: sunlight(1.00, [1.00, 0.97, 0.90]),
		ambient:     [0.48, 0.50, 0.55],
		zenith:      [0.20, 0.45, 0.85],
		horizon:     [0.62, 0.78, 0.92],
		star_intensity: 0.0, glow_strength: 0.0,
	}),
	(0.35, Atmosphere { // mid-morning / afternoon
		light_color: sunlight(1.00, [1.00, 0.90, 0.74]),
		ambient:     [0.45, 0.46, 0.50],
		zenith:      [0.22, 0.42, 0.78],
		horizon:     [0.70, 0.75, 0.82],
		star_intensity: 0.0, glow_strength: 0.0,
	}),
	(0.10, Atmosphere { // golden hour
		light_color: sunlight(0.91, [1.00, 0.68, 0.39]),
		ambient:     [0.36, 0.33, 0.36],
		zenith:      [0.22, 0.30, 0.55],
		horizon:     [0.95, 0.60, 0.35],
		star_intensity: 0.0, glow_strength: 0.7,
	}),
	(0.00, Atmosphere { // sun at the horizon
		light_color: sunlight(0.65, [1.00, 0.52, 0.30]),
		ambient:     [0.26, 0.24, 0.30],
		zenith:      [0.10, 0.14, 0.32],
		horizon:     [0.80, 0.42, 0.28],
		star_intensity: 0.0, glow_strength: 1.0,
	}),
	(-0.12, Atmosphere { // dusk
		light_color: moonlight(0.90, [0.56, 0.67, 1.00]),
		ambient:     [0.12, 0.13, 0.18],
		zenith:      [0.03, 0.04, 0.10],
		horizon:     [0.10, 0.10, 0.18],
		star_intensity: 0.45, glow_strength: 0.3,
	}),
	(-0.35, Atmosphere { // night
		light_color: moonlight(1.00, [0.60, 0.70, 1.00]),
		ambient:     [0.08, 0.09, 0.13],
		zenith:      [0.010, 0.015, 0.040],
		horizon:     [0.04, 0.05, 0.09],
		star_intensity: 1.0, glow_strength: 0.0,
	}),
];

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
	[
		a[0] + (b[0] - a[0]) * t,
		a[1] + (b[1] - a[1]) * t,
		a[2] + (b[2] - a[2]) * t,
	]
}

impl Atmosphere {
	/// Sample the table at sun sin-elevation `e`, piecewise-linear;
	/// clamps beyond the first/last key.
	pub fn sample(e: f32) -> Self {
		let first = &KEYS[0];
		if e >= first.0 {
			return first.1;
		}
		for pair in KEYS.windows(2) {
			let (ea, a) = pair[0];
			let (eb, b) = pair[1];
			if e >= eb {
				let t = (ea - e) / (ea - eb);
				return Self {
					light_color: lerp3(a.light_color, b.light_color, t),
					ambient: lerp3(a.ambient, b.ambient, t),
					zenith: lerp3(a.zenith, b.zenith, t),
					horizon: lerp3(a.horizon, b.horizon, t),
					star_intensity: a.star_intensity + (b.star_intensity - a.star_intensity) * t,
					glow_strength: a.glow_strength + (b.glow_strength - a.glow_strength) * t,
				};
			}
		}
		KEYS[KEYS.len() - 1].1
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn key_points_return_key_values() {
		// Lerp at t = 1.0 is not bit-exact in f32; compare with tolerance.
		for &(e, ref key) in KEYS {
			let s = Atmosphere::sample(e);
			let pairs = [
				(s.light_color, key.light_color),
				(s.ambient, key.ambient),
				(s.zenith, key.zenith),
				(s.horizon, key.horizon),
				(
					[s.star_intensity, s.glow_strength, 0.0],
					[key.star_intensity, key.glow_strength, 0.0],
				),
			];
			for (got, want) in pairs {
				for (g, w) in got.iter().zip(&want) {
					assert!((g - w).abs() < 1e-5, "at e = {e}: {got:?} vs {want:?}");
				}
			}
		}
	}

	#[test]
	fn samples_stay_in_unit_range() {
		let mut e = -1.0;
		while e <= 1.0 {
			let a = Atmosphere::sample(e);
			for c in a
				.light_color
				.iter()
				.chain(&a.ambient)
				.chain(&a.zenith)
				.chain(&a.horizon)
				.chain([a.star_intensity, a.glow_strength].iter())
			{
				assert!((0.0..=1.0).contains(c), "out of range at e = {e}: {c}");
			}
			e += 0.01;
		}
	}

	/// The peak is a ceiling: `level` only means brightness while no tint
	/// carries any of its own. A tint channel past 1 or a level past 1 is
	/// the way that breaks, and both land here.
	#[test]
	fn no_key_outshines_its_body() {
		let mut seen = 0;
		for &(e, ref key) in KEYS {
			let peak = if e >= 0.0 { SUN_PEAK } else { MOON_PEAK };
			for c in key.light_color {
				assert!(c <= peak + 1e-6, "light {c} past peak {peak} at e = {e}");
			}
			seen += 1;
		}
		assert_eq!(seen, KEYS.len());
	}

	/// A night's whole character is its distance from the day, so the two
	/// gains have to keep composing: full moonlight stays MOON_GAIN of
	/// full sunlight however either one is retuned.
	#[test]
	fn full_moonlight_is_moon_gain_of_full_sunlight() {
		let noon = peak(Atmosphere::sample(1.0).light_color);
		let night = peak(Atmosphere::sample(-1.0).light_color);
		assert!((night / noon - MOON_GAIN).abs() < 1e-5, "{night} / {noon}");
	}

	/// Daylight only ever dims toward the horizon — the shape the levels
	/// author, and one a tint edit could otherwise invert.
	#[test]
	fn daylight_dims_toward_the_horizon() {
		let mut prev = f32::INFINITY;
		let mut seen = 0;
		for &(e, ref key) in KEYS {
			if e < 0.0 {
				continue;
			}
			let p = peak(key.light_color);
			assert!(
				p <= prev + 1e-6,
				"sun key at e = {e} brightens: {p} > {prev}"
			);
			prev = p;
			seen += 1;
		}
		assert!(seen >= 3, "only {seen} sun keys swept");
	}

	fn peak(c: [f32; 3]) -> f32 {
		c[0].max(c[1]).max(c[2])
	}

	#[test]
	fn night_is_darker_than_day() {
		let noon = Atmosphere::sample(1.0);
		let night = Atmosphere::sample(-1.0);
		for i in 0..3 {
			assert!(night.ambient[i] < noon.ambient[i]);
		}
		assert!(night.star_intensity > 0.9);
		assert!(noon.star_intensity == 0.0);
	}
}
