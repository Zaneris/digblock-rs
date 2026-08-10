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
	/// Inventory screen toggle.
	I,
	/// Alchemy screen toggle.
	K,
	/// Character (skills) screen toggle.
	C,
	/// Return to spawn ("home").
	H,
	/// Chat open (multiplayer).
	T,
	/// Number row digit 0..=9 (hotbar slot selection; numpad excluded —
	/// NumLock ambiguity isn't worth it).
	Digit(u8),
	/// Grave/backtick: debug overlay toggle.
	Backquote,
	Escape,
	Enter,
	Backspace,
	/// Chat command autocomplete.
	Tab,
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
	/// Vertical scroll in "wheel notches" (+1 = one notch away from the
	/// user). Line deltas pass through; pixel deltas (trackpads, web) are
	/// scaled down. Only sent while the cursor is captured.
	Scroll {
		dy: f32,
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
		K::KeyI => Key::I,
		K::KeyK => Key::K,
		K::KeyC => Key::C,
		K::KeyH => Key::H,
		K::KeyT => Key::T,
		K::Digit0 => Key::Digit(0),
		K::Digit1 => Key::Digit(1),
		K::Digit2 => Key::Digit(2),
		K::Digit3 => Key::Digit(3),
		K::Digit4 => Key::Digit(4),
		K::Digit5 => Key::Digit(5),
		K::Digit6 => Key::Digit(6),
		K::Digit7 => Key::Digit(7),
		K::Digit8 => Key::Digit(8),
		K::Digit9 => Key::Digit(9),
		K::Backquote => Key::Backquote,
		K::Escape => Key::Escape,
		K::Enter | K::NumpadEnter => Key::Enter,
		K::Backspace => Key::Backspace,
		K::Tab => Key::Tab,
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
