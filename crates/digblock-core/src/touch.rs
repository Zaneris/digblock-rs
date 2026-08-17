//! Touch controls — the Java World.processTouch scheme in normalized
//! coordinates (0..1 of the window, y down), grown a dig verb:
//! - left half: virtual joystick anchored at the touch-down point, never
//!   re-anchored; `JOYSTICK_FULL_DEFLECTION` of the screen height is full
//!   deflection (the Java scheme's joystick radius in virtual units)
//! - right half: relative drag-look once a touch travels past `TAP_SLOP`;
//!   a touch that instead stays put through `HOLD_DIG_MS` digs at the
//!   crosshair until it lifts, and moving DURING the dig still looks, so
//!   the crosshair steers the dig target
//! - double-tap on the look side inside `DOUBLE_TAP_MS` = jump (the Java
//!   feel, kept). Only clean taps chain: a touch that becomes a drag or a
//!   dig stops counting as one, so flick-looking and mining never jump.
//!
//! Placing is deliberately NOT a touch verb here — a place tap's first
//! touch would be indistinguishable from a double-tap's — it belongs to
//! the on-screen HUD button.
//!
//! Time is injected (`now` parameters) rather than sampled, so every
//! threshold is testable on a synthetic timeline. A stationary finger
//! sends no Moved events, which is why the hold promotion needs `tick`.
//!
//! Edge/home-indicator conflicts are handled platform-side (the iOS window
//! defers system edge gestures), so no inset logic is needed here.

use glam::Vec2;
use web_time::Instant;

const JOYSTICK_FULL_DEFLECTION: f32 = 0.1; // fraction of screen height
const LOOK_DEGREES_PER_SCREEN: f32 = 300.0;
const DOUBLE_TAP_MS: u128 = 600;
const HOLD_DIG_MS: u128 = 200;
const TAP_SLOP: f32 = 0.02; // fraction of the window, either axis

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
	Started,
	Moved,
	Ended,
	Cancelled,
}

struct LookTouch {
	id: u64,
	/// Last position look was accumulated from.
	last: Vec2,
	start: Vec2,
	start_time: Instant,
	/// Committed to looking: the tap/hold candidacy is dead.
	past_slop: bool,
	digging: bool,
}

pub struct TouchDriver {
	move_touch: Option<(u64, Vec2, Vec2)>, // id, anchor, current
	look_touch: Option<LookTouch>,
	look_accum: Vec2,
	last_tap: Option<Instant>,
	jump_pending: bool,
}

impl Default for TouchDriver {
	fn default() -> Self {
		Self::new()
	}
}

impl TouchDriver {
	pub fn new() -> Self {
		Self {
			move_touch: None,
			look_touch: None,
			look_accum: Vec2::ZERO,
			last_tap: None,
			jump_pending: false,
		}
	}

	pub fn handle(&mut self, id: u64, phase: Phase, x: f32, y: f32, now: Instant) {
		let p = Vec2::new(x, y);
		match phase {
			Phase::Started => {
				if x < 0.5 {
					if self.move_touch.is_none() {
						self.move_touch = Some((id, p, p));
					}
				} else {
					if self.look_touch.is_none() {
						self.look_touch = Some(LookTouch {
							id,
							last: p,
							start: p,
							start_time: now,
							past_slop: false,
							digging: false,
						});
					}
					// Java Player.jumpCount: double-tap on the look side.
					if let Some(last) = self.last_tap
						&& now.duration_since(last).as_millis() < DOUBLE_TAP_MS
					{
						self.jump_pending = true;
					}
					self.last_tap = Some(now);
				}
			}
			Phase::Moved => {
				if let Some((mid, _, current)) = &mut self.move_touch
					&& *mid == id
				{
					*current = p;
				}
				if let Some(lt) = &mut self.look_touch
					&& lt.id == id
				{
					if lt.past_slop || lt.digging {
						self.look_accum += (p - lt.last) * LOOK_DEGREES_PER_SCREEN;
						lt.last = p;
					} else if (p - lt.start).abs().max_element() > TAP_SLOP {
						// Committed to looking. The pre-slop delta is
						// dropped — accumulating it here would snap the
						// camera by the whole slop on the crossing frame —
						// and a drag is not a tap, so it must not arm half
						// of a double-tap.
						lt.past_slop = true;
						lt.last = p;
						self.last_tap = None;
					}
				}
			}
			Phase::Ended | Phase::Cancelled => {
				if matches!(self.move_touch, Some((mid, ..)) if mid == id) {
					self.move_touch = None;
				}
				if matches!(&self.look_touch, Some(lt) if lt.id == id) {
					self.look_touch = None;
				}
			}
		}
	}

	/// Promote a stationary look-side hold to digging. Poll-driven (once
	/// per engine update): a finger that never moves sends no events, so
	/// nothing else can observe the threshold crossing.
	pub fn tick(&mut self, now: Instant) {
		if let Some(lt) = &mut self.look_touch
			&& !lt.past_slop
			&& !lt.digging
			&& now.duration_since(lt.start_time).as_millis() >= HOLD_DIG_MS
		{
			lt.digging = true;
			// Mining is not tapping: a short dig's touch-down must not arm
			// half of a double-tap for the next one to complete.
			self.last_tap = None;
		}
	}

	/// All touches gone (app suspend / focus loss).
	pub fn clear(&mut self) {
		self.move_touch = None;
		self.look_touch = None;
	}

	/// Movement axis: x = strafe, y = forward. Player clamps to unit.
	pub fn move_axis(&self) -> Vec2 {
		match self.move_touch {
			Some((_, anchor, current)) => {
				let d = (current - anchor) / JOYSTICK_FULL_DEFLECTION;
				// Screen y grows downward; dragging up = forward (Java).
				Vec2::new(d.x, -d.y)
			}
			None => Vec2::ZERO,
		}
	}

	/// The live joystick, for the HUD's visualization: the anchor the
	/// touch went down at and the finger's current position, both in
	/// normalized window coordinates.
	pub fn joystick(&self) -> Option<(Vec2, Vec2)> {
		self.move_touch
			.map(|(_, anchor, current)| (anchor, current))
	}

	/// Accumulated look delta in degrees (yaw, pitch), consumed per frame.
	pub fn take_look(&mut self) -> Vec2 {
		std::mem::take(&mut self.look_accum)
	}

	/// A look-side hold is mining at the crosshair. Level state, not an
	/// edge: it reads true from the hold's promotion until the touch ends.
	pub fn dig_held(&self) -> bool {
		self.look_touch.as_ref().is_some_and(|lt| lt.digging)
	}

	pub fn take_jump(&mut self) -> bool {
		std::mem::take(&mut self.jump_pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	/// Just past the hold threshold on the synthetic timeline.
	fn held(t0: Instant) -> Instant {
		t0 + Duration::from_millis(HOLD_DIG_MS as u64 + 1)
	}

	#[test]
	fn joystick_anchored_and_scaled() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(1, Phase::Started, 0.25, 0.6, t0);
		t.handle(1, Phase::Moved, 0.25, 0.5, t0); // dragged up a full deflection
		let m = t.move_axis();
		assert!(
			(m.y - 1.0).abs() < 1e-5,
			"full forward deflection, got {m:?}"
		);
		assert_eq!(
			t.joystick(),
			Some((Vec2::new(0.25, 0.6), Vec2::new(0.25, 0.5)))
		);
		t.handle(1, Phase::Ended, 0.25, 0.5, t0);
		assert_eq!(t.move_axis(), Vec2::ZERO);
		assert_eq!(t.joystick(), None);
	}

	#[test]
	fn look_is_relative_and_consumed() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(2, Phase::Started, 0.75, 0.5, t0);
		t.handle(2, Phase::Moved, 0.80, 0.5, t0); // past the slop: committed
		t.handle(2, Phase::Moved, 0.85, 0.5, t0);
		let l = t.take_look();
		assert!(l.x > 10.0, "look right, got {l:?}");
		assert_eq!(t.take_look(), Vec2::ZERO, "consumed");
	}

	#[test]
	fn look_accumulates_only_past_the_slop_crossing() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(2, Phase::Started, 0.75, 0.5, t0);
		// Inside the slop: still a tap/hold candidate, no camera motion.
		t.handle(2, Phase::Moved, 0.75 + TAP_SLOP * 0.5, 0.5, t0);
		assert_eq!(t.take_look(), Vec2::ZERO);
		// The crossing itself contributes nothing (no snap)…
		t.handle(2, Phase::Moved, 0.75 + TAP_SLOP * 2.0, 0.5, t0);
		assert_eq!(t.take_look(), Vec2::ZERO);
		// …and motion after it is ordinary look.
		t.handle(2, Phase::Moved, 0.80 + TAP_SLOP * 2.0, 0.5, t0);
		assert!(t.take_look().x > 10.0);
	}

	#[test]
	fn double_tap_jumps() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(3, Phase::Started, 0.8, 0.5, t0);
		t.handle(3, Phase::Ended, 0.8, 0.5, t0);
		t.handle(4, Phase::Started, 0.8, 0.5, t0 + Duration::from_millis(1));
		assert!(t.take_jump());
		assert!(!t.take_jump());
	}

	#[test]
	fn a_drag_is_not_half_a_double_tap() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(3, Phase::Started, 0.8, 0.5, t0);
		t.handle(3, Phase::Moved, 0.9, 0.5, t0); // a look flick
		t.handle(3, Phase::Ended, 0.9, 0.5, t0);
		t.handle(4, Phase::Started, 0.8, 0.5, t0 + Duration::from_millis(1));
		assert!(!t.take_jump());
	}

	#[test]
	fn hold_digs_until_release() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(5, Phase::Started, 0.8, 0.5, t0);
		t.tick(t0);
		assert!(!t.dig_held(), "not before the hold threshold");
		t.tick(held(t0));
		assert!(t.dig_held());
		t.handle(5, Phase::Ended, 0.8, 0.5, held(t0));
		assert!(!t.dig_held());
	}

	#[test]
	fn a_short_dig_is_not_half_a_double_tap() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(5, Phase::Started, 0.8, 0.5, t0);
		t.tick(held(t0));
		t.handle(5, Phase::Ended, 0.8, 0.5, held(t0));
		t.handle(
			6,
			Phase::Started,
			0.8,
			0.5,
			held(t0) + Duration::from_millis(1),
		);
		assert!(!t.take_jump());
	}

	#[test]
	fn a_drag_never_becomes_a_dig() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(5, Phase::Started, 0.8, 0.5, t0);
		t.handle(5, Phase::Moved, 0.9, 0.5, t0);
		t.tick(held(t0));
		assert!(!t.dig_held());
	}

	#[test]
	fn dig_keeps_look_alive() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(5, Phase::Started, 0.8, 0.5, t0);
		t.tick(held(t0));
		assert!(t.dig_held());
		// Fine-aiming during the dig: the crosshair steers the target.
		t.handle(5, Phase::Moved, 0.85, 0.5, held(t0));
		assert!(t.dig_held());
		assert!(t.take_look().x > 10.0);
	}

	#[test]
	fn two_finger_move_and_look() {
		let t0 = Instant::now();
		let mut t = TouchDriver::new();
		t.handle(1, Phase::Started, 0.2, 0.6, t0);
		t.handle(2, Phase::Started, 0.8, 0.5, t0);
		t.handle(1, Phase::Moved, 0.2, 0.55, t0);
		t.handle(2, Phase::Moved, 0.85, 0.5, t0); // past the slop: committed
		t.handle(2, Phase::Moved, 0.90, 0.5, t0);
		assert!(t.move_axis().y > 0.0);
		assert!(t.take_look().x > 0.0);
	}
}
