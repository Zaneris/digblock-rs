//! Assembling the frame and handing it to the renderer. Five passes,
//! the last of which clears the shared depth attachment so the
//! viewmodel can be drawn in its own narrow projection.

use super::*;

impl Engine {
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
		self.vfx.carried.push(
			CARRIER_SELF,
			self.camera.pos,
			self.vfx
				.view_swap
				.shown()
				.and_then(crate::ui::hud::icon_block),
			self.skills.level(digblock_core::skills::Skill::Mining),
		);
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
				},
			);
		}
		self.vfx.carried.finish(self.camera.pos);
		let frame = self.frame_uniforms();
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		let view_proj = self.camera.view_proj(aspect);
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
					text: String::new(),
				});
			}
			let tag = &mut self.vfx.nametag_scratch[tags];
			tag.x = (ndc.x * 0.5 + 0.5) * size.0 as f32;
			tag.y = (1.0 - (ndc.y * 0.5 + 0.5)) * size.1 as f32;
			tag.color = pack_rgb(player_color(d.id));
			tag.text.clear();
			match self.session.player_names.get(&d.id) {
				Some(n) => tag.text.push_str(n),
				None => {
					use std::fmt::Write;
					let _ = write!(tag.text, "player {}", d.id);
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
				seed: self.seed,
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
		if in_game {
			let hotbar_view = crate::hotbar::HotbarView::build(
				&self.hotbar,
				&self.inventory,
				&self.input.tool_swap,
			);
			let owned_tools = self.owned_tools();
			let mut ui = crate::ui::Ui {
				dl: &mut self.ui.ui_dl,
				hits: &mut self.ui.ui_hits,
				style: crate::ui::Style::new(self.ui.ui_scale),
			};
			if self.ui.inv_screen.is_open() {
				// Screen first: its dim/panel sit under the bar's quads.
				self.ui
					.inv_screen
					.build(&mut ui, size, &self.inv_grid, &self.inventory, daylight);
			} else if self.ui.alch_screen.is_open() {
				let view =
					crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
				self.ui.alch_screen.build(&mut ui, size, &view, daylight);
			} else if self.ui.char_screen.is_open() {
				let view = crate::ui::screens::character::CharView::build(
					&self.skills,
					self.camera.pos.y.floor() as i32,
				);
				self.ui.char_screen.build(&mut ui, size, &view, daylight);
			} else {
				crate::ui::hud::crosshair(ui.dl, size, ui.style.scale);
			}
			crate::ui::hud::hotbar(&mut ui, size, &hotbar_view, |i| {
				crate::ui::InvId::Bar(i as u8)
			});
			crate::ui::hud::xp_toasts(ui.dl, &ui.style, size, &self.vfx.xp_toasts);
			// The background bench's tell: only while a job runs with the
			// screen CLOSED — open, the screen's own progress row is the
			// richer version of the same fact.
			if !self.ui.alch_screen.is_open()
				&& let Some(job) = self.ui.alch_screen.job()
			{
				use crate::ui::screens::alchemy::{BenchJob, block_name};
				let BenchJob::Craft(item, left) = job;
				let text = format!("BENCH  {} ×{left}", block_name(item));
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
			// The touch HUD draws last so its corner button sits over an
			// open screen's dim — it is that screen's close control.
			if self.config.touch_hud {
				let screen_open = self.ui.inv_screen.is_open()
					|| self.ui.alch_screen.is_open()
					|| self.ui.char_screen.is_open();
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
		// Last, and over everything: while the arrival hold is up, the
		// world behind this is pristine worldgen the journal is about to
		// overwrite. The menu path covers its own case (the Connecting
		// screen simply stays up), so this is the in-game one — a teleport.
		if self.save.arrival_hold.is_some() && in_game {
			crate::ui::hud::arrival_cover(
				&mut self.ui.ui_dl,
				&crate::ui::Style::new(self.ui.ui_scale),
				size,
				"Loading world...",
			);
		}
		let highlight_target = if in_game { self.dig_progress() } else { None };
		// The hand is part of playing, not part of reading a screen: a
		// menu, the inventory or the alchemy page all put it away.
		let viewmodel = (in_game
			&& !self.ui.inv_screen.is_open()
			&& !self.ui.alch_screen.is_open()
			&& !self.ui.char_screen.is_open())
		.then(|| self.viewmodel_frame(&frame))
		.flatten();
		self.bench.pending.prep_ms = self
			.bench
			.prep_start
			.take()
			.map(ms_since)
			.unwrap_or_default();
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
				confetti: &self.vfx.confetti,
				// The stream ends where the tool's own light hangs, and
				// the destination is handed over rather than derived in
				// the shader so the two cannot drift — see `siphon`.
				siphon: self.vfx.siphon.map(|s| {
					crate::siphon::instance(
						s.source.to_array(),
						digblock_core::gi::carried_origin(self.camera.pos).to_array(),
						s.tint,
						s.normal,
						s.progress,
						s.envelope,
					)
				}),
				overlay: overlay_stats
					.as_ref()
					.map(|s| (s, self.vfx.history_scratch.as_slice())),
				feed: &self.session.feed,
				target: highlight_target,
				viewmodel,
				ui: &self.ui.ui_dl,
				chat: (in_game && self.ui.chat.is_open()).then(|| {
					self.ui
						.chat
						.view(((self.time_secs * 2.0) as u32).is_multiple_of(2))
				}),
				nametags: &self.vfx.nametag_scratch[..self.vfx.nametag_count],
			};
			let stats = r.render(gpu, target, &scene);
			self.bench.pending.encode_ms = ms_since(t_encode);
			self.bench.metrics.render = stats;
			self.bench.metrics.gpu = r.gpu_pass_ms();
			self.bench.metrics.gpu_buffer_bytes = r.gpu_buffer_bytes;
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
