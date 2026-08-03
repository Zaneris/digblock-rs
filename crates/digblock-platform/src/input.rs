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
	/// Debug: arm water-source placement.
	B,
	/// Grave/backtick: debug overlay toggle.
	Backquote,
	Escape,
	Enter,
	Backspace,
	Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
	Left,
	Right,
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
	/// Mouse buttons. While the cursor is captured these are gameplay
	/// clicks; while free (menu open, i.e. the game doesn't want the grab)
	/// they are UI clicks paired with the last `CursorMoved`. The click
	/// that re-grabs the cursor is swallowed by the shell.
	MouseButton {
		button: MouseButton,
		pressed: bool,
	},
	/// Window-space cursor position in physical pixels (the overlay's
	/// coordinate space); only while the cursor is NOT captured.
	CursorMoved {
		x: f32,
		y: f32,
	},
	/// Produced text (layout- and shift-aware), for menu text fields.
	/// Control characters never arrive here — Backspace/Enter are `Key`s.
	Text {
		ch: char,
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
	/// Web only: the browser revoked pointer lock (user pressed Escape,
	/// switched tabs) while the game still wanted it. Engine must clear
	/// held state and should surface its pause UI. Never sent natively.
	PointerLockLost,
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
		K::KeyB => Key::B,
		K::Backquote => Key::Backquote,
		K::Escape => Key::Escape,
		K::Enter | K::NumpadEnter => Key::Enter,
		K::Backspace => Key::Backspace,
		_ => Key::Other,
	}
}

pub(crate) fn map_button(button: winit::event::MouseButton) -> MouseButton {
	use winit::event::MouseButton as B;
	match button {
		B::Left => MouseButton::Left,
		B::Right => MouseButton::Right,
		_ => MouseButton::Other,
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
