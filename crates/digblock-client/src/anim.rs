//! Presentation timers as types instead of bare `f32` fields counted
//! down inline in `step()`. All plain polled state — no closures, no
//! callbacks: the owner ticks them with `dt` and reads what it needs,
//! matching how every other per-frame system here works. Levels land on
//! EXACTLY 0.0 / 1.0 (min/max clamps, no exp tails), so idle UI and
//! settled effects produce bit-identical frames — the property the GI
//! history and light-list equality checks rely on. The module grows with
//! its consumers: a type is added when the first call site lands, never
//! speculatively (dead code is denied).

/// One-shot countdown. `tick` reports the crossing frame exactly once;
/// after that the timer is idle until `start`ed again.
#[derive(Default)]
pub(crate) struct Timer {
	left: Option<f32>,
}

impl Timer {
	pub(crate) fn start(&mut self, dur: f32) {
		self.left = Some(dur);
	}

	/// Advance; true exactly on the frame the countdown runs out.
	pub(crate) fn tick(&mut self, dt: f32) -> bool {
		let Some(left) = self.left else { return false };
		let left = left - dt;
		if left <= 0.0 {
			self.left = None;
			true
		} else {
			self.left = Some(left);
			false
		}
	}

	pub(crate) fn running(&self) -> bool {
		self.left.is_some()
	}

	/// Abandon the countdown without it ever firing.
	pub(crate) fn stop(&mut self) {
		self.left = None;
	}
}

/// A gate that refuses until its window runs out. Not a [`Timer`]: no
/// one-shot edge — call sites ask `ready()`/`left()` whenever they care.
#[derive(Default)]
pub(crate) struct Cooldown {
	left: f32,
}

impl Cooldown {
	pub(crate) fn start(&mut self, dur: f32) {
		self.left = dur;
	}

	pub(crate) fn tick(&mut self, dt: f32) {
		self.left = (self.left - dt).max(0.0);
	}

	pub(crate) fn ready(&self) -> bool {
		self.left <= 0.0
	}

	/// Seconds remaining (0.0 when ready) — what countdown labels show.
	pub(crate) fn left(&self) -> f32 {
		self.left
	}

	/// Pin a mid-window state directly; tests only.
	#[cfg(test)]
	pub(crate) fn force(&mut self, left: f32) {
		self.left = left;
	}
}

/// A 0..=1 level that rises while `held` and falls when released, at
/// separate speeds, landing EXACTLY on the rails (min/max, no exp tail).
/// The channel-aura shape: attack readable as intent, release as a fade.
#[derive(Default)]
pub(crate) struct Envelope {
	level: f32,
}

impl Envelope {
	/// Advance one frame; `attack`/`release` are seconds rail-to-rail.
	pub(crate) fn tick(&mut self, dt: f32, held: bool, attack: f32, release: f32) {
		self.level = if held {
			(self.level + dt / attack).min(1.0)
		} else {
			(self.level - dt / release).max(0.0)
		};
	}

	pub(crate) fn level(&self) -> f32 {
		self.level
	}

	/// World swap: the aura of a spell that no longer exists.
	pub(crate) fn reset(&mut self) {
		self.level = 0.0;
	}
}

#[cfg(test)]
mod tests;
