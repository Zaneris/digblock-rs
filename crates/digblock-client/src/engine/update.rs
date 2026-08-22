//! One simulation step: input intent, physics, prediction, streaming
//! and the screens that tick with the world closed. Everything here
//! rides `dt`, which is why `--fixed-dt` is required for any A/B.

use super::*;

impl Engine {
	pub(super) fn step(&mut self, dt: Duration) {
		let t_update = web_time::Instant::now();
		let dt = dt.as_secs_f32();
		self.time_secs = (self.time_secs + dt) % 600.0;
		// The scheduler's clock, never the wrapping shader one above.
		self.world_secs += f64::from(dt);
		// The F7 GI drill on a timer, so the ratchet is measurable WITHOUT a
		// human at the keyboard. Pair with `--fly N --frames M` (N < drill <
		// M): the camera freezes at N, the drill fires mid-freeze, and the
		// CSV rows either side are the same pose in the same thermal state
		// with only the GI's derived age differing — which is the only way
		// to separate "the ceiling ratcheted" from "the view got busier".
		self.bench.frames_seen += 1;
		if self.config.gi_drill_at == Some(self.bench.frames_seen) {
			let (sky, emit) = self.renderer_gi_state();
			if let Some(r) = self.renderer.as_mut() {
				r.gi_clear_derived_state();
			}
			self.pipeline.gi_redirty_all();
			log::info!(
				"GI drill at frame {}: cleared (sky_y {sky:.0}, {emit} emitters)",
				self.bench.frames_seen
			);
		}
		// DIG_SHAPES showcase: chunks stream in asynchronously, so retry
		// the leftovers each frame until every cell has landed.
		if !self.debug_shapes_pending.is_empty() {
			let mut pending = std::mem::take(&mut self.debug_shapes_pending);
			pending.retain(|&(x, y, z, cell)| {
				self.pipeline.cell_at(x, y, z) != Some(cell)
					&& self.pipeline.set_cell(x, y, z, cell).is_none()
			});
			self.debug_shapes_pending = pending;
		}
		// A fixed --tod freezes the sun angle (dev builds only): advance
		// the day cycle, then re-assert the override on top, so a capture
		// run has a genuinely static sun for the GI to converge against.
		let fixed_tod = self.config.tod;
		self.sun.update(dt);
		if let Some(tod) = fixed_tod {
			self.sun.tod = tod;
		}
		// Scripted flight — the bench's, and the capture harness's. Inputs
		// are off and the camera is driven outright, so both return before
		// the interactive half of update; streaming still runs, because
		// `pump_pipeline` hangs off `render`, not off this.
		if self.config.bench {
			// `--bench --fly N` freezes the camera at step N instead of
			// flying forever. Without this the only way to get a frozen
			// pose was `--fly` WITHOUT `--bench`, which is not the same
			// run at all: `interactive` is derived as `!bench && ...`, so
			// that path opens the menu, loads the user's SAVED settings,
			// turns audio on and dials the master. A measurement taken
			// there is not hermetic and not comparable to anything.
			if self.config.fly.is_none_or(|n| self.bench.fly_frames < n) {
				self.fly_step();
				self.bench.fly_frames += 1;
			}
			self.advance_hand(dt);
			self.bench.pending.update_ms = ms_since(t_update);
			return;
		}
		if let Some(n) = self.config.fly {
			// `--warp N` is `--fly N` with every step taken on frame 0 —
			// literally the same loop, so the two land on a bit-identical
			// pose. Re-deriving the pose in closed form would not: the yaw
			// accumulates through N `look` calls, and one multiply by N
			// differs in the last ulp, which is a subpixel shift across the
			// whole frame and would swamp the diff the harness exists to
			// read.
			while self.bench.fly_frames < n {
				self.fly_step();
				self.bench.fly_frames += 1;
				if !self.config.warp {
					break;
				}
			}
			self.advance_hand(dt);
			self.bench.pending.update_ms = ms_since(t_update);
			return;
		}

		const LOOK_SENS: f32 = 0.12; // degrees per raw mouse unit
		let (lx, ly) = std::mem::take(&mut self.input.look_accum);
		let touch_look = self.input.touch.take_look();
		self.camera
			.look(lx * LOOK_SENS + touch_look.x, ly * LOOK_SENS + touch_look.y);

		// Menu plumbing: service responses in, commands out, disconnect
		// intents applied. The world (and any live connection) keeps
		// running underneath the open menu.
		self.ui.menu.set_connected(self.session.client.is_some());
		let responses: Vec<crate::menu::MenuResponse> = self
			.services
			.as_ref()
			.map(|s| s.resp_rx.try_iter().collect())
			.unwrap_or_default();
		for resp in responses {
			if let Some((transport, token)) = self.ui.menu.on_response(resp) {
				self.set_transport_with_token(transport, token);
			}
		}
		for intent in self.ui.menu.take_world_intents() {
			self.handle_world_intent(intent);
		}
		if self.ui.menu.take_disconnect() && self.leave_server() {
			self.session.pending_ack = None;
			self.session.correction_offset = Vec3::ZERO;
			self.session.my_player_id = None;
			self.push_feed(crate::voice::Msg::LeftServer);
			// Without a store there is no save list to return to, so a
			// store-less run keeps playing the server's seed offline
			// rather than dropping into a menu it can't leave.
			if self.save.world_store.is_some() {
				self.return_to_title();
			}
		}
		// The local-world counterpart: flush the save first, then take the
		// same road back to the title. Unconditional — a store-less
		// ephemeral "Play Local" world has a title menu to go back to.
		if self.ui.menu.take_leave_world() {
			self.close_local_save();
			self.push_feed(crate::voice::Msg::LeftWorld);
			self.return_to_title();
		}
		// The home spell's cooldown ticks here; the spellbook page reads
		// it live. Connected, the server teleports us (Teleported lands
		// in pump_net); local play resolves and lands immediately.
		self.save.teleport_cooldown = (self.save.teleport_cooldown - dt).max(0.0);
		// The wisp burns down on the same clock.
		self.wisp_left = (self.wisp_left - dt).max(0.0);
		// The menu's Inventory/Alchemy rows: the menu closed itself; the
		// screen takes over the free cursor seamlessly.
		if self.ui.menu.take_inventory() {
			self.ui.inv_screen.open();
		}
		if self.ui.menu.take_alchemy() {
			self.ui.alch_screen.open();
		}
		if self.ui.menu.take_character() {
			self.ui.char_screen.open();
		}
		if self.ui.menu.take_spellbook() {
			self.ui.spellbook.open();
		}
		// The H shortcut: begin the home channel — or, on cooldown, show
		// the page with the countdown instead of doing nothing silently.
		if std::mem::take(&mut self.save.return_spawn_requested) {
			self.start_cast(super::CastTarget::Home);
		}
		if let Some(services) = &self.services {
			for cmd in self.ui.menu.take_commands() {
				// The account's name becomes the session name, so the Hello
				// introduces us consistently. It is a courtesy only: the
				// server overwrites it with the master's answer, which is
				// now the same string by construction.
				if matches!(cmd, crate::menu::MenuCommand::Join { .. }) {
					self.config.player_name = self.ui.menu.account().and_then(|a| a.name.clone());
				}
				let _ = services.cmd_tx.send(cmd);
			}
		}
		if let Some(s) = self.ui.menu.take_settings_change() {
			self.apply_settings(s);
		}

		let t_net = web_time::Instant::now();
		self.pump_net(dt);
		self.bench.pending.net_ms = ms_since(t_net);
		// Keep the arrangement invariants (a type lives on the bar or in
		// the grid, never both; nothing owned is unplaced) and mirror any
		// change to the server BEFORE this frame's clicks send edits —
		// the server's slot enforcement validates against the copy that
		// arrives, in order, on the same reliable stream.
		digblock_core::inv_grid::reconcile_layout(
			&self.inventory,
			&mut self.hotbar,
			&mut self.inv_grid,
		);
		if let Some(net) = self.session.client.as_mut()
			&& net.ready()
		{
			net.predictor.active_slot = self.hotbar.selected_index() as u8;
			// First send only after the join-sync `ServerMsg::Layout`
			// primed the dedup — reporting a pre-sync default would
			// overwrite the identity's stored arrangement.
			if let Some((last_bar, last_grid)) = self.session.last_layout_sent.as_ref() {
				// `to_wire` returns arrays, so the comparison is free;
				// only an actual change pays for the Vecs.
				let bar = self.hotbar.to_wire();
				let grid = self.inv_grid.to_wire();
				if last_bar.as_slice() != bar.as_slice() || last_grid.as_slice() != grid.as_slice()
				{
					net.send_layout(&bar, &grid);
					self.session.last_layout_sent = Some((bar.to_vec(), grid.to_vec()));
				}
			}
		}
		// Expired messages are kept, not dropped: the chat box shows recent
		// history while you type, and there is nothing to show if the only
		// copy was thrown away the moment it faded. `feed` is capped at
		// FEED_MAX either way, so this is a fixed-size ring.
		for m in &mut self.session.feed {
			m.1 = (m.1 - dt).max(FEED_TTL_FLOOR);
		}
		// XP toasts fade out and drop (unlike the feed, nothing rereads
		// an expired toast).
		for t in &mut self.vfx.xp_toasts {
			t.ttl -= dt;
		}
		self.vfx.xp_toasts.retain(|t| t.ttl > 0.0);
		// Confetti: the age in lane 3 IS the simulation — every flake's
		// arc is a pure function of it in the shader. A burst is dropped
		// the moment it reaches the life the shader fades to zero at.
		for b in &mut self.vfx.confetti {
			b[3] += dt;
		}
		self.vfx.confetti.retain(|b| b[3] < crate::confetti::LIFE);
		// Teleport bursts age the same way — the age IS the simulation —
		// and the channel aura's envelope opens while a cast channels and
		// dissolves when one ends or breaks, so an interrupt fades rather
		// than pops. The flash is the teleport instant's overlay wash.
		for fx in &mut self.vfx.casts {
			fx.age += dt;
		}
		self.vfx.casts.retain(|fx| fx.age < crate::teleport::LIFE);
		let channel_live = self.cast.is_some() || self.config.debug_cast.is_some();
		self.vfx.channel_env = if channel_live {
			(self.vfx.channel_env + dt / crate::teleport::FADE_IN_SECS).min(1.0)
		} else {
			(self.vfx.channel_env - dt / crate::teleport::FADE_OUT_SECS).max(0.0)
		};
		self.vfx.flash = (self.vfx.flash - dt * 2.2).max(0.0);
		// DIG_CONFETTI=1: a burst every couple of seconds a few blocks
		// ahead, so a capture run can photograph one at every age.
		if self.config.debug_confetti && self.vfx.confetti.is_empty() {
			let at = self.camera.pos + self.camera.dir() * 4.0 - Vec3::Y * 0.5;
			self.throw_confetti(at);
		}
		// The bench drip: a queued transmute batch sends one single-item
		// Craft per cycle — the server paces the same rhythm out of its
		// bench bucket. The view is rebuilt here rather than borrowed
		// from the draw, so the queue's affordability check reads the same
		// number the button greys on. `busy` keeps the drip alive with the
		// SCREEN closed (§13.4's background bench): the pace is slow
		// enough that standing at the bench for it would be idle time,
		// so the job runs while you play and only leaving the world
		// abandons it.
		if self.ui.alch_screen.is_open() || self.ui.alch_screen.busy() {
			let view = crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
			if let Some(act) = self.ui.alch_screen.tick(dt, &view) {
				self.apply_alchemy_action(act);
			}
		}
		if let Some(net) = self.session.client.as_ref() {
			// Reconciliation health line, on the metrics cadence.
			self.session.net_log_timer += dt;
			if self.session.net_log_timer >= 5.0 && net.ready() {
				let avg = if self.session.net_corrections > 0 {
					self.session.net_error_accum / self.session.net_corrections as f32
				} else {
					0.0
				};
				log::info!(
					"net: {} corrections (avg {:.3} blocks), {} pending edits, {} deferred",
					self.session.net_corrections,
					avg,
					net.edits.len(),
					self.pipeline.deferred_edits(),
				);
				self.session.net_log_timer = 0.0;
				self.session.net_corrections = 0;
				self.session.net_error_accum = 0.0;
			}
		}

		// Crosshair target → placement edge → hold-to-dig stepping (at the
		// fixed sim rate, so offline and online dig timing share the
		// server's tick domain, in walk AND fly mode). All outside the physics
		// `block_at` borrow: raycast (immutable) → set_block (mutable) →
		// player tick (immutable), strictly sequential.
		self.update_target();
		self.apply_place();
		// A stationary touch hold sends no events, so its promotion to
		// digging has to be polled — ahead of the dig ticks that read it.
		self.input.touch.tick(web_time::Instant::now());
		self.dig_accum = (self.dig_accum + dt).min(0.25);
		while self.dig_accum >= TICK {
			self.dig_accum -= TICK;
			self.step_dig();
			self.step_cast();
		}
		// The siphon's stream reads the state those ticks just left, so it
		// ages after them and on the FRAME clock rather than the sim one:
		// it is presentation, and its envelope should not step in ticks.
		self.step_siphon(dt);

		// Water flow runs on its own world-tick cadence, independent of
		// the walk/fly camera mode; idle (empty queue) it costs nothing.
		// Connected, the server owns fluid — its results arrive as edits.
		let t_sim = web_time::Instant::now();
		if self.session.client.is_none() {
			use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
			self.flow_accum = (self.flow_accum + dt).min(1.0);
			while self.flow_accum >= FLOW_STEP_SECS {
				self.flow_accum -= FLOW_STEP_SECS;
				let mut world = PipelineWorld(&mut self.pipeline, &self.protected);
				self.fluid.step(&mut world, FLOW_BUDGET);
			}
			self.step_world_jobs();
			// The spawn structure owed to this save, landed as chunks
			// become resident — the engine's arm of the server's boot
			// stamp. `set_cell` answering None keeps the cell owed.
			if !self.pending_stamp.is_empty() {
				let pipeline = &mut self.pipeline;
				self.pending_stamp
					.retain(|&(x, y, z, v)| match pipeline.cell_at(x, y, z) {
						Some(cur) if cur == v => false,
						Some(_) => pipeline.set_cell(x, y, z, v).is_none(),
						None => true,
					});
			}
		}
		self.bench.pending.sim_ms = ms_since(t_sim);

		// Named-save persistence, after every world mutation this frame
		// (dig, place, fluid): forward recorded edits, throttle player
		// snapshots, and give the store its bounded upkeep step. The
		// backdrop world's recordings are drained and dropped.
		let t_persist = web_time::Instant::now();
		let recorded = self.pipeline.take_recorded_edits();
		if let Some(store) = self.save.world_store.as_mut() {
			if self.save.in_local_save && self.session.client.is_none() {
				for (x, y, z, cell) in recorded {
					store.append_edit(x, y, z, cell);
				}
				self.save.player_save_accum += dt;
				if self.save.player_save_accum >= 5.0 {
					self.save.player_save_accum = 0.0;
					store.save_player(
						&self.inventory.to_wire(),
						&self.hotbar.to_wire(),
						&self.inv_grid.to_wire(),
						&self.skills.to_wire(),
						&digblock_persist::PlayerPersist {
							pos: self.player.pos.to_array(),
							yaw: self.camera.yaw_deg,
							pitch: self.camera.pitch_deg,
						},
					);
				}
			}
			store.maintain(self.pipeline.overlay());
		}
		self.bench.pending.persist_ms = ms_since(t_persist);

		// Arrival hold: run the wait down and give up on it if the marker
		// never lands. A server that wedges mid-sync, or one whose outbound
		// queue never opens the room join sync reserves, must not be able
		// to strand a player on a loading screen — the fallback is exactly
		// the behaviour this replaced.
		if let Some(left) = self.save.arrival_hold {
			let left = left - dt;
			// A connection that ended can never deliver the marker, and the
			// world we fall back to playing offline is generated, not
			// synced — so release now rather than making the player watch
			// the timeout run out over a server that is already gone. This
			// covers every drop path at once (menu disconnect, refusal,
			// timeout, `return_to_title`) instead of one hook each.
			if self.session.client.is_none() {
				self.end_arrival_hold();
			} else if left <= 0.0 {
				log::warn!(
					"arrival hold: no AreaReady in {:.0}s — entering the world anyway",
					digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS
				);
				self.end_arrival_hold();
			} else {
				self.save.arrival_hold = Some(left);
			}
		}

		if self.input.walking {
			// Fixed-rate player simulation (Java feel was tuned per-frame at
			// that same rate; the accumulator preserves it at any display rate).
			// Always drained, honoured only in double-tap mode — an undrained
			// edge would fire the moment the setting flipped back.
			let touch_jump = self.input.touch.take_jump();
			let jump = self.input.keys.up
				|| if self.config.touch_jump_button {
					self.input.hud_jump_held
				} else {
					touch_jump
				};
			let key_axis = Vec2::new(
				(self.input.keys.d as i32 - self.input.keys.a as i32) as f32,
				(self.input.keys.w as i32 - self.input.keys.s as i32) as f32,
			);
			let intent = PlayerIntent {
				move_axis: key_axis + self.input.touch.move_axis(),
				jump,
			};
			let dir = self.camera.dir();
			self.tick_accum = (self.tick_accum + dt).min(0.25);
			let pipeline = &self.pipeline;
			let cell_at = |x: i32, y: i32, z: i32| pipeline.cell_at(x, y, z);
			// Never simulate through ungenerated terrain: unloaded cells
			// are non-solid (the Java null-block), so ticking before the
			// player's column has generated free-falls through the world
			// — seconds of fall on web, where a fresh world streams in
			// on the inline executor. Hold the sim, dropping the
			// accumulated time, until the column lands (gen jobs land
			// whole column stacks, so one feet-cell probe is enough).
			// Outside the world's vertical range no chunk ever loads;
			// let the void keep its historical fall.
			let feet_y = (self.player.pos.y - digblock_core::player::EYE_TO_FEET).floor() as i32;
			let column_loaded = !(0..=digblock_core::chunk::WORLD_VBLOCK).contains(&feet_y)
				|| cell_at(
					self.player.pos.x.floor() as i32,
					feet_y,
					self.player.pos.z.floor() as i32,
				)
				.is_some();
			// The arrival hold suspends the sim on exactly the terms an
			// unloaded feet column does, and for the same reason: the
			// geometry under the player is not the geometry the server is
			// simulating against. Predicting through it is what produced
			// the correction that reads as being *moved* after joining —
			// you stand on worldgen, the server stands on the journal, and
			// the first snapshot settles it visibly.
			let world_ready = column_loaded && self.save.arrival_hold.is_none();
			if !world_ready {
				self.tick_accum = 0.0;
				// Zeroing the accumulator parks the lerp at alpha = 0, which
				// would draw a whole tick BACKWARDS; hold in place instead.
				self.session.tick_lerp.reset(self.player.pos);
			}
			match self.session.client.as_mut() {
				_ if !world_ready => {}
				Some(net) if net.ready() => {
					// Reconcile against the latest snapshot before predicting
					// this frame's ticks; corrections land as a decaying
					// visual offset, not a camera snap.
					if let Some((tick, state)) = self.session.pending_ack.take() {
						use digblock_net::predict::Reconciled;
						let outcome =
							net.predictor
								.reconcile(&mut self.player, tick, state, &cell_at);
						if let Reconciled::Replayed { error } = outcome {
							self.session.correction_offset += error;
							// The rewind moved the player by -error; the lerp
							// anchor has to travel with it or the next frame
							// interpolates from the pre-rewind position and
							// re-injects the jump the offset exists to hide.
							self.session.tick_lerp.rewind(error);
							self.session.net_corrections += 1;
							self.session.net_error_accum += error.length();
						}
						// The ack that drove this reconcile is also the RTT
						// sample — the overlay's ping comes from here.
						net.note_reconciled(outcome);
					}
					while self.tick_accum >= TICK {
						self.tick_accum -= TICK;
						self.session.tick_lerp.before_tick(self.player.pos);
						net.predictor
							.predict(&mut self.player, intent, dir, &cell_at);
					}
					net.send_inputs();
				}
				_ => {
					while self.tick_accum >= TICK {
						self.tick_accum -= TICK;
						self.session.tick_lerp.before_tick(self.player.pos);
						self.player.tick(TICK, dir, intent, &cell_at);
					}
				}
			}
			// Sub-second exponential decay; snapped off once imperceptible.
			self.session.correction_offset *= (-dt / 0.2).exp();
			if self.session.correction_offset.length_squared() < 1e-6 {
				self.session.correction_offset = Vec3::ZERO;
			}
			// Presentation only: `player.pos` stays the authority for
			// occupancy, the wire, and the save.
			self.camera.pos = self
				.session
				.tick_lerp
				.render_pos(self.player.pos, self.tick_accum)
				+ self.session.correction_offset;
		} else {
			// Debug fly camera.
			let speed = 24.0 * dt;
			let dir = self.camera.dir();
			let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
			let right = flat.cross(Vec3::Y);
			let mut mv = Vec3::ZERO;
			if self.input.keys.w {
				mv += flat;
			}
			if self.input.keys.s {
				mv -= flat;
			}
			if self.input.keys.d {
				mv += right;
			}
			if self.input.keys.a {
				mv -= right;
			}
			if self.input.keys.up {
				mv += Vec3::Y;
			}
			if self.input.keys.down {
				mv -= Vec3::Y;
			}
			self.camera.pos += mv.normalize_or_zero() * speed;
		}
		// Held-item animation and the audio both read this frame's
		// movement, so they sit together and `prev_player_pos` advances
		// once, after both — `step_audio` returns early on a silent run
		// and would otherwise starve the bob of every frame's delta.
		let moved = self.player.pos - self.audio.prev_player_pos;
		self.vfx.view_bob.advance(
			Vec3::new(moved.x, 0.0, moved.z).length(),
			dt,
			self.ground_below().1,
		);
		// The scoop's arc is started by the hold that opens it, in
		// `step_dig` — one stroke per bucket, so there is nothing to
		// re-arm here.
		self.vfx.view_swing.advance(dt);
		// The casting hand rides the channel's own envelope, so gesture
		// and aura rise and dissolve together. DIG_CAST holds both.
		self.vfx.view_cast.advance(dt, self.vfx.channel_env);
		// At the TAIL on the interactive path, after `reconcile_layout` and
		// `pump_net`: a slot whose count reached zero is only released by
		// the reconcile above, so reading the bar ahead of it drew — and
		// LIT — a torch from a hand that had just placed its last one. The
		// scripted-flight arms call this before their own returns instead,
		// because the hand is part of the frame they capture.
		self.advance_hand(dt);
		self.step_audio(dt);
		self.audio.prev_player_pos = self.player.pos;
		self.bench.pending.update_ms = ms_since(t_update);
	}
}
