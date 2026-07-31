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

/// Keyframes over `e` = sin of sun altitude (+1 noon, 0 horizon,
/// −1 midnight), descending. Noon is tuned so a fully lit top face gets
/// ambient + direct ≈ 1.15 — close to the old flat-lit 1.2 peak;
/// pushing the sum much past that blows the block textures out to neon.
#[rustfmt::skip]
const KEYS: &[(f32, Atmosphere)] = &[
	(1.00, Atmosphere { // noon
		light_color: [0.68, 0.66, 0.61],
		ambient:     [0.48, 0.50, 0.55],
		zenith:      [0.20, 0.45, 0.85],
		horizon:     [0.62, 0.78, 0.92],
		star_intensity: 0.0, glow_strength: 0.0,
	}),
	(0.35, Atmosphere { // mid-morning / afternoon
		light_color: [0.68, 0.61, 0.50],
		ambient:     [0.45, 0.46, 0.50],
		zenith:      [0.22, 0.42, 0.78],
		horizon:     [0.70, 0.75, 0.82],
		star_intensity: 0.0, glow_strength: 0.0,
	}),
	(0.10, Atmosphere { // golden hour
		light_color: [0.62, 0.42, 0.24],
		ambient:     [0.36, 0.33, 0.36],
		zenith:      [0.22, 0.30, 0.55],
		horizon:     [0.95, 0.60, 0.35],
		star_intensity: 0.0, glow_strength: 0.7,
	}),
	(0.00, Atmosphere { // sun at the horizon
		light_color: [0.44, 0.23, 0.13],
		ambient:     [0.26, 0.24, 0.30],
		zenith:      [0.10, 0.14, 0.32],
		horizon:     [0.80, 0.42, 0.28],
		star_intensity: 0.0, glow_strength: 1.0,
	}),
	(-0.12, Atmosphere { // dusk
		light_color: [0.10, 0.12, 0.18],
		ambient:     [0.12, 0.13, 0.18],
		zenith:      [0.03, 0.04, 0.10],
		horizon:     [0.10, 0.10, 0.18],
		star_intensity: 0.45, glow_strength: 0.3,
	}),
	(-0.35, Atmosphere { // night
		light_color: [0.12, 0.14, 0.20],
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
