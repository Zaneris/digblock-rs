//! Entering, leaving and swapping a world: local saves, the title
//! screen, and the transient state that must be cleared when the world
//! under the player is replaced.

use super::*;
use digblock_core::worldgen::Seed;

impl Engine {
	/// Welcome handshake: replace the locally-seeded world with the
	/// server's. Cheap at startup — nothing meaningful has streamed yet.
	pub(super) fn swap_world(&mut self, seed: Seed, spawn: Vec3) {
		log::info!("joining server world: seed {seed:?}");
		self.close_local_save();
		self.session.player_names.clear();
		self.push_feed(crate::voice::Msg::Connected);
		self.seed = seed;
		self.save.spawn_pos = spawn;
		// A server's protected regions are its own secret: refusals arrive
		// as `DigRefused`, never predicted.
		self.protected.clear();
		// No overlay: the server's journal owns this world's edits.
		self.install_pipeline(Pipeline::new(
			seed,
			self.config.draw_dist,
			self.config.workers,
		));
		self.player = Player::new(spawn);
		self.camera.pos = spawn;
		// The server's InventoryFull (right behind JournalDone) is the
		// authoritative base; local single-player stock must not leak in.
		self.inventory = Inventory::default();
		// Its `Skills` (always sent, right behind `Layout`) is the same kind of
		// base, and clearing these is not about the stale NUMBER — it is about
		// who reads it before the real one lands. `reset_world_transients`
		// below hands the Mining level straight to the pipeline we just
		// installed, so a previous world's level would stamp veins into THIS
		// world's terrain for the whole window between `Welcome` and `Skills`.
		// In that window `dig_mode` answers `NeedsSiphon` on cells the server
		// calls plain stone, and the correction costs a full `remesh_all`.
		// Level 1 is the honest guess: it under-reveals rather than over-, and
		// every reveal it misses arrives moments later with the real totals.
		self.skills = Default::default();
		self.vfx.toasts.clear();
		self.auth_essence = None;
		self.reset_world_transients(spawn, true);
	}

	/// The world-swap tail shared by server joins, save entry, and the
	/// leave-server backdrop: kill in-flight interaction and streaming
	/// state, and silence the old world.
	pub(super) fn reset_world_transients(&mut self, pos: Vec3, world_replaced: bool) {
		self.dig = None;
		self.cast = None;
		// The backstop only. A climb is UNWOUND — glass down, fare back —
		// by `abandon_sphere` at the sites that still have the sphere's own
		// world under them; by the time a swap gets here the pipeline may
		// already be the next world's, and writing into that one would
		// scatter the old sphere's cells through it.
		self.sphere_climbs.clear();
		self.wisp_left = 0.0;
		// Forgotten, never lifted: the pipeline may already be the next
		// world's, and the planks would land in it.
		self.causeway_left = 0.0;
		self.causeway.take();
		self.target = None;
		self.fluid = digblock_core::fluid::FluidSim::new();
		self.flow_accum = 0.0;
		// A sweep in flight names columns of the world being left.
		self.world_jobs.abandon();
		if world_replaced {
			// A fresh world's day starts at zero; a retained last_run
			// against the rewound clock would starve every job for a day.
			// `enter_local_world` winds this forward to the SAVE's age
			// afterwards — see there for why zero is wrong for one.
			self.world_secs = 0.0;
			self.world_jobs.reset();
			self.presence.reset();
		}
		self.tick_accum = 0.0;
		self.session.pending_ack = None;
		self.session.correction_offset = Vec3::ZERO;
		// Parked net events belong to the session that queued them. `pump_net`
		// clears the backlog when IT observes a disconnect, but a menu-driven
		// Leave Server drops `self.session.client` outright, so `pump_net` returns early
		// and the queue simply freezes — thousands of a half-streamed join
		// sync's edits waiting to be applied to whatever world comes next.
		// (A stale `Welcome` in there re-swaps the world to the old seed; a
		// stale `Disconnected` kills the fresh connection on arrival.)
		self.session.net_backlog.clear();
		// A stamp owed to the world being left must not pave the next one.
		self.pending_stamp.clear();
		// Ports are derived from the SEED, so a roster resolved against
		// the world just left would send a cast to a place in it.
		self.save.offline_waystones = digblock_core::waystones::Roster::new(self.seed);
		// Likewise session state: a cell one server refused stays refused at
		// the same coordinates in the next world otherwise, because the only
		// other clear needs the dig button held AND aimed elsewhere.
		self.refused_cell = None;
		self.drawn_cell = None;
		// Confetti is thrown at a world position; a new world has none of
		// them left hanging in the air. Same for the essence draw, which
		// names a cell in the world just left, the teleport light, and
		// the channel's fade.
		self.vfx.confetti.clear();
		self.vfx.siphon = None;
		self.vfx.casts.clear();
		self.vfx.kindled.clear();
		self.vfx.channel_env = 0.0;
		self.vfx.flash = 0.0;
		// The arrival marker follows the world's own spawn: the pad's
		// floor centre under the spawn eye.
		self.vfx.marker_sites = vec![Vec3::new(
			self.save.spawn_pos.x,
			self.save.spawn_pos.y - digblock_core::player::EYE_TO_FEET,
			self.save.spawn_pos.z,
		)];
		// And the bench job, which survives the SCREEN closing but not
		// the world: it points at this world's stacks and this server's
		// bucket, neither of which exists in whatever comes next.
		self.ui.alch_screen.abandon();
		self.session.tick_lerp.reset(pos);
		// A reused pipeline (entering the world already streamed behind
		// the menu) keeps its meshes and waterfall voices — the world is
		// identical, clearing would only force a full re-upload.
		if world_replaced {
			self.stream.pending_uploads.clear();
			// `clear_chunks` already dropped every GPU mesh, so queued
			// releases are not just redundant but dangerous: those
			// positions exist in the NEW world too, and releasing one later
			// would delete a chunk that legitimately belongs there.
			self.stream.pending_evicts.clear();
			self.stream.pending_evict_set.clear();
			self.stream.upload_sort_center = ChunkPos::new(i32::MAX, 0, i32::MAX);
			if let Some(r) = self.renderer.as_mut() {
				r.clear_chunks();
			}
			let mut cmds = std::mem::take(&mut self.audio.audio_scratch);
			self.audio.water_audio.clear(&mut cmds);
			for cmd in cmds.drain(..) {
				self.emit(cmd);
			}
			self.audio.audio_scratch = cmds;
		}
		// Old-world sounds don't survive the swap.
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_DIG,
		});
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_CAST,
		});
		self.audio.ambient.reset();
		self.audio.prospect.reset();
		self.audio.swim_strokes.reset();
		self.vfx.view_bob.reset();
		self.vfx.view_swing.reset();
		self.vfx.view_cast.reset();
		self.vfx.view_swap.reset();
		// The tool slot's handover animates whenever the active tool changes,
		// which is right for a cycle and wrong for a world entry: the bar we
		// are about to draw is state ARRIVING, not an exchange anybody
		// performed. Advancing by a full window completes whatever was in
		// flight and adopts the tool the bar holds now.
		//
		// A bare `Default::default()` will not do — it shows the BUCKET, so
		// entering a save whose layout makes any other tool active would grow
		// that tool in from a bucket that was never in the hand. Callers
		// install the layout first; a join has none yet, and the server
		// `Layout` rebase animating when it lands is what `ToolSwap` documents.
		self.input
			.tool_swap
			.advance(crate::hotbar::ToolSwap::SECS, self.hotbar.active_tool());
		// A replaced pipeline starts with no Mining level at all, so a
		// world entry has to hand it the level again or a high-level miner
		// walks into plain stone until the next level-up. Silent: the
		// discovery was made long ago.
		self.sync_node_reveal(false);
		self.audio.prev_player_pos = pos;
		self.audio.prev_wet = true;
		self.audio.fall_from_y = pos.y;
	}

	/// Rebuild the standing-spiral anchors from what this world knows:
	/// the spawn pad, plus every resolved waystone. Eyes become pad-floor
	/// positions — the spiral rises out of the platform, not the air.
	pub(super) fn refresh_marker_sites(&mut self) {
		let pad = |eye: Vec3| Vec3::new(eye.x, eye.y - digblock_core::player::EYE_TO_FEET, eye.z);
		let mut sites = vec![pad(self.save.spawn_pos)];
		for &(_, eye) in &self.session.waystones {
			sites.push(pad(Vec3::from_array(eye)));
		}
		self.vfx.marker_sites = sites;
	}

	/// Flush and detach the active named save (leaving for a server,
	/// another save, or a backdrop world). Drains this frame's recorded
	/// edits and flushes WITH the live overlay before closing — a
	/// snapshot-based store (web) serializes from that overlay, and this
	/// is its last chance to see it before the pipeline is replaced.
	pub(super) fn close_local_save(&mut self) {
		// Before the edits are drained: a sphere still climbing has to come
		// back down INTO this save, or the file keeps half a shell and the
		// caster keeps none of the bill. Every path that
		// puts a local world down comes through here.
		self.abandon_sphere();
		let recorded = self.pipeline.take_recorded_edits();
		if let Some(store) = self.save.world_store.as_mut()
			&& self.save.in_local_save
		{
			for (x, y, z, cell) in recorded {
				store.append_edit(x, y, z, cell);
			}
			store.flush(self.pipeline.overlay());
			store.close_world();
		}
		self.save.in_local_save = false;
	}

	/// Swap to a fresh backdrop world and relock the startup menu — the shared tail of leaving a
	/// server and leaving a local world. A new clock-derived seed means the menu's backdrop is
	/// visibly not the save behind it, rather than phantom-playing the world we just left. The
	/// caller detaches whatever it was attached to first ([`Self::close_local_save`] / dropping
	/// the transport).
	pub(super) fn return_to_title(&mut self) {
		// While the world under it is still this one: a climb in flight
		// has glass standing in it and a fare spent on it. The local-save
		// paths reach this through `close_local_save`; a server we just
		// left has no save to flush, so it comes through here.
		self.abandon_sphere();
		let nanos = web_time::SystemTime::now()
			.duration_since(web_time::UNIX_EPOCH)
			.map_or(0, |d| d.subsec_nanos());
		let seed = Seed::current(nanos as f64 / u32::MAX as f64 * 10000.0);
		self.seed = seed;
		let spawn = digblock_core::player::spawn(seed);
		self.save.spawn_pos = spawn;
		// The backdrop is ephemeral — nothing persists, so nothing needs
		// an arrival lock, and the world just left must not leak its own.
		self.protected.clear();
		self.install_pipeline(Pipeline::with_overlay(
			seed,
			self.config.draw_dist,
			self.config.workers,
			digblock_core::overlay::EditOverlay::default(),
		));
		self.player = Player::new(spawn);
		self.camera.pos = spawn;
		self.inventory = Inventory::default();
		self.skills = Default::default();
		self.vfx.toasts.clear();
		self.auth_essence = None;
		self.session.player_names.clear();
		self.reset_world_transients(spawn, true);
		self.save.in_local_save = false;
		self.ui.menu.on_returned_to_title();
	}

	/// Enter an opened named save: rebuild the world around its seed +
	/// edit overlay, restore inventory and (when present) the player's
	/// last position and view.
	pub(super) fn enter_local_world(&mut self, save: digblock_persist::WorldSave) {
		log::info!(
			"entering local world '{}' (seed {:?})",
			save.meta.name,
			save.meta.seed
		);
		self.session.player_names.clear();
		let spawn = digblock_core::player::spawn(save.meta.seed);
		self.save.spawn_pos = spawn;
		let overlay: digblock_core::overlay::EditOverlay = save.edits.into_iter().collect();
		// Same world already streamed in behind the menu (Create adopts the backdrop's seed to hit
		// this deliberately) and no edits on either side: keep the pipeline, its generated chunks
		// and uploaded meshes, instead of regenerating. That window is when the player would
		// otherwise stand over an empty world, held by the walk gate — seconds long on web's
		// inline executor.
		let reuse = self.session.client.is_none()
			&& save.meta.seed == self.seed
			&& overlay.is_empty()
			&& self.pipeline.overlay().is_some_and(|o| o.is_empty());
		self.seed = save.meta.seed;
		if !reuse {
			self.install_pipeline(Pipeline::with_overlay(
				save.meta.seed,
				self.config.draw_dist,
				self.config.workers,
				overlay,
			));
		}
		// Resume in place when the save carries a position (edits restore
		// identically, so the spot the player stood on regenerates under
		// them); a fresh save starts at the world spawn.
		let start = save.player.map_or(spawn, |p| Vec3::from_array(p.pos));
		self.player = Player::new(start);
		self.camera.pos = start;
		if let Some(p) = save.player {
			self.camera.yaw_deg = p.yaw;
			self.camera.pitch_deg = p.pitch.clamp(-89.0, 89.0);
		}
		self.inventory = Inventory::from_wire(&save.inventory);
		// §13.5's migration grant: a save written before the bucket
		// joined the catalog came from a build where every player owned
		// one, so it keeps its bucket. Never a windfall — a fresh world
		// opens AT the current revision and gets nothing, so the only
		// saves this touches really did have the tool.
		if save.rev < digblock_persist::REV_CRAFTED_BUCKET {
			self.inventory.add(BlockType::Bucket, 1);
		}
		// §14's one-time balance reset: essence below this rev was earned
		// against the unbounded bench the redesign removed, so everything
		// but Aether clears — the server's record reader runs the same
		// reset at its own version, so online and local careers cross the
		// redesign identically.
		if save.rev < digblock_persist::REV_FOUND_ESSENCE {
			self.inventory.clear_pre_redesign_essence();
		}
		// Protection is always on offline too: the pipeline is the
		// authority here, so it enforces the spawn sphere itself — around
		// the RESOLVED spawn, so a save whose anchor was dug out before
		// this rule existed locks the relocated pad, never the crater.
		self.protected = vec![digblock_core::spawn::lock_region(
			self.local_spawn_resolved(),
		)];
		// The save's skill totals (empty for pre-skills saves = level 1);
		// stale toasts belong to whatever world we just left.
		self.skills = digblock_core::skills::SkillSet::from_wire(&save.skills);
		self.vfx.toasts.clear();
		// Saved arrangement first (sanitizing decode), then reconcile as
		// the backstop: legacy saves get the derived layout, and types
		// the arrangement doesn't cover still land on the bar or grid.
		if let Ok(wire) =
			<[u16; digblock_core::hotbar::HOTBAR_SLOTS]>::try_from(save.hotbar.as_slice())
		{
			self.hotbar = Hotbar::from_wire(&wire);
		}
		self.inv_grid = digblock_core::inv_grid::InvGrid::from_wire(&save.grid);
		digblock_core::inv_grid::reconcile_layout(
			&self.inventory,
			&mut self.hotbar,
			&mut self.inv_grid,
		);
		self.reset_world_transients(start, !reuse);
		// The scheduler's clock is the world's AGE, never this session's.
		// `worldjob::regrow` salts its roll with the run's SLOT, so a
		// clock that restarted at zero on every entry replayed the same
		// slot and rolled the same answer: a spent node that lost its
		// roll lost it again, for ever. Both stamps are already saved and
		// the store writes `last_played_ms` as it hands the save over, so
		// their difference is how old this world is right now — monotone
		// across entries with no new persisted field. Catch-up is capped
		// at one run, so a world returned to after a week sweeps once.
		self.world_secs = save
			.meta
			.last_played_ms
			.saturating_sub(save.meta.created_ms) as f64
			/ 1000.0;
		// The marker stands on the RESOLVED pad — the eye the lock
		// anchors on — not the pristine anchor a damaged save moved off.
		let pad = self.local_spawn_resolved();
		self.vfx.marker_sites = vec![Vec3::new(
			pad.x,
			pad.y - digblock_core::player::EYE_TO_FEET,
			pad.z,
		)];
		// And the pad wears its structure offline too: the same pure
		// stamp the server applies, owed until each chunk is resident
		// and drained in `update` — idempotent, so an already-stamped
		// save owes nothing.
		self.pending_stamp = digblock_core::waystones::stamp(pad);
		self.save.in_local_save = true;
		self.save.player_save_accum = 0.0;
		self.ui.menu.on_world_entered();
	}

	/// Handle one menu world intent against the injected store.
	pub(super) fn handle_world_intent(&mut self, intent: crate::menu::WorldIntent) {
		use crate::menu::WorldIntent;
		match intent {
			WorldIntent::Refresh => self.push_world_list(),
			WorldIntent::Create { name } => {
				// On a backdrop world (not in a save, not on a server), adopt the backdrop's
				// seed: the world behind the menu is exactly the one being created, so
				// `enter_local_world` keeps the already-streamed pipeline instead of regenerating
				// it. Otherwise a fresh clock-derived seed (no `rand` — wasm cleanliness).
				let seed = if self.session.client.is_none() && !self.save.in_local_save {
					self.seed
				} else {
					let nanos = web_time::SystemTime::now()
						.duration_since(web_time::UNIX_EPOCH)
						.map_or(0, |d| d.subsec_nanos());
					// A world created here is created NOW, under the current generator.
					Seed::current(nanos as f64 / u32::MAX as f64 * 10000.0)
				};
				let opened = self.save.world_store.as_mut().map(|store| {
					store
						.create_world(&name, seed)
						.and_then(|meta| store.open_world(&meta.id))
				});
				match opened {
					Some(Ok(save)) => {
						self.close_local_save();
						self.enter_local_world(save);
					}
					Some(Err(e)) => {
						self.ui.menu.set_worlds_status(e);
						self.push_world_list();
					}
					None => {}
				}
			}
			WorldIntent::Enter { id } => {
				// Close the current save BEFORE opening the next (single
				// active world per store).
				self.close_local_save();
				let opened = self.save.world_store.as_mut().map(|s| s.open_world(&id));
				match opened {
					Some(Ok(save)) => self.enter_local_world(save),
					Some(Err(e)) => self.ui.menu.set_worlds_status(e),
					None => {}
				}
			}
			WorldIntent::Delete { id } => {
				if let Some(store) = self.save.world_store.as_mut() {
					// The store drops the active world itself if this is it.
					store.delete_world(&id);
				}
				self.push_world_list();
			}
		}
	}

	/// Send the store's world list to the menu, ages formatted.
	pub(super) fn push_world_list(&mut self) {
		let Some(store) = self.save.world_store.as_mut() else {
			return;
		};
		let now = web_time::SystemTime::now()
			.duration_since(web_time::UNIX_EPOCH)
			.map_or(0, |d| d.as_millis() as u64);
		let rows = store
			.list_worlds()
			.into_iter()
			.map(|w| crate::menu::WorldRow {
				detail: if w.compatible {
					age_label(now, w.last_played_ms)
				} else {
					"old version".into()
				},
				id: w.id,
				name: w.name,
				compatible: w.compatible,
			})
			.collect();
		self.ui.menu.set_world_list(rows);
	}

	/// Hold the world after an arrival — a join or a teleport — until the server reports the
	/// journaled columns around the destination sent (`NetEvent::AreaReady`), or the wait runs out.
	/// Offline arrivals never hold, guaranteed by the CALL SITES: both are `pump_net` arms and an
	/// offline session produces no `NetEvent`s. Never guard this on `self.session.client` —
	/// `pump_net` owns it by value for the drain, so `is_some()` reads false on both hold paths.
	pub(super) fn begin_arrival_hold(&mut self) {
		self.save.arrival_hold = Some(digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS);
	}

	/// Release it, whichever way it ended. Also closes the join's
	/// Connecting screen, which is a no-op for a teleport (that path
	/// covers itself from the HUD) and for a hold that timed out with the
	/// menu already gone.
	pub(super) fn end_arrival_hold(&mut self) {
		if self.save.arrival_hold.take().is_some() {
			self.ui.menu.on_welcome();
			// The pose we are about to start simulating from is the one the
			// server gave us, now standing on the real world rather than on
			// worldgen. Re-anchor so the first drawn frame does not
			// interpolate out of wherever the held frames left the lerp.
			self.session.tick_lerp.reset(self.player.pos);
			self.audio.prev_player_pos = self.player.pos;
		}
	}
}
