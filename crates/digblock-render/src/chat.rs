//! Chat input line: a pure state machine in the `menu.rs` mold — no
//! GPU, no I/O, unit-tested by feeding synthetic `InputEvent`s. The
//! engine owns when it opens (T/Enter while connected) and what happens
//! to a sent line (`NetClient::send_chat`); received chat renders
//! through the ordinary feed.

use digblock_net::protocol::MAX_CHAT_LEN;
use digblock_platform::input::{InputEvent, Key};

/// What a handled input asks the engine to do.
#[derive(Debug, PartialEq, Eq)]
pub enum ChatAction {
	/// Enter on a non-empty line: send this text (already trimmed).
	Send(String),
	/// The box closed without sending (Escape, or Enter on empty).
	Closed,
}

/// Immutable per-frame view for the overlay.
#[derive(Clone, Copy)]
pub struct ChatView<'a> {
	pub line: &'a str,
	/// Caret blink phase, driven by the engine clock.
	pub caret: bool,
}

#[derive(Default)]
pub struct Chat {
	open: bool,
	buffer: String,
	/// The keypress that opened the box (T, Enter) also produces its own
	/// `Text` event a moment later — swallow exactly that one so the
	/// buffer doesn't start with a stray "t".
	suppress_text: bool,
}

impl Chat {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		if !self.open {
			self.open = true;
			self.buffer.clear();
			self.suppress_text = true;
		}
	}

	pub fn view(&self, caret: bool) -> ChatView<'_> {
		ChatView {
			line: &self.buffer,
			caret,
		}
	}

	/// Handle one input while open. Returns an action when the box
	/// closes; `None` while it stays open. Must only be called while
	/// `is_open()`.
	pub fn handle_input(&mut self, event: &InputEvent) -> Option<ChatAction> {
		match event {
			InputEvent::Text { ch } => {
				if std::mem::take(&mut self.suppress_text) {
					return None;
				}
				if self.buffer.chars().count() < MAX_CHAT_LEN && matches!(ch, ' '..='~') {
					self.buffer.push(*ch);
				}
				None
			}
			InputEvent::Key { key, pressed: true } => match key {
				Key::Backspace => {
					self.suppress_text = false;
					self.buffer.pop();
					None
				}
				Key::Enter => {
					self.open = false;
					let text = self.buffer.trim().to_string();
					self.buffer.clear();
					if text.is_empty() {
						Some(ChatAction::Closed)
					} else {
						Some(ChatAction::Send(text))
					}
				}
				Key::Escape => {
					self.open = false;
					self.buffer.clear();
					Some(ChatAction::Closed)
				}
				// Any other keydown ends the open-key suppression window
				// without doing anything else.
				_ => {
					self.suppress_text = false;
					None
				}
			},
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn text(c: &mut Chat, s: &str) -> Option<ChatAction> {
		let mut last = None;
		for ch in s.chars() {
			last = c.handle_input(&InputEvent::Text { ch });
		}
		last
	}

	fn key(c: &mut Chat, k: Key) -> Option<ChatAction> {
		c.handle_input(&InputEvent::Key {
			key: k,
			pressed: true,
		})
	}

	#[test]
	fn open_key_text_is_swallowed_once() {
		let mut c = Chat::default();
		c.open();
		// The 't' produced by the T that opened the box...
		text(&mut c, "t");
		// ...then real typing.
		text(&mut c, "hi");
		assert_eq!(c.view(false).line, "hi");
	}

	#[test]
	fn type_backspace_enter_sends_trimmed() {
		let mut c = Chat::default();
		c.open();
		text(&mut c, "t"); // swallowed opener
		text(&mut c, " hey therez");
		key(&mut c, Key::Backspace);
		assert_eq!(
			key(&mut c, Key::Enter),
			Some(ChatAction::Send("hey there".into()))
		);
		assert!(!c.is_open());
	}

	#[test]
	fn escape_discards() {
		let mut c = Chat::default();
		c.open();
		text(&mut c, "t");
		text(&mut c, "draft");
		assert_eq!(key(&mut c, Key::Escape), Some(ChatAction::Closed));
		assert!(!c.is_open());
		// Reopening starts clean.
		c.open();
		assert_eq!(c.view(false).line, "");
	}

	#[test]
	fn enter_on_empty_closes_without_send() {
		let mut c = Chat::default();
		c.open();
		// Opened via Enter: its own Text (none for Enter) — press Enter
		// again immediately.
		assert_eq!(key(&mut c, Key::Enter), Some(ChatAction::Closed));
		assert!(!c.is_open());
	}

	#[test]
	fn caps_length_and_drops_non_ascii() {
		let mut c = Chat::default();
		c.open();
		c.suppress_text = false;
		text(&mut c, "héllo\u{7}");
		assert_eq!(c.view(false).line, "hllo");
		for _ in 0..300 {
			text(&mut c, "x");
		}
		assert_eq!(c.view(false).line.len(), MAX_CHAT_LEN);
	}

	#[test]
	fn backspace_ends_suppression() {
		let mut c = Chat::default();
		c.open();
		// A user who opens and immediately backspaces then types must
		// not lose the first typed char to the stale suppression.
		key(&mut c, Key::Backspace);
		text(&mut c, "a");
		assert_eq!(c.view(false).line, "a");
	}
}
