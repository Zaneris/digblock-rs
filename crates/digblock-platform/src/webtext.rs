//! The web soft-keyboard bridge: a hidden `<input>` whose focus is what
//! makes a phone raise its keyboard, forwarding what the user types into
//! the engine's ordinary `InputEvent::Text`/`Key` vocabulary.
//!
//! Lives in the platform crate rather than the launcher because the
//! focus call has to run synchronously inside the tap's own winit event
//! dispatch — the only context a mobile browser honours it in — and
//! app.rs is the only code positioned there (see `after_game_input`).
//!
//! The input is on-screen but invisible: parked off-screen, iOS scrolls
//! the page to it when the keyboard rises. Its value is kept permanently
//! empty (every edit is forwarded and then suppressed), so no IME state
//! accumulates and the charset rules stay engine-side where they belong.

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::app::UserEvent;
use crate::input::Key;

pub(crate) struct TextBridge {
	input: web_sys::HtmlInputElement,
	focused: bool,
}

impl TextBridge {
	pub(crate) fn new(proxy: winit::event_loop::EventLoopProxy<UserEvent>) -> Option<Self> {
		let document = web_sys::window()?.document()?;
		let input = document
			.create_element("input")
			.ok()?
			.dyn_into::<web_sys::HtmlInputElement>()
			.ok()?;
		input
			.set_attribute(
				"style",
				"position:fixed;top:0;left:0;width:1px;height:1px;\
				 opacity:0;border:none;padding:0;background:transparent;",
			)
			.ok()?;
		for (attr, v) in [
			("autocapitalize", "off"),
			("autocomplete", "off"),
			("autocorrect", "off"),
			("spellcheck", "false"),
			("enterkeyhint", "done"),
		] {
			input.set_attribute(attr, v).ok()?;
		}
		document.body()?.append_child(&input).ok()?;

		// Enter and Backspace arrive as keydowns where the keyboard
		// identifies them; preventing their default also stops the
		// matching beforeinput, so nothing is delivered twice. Printable
		// keys pass through untouched and arrive as beforeinput below.
		let p = proxy.clone();
		let on_key =
			Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
				let key = match e.key().as_str() {
					"Enter" => Key::Enter,
					"Backspace" => Key::Backspace,
					_ => return,
				};
				e.prevent_default();
				let _ = p.send_event(UserEvent::TextKey(key));
			});
		input
			.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref())
			.ok()?;
		on_key.forget();

		// The produced text. Some IMEs identify no keys at all in keydown
		// ("Unidentified"), so their backspace lands here instead.
		let p = proxy;
		let on_before =
			Closure::<dyn FnMut(web_sys::InputEvent)>::new(move |e: web_sys::InputEvent| {
				match e.input_type().as_str() {
					"insertText" | "insertFromPaste" => {
						if let Some(data) = e.data() {
							for ch in data.chars().filter(|c| !c.is_control()) {
								let _ = p.send_event(UserEvent::TextChar(ch));
							}
						}
					}
					"deleteContentBackward" => {
						let _ = p.send_event(UserEvent::TextKey(Key::Backspace));
					}
					_ => {}
				}
				// Forwarded, so never applied: the value stays empty.
				e.prevent_default();
			});
		input
			.add_event_listener_with_callback("beforeinput", on_before.as_ref().unchecked_ref())
			.ok()?;
		on_before.forget();

		Some(Self {
			input,
			focused: false,
		})
	}

	/// Follow the game's text-field state. The rising edge must run
	/// inside the activating gesture's dispatch — focusing is what raises
	/// the keyboard, and a mobile browser only honours it there. The
	/// falling edge blurs, which is what dismisses it.
	pub(crate) fn sync(&mut self, active: bool) {
		if active == self.focused {
			return;
		}
		self.focused = active;
		if active {
			let _ = self.input.focus();
		} else {
			let _ = self.input.blur();
		}
	}
}
