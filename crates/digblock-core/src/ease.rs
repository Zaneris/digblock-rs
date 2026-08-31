//! Easing curves and frame-rate-independent damping — the shared
//! vocabulary for everything presentational that moves. Curves are pure
//! `f32 -> f32` on 0..=1 with EXACT endpoints (`f(0) == 0.0`,
//! `f(1) == 1.0`, bitwise): several consumers compare positions with
//! `==` to detect rest (GI history, the carried-light list), so a curve
//! that lands on 0.999... would keep them awake forever. [`Curve`] names
//! each curve as DATA, so tween state stays a plain struct with no
//! closures in it. [`exp_decay`] standardizes the `(-dt / tau).exp()`
//! damping the client used to hand-roll per site — it never quite
//! reaches its target, so call sites snap below an epsilon exactly as
//! they always have.

use glam::Vec3;

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
	a + (b - a) * t
}

/// Map `x` from `[in_a, in_b]` onto `[out_a, out_b]`, unclamped.
pub fn remap(x: f32, in_a: f32, in_b: f32, out_a: f32, out_b: f32) -> f32 {
	lerp(out_a, out_b, (x - in_a) / (in_b - in_a))
}

/// Clamped hermite on 0..=1 (the worldgen `smoothstep` shape, shared).
pub fn smoothstep01(t: f32) -> f32 {
	let t = t.clamp(0.0, 1.0);
	t * t * (3.0 - 2.0 * t)
}

pub fn in_quad(t: f32) -> f32 {
	t * t
}

pub fn out_quad(t: f32) -> f32 {
	1.0 - (1.0 - t) * (1.0 - t)
}

pub fn in_out_quad(t: f32) -> f32 {
	if t < 0.5 {
		2.0 * t * t
	} else {
		1.0 - 2.0 * (1.0 - t) * (1.0 - t)
	}
}

pub fn in_cubic(t: f32) -> f32 {
	t * t * t
}

pub fn out_cubic(t: f32) -> f32 {
	let u = 1.0 - t;
	1.0 - u * u * u
}

pub fn in_out_cubic(t: f32) -> f32 {
	if t < 0.5 {
		4.0 * t * t * t
	} else {
		let u = 1.0 - t;
		1.0 - 4.0 * u * u * u
	}
}

/// Overshoot factor of [`out_back`] — the classic value.
const BACK: f32 = 1.70158;

/// Eases out past 1.0 and settles back — the "pop" curve. Endpoint
/// guards, because the polynomial's f32 rounding does not owe us an
/// exact 0.0 at the start.
pub fn out_back(t: f32) -> f32 {
	if t <= 0.0 {
		return 0.0;
	}
	if t >= 1.0 {
		return 1.0;
	}
	let u = t - 1.0;
	1.0 + u * u * ((BACK + 1.0) * u + BACK)
}

/// One overshoot wobble, fast settle — elastic without the ringing.
pub fn out_elastic(t: f32) -> f32 {
	if t <= 0.0 {
		return 0.0;
	}
	if t >= 1.0 {
		return 1.0;
	}
	let u = 1.0 - t;
	1.0 - u * u * (std::f32::consts::TAU * 1.25 * t).cos()
}

/// Frame-rate-independent damping toward `target`: the same distance is
/// covered whether a span arrives as one `dt` or many. Never REACHES the
/// target — snap below an epsilon at the call site, which is what keeps
/// exact-rest consumers (GI history, light lists) asleep.
pub fn exp_decay(current: f32, target: f32, rate: f32, dt: f32) -> f32 {
	current + (target - current) * (1.0 - (-rate * dt).exp())
}

pub fn exp_decay3(current: Vec3, target: Vec3, rate: f32, dt: f32) -> Vec3 {
	current + (target - current) * (1.0 - (-rate * dt).exp())
}

/// A curve as data, so a tween or a step table can carry one without a
/// closure. `apply` clamps first: every curve answers on 0..=1.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Curve {
	#[default]
	Linear,
	Smooth,
	InQuad,
	OutQuad,
	InOutQuad,
	InCubic,
	OutCubic,
	InOutCubic,
	OutBack,
	OutElastic,
}

impl Curve {
	pub fn apply(self, t: f32) -> f32 {
		let t = t.clamp(0.0, 1.0);
		match self {
			Curve::Linear => t,
			Curve::Smooth => smoothstep01(t),
			Curve::InQuad => in_quad(t),
			Curve::OutQuad => out_quad(t),
			Curve::InOutQuad => in_out_quad(t),
			Curve::InCubic => in_cubic(t),
			Curve::OutCubic => out_cubic(t),
			Curve::InOutCubic => in_out_cubic(t),
			Curve::OutBack => out_back(t),
			Curve::OutElastic => out_elastic(t),
		}
	}

	/// Every curve, for endpoint/shape sweeps in tests.
	pub const ALL: [Curve; 10] = [
		Curve::Linear,
		Curve::Smooth,
		Curve::InQuad,
		Curve::OutQuad,
		Curve::InOutQuad,
		Curve::InCubic,
		Curve::OutCubic,
		Curve::InOutCubic,
		Curve::OutBack,
		Curve::OutElastic,
	];
}

#[cfg(test)]
mod tests;
