//! Every key, click and touch the platform delivers. While ANY UI
//! surface is open, cursor and left-button are synthesized from the
//! primary touch at the top of `handle_input` — one site gives iOS
//! every screen, and no screen knows about touch.

use super::*;

impl Engine {
	pub(super) fn handle_input(&mut self, event: InputEvent) {
		// A capture hears nothing. Benchmarks must be input-independent
		// to stay comparable, and every other capture path wants it for a
		// plainer reason: the operator is sitting at the machine that
		// opened the window, and a keystroke meant for their editor must
		// not open the inventory into the frame. `InputEvent` is only
		// ever user input — resize, scale-factor and lifecycle arrive
		// through their own `Game` methods — so dropping all of it here
		// costs a capture nothing it needs.
		if !self.config.interactive {
			return;
		}
		// UI surfaces speak pointer. While any is open, the PRIMARY touch
		// synthesizes cursor + left-button events at this one site, so
		// iOS gets menu taps, inventory drags, and the extract-slot drag
		// (design-doc M2 debt) without any screen learning about touch.
		// Gameplay touch (look/move) keeps its own arm below.
		if let InputEvent::Touch { id, phase, x, y } = event {
			// A HUD-claimed touch is consumed to its end wherever its later
			// events land — the button it pressed may have rerouted them
			// (opening the menu sends this id's Moved/Ended to the screen
			// path, which must not treat them as a fresh pointer).
			if let Some(i) = self.hud_touches.iter().position(|&(hid, _)| hid == id) {
				if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
					self.hud_touches.swap_remove(i);
					self.hud_jump_held = self
						.hud_touches
						.iter()
						.any(|&(_, b)| b == crate::ui::hud::HudButton::Jump);
				}
				return;
			}
			// The corner button doubles as an open screen's close control:
			// I/K/C/Escape are its only other closes, none of which a phone
			// has. Checked ahead of the pointer synthesis so the tap cannot
			// also land as a click inside the screen.
			//
			// The MENU gets it too, as its back control. Its own rows do not
			// cover this — the server browser and the world list have no Back
			// row, and the hint under them names a key a phone does not have
			// — which left a finger that opened Multiplayer with no way out
			// of it at all. `can_go_back` is what keeps the button off the
			// locked title screen, where Escape is deliberately inert.
			if self.config.touch_hud
				&& matches!(phase, TouchPhase::Started)
				&& (self.menu.can_go_back()
					|| (!self.menu.is_open()
						&& (self.inv_screen.is_open()
							|| self.alch_screen.is_open()
							|| self.char_screen.is_open())))
			{
				let (px, py) = (x * self.size.0 as f32, y * self.size.1 as f32);
				if crate::ui::hud::corner_button_rect(self.size, self.ui_scale, &self.safe_area)
					.contains(px, py)
				{
					self.hud_touches
						.push((id, crate::ui::hud::HudButton::Corner));
					if self.menu.is_open() {
						// The same door the key opens, so a page that cancels
						// a connect or a sign-in on the way out still does.
						self.menu.handle_input(
							&InputEvent::Key {
								key: Key::Escape,
								pressed: true,
							},
							self.size,
							self.ui_scale,
						);
					} else {
						self.inv_screen.close();
						self.alch_screen.close();
						self.char_screen.close();
					}
					return;
				}
			}
			if self.menu.is_open()
				|| self.inv_screen.is_open()
				|| self.alch_screen.is_open()
				|| self.char_screen.is_open()
			{
				let (px, py) = (x * self.size.0 as f32, y * self.size.1 as f32);
				match phase {
					TouchPhase::Started if self.ui_touch.is_none() => {
						self.ui_touch = Some(id);
						self.input(InputEvent::CursorMoved { x: px, y: py });
						self.input(InputEvent::MouseButton {
							button: MouseButton::Left,
							pressed: true,
						});
					}
					TouchPhase::Moved if self.ui_touch == Some(id) => {
						self.input(InputEvent::CursorMoved { x: px, y: py });
					}
					TouchPhase::Ended | TouchPhase::Cancelled if self.ui_touch == Some(id) => {
						self.ui_touch = None;
						self.input(InputEvent::CursorMoved { x: px, y: py });
						self.input(InputEvent::MouseButton {
							button: MouseButton::Left,
							pressed: false,
						});
					}
					_ => {} // secondary touches park while a UI is up
				}
				return;
			}
			// No UI up: any lingering primary marker is stale.
			self.ui_touch = None;
		}
		// Menu open: every input belongs to the menu — gameplay keys must
		// not leak (typing a name would also walk). FocusLost still clears
		// held state.
		if self.menu.is_open() {
			// The menu can surface underneath the inventory (disconnect,
			// leave-server) — the screens yield rather than stacking.
			self.inv_screen.close();
			self.alch_screen.close();
			self.char_screen.close();
			if matches!(event, InputEvent::FocusLost | InputEvent::PointerLockLost) {
				self.keys = HeldKeys::default();
				self.clear_touch_state();
			} else {
				self.menu.handle_input(&event, self.size, self.ui_scale);
				// Drained HERE, not in `update`. A browser only honours a
				// clipboard write that happens inside the same task as the
				// click that asked for it; `update` runs in a later
				// animation frame, by which point the gesture is gone and
				// the write is silently refused. Every other menu intent
				// is drained in `update`, and this one cannot be.
				if let Some(text) = self.menu.take_clipboard()
					&& let Some(copy) = &mut self.clipboard
				{
					copy(&text);
				}
				// Same gesture rule as the clipboard: the shell's
				// fullscreen call is only honoured inside this task.
				self.fullscreen_requested |= self.menu.take_fullscreen();
			}
			return;
		}
		// Inventory open: cursor is free; drags go to the screen and the
		// returned actions mutate the core hotbar here. I/Escape close
		// (Escape is consumed — it must not also open the pause menu).
		if self.inv_screen.is_open() {
			match event {
				// Focus loss abandons the drag but keeps the screen open
				// (a cmd-tab shouldn't slam it shut — menu parity).
				InputEvent::FocusLost | InputEvent::PointerLockLost => {
					self.keys = HeldKeys::default();
					self.clear_touch_state();
					self.dig_held = false;
					self.inv_screen.cancel_drag();
				}
				InputEvent::Key {
					key: Key::I | Key::Escape,
					pressed: true,
				} => self.inv_screen.close(),
				_ => {
					let bar = crate::hotbar::HotbarView::build(
						&self.hotbar,
						&self.inventory,
						&self.tool_swap,
					);
					let style = crate::ui::Style::new(self.ui_scale);
					let act = self.inv_screen.handle_input(
						&event,
						&self.ui_hits,
						&style,
						&self.inv_grid,
						&bar,
					);
					if let Some(act) = act {
						self.apply_inv_action(act);
					}
				}
			}
			return;
		}
		// Alchemy open: cursor is free; clicks resolve to convert/craft
		// actions applied below. K/Escape close (Escape consumed like the
		// inventory's).
		if self.alch_screen.is_open() {
			match event {
				InputEvent::FocusLost | InputEvent::PointerLockLost => {
					self.keys = HeldKeys::default();
					self.clear_touch_state();
					self.dig_held = false;
					let view =
						crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
					self.alch_screen
						.handle_input(&InputEvent::FocusLost, &self.ui_hits, &view);
				}
				InputEvent::Key {
					key: Key::K | Key::Escape,
					pressed: true,
				} => self.alch_screen.close(),
				_ => {
					let view =
						crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
					let act = self.alch_screen.handle_input(&event, &self.ui_hits, &view);
					if let Some(act) = act {
						self.apply_alchemy_action(act);
					}
				}
			}
			return;
		}
		// Character screen open: read-only, so the only inputs it eats
		// are the ones that close it (C/Escape, consumed like the
		// others') and the held-state hygiene on focus loss.
		if self.char_screen.is_open() {
			match event {
				InputEvent::FocusLost | InputEvent::PointerLockLost => {
					self.keys = HeldKeys::default();
					self.clear_touch_state();
					self.dig_held = false;
				}
				InputEvent::Key {
					key: Key::C | Key::Escape,
					pressed: true,
				} => self.char_screen.close(),
				_ => {}
			}
			return;
		}
		// Chat open: same containment as the menu, but the pointer stays
		// grabbed (the shell delivers Text via wants_text_input).
		if self.chat.is_open() {
			if matches!(event, InputEvent::FocusLost | InputEvent::PointerLockLost) {
				self.keys = HeldKeys::default();
				self.clear_touch_state();
				// Losing the lock also surfaces the pause menu; a chat
				// draft doesn't survive that.
				self.chat.handle_input(&InputEvent::Key {
					key: Key::Escape,
					pressed: true,
				});
				if matches!(event, InputEvent::PointerLockLost) {
					self.menu.open();
				}
			} else {
				match self.chat.handle_input(&event) {
					Some(crate::chat::ChatAction::Send(text)) => {
						if let Some(net) = self.net.as_mut() {
							net.send_chat(&text);
						}
					}
					Some(crate::chat::ChatAction::Hint(usage)) => self.push_feed(usage),
					Some(crate::chat::ChatAction::Closed) | None => {}
				}
			}
			return;
		}
		match event {
			InputEvent::Key { key, pressed } => match key {
				Key::W | Key::Up => self.keys.w = pressed,
				Key::S | Key::Down => self.keys.s = pressed,
				Key::A | Key::Left => self.keys.a = pressed,
				Key::D | Key::Right => self.keys.d = pressed,
				Key::Space => self.keys.up = pressed,
				Key::Shift => self.keys.down = pressed,
				Key::F => {
					// Fly is a single-player debug tool; the server only
					// simulates walking.
					if pressed && !self.fly_key_down && self.net.is_none() {
						self.walking = !self.walking;
						if !self.walking {
							// Enter fly where the player stands.
							self.camera.pos = self.player.pos;
						} else {
							// Re-enter walk at the camera position.
							self.player.pos = self.camera.pos;
							self.tick_lerp.reset(self.player.pos);
						}
						log::info!("mode: {}", if self.walking { "walk" } else { "fly" });
					}
					self.fly_key_down = pressed;
				}
				Key::Digit(n) => {
					if pressed {
						// §6: 9 on the already-selected tool slot cycles
						// the owned roster instead of reselecting it. The
						// resulting Layout diff carries the new pick to
						// the server before any edit that uses it.
						let tool_slot = digblock_core::hotbar::HOTBAR_SLOTS;
						if n as usize == tool_slot && self.hotbar.selected_index() == tool_slot - 1
						{
							let owned = self.owned_tools();
							self.hotbar.cycle_tool(&owned);
						} else {
							self.hotbar.select_digit(n);
						}
					}
				}
				Key::Backquote => {
					if pressed && !self.overlay_key_down {
						self.overlay_visible = !self.overlay_visible;
					}
					self.overlay_key_down = pressed;
				}
				// F7: the GI ratchet drill. Clears the derived state that
				// only ever grows (the coarse mirror behind `sky_y`, the
				// emitter map) and re-queues every resident chunk, so both
				// rebuild from what is ACTUALLY resident.
				//
				// A degradation that develops over minutes of wandering and
				// never recovers is exactly what a ratchet looks like. If
				// fps climbs back after this, the cost was stale state and
				// the fix belongs in eviction; if it does not move, the cost
				// is the filled window itself and no bookkeeping fix exists.
				Key::F7 => {
					if pressed && !self.gi_drill_key_down {
						let (sky_before, emit_before) = self.renderer_gi_state();
						if let Some(r) = self.renderer.as_mut() {
							r.gi_clear_derived_state();
						}
						self.pipeline.gi_redirty_all();
						log::info!(
							"F7 GI drill: cleared derived state (sky_y {sky_before:.0}, \
							 {emit_before} emitters) — watch both rebuild"
						);
					}
					self.gi_drill_key_down = pressed;
				}
				Key::Escape => {
					if pressed {
						self.dig_held = false;
						self.menu.open();
					}
				}
				Key::I => {
					if pressed {
						self.keys = HeldKeys::default();
						self.dig_held = false;
						self.inv_screen.open();
					}
				}
				Key::K => {
					if pressed {
						self.keys = HeldKeys::default();
						self.dig_held = false;
						self.alch_screen.open();
					}
				}
				Key::C => {
					if pressed {
						self.keys = HeldKeys::default();
						self.dig_held = false;
						self.char_screen.open();
					}
				}
				// Home: same request the menu row raises; update() gates
				// it on the shared cooldown.
				Key::H => {
					if pressed {
						self.return_spawn_requested = true;
					}
				}
				// Chat is multiplayer-only — there is no one to hear a
				// single-player message.
				Key::T | Key::Enter if pressed && self.net.is_some() => {
					self.keys = HeldKeys::default();
					self.dig_held = false;
					self.chat.open();
				}
				_ => {}
			},
			InputEvent::MouseDelta { dx, dy } => {
				self.look_accum.0 += dx as f32;
				self.look_accum.1 += dy as f32;
			}
			InputEvent::Scroll { dy } => self.hotbar.scroll(dy),
			InputEvent::MouseButton { button, pressed } => match button {
				MouseButton::Left => self.dig_held = pressed,
				MouseButton::Right => {
					// An edge, always: right-click interacts or places, and
					// a charged bucket pours through exactly that path.
					if pressed {
						self.click_place = true;
					}
				}
				MouseButton::Other => {}
			},
			InputEvent::FocusLost => {
				self.keys = HeldKeys::default();
				self.clear_touch_state();
				self.dig_held = false;
			}
			InputEvent::PointerLockLost => {
				self.keys = HeldKeys::default();
				self.clear_touch_state();
				self.dig_held = false;
				// Web parity with desktop Escape: a browser-initiated unlock
				// pauses into the menu (no-op when the menu is disabled — the
				// shell's click-to-recapture covers that case).
				self.menu.open();
			}
			// Only arrive ungrabbed (menu transitions); nothing in-game
			// consumes them.
			InputEvent::CursorMoved { .. } | InputEvent::Text { .. } => {}
			InputEvent::Touch { id, phase, x, y } => {
				// HUD buttons claim their touch before the driver can turn
				// it into look or movement (later events of a claimed id are
				// consumed at the top of `input`).
				if self.config.touch_hud && matches!(phase, TouchPhase::Started) {
					use crate::ui::hud::HudButton;
					let (px, py) = (x * self.size.0 as f32, y * self.size.1 as f32);
					if let Some(b) = crate::ui::hud::touch_hit(
						self.size,
						self.ui_scale,
						&self.safe_area,
						self.config.touch_jump_button,
						px,
						py,
					) {
						match b {
							HudButton::Corner => {
								self.dig_held = false;
								self.clear_touch_state();
								self.menu.open();
							}
							// The same edge a right-click sets; use-beats-
							// place and the bucket pour ride along.
							HudButton::Place => self.click_place = true,
							HudButton::Jump => self.hud_jump_held = true,
							HudButton::Slot(i) => {
								// Mirror the digit keys, tool-slot cycle
								// included (§6: reselecting the tool cycles
								// the owned roster).
								let i = i as usize;
								if i == digblock_core::hotbar::HOTBAR_SLOTS - 1
									&& self.hotbar.selected_index() == i
								{
									let owned = self.owned_tools();
									self.hotbar.cycle_tool(&owned);
								} else {
									self.hotbar.select_digit(i as u8 + 1);
								}
							}
						}
						self.hud_touches.push((id, b));
						return;
					}
				}
				let phase = match phase {
					TouchPhase::Started => Phase::Started,
					TouchPhase::Moved => Phase::Moved,
					TouchPhase::Ended => Phase::Ended,
					TouchPhase::Cancelled => Phase::Cancelled,
				};
				self.touch.handle(id, phase, x, y, web_time::Instant::now());
			}
		}
	}
}
