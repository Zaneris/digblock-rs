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
	/// Hiscores screen toggle.
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
	// The rebind vocabulary: keys with no shipped default that a player may
	// bind on the Controls page. F9/F10 are deliberately absent — the shell
	// consumes them as lifecycle drills before the engine ever hears them.
	B,
	E,
	G,
	L,
	M,
	N,
	O,
	P,
	Q,
	R,
	U,
	V,
	X,
	Y,
	Z,
	/// Either Alt key.
	Alt,
	F1,
	F2,
	F3,
	F4,
	F5,
	F6,
	F8,
	F11,
	F12,
	Minus,
	Equal,
	BracketLeft,
	BracketRight,
	Semicolon,
	Quote,
	Comma,
	Period,
	Slash,
	Backslash,
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
			Key::B => "B",
			Key::E => "E",
			Key::G => "G",
			Key::L => "L",
			Key::M => "M",
			Key::N => "N",
			Key::O => "O",
			Key::P => "P",
			Key::Q => "Q",
			Key::R => "R",
			Key::U => "U",
			Key::V => "V",
			Key::X => "X",
			Key::Y => "Y",
			Key::Z => "Z",
			Key::Alt => "Alt",
			Key::F1 => "F1",
			Key::F2 => "F2",
			Key::F3 => "F3",
			Key::F4 => "F4",
			Key::F5 => "F5",
			Key::F6 => "F6",
			Key::F8 => "F8",
			Key::F11 => "F11",
			Key::F12 => "F12",
			Key::Minus => "-",
			Key::Equal => "=",
			Key::BracketLeft => "[",
			Key::BracketRight => "]",
			Key::Semicolon => ";",
			Key::Quote => "'",
			Key::Comma => ",",
			Key::Period => ".",
			Key::Slash => "/",
			Key::Backslash => "\\",
			Key::Other => "?",
		}
	}

	/// Stable persistence name ("w", "f8", "bracket_left"). These strings,
	/// never enum discriminants, are the contract with the settings file;
	/// renaming a variant later means keeping its old string as a `parse`
	/// alias. `Other` has no identity and answers the empty string.
	pub fn name(self) -> &'static str {
		match self {
			Key::W => "w",
			Key::A => "a",
			Key::S => "s",
			Key::D => "d",
			Key::Up => "up",
			Key::Down => "down",
			Key::Left => "left",
			Key::Right => "right",
			Key::Space => "space",
			Key::Shift => "shift",
			Key::Ctrl => "ctrl",
			Key::F => "f",
			Key::I => "i",
			Key::K => "k",
			Key::C => "c",
			Key::H => "h",
			Key::J => "j",
			Key::T => "t",
			Key::Digit(n) => {
				const NAMES: [&str; 10] = [
					"digit_0", "digit_1", "digit_2", "digit_3", "digit_4", "digit_5", "digit_6",
					"digit_7", "digit_8", "digit_9",
				];
				NAMES[usize::from(n.min(9))]
			}
			Key::Backquote => "backquote",
			Key::F7 => "f7",
			Key::Escape => "escape",
			Key::Enter => "enter",
			Key::Backspace => "backspace",
			Key::Tab => "tab",
			Key::B => "b",
			Key::E => "e",
			Key::G => "g",
			Key::L => "l",
			Key::M => "m",
			Key::N => "n",
			Key::O => "o",
			Key::P => "p",
			Key::Q => "q",
			Key::R => "r",
			Key::U => "u",
			Key::V => "v",
			Key::X => "x",
			Key::Y => "y",
			Key::Z => "z",
			Key::Alt => "alt",
			Key::F1 => "f1",
			Key::F2 => "f2",
			Key::F3 => "f3",
			Key::F4 => "f4",
			Key::F5 => "f5",
			Key::F6 => "f6",
			Key::F8 => "f8",
			Key::F11 => "f11",
			Key::F12 => "f12",
			Key::Minus => "minus",
			Key::Equal => "equal",
			Key::BracketLeft => "bracket_left",
			Key::BracketRight => "bracket_right",
			Key::Semicolon => "semicolon",
			Key::Quote => "quote",
			Key::Comma => "comma",
			Key::Period => "period",
			Key::Slash => "slash",
			Key::Backslash => "backslash",
			Key::Other => "",
		}
	}

	/// Inverse of [`Key::name`]; unknown or empty names answer `None`.
	pub fn parse(s: &str) -> Option<Key> {
		Key::NAMEABLE.iter().copied().find(|k| k.name() == s)
	}

	/// Every key with a persistence identity — all of them but `Other`.
	pub const NAMEABLE: &'static [Key] = &[
		Key::W,
		Key::A,
		Key::S,
		Key::D,
		Key::Up,
		Key::Down,
		Key::Left,
		Key::Right,
		Key::Space,
		Key::Shift,
		Key::Ctrl,
		Key::F,
		Key::I,
		Key::K,
		Key::C,
		Key::H,
		Key::J,
		Key::T,
		Key::Digit(0),
		Key::Digit(1),
		Key::Digit(2),
		Key::Digit(3),
		Key::Digit(4),
		Key::Digit(5),
		Key::Digit(6),
		Key::Digit(7),
		Key::Digit(8),
		Key::Digit(9),
		Key::Backquote,
		Key::F7,
		Key::Escape,
		Key::Enter,
		Key::Backspace,
		Key::Tab,
		Key::B,
		Key::E,
		Key::G,
		Key::L,
		Key::M,
		Key::N,
		Key::O,
		Key::P,
		Key::Q,
		Key::R,
		Key::U,
		Key::V,
		Key::X,
		Key::Y,
		Key::Z,
		Key::Alt,
		Key::F1,
		Key::F2,
		Key::F3,
		Key::F4,
		Key::F5,
		Key::F6,
		Key::F8,
		Key::F11,
		Key::F12,
		Key::Minus,
		Key::Equal,
		Key::BracketLeft,
		Key::BracketRight,
		Key::Semicolon,
		Key::Quote,
		Key::Comma,
		Key::Period,
		Key::Slash,
		Key::Backslash,
	];
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
		K::KeyB => Key::B,
		K::KeyE => Key::E,
		K::KeyG => Key::G,
		K::KeyL => Key::L,
		K::KeyM => Key::M,
		K::KeyN => Key::N,
		K::KeyO => Key::O,
		K::KeyP => Key::P,
		K::KeyQ => Key::Q,
		K::KeyR => Key::R,
		K::KeyU => Key::U,
		K::KeyV => Key::V,
		K::KeyX => Key::X,
		K::KeyY => Key::Y,
		K::KeyZ => Key::Z,
		K::AltLeft | K::AltRight => Key::Alt,
		K::F1 => Key::F1,
		K::F2 => Key::F2,
		K::F3 => Key::F3,
		K::F4 => Key::F4,
		K::F5 => Key::F5,
		K::F6 => Key::F6,
		K::F8 => Key::F8,
		K::F11 => Key::F11,
		K::F12 => Key::F12,
		K::Minus => Key::Minus,
		K::Equal => Key::Equal,
		K::BracketLeft => Key::BracketLeft,
		K::BracketRight => Key::BracketRight,
		K::Semicolon => Key::Semicolon,
		K::Quote => Key::Quote,
		K::Comma => Key::Comma,
		K::Period => Key::Period,
		K::Slash => Key::Slash,
		K::Backslash => Key::Backslash,
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_nameable_key_roundtrips_and_is_unique() {
		for &k in Key::NAMEABLE {
			assert!(!k.name().is_empty(), "{k:?} must have a persistence name");
			assert_eq!(
				Key::parse(k.name()),
				Some(k),
				"{k:?} must roundtrip through its name"
			);
		}
		for (i, &a) in Key::NAMEABLE.iter().enumerate() {
			for &b in &Key::NAMEABLE[i + 1..] {
				assert_ne!(a.name(), b.name(), "{a:?} and {b:?} share a name");
			}
		}
		// `Other` has no identity: it never parses and never labels usefully.
		assert_eq!(Key::parse(""), None);
		assert_eq!(Key::parse("no_such_key"), None);
		assert_eq!(Key::Other.name(), "");
	}
}
