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
	/// Either Control key (alchemy's largest craft multiplier).
	Ctrl,
	F,
	/// Inventory screen toggle.
	I,
	/// Alchemy screen toggle.
	K,
	/// Character (skills) screen toggle.
	C,
	/// Cast Return Home (starts the channel).
	H,
	/// Spellbook screen toggle.
	J,
	/// Chat open (multiplayer).
	T,
	/// Number row digit 0..=9 (hotbar slot; numpad excluded — NumLock ambiguity isn't worth it).
	Digit(u8),
	/// Grave/backtick: debug overlay toggle.
	Backquote,
	/// F7: rebuild the GI window's derived state — a drill routed to the ENGINE, not the shell.
	F7,
	Escape,
	Enter,
	Backspace,
	/// Chat command autocomplete.
	Tab,
	Other,
}

impl Key {
	/// The name a HINT calls a key by ("press K") — UI text, not what the
	/// OS reports, so it is layout-independent exactly like the bindings.
	/// Every variant answers: a hint that names an unnameable key is a
	/// bug at the hint's call site, not a panic here.
	pub fn label(self) -> &'static str {
		match self {
			Key::W => "W",
			Key::A => "A",
			Key::S => "S",
			Key::D => "D",
			Key::Up => "Up",
			Key::Down => "Down",
			Key::Left => "Left",
			Key::Right => "Right",
			Key::Space => "Space",
			Key::Shift => "Shift",
			Key::Ctrl => "Ctrl",
			Key::F => "F",
			Key::I => "I",
			Key::K => "K",
			Key::C => "C",
			Key::H => "H",
			Key::J => "J",
			Key::T => "T",
			Key::Digit(n) => {
				const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
				DIGITS[usize::from(n.min(9))]
			}
			Key::Backquote => "`",
			Key::F7 => "F7",
			Key::Escape => "Esc",
			Key::Enter => "Enter",
			Key::Backspace => "Backspace",
			Key::Tab => "Tab",
			Key::Other => "?",
		}
	}
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
	/// Raw, unaccelerated mouse deltas (DeviceEvent), only while the cursor is captured.
	MouseDelta {
		dx: f64,
		dy: f64,
	},
	/// Scroll in wheel notches, pixel deltas scaled down; only while the cursor is captured.
	Scroll {
		dy: f32,
	},
	/// Gameplay clicks while captured; UI clicks paired with the last `CursorMoved` while free.
	MouseButton {
		button: MouseButton,
		pressed: bool,
	},
	/// Window-space cursor position in physical pixels; only while the cursor is NOT captured.
	CursorMoved {
		x: f32,
		y: f32,
	},
	/// Produced text (layout- and shift-aware); Backspace/Enter arrive as `Key`s, never here.
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
	/// Web only: the browser revoked pointer lock while the game still wanted it; clear held state.
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
		K::ControlLeft | K::ControlRight => Key::Ctrl,
		K::KeyF => Key::F,
		K::KeyI => Key::I,
		K::KeyK => Key::K,
		K::KeyC => Key::C,
		K::KeyH => Key::H,
		K::KeyJ => Key::J,
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
		K::F7 => Key::F7,
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
