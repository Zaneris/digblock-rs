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
	/// Tab completed a command: show this usage line as a feed hint.
	Hint(String),
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
	/// Server-advertised `(usage, description)` command pairs (admin
	/// only; empty otherwise). The client bakes in no command knowledge —
	/// Tab completes against usage's first token.
	commands: Vec<(String, String)>,
	/// Tab-cycle state: the stem being completed and the next match
	/// index. Cleared by any other edit.
	cycle: Option<(String, usize)>,
}

impl Chat {
	pub fn is_open(&self) -> bool {
		self.open
	}

	/// Install the server's command set (`ServerMsg::AdminCommands`).
	pub fn set_commands(&mut self, commands: Vec<(String, String)>) {
		self.commands = commands;
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
				// Same alphabet the server sanitizes to, so nothing a
				// player can type gets silently eaten on the way out.
				if self.buffer.chars().count() < MAX_CHAT_LEN
					&& digblock_net::protocol::is_text_char(*ch)
				{
					self.buffer.push(*ch);
					self.cycle = None;
				}
				None
			}
			InputEvent::Key { key, pressed: true } => match key {
				Key::Backspace => {
					self.suppress_text = false;
					self.buffer.pop();
					self.cycle = None;
					None
				}
				Key::Tab => {
					self.suppress_text = false;
					self.complete()
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

	/// Tab: prefix-complete the command token against the advertised
	/// set, cycling through matches on repeated presses. Only while the
	/// buffer is a bare `/stem` (no arguments yet); no-op when nothing
	/// was advertised (non-admin, or an older server).
	fn complete(&mut self) -> Option<ChatAction> {
		if !self.buffer.starts_with('/') || self.buffer.contains(' ') {
			return None;
		}
		// Repeated Tab keeps completing the ORIGINAL stem; any edit
		// cleared `cycle`, so a stale stem can't resurrect.
		let (stem, idx) = match self.cycle.take() {
			Some(state) => state,
			None => (self.buffer.clone(), 0),
		};
		let matches: Vec<&(String, String)> = self
			.commands
			.iter()
			.filter(|(usage, _)| {
				usage
					.split_ascii_whitespace()
					.next()
					.is_some_and(|name| name.starts_with(&stem))
			})
			.collect();
		let (usage, desc) = matches.get(idx % matches.len().max(1))?;
		let name = usage.split_ascii_whitespace().next().unwrap_or(usage);
		self.buffer = name.to_string();
		self.cycle = Some((stem, idx + 1));
		Some(ChatAction::Hint(format!("{usage} - {desc}")))
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
	fn caps_length_and_drops_untypeable_chars() {
		let mut c = Chat::default();
		c.open();
		c.suppress_text = false;
		// Accented Latin is typeable now; controls and other scripts
		// still are not.
		text(&mut c, "héllo\u{7}Ω");
		assert_eq!(c.view(false).line, "héllo");
		for _ in 0..300 {
			text(&mut c, "x");
		}
		// The cap counts characters, not bytes — the "é" above is two.
		assert_eq!(c.view(false).line.chars().count(), MAX_CHAT_LEN);
	}

	fn with_commands() -> Chat {
		let mut c = Chat::default();
		c.set_commands(vec![
			("/help".into(), "list admin commands".into()),
			("/ban <name>".into(), "ban by IP".into()),
			("/ban-list".into(), "list bans".into()),
			("/tp <x> <z>".into(), "teleport".into()),
		]);
		c.open();
		c.suppress_text = false;
		c
	}

	#[test]
	fn tab_completes_and_cycles_matches() {
		let mut c = with_commands();
		text(&mut c, "/b");
		let hint = key(&mut c, Key::Tab);
		assert_eq!(c.view(false).line, "/ban");
		assert_eq!(
			hint,
			Some(ChatAction::Hint("/ban <name> - ban by IP".into()))
		);
		// Repeated Tab cycles the ORIGINAL stem's matches.
		key(&mut c, Key::Tab);
		assert_eq!(c.view(false).line, "/ban-list");
		key(&mut c, Key::Tab);
		assert_eq!(c.view(false).line, "/ban");
		// Typing resumes normal editing after a completion.
		text(&mut c, " ted");
		assert_eq!(c.view(false).line, "/ban ted");
	}

	#[test]
	fn tab_is_a_no_op_without_commands_or_slash_or_with_args() {
		let mut c = Chat::default();
		c.open();
		c.suppress_text = false;
		text(&mut c, "/b");
		assert_eq!(key(&mut c, Key::Tab), None); // nothing advertised
		assert_eq!(c.view(false).line, "/b");

		let mut c = with_commands();
		text(&mut c, "hello");
		assert_eq!(key(&mut c, Key::Tab), None); // not a command
		assert_eq!(c.view(false).line, "hello");

		let mut c = with_commands();
		text(&mut c, "/tp 1");
		assert_eq!(key(&mut c, Key::Tab), None); // already past the stem
		assert_eq!(c.view(false).line, "/tp 1");
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
