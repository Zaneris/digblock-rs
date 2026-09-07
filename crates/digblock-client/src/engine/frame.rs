//! Assembling the frame and handing it to the renderer. Five passes,
//! the last of which clears the shared depth attachment so the
//! viewmodel can be drawn in its own narrow projection.

use super::*;

impl Engine {
	/// The sky cull's floor for this frame (`gi::sky_floor`), fed by the
	/// incremental column-top tally — see the field docs. A column still
	/// generating is skipped rather than defaulted, so it cannot vote a
	/// height that is not there; until the first pass completes the tally
	/// is empty and the cull stays disarmed.
	fn sky_floor(&mut self) -> Option<f32> {
		// The grid: [`gi::SKY_TALLY_REACH`]'s disc at this stride, walked
		// this many columns per frame — a full pass lands in about a
		// second, which is the lag streaming, digging and flying all see.
		// A discontinuity does NOT wait for it: `reset_sky_tally`.
		const STRIDE: i32 = 8;
		const PER_FRAME: usize = 8;
		// Rounded DOWN to the stride and back, so the disc the walk covers
		// is the disc the reach test admits — otherwise a reach that is not
		// a multiple of the stride silently samples a smaller circle than
		// it names.
		let r = (crate::gi::SKY_TALLY_REACH as i32 / STRIDE) * STRIDE;
		let w = (2 * (r / STRIDE) + 1) as usize;
		let cam = self.camera.pos;
		if self.sky_cursor == 0 {
			self.sky_origin = (cam.x as i32, cam.z as i32);
			self.sky_stage.clear();
		}
		let mut walked = 0;
		while self.sky_cursor < w * w && walked < PER_FRAME {
			let (i, j) = (self.sky_cursor % w, self.sky_cursor / w);
			self.sky_cursor += 1;
			let dx = (i as i32) * STRIDE - r;
			let dz = (j as i32) * STRIDE - r;
			if dx * dx + dz * dz > r * r {
				continue;
			}
			walked += 1;
			if let Some(top) = self
				.pipeline
				.column_top(self.sky_origin.0 + dx, self.sky_origin.1 + dz)
			{
				self.sky_stage.push(top);
			}
		}
		if self.sky_cursor >= w * w {
			std::mem::swap(&mut self.sky_tally, &mut self.sky_stage);
			self.sky_cursor = 0;
		}
		crate::gi::sky_floor(cam.y, &mut self.sky_tally)
	}

	/// Throw the sky-cull tally away because the camera is no longer over the ground that filled
	/// it. The incremental walk is right for a discontinuity it can SEE — a stride, a stream, a
	/// dig — and wrong for one it cannot: a teleport or a world swap leaves a floor describing
	/// terrain the player has left, for the rest of the running pass plus a whole one after it,
	/// and the cull is absolute rather than a ranking. Port down off a highland and the stale
	/// high floor culls the arrival pad's own lights at the moment somebody is looking at them.
	/// Clearing rather than re-seeding, because the disarmed state is the safe one to be wrong in.
	pub(super) fn reset_sky_tally(&mut self) {
		self.sky_tally.clear();
		self.sky_stage.clear();
		self.sky_cursor = 0;
	}

	pub(super) fn draw(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32)) {
		self.ui.size = size;
		self.recompute_ui_scale();
		// Streaming happens here (not update) because uploads need the GPU.
		self.pump_pipeline(gpu);
		// Everything from here to the encode — remote sampling, name tags,
		// menu/UI rebuild, frame uniforms — used to be untimed, so a spike
		// in any of it showed up in `total_ms` and in no phase at all.
		self.bench.prep_start = Some(web_time::Instant::now());
		// Remote players, sampled an interpolation delay in the past;
		// the instance list rides FrameScene and re-uploads every frame.
		self.vfx.remote_instances.clear();
		self.vfx.held_instances.clear();
		self.vfx.carried.begin();
		// Your own hand has no world position — the viewmodel is authored
		// in view space — so the LIGHT is body-anchored exactly like
		// everyone else's (`gi::carried_origin`). Keyed off the item the
		// renderer DRAWS rather than the hotbar's selection, so the light
		// and the visible torch arrive and leave together through the swap
		// dip; and read unconditionally, because opening the inventory
		// puts the hand away, not the torch out.
		let held_icon = self
			.vfx
			.view_swap
			.shown()
			.and_then(crate::ui::hud::icon_block);
		if matches!(self.mode, CameraMode::Cinematic(_)) {
			// Detached: the body is ON SCREEN, drawn through the remote
			// path (body + held item rendering already exist there); its
			// carried light rides that push under the SELF key, so the
			// torch stays on the body rather than flying with the camera.
			let d = crate::camera::dir_from(self.look.yaw_deg, 0.0);
			push_remote(
				&mut self.vfx.remote_instances,
				&mut self.vfx.held_instances,
				&mut self.vfx.carried,
				&Remote {
					key: CARRIER_SELF,
					id: self.session.my_player_id.unwrap_or(0),
					pos: self.fp_eye,
					yaw: d.x.atan2(d.z),
					held: held_icon.map_or(0, |b| b as u16),
					fx: 0,
				},
			);
		} else {
			self.vfx.carried.push(
				CARRIER_SELF,
				self.body_anchor(),
				held_icon,
				self.skills.level(digblock_core::skills::Skill::Mining),
			);
		}
		// The wisp: a second light on the same body while it burns.
		if self.wisp_left > 0.0 {
			let (level, whitened) = digblock_core::unlock::wisp_light(
				self.skills.level(digblock_core::skills::Skill::Arcana),
			);
			self.vfx.carried.push_arcane(
				carried::CARRIER_WISP,
				digblock_core::gi::carried_origin(self.body_anchor()),
				level,
				whitened,
			);
		}
		// Every kindled light: a standing entry under its own key, at
		// the row's own level — nobody's Arcana is attached to it.
		for k in &self.vfx.kindled {
			self.vfx.carried.push_arcane(
				carried::KINDLED_BANK | k.key,
				digblock_core::kindle::origin(k.cell),
				0,
				false,
			);
		}
		if let Some(net) = self.session.client.as_ref() {
			net.remotes.sample(&mut self.vfx.remote_scratch);
			for i in 0..self.vfx.remote_scratch.len() {
				let d = self.vfx.remote_scratch[i];
				push_remote(
					&mut self.vfx.remote_instances,
					&mut self.vfx.held_instances,
					&mut self.vfx.carried,
					&Remote {
						key: u16::from(d.id),
						id: d.id,
						pos: d.pos,
						yaw: d.yaw,
						held: d.held,
						fx: d.fx,
					},
				);
			}
		}
		// DIG_PLAYERBOX=1: park a fake remote player a few blocks ahead —
		// a GPU-path check that needs no second client (held cube: dirt).
		if let Some(held) = self.config.debug_playerbox {
			// Flat-forward, not along the camera's pitch: a remote player
			// stands on the ground rather than hovering wherever you happen
			// to be looking.
			let d = self.camera.dir();
			let flat = Vec3::new(d.x, 0.0, d.z).normalize_or_zero();
			let p = self.camera.pos + flat * 5.0;
			// Facing the camera, not a fixed yaw. The hand is on one side
			// of the body, so a yaw the capture does not control decides
			// whether the held item is on screen at all — and it hid the
			// item entirely once the hand moved to the correct side.
			let yaw = (-flat.x).atan2(-flat.z);
			push_remote(
				&mut self.vfx.remote_instances,
				&mut self.vfx.held_instances,
				&mut self.vfx.carried,
				&Remote {
					key: CARRIER_DEBUG,
					id: 2,
					pos: p,
					yaw,
					held,
					fx: 0,
				},
			);
		}
		self.vfx.carried.finish(self.body_anchor());
		let cam_fx = self.vfx.camera_fx.offset();
		let mut frame = self.frame_uniforms(&cam_fx);
		if let Some(age) = self.config.debug_fx {
			frame.light_color[3] = age * 4.0;
		}
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		let view_proj = self.camera.view_proj_with(aspect, &cam_fx);
		// Name tags: project each nearby remote's head-top to screen px.
		// No depth (overlay) — tags read through walls within range.
		const NAMETAG_RANGE: f32 = 32.0;
		// Entries past `tags` keep their String buffers for next frame; the
		// live slice is `[..tags]`.
		let mut tags = 0usize;
		for d in &self.vfx.remote_scratch {
			if self.session.client.is_none() || d.pos.distance(self.camera.pos) > NAMETAG_RANGE {
				continue;
			}
			// Remote pos is the eye; the box top sits just above it, and the
			// tag floats above that.
			let clip = view_proj * (d.pos + Vec3::Y * 0.55).extend(1.0);
			if clip.w <= 0.0 {
				continue;
			}
			let ndc = clip.truncate() / clip.w;
			if ndc.x.abs() > 1.2 || ndc.y.abs() > 1.2 {
				continue;
			}
			// Reuse the slot's String rather than allocating a fresh one
			// per visible player per frame.
			if tags == self.vfx.nametag_scratch.len() {
				self.vfx.nametag_scratch.push(crate::renderer::NameTag {
					x: 0.0,
					y: 0.0,
					color: 0,
					mastery: 0,
					text: String::new(),
				});
			}
			let tag = &mut self.vfx.nametag_scratch[tags];
			tag.x = (ndc.x * 0.5 + 0.5) * size.0 as f32;
			tag.y = (1.0 - (ndc.y * 0.5 + 0.5)) * size.1 as f32;
			tag.color = pack_rgb(player_color(d.id));
			tag.mastery = d.mastery;
			tag.text.clear();
			match self.session.player_names.get(&d.id) {
				Some(n) => tag.text.push_str(n),
				None => {
					use std::fmt::Write;
					let _ = write!(tag.text, "Player {}", d.id);
				}
			}
			tags += 1;
		}
		self.vfx.nametag_count = tags;
		// Overlay data is last frame's numbers (metrics.frame() runs at the
		// end of this fn) and GPU ms lags the readback ring — fine for a
		// debug HUD.
		let overlay_stats = if self.ui.overlay_visible {
			self.bench
				.metrics
				.history_tail(crate::overlay::GRAPH_FRAMES, &mut self.vfx.history_scratch);
			let mut stats = self.bench.metrics.overlay_stats();
			// The reproduction line. `body_pos` and not `camera.pos`: the
			// camera carries the tick lerp and the reconciliation offset,
			// and the number here has to be the one `--pos` takes back.
			stats.world = digblock_core::metrics::WorldStats {
				pos: self.body_pos().to_array(),
				yaw_deg: self.camera.yaw_deg,
				pitch_deg: self.camera.pitch_deg,
				tod_deg: self.sun.tod,
				seed: self.seed.value,
			};
			Some(stats)
		} else {
			None
		};
		let menu_view = self.ui.menu.is_open().then(|| {
			self.ui.menu.view(
				size,
				self.ui.ui_scale,
				((self.time_secs * 2.0) as u32).is_multiple_of(2),
			)
		});
		// HUD state gates with the crosshair: hidden under the menu and in
		// bench captures' default state comparability is preserved (bench
		// never digs, hotbar renders its fixed default row).
		let in_game = menu_view.is_none();
		// A running cinematic owns the whole screen: no crosshair, hotbar,
		// hand or touch cluster — the shot is the interface.
		let cinematic = matches!(self.mode, CameraMode::Cinematic(_));
		// A capture's parked hover: the tile exists in LAST frame's hit
		// map, so it is fed here, before the clear, as the cursor move it
		// would have been.
		if let Some(id) = self.ui.debug_hover
			&& let Some(r) = self.ui.ui_hits.rect_of(id)
		{
			self.ui.debug_hover = None;
			// Straight to the screen: a capture is not interactive, and
			// `Engine::input` drops everything for one that is not.
			let ev = digblock_platform::InputEvent::CursorMoved {
				x: r.x + r.w / 2.0,
				y: r.y + r.h / 2.0,
			};
			if self.ui.inv_screen.is_open() {
				let bar = crate::hotbar::HotbarView::build(
					&self.hotbar,
					&self.inventory,
					&self.input.tool_swap,
				);
				let style = crate::ui::Style::new(self.ui.ui_scale);
				self.ui.inv_screen.handle_input(
					&ev,
					&self.ui.ui_hits,
					&style,
					&self.inv_grid,
					&bar,
				);
			} else if self.ui.alch_screen.is_open() {
				let view =
					crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
				self.ui
					.alch_screen
					.handle_input(&ev, &self.ui.ui_hits, &view);
			} else if self.ui.spellbook.is_open() {
				let ports = self.known_ports();
				let view = crate::ui::screens::spellbook::SpellView::build(
					&self.skills,
					self.save.teleport_cooldown.left().ceil() as u32,
					&ports,
					&self.inventory,
					self.causeway_left.ceil() as u32,
				);
				let style = crate::ui::Style::new(self.ui.ui_scale);
				self.ui.spellbook.handle_input(
					&ev,
					&self.ui.ui_hits,
					&style,
					&view,
					&self.spell_bar,
				);
			} else if self.ui.char_screen.is_open() {
				self.ui.char_screen.handle_input(&ev);
			}
		}
		self.ui.ui_dl.clear();
		self.ui.ui_hits.clear();
		// The menus' full-screen dim fades with the world's own darkness
		// (night scenes shouldn't be double-darkened); the sample is pure
		// and cheap, so re-deriving it here beats plumbing it out of
		// frame_uniforms.
		let daylight =
			1.0 - digblock_core::sky::Atmosphere::sample(self.sun.elevation()).star_intensity;
		if let Some(view) = &menu_view {
			view.emit(
				&mut self.ui.ui_dl,
				&crate::ui::Style::new(self.ui.ui_scale),
				size,
				daylight,
			);
			// The menu's back control on a touch device — the same corner
			// button the in-game screens close with, so one control means
			// one thing everywhere. Drawn after the panel so it sits over
			// the dim, and only where Escape has somewhere to go.
			if self.config.touch_hud && self.ui.menu.can_go_back() {
				crate::ui::hud::corner_button(
					&mut self.ui.ui_dl,
					self.ui.ui_scale,
					size,
					&self.input.safe_area,
					true,
				);
			}
		}
		// The editor draws its own overlay: crosshair + the side panel, no
		// hotbar, spell bar or pages.
		if self.config.debug_editor {
			crate::ui::hud::crosshair(&mut self.ui.ui_dl, size, self.ui.ui_scale);
			self.editor_panel(size);
		} else if in_game && !cinematic {
			let hotbar_view = crate::hotbar::HotbarView::build(
				&self.hotbar,
				&self.inventory,
				&self.input.tool_swap,
			);
			let owned_tools = self.owned_tools();
			// Hoisted before the DrawList borrow: `known_ports` reads self.
			// The spell readiness view is built every in-game frame — the
			// bar's fades, badges and tooltips need it whether or not the
			// spellbook is up, and the open book reuses the same view.
			let ports = self.known_ports();
			let cooldown = self.save.teleport_cooldown.left().ceil() as u32;
			let spell_view = crate::ui::screens::spellbook::SpellView::build(
				&self.skills,
				cooldown,
				&ports,
				&self.inventory,
				self.causeway_left.ceil() as u32,
			);
			let spells = crate::spellbar::SpellBarView::build(
				&self.spell_bar,
				&spell_view,
				cooldown,
				self.input.safe_area,
			);
			// Hoisted before the DrawList borrow, like `ports`.
			let page_open = self.open_page().is_some();
			let pick_toggle = self.pick_toggle_visible().then(|| {
				let width = digblock_core::unlock::pick_cells(
					self.skills.level(digblock_core::skills::Skill::Mining),
				);
				self.dig_orientation.clamped(width)
			});
			let mut ui = crate::ui::Ui {
				dl: &mut self.ui.ui_dl,
				hits: &mut self.ui.ui_hits,
				style: crate::ui::Style::new(self.ui.ui_scale),
			};
			if self.ui.inv_screen.is_open() {
				// Screen first: its dim/panel sit under the bar's quads.
				self.ui.inv_screen.build(
					&mut ui,
					size,
					&self.inv_grid,
					&self.inventory,
					&hotbar_view,
					&spells,
					daylight,
				);
			} else if self.ui.alch_screen.is_open() {
				let view =
					crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
				self.ui
					.alch_screen
					.build(&mut ui, size, &view, &hotbar_view, &spells, daylight);
			} else if self.ui.char_screen.is_open() {
				let view = crate::ui::screens::character::CharView::build(
					&self.skills,
					self.camera.pos.y.floor() as i32,
					self.ranks.as_deref().unwrap_or(&[]),
				);
				self.ui
					.char_screen
					.build(&mut ui, size, &view, &hotbar_view, &spells, daylight);
			} else if self.ui.hiscores.is_open() {
				let view = crate::ui::screens::hiscores::HiscoresView::build(
					&self.board,
					self.services.is_some(),
				);
				self.ui
					.hiscores
					.build(&mut ui, size, &view, &hotbar_view, &spells, daylight);
			} else if self.ui.spellbook.is_open() {
				self.ui.spellbook.build(
					&mut ui,
					size,
					&spell_view,
					&hotbar_view,
					&spells,
					daylight,
				);
			} else {
				crate::ui::hud::crosshair(ui.dl, size, ui.style.scale);
			}
			crate::ui::hud::hotbar(&mut ui, size, &hotbar_view, |i| {
				crate::ui::InvId::Bar(i as u8)
			});
			// The spell bar right after the item bar, so its hit rects
			// stand as drop targets under every open page.
			crate::ui::hud::spell_bar(&mut ui, size, &spells, |i| {
				crate::ui::InvId::SpellBar(i as u8)
			});
			crate::ui::hud::toasts(ui.dl, &ui.style, size, &self.vfx.toasts);
			// The teleport instant's wash: one full-rect quad over the
			// whole overlay, gone in under half a second. Ghost is the
			// topmost layer, which is the point — arrival paints across
			// everything for one beat.
			if self.vfx.flash > 0.0 {
				let a = ((self.vfx.flash * 0.5).min(1.0) * 255.0) as u32;
				ui.dl.quad(
					crate::ui::draw::Layer::Ghost,
					crate::ui::Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
					(a << 24) | 0x00FF_FFFF,
				);
			}
			// The background bench's tell: only while a job runs with the
			// screen CLOSED — open, the screen's own progress row is the
			// richer version of the same fact.
			if !self.ui.alch_screen.is_open()
				&& let Some(job) = self.ui.alch_screen.job()
			{
				use crate::ui::screens::alchemy::BenchJob;
				let BenchJob::Craft(item, left) = job;
				let text = format!("Bench  {} ×{left}", item.name());
				crate::ui::hud::bench_tell(ui.dl, &ui.style, size, &text);
			}
			crate::ui::hud::tool_rail(
				ui.dl,
				&ui.style,
				size,
				&owned_tools,
				self.input.tool_swap.shown(),
				self.inventory.bucket_full(),
				self.input.tool_swap.present(),
			);
			// The pick-orientation indicator, bottom-right: tappable on
			// touch, display-only under a locked pointer. Suppressed by an
			// open page like the thumb cluster - the world verbs mean
			// nothing under a screen.
			if let (false, Some(o)) = (page_open, pick_toggle) {
				crate::ui::hud::pick_toggle(
					ui.dl,
					ui.style.scale,
					size,
					&self.input.safe_area,
					self.config.touch_hud,
					o,
				);
			}
			// The touch HUD draws last so its corner button sits over an
			// open screen's dim — it is that screen's close control.
			if self.config.touch_hud {
				let screen_open = self.ui.inv_screen.is_open()
					|| self.ui.alch_screen.is_open()
					|| self.ui.char_screen.is_open()
					|| self.ui.hiscores.is_open()
					|| self.ui.spellbook.is_open();
				crate::ui::hud::touch_buttons(
					ui.dl,
					ui.style.scale,
					size,
					&self.input.safe_area,
					screen_open,
					self.config.touch_jump_button,
				);
				if !screen_open && let Some((anchor, finger)) = self.input.touch.joystick() {
					let to_px = |v: glam::Vec2| (v.x * size.0 as f32, v.y * size.1 as f32);
					crate::ui::hud::joystick_overlay(
						ui.dl,
						ui.style.scale,
						to_px(anchor),
						to_px(finger),
					);
				}
			}
		}
		// The tutorial's prompt panel + anchored highlight. After the
		// hotbar and screens, so the hit map already carries this frame's
		// rects; the highlight rides Ghost, over everything. It pauses out
		// of sight with the HUD under the open menu, EXCEPT on the one
		// step performed inside the menu (open your inventory), where the
		// panel draws over the menu's own quads.
		// The cinematic letterbox: two black bars, drawn on Ghost so they
		// mask whatever world-space overlays reach the frame's edges.
		if cinematic {
			let bar = size.1 as f32 * 0.11;
			let (w, h) = (size.0 as f32, size.1 as f32);
			let ink = 0xFF00_0000;
			self.ui.ui_dl.quad(
				crate::ui::draw::Layer::Ghost,
				crate::ui::Rect::new(0.0, 0.0, w, bar),
				ink,
			);
			self.ui.ui_dl.quad(
				crate::ui::draw::Layer::Ghost,
				crate::ui::Rect::new(0.0, h - bar, w, bar),
				ink,
			);
		}
		// A running cinematic owns the screen whole: the prompt waits for
		// the camera to come home.
		// The steps performed from INSIDE the menu (open this page) keep
		// the prompt up over it; every other step pauses out of sight
		// with the HUD.
		use crate::tutorial::StepId as S;
		if let Some(view) = self.tutorial.view(self.config.touch_hud)
			&& (in_game || matches!(view.step, S::OpenInventory | S::OpenBench | S::OpenArcana))
			&& !cinematic
		{
			let style = crate::ui::Style::new(self.ui.ui_scale);
			let text = crate::voice::plain(
				&crate::voice::Msg::TutorialStep {
					step: view.step,
					touch: self.config.touch_hud,
					jump_button: self.config.touch_jump_button,
					spell_slot: self.wisp_bar_slot().unwrap_or(0),
				},
				&self.input.bindings,
			);
			let skip_hint = if self.config.touch_hud {
				"Skip: Settings"
			} else {
				"Skip: Tab"
			};
			crate::ui::tutorial::panel(
				&mut self.ui.ui_dl,
				&style,
				size,
				&self.input.safe_area,
				&view,
				&text,
				skip_hint,
			);
			// The jump anchor exists only when the jump-mode setting draws
			// the button; under the double-tap there is nothing on screen
			// to point at. The bench steps' anchors come from the page's
			// own guide (family tile, siphon tile, TRANSMUTE button) —
			// the same phase read the poll advances on, so the pulse and
			// the words can never point at different tiles.
			// With the MENU up on an open-this-page step, the pulse moves
			// to the page's own row, resolved through the same acts table
			// the taps run — the corner button reads Back here and must
			// not pulse toward closing the menu. A submenu has no such
			// row and gets the words alone.
			if !in_game {
				let kind = match view.step {
					S::OpenInventory => Some(crate::events::ScreenKind::Inventory),
					S::OpenBench => Some(crate::events::ScreenKind::Alchemy),
					S::OpenArcana => Some(crate::events::ScreenKind::Spellbook),
					_ => None,
				};
				if let Some(row) = kind.and_then(|k| self.ui.menu.screen_row(k))
					&& let Some(mv) = &menu_view
					&& let Some(r) = mv.hits.rect_of(row)
				{
					crate::ui::tutorial::highlight(&mut self.ui.ui_dl, &style, r, self.time_secs);
				}
			} else if view.step == S::SlotWisp {
				// The drag step draws a two-ended hint instead of one pulse:
				// the wisp row (the table's anchor) outlined steady, the first
				// free bar slot pulsing as the drop target, dots streaming
				// between them. Both rects are this frame's hits, so a closed
				// book simply draws nothing.
				let to_slot = (0..crate::spellbar::SPELL_SLOTS as u8)
					.find(|&i| self.spell_bar.get(i as usize).is_none())
					.unwrap_or(0);
				let rect_of = |a| {
					crate::ui::anchors::anchor_rect(
						a,
						&self.ui.ui_hits,
						size,
						style.scale,
						&self.input.safe_area,
					)
				};
				let from = view.anchor.and_then(rect_of);
				let to = rect_of(crate::ui::anchors::AnchorId::Ui(
					crate::ui::InvId::SpellBar(to_slot),
				));
				if let (Some(f), Some(t)) = (from, to) {
					crate::ui::tutorial::drag_hint(
						&mut self.ui.ui_dl,
						&style,
						f,
						t,
						self.time_secs,
					);
				}
			} else {
				let anchor = if matches!(view.step, S::PickTools | S::PickSiphon | S::CraftSiphon) {
					self.ui
						.alch_screen
						.guide_to(digblock_core::block::BlockType::EssenceSiphon)
						.map(crate::ui::anchors::AnchorId::Ui)
				} else if view.step == S::CastWisp {
					// The bar slot the player chose — a runtime fact.
					self.wisp_bar_slot()
						.map(|k| crate::ui::anchors::AnchorId::Ui(crate::ui::InvId::SpellBar(k)))
				} else {
					view.anchor.filter(|a| {
						*a != crate::ui::anchors::AnchorId::JumpButton
							|| self.config.touch_jump_button
					})
				};
				if let Some(a) = anchor
					&& let Some(r) = crate::ui::anchors::anchor_rect(
						a,
						&self.ui.ui_hits,
						size,
						style.scale,
						&self.input.safe_area,
					) {
					crate::ui::tutorial::highlight(&mut self.ui.ui_dl, &style, r, self.time_secs);
				}
			}
		}
		// Last, and over everything: while the arrival hold is up, the
		// world behind this is pristine worldgen the journal is about to
		// overwrite. The menu path covers its own case (the Connecting
		// screen simply stays up), so this is the in-game one — a teleport.
		if self.save.arrival_hold.running() && in_game {
			crate::ui::hud::arrival_cover(
				&mut self.ui.ui_dl,
				&crate::ui::Style::new(self.ui.ui_scale),
				size,
				"Loading world...",
			);
		}
		if in_game {
			self.fill_highlight_targets();
		} else {
			self.vfx.highlight_scratch.clear();
		}
		// The hand is part of playing, not part of reading a screen: a
		// menu, the inventory, the alchemy page or a cinematic all put it
		// away.
		let viewmodel = (in_game
			&& !cinematic
			&& !self.ui.inv_screen.is_open()
			&& !self.ui.alch_screen.is_open()
			&& !self.ui.char_screen.is_open()
			&& !self.ui.hiscores.is_open()
			&& !self.ui.spellbook.is_open())
		.then(|| self.viewmodel_frame(&frame))
		.flatten();
		self.bench.pending.prep_ms = self
			.bench
			.prep_start
			.take()
			.map(ms_since)
			.unwrap_or_default();
		// Pack the teleport light. Filled in PRIORITY order, because the
		// upload truncates at `MAX_FX` by position: the caster's own
		// channel aura first (it is the only thing on screen that says a
		// channel is running), then the live bursts, then the standing
		// orbs out of their own reserved share. Additive blending makes
		// the order free to be a policy rather than a look.
		self.vfx.fx_scratch.clear();
		// DIG_CAST poses the channel at a fixed progress — standing in a
		// hold is the one thing a headless run cannot do (the DIG_SIPHON
		// rule).
		if self.vfx.channel_env.level() > 0.0 {
			let progress = self.config.debug_cast.unwrap_or_else(|| {
				self.cast.as_ref().map_or(1.0, |c| {
					let need = digblock_core::unlock::channel_ticks(
						self.skills.level(digblock_core::skills::Skill::Arcana),
					)
					.max(1);
					(c.ticks as f32 / need as f32).min(1.0)
				})
			});
			let feet = self.player.pos - Vec3::new(0.0, digblock_core::player::EYE_TO_FEET, 0.0);
			self.vfx.fx_scratch.push(crate::teleport::instance(
				feet.to_array(),
				crate::teleport::Mode::Channel,
				progress,
				self.vfx.channel_env.level(),
				3.7,
			));
		}
		for fx in &self.vfx.casts {
			self.vfx.fx_scratch.push(crate::teleport::instance(
				fx.pos.to_array(),
				fx.mode,
				fx.age,
				1.0,
				fx.seed,
			));
		}
		// The kindled orbs. Culled far like the markers (an additive speck at range costs a draw
		// and shows nothing) and ramped down NEAR, or the one standing in your own cell fills the
		// screen. Their age drives the fade-in; the last seconds of the countdown are the envelope,
		// so a light dims out rather than blinks. Only the nearest `MAX_KINDLED_FX` are uploaded.
		let cam = self.camera.pos;
		// One answer per orb, asked twice — to rank the share and to fill
		// it — so a light cannot be ranked on one rule and drawn by
		// another. Zero envelope is "does not draw", which also parks it
		// at the back of the ranking rather than spending a slot.
		let shown = |k: &KindledLight| {
			let origin = digblock_core::kindle::origin(k.cell);
			let dist = origin.distance(cam);
			let env = if dist > crate::teleport::KINDLED_RADIUS {
				0.0
			} else {
				crate::teleport::near_fade(dist)
					* (k.left / crate::teleport::FADE_OUT_SECS).min(1.0)
			};
			(origin, dist, env)
		};
		let (picked, n) = crate::teleport::nearest_orbs(self.vfx.kindled.iter().map(|k| {
			let (_, dist, env) = shown(k);
			if env > 0.0 { dist } else { f32::INFINITY }
		}));
		for &i in &picked[..n] {
			let k = &self.vfx.kindled[i as usize];
			let (origin, _, envelope) = shown(k);
			if envelope <= 0.0 {
				continue;
			}
			self.vfx.fx_scratch.push(crate::teleport::instance(
				origin.to_array(),
				crate::teleport::Mode::Kindled,
				k.age,
				envelope,
				f32::from(k.key),
			));
		}
		// The standing markers, culled here on the CPU (reused scratch);
		// an empty scratch is no draw, which is what DIG_NO_MARKERS asks.
		{
			let mut scratch = std::mem::take(&mut self.vfx.marker_scratch);
			if self.config.markers {
				crate::waystone_fx::visible(&self.vfx.marker_sites, self.camera.pos, &mut scratch);
			} else {
				scratch.clear();
			}
			self.vfx.marker_scratch = scratch;
		}
		if let Some(age) = self.config.debug_fx {
			use crate::teleport::Mode;
			let forward =
				Vec3::new(self.camera.dir().x, 0.0, self.camera.dir().z).normalize_or_zero();
			let base = self.camera.pos + forward * 7.0 - Vec3::Y * 1.5;
			let right = forward.cross(Vec3::Y);
			self.vfx.fx_scratch.clear();
			for (i, mode) in [Mode::Departure, Mode::Arrival, Mode::Channel, Mode::Kindled]
				.into_iter()
				.enumerate()
			{
				let at = base
					+ right * (i as f32 - 1.5) * 2.2
					+ if mode == Mode::Kindled {
						Vec3::Y * 1.2
					} else {
						Vec3::ZERO
					};
				let time = if mode == Mode::Channel {
					age
				} else {
					age * crate::teleport::LIFE
				};
				self.vfx.fx_scratch.push(crate::teleport::instance(
					at.to_array(),
					mode,
					time,
					1.0,
					3.7,
				));
			}
			self.vfx.marker_scratch.clear();
			self.vfx.marker_scratch.push(crate::waystone_fx::instance(
				(base + forward * 4.0).to_array(),
				0.0,
				1.4,
				3.2,
			));
		}
		let sky_floor = self.sky_floor();
		// Hoisted: the renderer borrow below holds `self` mutably.
		let body_anchor = self.body_anchor();
		if let Some(r) = self.renderer.as_mut() {
			// A carried light that has travelled — or has just gone out —
			// re-places itself through the same hook a block edit uses, so
			// the probes around it re-trace at full weight instead of
			// decaying from a stale value. See `Carried`. `commit` runs
			// only once they have actually been queued.
			for &(_, p) in &self.vfx.carried.pending {
				r.gi_invalidate_moving(p);
			}
			self.vfx.carried.commit();
			let t_encode = web_time::Instant::now();
			let scene = FrameScene {
				uniforms: frame,
				view_proj,
				size,
				scale: crate::renderer::RenderScale {
					render: self.config.render_scale,
					gi_div: self.config.gi_scale_div,
				},
				ui_scale: self.ui.ui_scale,
				players: &self.vfx.remote_instances,
				held: &self.vfx.held_instances,
				carried: &self.vfx.carried.lights,
				cam_forward: self.camera.dir().to_array(),
				light_cap: self.config.max_lights.max(1) as usize,
				sky_floor,
				confetti: &self.vfx.confetti,
				bursts: &self.vfx.bursts,
				// The stream ends where the tool's own light hangs, and
				// the destination is handed over rather than derived in
				// the shader so the two cannot drift — see `siphon`.
				siphon: self.vfx.siphon.map(|s| {
					crate::siphon::instance(
						s.source.to_array(),
						digblock_core::gi::carried_origin(body_anchor).to_array(),
						s.tint,
						s.normal,
						s.progress,
						s.envelope,
					)
				}),
				teleport: &self.vfx.fx_scratch,
				markers: &self.vfx.marker_scratch,
				overlay: overlay_stats
					.as_ref()
					.map(|s| (s, self.vfx.history_scratch.as_slice())),
				feed: &self.session.feed,
				targets: &self.vfx.highlight_scratch,
				viewmodel,
				ui: &self.ui.ui_dl,
				chat: (in_game && self.ui.chat.is_open()).then(|| {
					self.ui
						.chat
						.view(((self.time_secs * 2.0) as u32).is_multiple_of(2))
				}),
				nametags: &self.vfx.nametag_scratch[..self.vfx.nametag_count],
				binds: &self.input.bindings,
			};
			let stats = r.render(gpu, target, &scene);
			self.bench.pending.encode_ms = ms_since(t_encode);
			self.bench.metrics.render = stats;
			self.bench.metrics.gpu = r.gpu_pass_ms();
			self.bench.metrics.gpu_mem = r.ledger.snapshot();
		}
		// Occupancy: the whole handler span minus the vsync wait that blocks
		// inside it on native (and is effectively absent on the web). Closed
		// here rather than by the platform after `render` so it reaches this
		// frame's record instead of the next one's.
		self.bench.pending.busy_ms = self
			.bench
			.frame_start
			.take()
			.map(|t| (ms_since(t) - self.bench.pending.acquire_ms).max(0.0))
			.unwrap_or_default();
		// total_ms is measured inside frame() from the frame-to-frame delta.
		self.bench
			.metrics
			.frame(std::mem::take(&mut self.bench.pending));
	}
}
