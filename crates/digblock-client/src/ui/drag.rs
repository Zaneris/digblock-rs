//! Generic press–drag–release state machine for slot-to-slot item
//! drags. Pure and screen-agnostic: ids and payload are type
//! parameters, hit resolution goes through the caller's [`HitMap`].
//! The payload is captured at press time, so state churn mid-drag
//! (an authoritative inventory snapshot reshuffling a grid) can never
//! change what's in hand.

use super::hit::HitMap;

/// Logical drag threshold in px — callers multiply by the UI scale.
/// Below it a press+release is a [`DragEvent::Click`], not a drop.
pub const DRAG_THRESHOLD_PX: f32 = 4.0;

#[derive(Debug)]
pub enum DragEvent<Id, P> {
	None,
	/// Press and release under the threshold: a plain click on `id`.
	Click {
		id: Id,
	},
	/// A completed drag; `to` is `None` when released over nothing.
	Drop {
		from: Id,
		payload: P,
		to: Option<Id>,
	},
}

pub struct DragState<Id, P> {
	/// Armed by a press over a slot: (source id, payload, press pos).
	pressed: Option<(Id, P, (f32, f32))>,
	/// True once the cursor moved past the threshold — the ghost shows
	/// and release becomes a drop.
	dragging: bool,
	pub cursor: (f32, f32),
}

impl<Id, P> Default for DragState<Id, P> {
	fn default() -> Self {
		Self {
			pressed: None,
			dragging: false,
			cursor: (-1.0, -1.0),
		}
	}
}

impl<Id: Copy, P> DragState<Id, P> {
	pub fn on_press(&mut self, pos: (f32, f32), id: Id, payload: P) {
		self.cursor = pos;
		self.pressed = Some((id, payload, pos));
		self.dragging = false;
	}

	pub fn on_move(&mut self, pos: (f32, f32), threshold_px: f32) {
		self.cursor = pos;
		if let Some((_, _, press)) = &self.pressed {
			let (dx, dy) = (pos.0 - press.0, pos.1 - press.1);
			if dx * dx + dy * dy > threshold_px * threshold_px {
				self.dragging = true;
			}
		}
	}

	pub fn on_release(&mut self, hits: &HitMap<Id>) -> DragEvent<Id, P>
	where
		Id: PartialEq,
	{
		let dragging = std::mem::take(&mut self.dragging);
		match self.pressed.take() {
			None => DragEvent::None,
			Some((from, payload, _)) if dragging => DragEvent::Drop {
				from,
				payload,
				to: hits.hit(self.cursor.0, self.cursor.1),
			},
			Some((id, _, _)) => DragEvent::Click { id },
		}
	}

	/// Abandon any in-flight drag (focus loss, screen close).
	pub fn cancel(&mut self) {
		self.pressed = None;
		self.dragging = false;
	}

	/// The payload to draw at [`Self::cursor`], once past the threshold.
	pub fn ghost(&self) -> Option<&P> {
		match (&self.pressed, self.dragging) {
			(Some((_, payload, _)), true) => Some(payload),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::Rect;
	use std::assert_matches;

	fn map() -> HitMap<u8> {
		let mut m = HitMap::default();
		m.push(Rect::new(0.0, 0.0, 50.0, 50.0), 1);
		m.push(Rect::new(100.0, 0.0, 50.0, 50.0), 2);
		m
	}

	#[test]
	fn under_threshold_is_a_click() {
		let mut d: DragState<u8, char> = DragState::default();
		d.on_press((10.0, 10.0), 1, 'a');
		d.on_move((12.0, 11.0), DRAG_THRESHOLD_PX);
		assert!(d.ghost().is_none(), "no ghost under the threshold");
		match d.on_release(&map()) {
			DragEvent::Click { id: 1 } => {}
			_ => panic!("expected click"),
		}
	}

	#[test]
	fn past_threshold_drops_on_target_or_nothing() {
		let mut d: DragState<u8, char> = DragState::default();
		d.on_press((10.0, 10.0), 1, 'a');
		d.on_move((120.0, 20.0), DRAG_THRESHOLD_PX);
		assert_eq!(d.ghost(), Some(&'a'), "ghost shows mid-drag");
		match d.on_release(&map()) {
			DragEvent::Drop {
				from: 1,
				payload: 'a',
				to: Some(2),
			} => {}
			_ => panic!("expected drop on 2"),
		}

		d.on_press((10.0, 10.0), 2, 'b');
		d.on_move((300.0, 300.0), DRAG_THRESHOLD_PX);
		match d.on_release(&map()) {
			DragEvent::Drop {
				from: 2, to: None, ..
			} => {}
			_ => panic!("expected drop on nothing"),
		}
	}

	#[test]
	fn cancel_and_bare_release_are_inert() {
		let mut d: DragState<u8, char> = DragState::default();
		d.on_press((10.0, 10.0), 1, 'a');
		d.on_move((120.0, 20.0), DRAG_THRESHOLD_PX);
		d.cancel();
		assert!(d.ghost().is_none());
		assert_matches!(d.on_release(&map()), DragEvent::None);
		// A release with no press at all (click began outside any slot).
		assert_matches!(d.on_release(&map()), DragEvent::None);
	}
}
