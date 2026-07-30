//! Engine-facing input vocabulary. winit types never leak past this module.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
	W,
	A,
	S,
	D,
	Up,
	Down,
	Left,
	Right,
	Space,
	Shift,
	F,
	Escape,
	Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchPhase {
	Started,
	Moved,
	Ended,
	Cancelled,
}

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
	Key {
		key: Key,
		pressed: bool,
	},
	/// Raw, unaccelerated mouse deltas (DeviceEvent), only while the cursor
	/// is captured.
	MouseDelta {
		dx: f64,
		dy: f64,
	},
	/// Touch position normalized to 0..1 of the window.
	Touch {
		id: u64,
		phase: TouchPhase,
		x: f32,
		y: f32,
	},
	/// Engine must clear all held keys/touches.
	FocusLost,
}

pub(crate) fn map_key(code: winit::keyboard::KeyCode) -> Key {
	use winit::keyboard::KeyCode as K;
	match code {
		K::KeyW => Key::W,
		K::KeyA => Key::A,
		K::KeyS => Key::S,
		K::KeyD => Key::D,
		K::ArrowUp => Key::Up,
		K::ArrowDown => Key::Down,
		K::ArrowLeft => Key::Left,
		K::ArrowRight => Key::Right,
		K::Space => Key::Space,
		K::ShiftLeft | K::ShiftRight => Key::Shift,
		K::KeyF => Key::F,
		K::Escape => Key::Escape,
		_ => Key::Other,
	}
}

pub(crate) fn map_touch_phase(phase: winit::event::TouchPhase) -> TouchPhase {
	use winit::event::TouchPhase as P;
	match phase {
		P::Started => TouchPhase::Started,
		P::Moved => TouchPhase::Moved,
		P::Ended => TouchPhase::Ended,
		P::Cancelled => TouchPhase::Cancelled,
	}
}
