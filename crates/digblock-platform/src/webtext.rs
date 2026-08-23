//! The web soft-keyboard bridge: a hidden `<input>` whose focus is what makes a
//! phone raise its keyboard. While a field is active the DOM input is the
//! AUTHORITATIVE buffer: every `input` event restates the whole value and the
//! engine replaces the field with its own filtered reading (`Game::text_replace`),
//! so IME composition, paste, autocorrect and caret edits all arrive by one
//! path and the charset rules stay engine-side. Forwarding per-key events
//! instead is a trap: Android IMEs compose plain typing, delivering neither
//! keydowns nor `insertText`. Named keys (Enter, Tab, arrows) still travel as
//! keydowns — they navigate rather than edit. Lives in the platform crate
//! because focus must run inside the tap's own dispatch (`after_game_input`).
//! Taking DOM focus is the one side effect the rest of the shell must see:
//! winit gates device events and canvas keydowns on the CANVAS holding
//! focus, so the falling edge hands focus back to it (`owns_focus` tells the
//! `Focused(false)` arm the rising edge's blur was ours, not a real loss).

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::app::UserEvent;
use crate::input::Key;

pub(crate) struct TextBridge {
	input: web_sys::HtmlInputElement,
	/// The game-side state `sync` last saw; the falling edge clears and
	/// blurs, which is what dismisses the keyboard.
	active: bool,
	/// Real DOM focus, from the input's own focus/blur listeners. winit
	/// re-focuses the canvas on every pointerdown, so game-side state
	/// alone goes stale the moment the player taps anywhere; `sync`
	/// re-takes focus whenever the field is active and the DOM disagrees.
	dom_focused: Rc<Cell<bool>>,
}

impl TextBridge {
	pub(crate) fn new(proxy: winit::event_loop::EventLoopProxy<UserEvent>) -> Option<Self> {
		let document = web_sys::window()?.document()?;
		let input = document
			.create_element("input")
			.ok()?
			.dyn_into::<web_sys::HtmlInputElement>()
			.ok()?;
		// On-screen but invisible: parked at the top corner, iOS scrolls
		// the page to it when the keyboard rises.
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

		let dom_focused = Rc::new(Cell::new(false));
		for (event, val) in [("focus", true), ("blur", false)] {
			let flag = dom_focused.clone();
			let on_edge = Closure::<dyn FnMut()>::new(move || flag.set(val));
			input
				.add_event_listener_with_callback(event, on_edge.as_ref().unchecked_ref())
				.ok()?;
			on_edge.forget();
		}

		// Keys that navigate rather than edit. Enter/Tab/arrows prevent
		// their defaults (submit-ish behaviour, focus traversal, caret
		// movement) so the input's only job stays holding the text;
		// Escape's default (leaving fullscreen) is not preventable anyway.
		let p = proxy.clone();
		let on_key =
			Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
				let (key, prevent) = match e.key().as_str() {
					"Enter" => (Key::Enter, true),
					"Tab" => (Key::Tab, true),
					"Escape" => (Key::Escape, false),
					"ArrowUp" => (Key::Up, true),
					"ArrowDown" => (Key::Down, true),
					"ArrowLeft" => (Key::Left, true),
					"ArrowRight" => (Key::Right, true),
					_ => return,
				};
				if prevent {
					e.prevent_default();
				}
				let _ = p.send_event(UserEvent::TextKey(key));
			});
		input
			.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref())
			.ok()?;
		on_key.forget();

		// Some IMEs deliver Enter as a line-break edit instead of a
		// keydown; catch it before it lands in the value.
		let p = proxy.clone();
		let on_before =
			Closure::<dyn FnMut(web_sys::InputEvent)>::new(move |e: web_sys::InputEvent| {
				if e.input_type() == "insertLineBreak" {
					e.prevent_default();
					let _ = p.send_event(UserEvent::TextKey(Key::Enter));
				}
			});
		input
			.add_event_listener_with_callback("beforeinput", on_before.as_ref().unchecked_ref())
			.ok()?;
		on_before.forget();

		// Every edit, whatever produced it, restates the whole value.
		// `input` (not `beforeinput`) so composition updates are read
		// AFTER the IME committed them to the value.
		let src = input.clone();
		let on_input = Closure::<dyn FnMut()>::new(move || {
			let _ = proxy.send_event(UserEvent::TextValue(src.value()));
		});
		input
			.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref())
			.ok()?;
		on_input.forget();

		Some(Self {
			input,
			active: false,
			dom_focused,
		})
	}

	/// Follow the game's text-field state. The rising edge must run inside the activating
	/// gesture's dispatch — focusing raises the keyboard, and a mobile browser only honours it
	/// there; any later tap reaching here re-takes the focus winit's canvas grab just stole. The
	/// falling edge returns focus to `canvas`, since a blur alone leaves it on the body where
	/// winit delivers neither motion nor keys; a canvas is not editable, so the keyboard drops.
	pub(crate) fn sync(
		&mut self,
		active: bool,
		contents: impl FnOnce() -> String,
		canvas: &web_sys::HtmlElement,
	) {
		if active {
			if !self.active || !self.dom_focused.get() {
				self.active = true;
				self.reseed(contents());
				let _ = self.input.focus();
			}
		} else if self.active {
			self.active = false;
			self.input.set_value("");
			let _ = self.input.blur();
			let _ = canvas.focus();
		}
	}

	/// Whether the bridge holds, or is taking, DOM focus on the game's
	/// behalf. Keyed on the game-side edge rather than `dom_focused`: the
	/// canvas's blur is dispatched BEFORE the input's focus, so the DOM flag
	/// is still false at the moment the shell asks.
	pub(crate) fn owns_focus(&self) -> bool {
		self.active
	}

	/// Make the DOM buffer the engine's truth, caret at the end — on
	/// every focus take (so deletion can reach characters typed before
	/// the keyboard was last dismissed) and after a named key the engine
	/// may have edited the field with (Tab completion), where a stale
	/// buffer would restate overwritten text on the next keystroke.
	pub(crate) fn reseed(&self, contents: String) {
		let len = contents.encode_utf16().count() as u32;
		self.input.set_value(&contents);
		let _ = self.input.set_selection_range(len, len);
	}
}
