//! Touch controls — the Java World.processTouch scheme in normalized
//! coordinates (0..1 of the window, y down):
//! - left half: virtual joystick anchored at the touch-down point, never
//!   re-anchored; ~10% of screen height = full deflection (Java 100
//!   virtual units of 1000)
//! - right half: relative drag-look
//! - double-tap on the look side within 600 ms = jump
//!
//! Edge/home-indicator conflicts are handled platform-side (the iOS window
//! defers system edge gestures), so no inset logic is needed here.

use glam::Vec2;
use web_time::Instant;

const JOYSTICK_FULL_DEFLECTION: f32 = 0.1; // fraction of screen height
const LOOK_DEGREES_PER_SCREEN: f32 = 300.0;
const DOUBLE_TAP_MS: u128 = 600;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
	Started,
	Moved,
	Ended,
	Cancelled,
}

pub struct TouchDriver {
	move_touch: Option<(u64, Vec2, Vec2)>, // id, anchor, current
	look_touch: Option<(u64, Vec2)>,       // id, last position
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

	pub fn handle(&mut self, id: u64, phase: Phase, x: f32, y: f32) {
		let p = Vec2::new(x, y);
		match phase {
			Phase::Started => {
				if x < 0.5 {
					if self.move_touch.is_none() {
						self.move_touch = Some((id, p, p));
					}
				} else {
					if self.look_touch.is_none() {
						self.look_touch = Some((id, p));
					}
					// Java Player.jumpCount: double-tap on the look side.
					let now = Instant::now();
					if let Some(last) = self.last_tap {
						if now.duration_since(last).as_millis() < DOUBLE_TAP_MS {
							self.jump_pending = true;
						}
					}
					self.last_tap = Some(now);
				}
			}
			Phase::Moved => {
				if let Some((mid, _, current)) = &mut self.move_touch {
					if *mid == id {
						*current = p;
					}
				}
				if let Some((lid, last)) = &mut self.look_touch {
					if *lid == id {
						self.look_accum += (p - *last) * LOOK_DEGREES_PER_SCREEN;
						*last = p;
					}
				}
			}
			Phase::Ended | Phase::Cancelled => {
				if matches!(self.move_touch, Some((mid, ..)) if mid == id) {
					self.move_touch = None;
				}
				if matches!(self.look_touch, Some((lid, _)) if lid == id) {
					self.look_touch = None;
				}
			}
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

	/// Accumulated look delta in degrees (yaw, pitch), consumed per frame.
	pub fn take_look(&mut self) -> Vec2 {
		std::mem::take(&mut self.look_accum)
	}

	pub fn take_jump(&mut self) -> bool {
		std::mem::take(&mut self.jump_pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn joystick_anchored_and_scaled() {
		let mut t = TouchDriver::new();
		t.handle(1, Phase::Started, 0.25, 0.6);
		t.handle(1, Phase::Moved, 0.25, 0.5); // dragged up 10% of screen
		let m = t.move_axis();
		assert!(
			(m.y - 1.0).abs() < 1e-5,
			"full forward deflection, got {m:?}"
		);
		t.handle(1, Phase::Ended, 0.25, 0.5);
		assert_eq!(t.move_axis(), Vec2::ZERO);
	}

	#[test]
	fn look_is_relative_and_consumed() {
		let mut t = TouchDriver::new();
		t.handle(2, Phase::Started, 0.75, 0.5);
		t.handle(2, Phase::Moved, 0.80, 0.5);
		let l = t.take_look();
		assert!(l.x > 10.0, "look right, got {l:?}");
		assert_eq!(t.take_look(), Vec2::ZERO, "consumed");
	}

	#[test]
	fn double_tap_jumps() {
		let mut t = TouchDriver::new();
		t.handle(3, Phase::Started, 0.8, 0.5);
		t.handle(3, Phase::Ended, 0.8, 0.5);
		t.handle(4, Phase::Started, 0.8, 0.5);
		assert!(t.take_jump());
		assert!(!t.take_jump());
	}

	#[test]
	fn two_finger_move_and_look() {
		let mut t = TouchDriver::new();
		t.handle(1, Phase::Started, 0.2, 0.6);
		t.handle(2, Phase::Started, 0.8, 0.5);
		t.handle(1, Phase::Moved, 0.2, 0.55);
		t.handle(2, Phase::Moved, 0.85, 0.5);
		assert!(t.move_axis().y > 0.0);
		assert!(t.take_look().x > 0.0);
	}
}
