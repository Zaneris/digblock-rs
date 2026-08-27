//! Construction and the launcher's injection points. Every `set_*`
//! here is optional by design — the engine boots headless with none of
//! them, which is what makes bench and screenshot runs hermetic.

use super::*;
use digblock_core::worldgen::Seed;

impl Engine {
	pub fn new(config: EngineConfig) -> Self {
		// Pay the sub-block variant bakes here rather than in whichever
		// frame first meshes a door (see `shapes::prewarm`).
		digblock_core::shapes::prewarm();
		let mut metrics = Metrics::new(config.metrics_csv.as_deref(), config.spike_log);
		if config.bench {
			metrics.record_run_history();
		}
		// Java: seed = Math.random() * 10000, a domain offset for the noise.
		// Clock-derived instead of `rand`: good enough for a default world
		// seed, and it keeps getrandom (and its wasm backend cfg) out of
		// the dependency tree.
		// A world starting here is a world being MADE, so it is stamped with the current
		// generator. Every other `Seed` the engine holds arrives recorded — from a save's
		// header, or from the server's `Welcome`.
		let seed = Seed::current(config.seed.unwrap_or_else(|| {
			let nanos = web_time::SystemTime::now()
				.duration_since(web_time::UNIX_EPOCH)
				.map_or(0, |d| d.subsec_nanos());
			nanos as f64 / u32::MAX as f64 * 10000.0
		}));
		let far = far_plane(config.draw_dist);
		// Even the ephemeral backdrop world gets an overlay: local edits
		// must survive chunk eviction within a session, saved or not.
		let pipeline = Pipeline::with_overlay(
			seed,
			config.draw_dist,
			config.workers,
			digblock_core::overlay::EditOverlay::default(),
		);
		// The pipeline's own count, not `config.workers`: on the web that
		// is a REQUEST the launcher may not be able to honour, and a pool
		// is attached later (and reports zero until its workers answer).
		log::info!(
			"world seed: {seed:?} | {} chunk workers",
			pipeline.worker_count()
		);
		let sun = config.tod.map_or_else(Sun::new, Sun::with_tod);
		let overlay_visible = config.overlay;
		let stream_floor = config.stream_budget_ms;
		// `--pos` replaces the seed's spawn wholesale, debug plans and
		// return-to-spawn included: the point of standing somewhere is that
		// everything keyed on the spawn follows you there.
		let spawn = config
			.start_pos
			.map_or_else(|| digblock_core::player::spawn(seed), Vec3::from);
		// DIG_ORE hollows its chamber around a deposit and stands the
		// camera IN it, so the scene has to resolve before the pose is
		// baked into the camera, the player and the tick lerp. An explicit
		// `--pos` still wins: the scene only moves a camera that had
		// nowhere particular to be.
		let ore_scene = if config.debug_ore > 0 {
			let want = digblock_core::worldgen::ore::ORES[config
				.debug_ore
				.clamp(1, digblock_core::worldgen::ore::ORES.len())
				- 1];
			debug_ore_plan(spawn, seed, want)
		} else if let Some(node) = config.debug_node {
			debug_node_plan(spawn, seed, node)
		} else {
			(spawn, Vec::new())
		};
		let spawn = if config.start_pos.is_some() {
			spawn
		} else {
			ore_scene.0
		};
		let mut camera = FlyCamera::new(far, spawn);
		if let Some(yaw) = config.yaw {
			camera.yaw_deg = yaw;
		}
		if let Some(pitch) = config.pitch {
			camera.pitch_deg = pitch.clamp(-89.0, 89.0);
		}
		let mut menu = crate::menu::Menu::new(
			config.menu,
			crate::settings::Settings {
				draw_dist: config.draw_dist,
				music_vol: config.music_vol,
				sfx_vol: config.sfx_vol,
				ssr: config.ssr,
				ui_scale: config.ui_scale,
				render_scale: config.render_scale,
				gi_scale_div: config.gi_scale_div,
				max_lights: config.max_lights,
				touch_jump_button: config.touch_jump_button,
			},
			config.auto_join.clone(),
			config.account.clone(),
		);
		menu.set_draw_min(config.draw_min);
		if config.touch_hud {
			menu.set_touch();
		}
		if config.fullscreen_button {
			menu.set_fullscreen_available();
		}
		if config.fullscreen_hint {
			menu.set_fullscreen_hint();
		}
		let mut engine = Self {
			ui: UiState {
				size: (1, 1),
				scale_factor: 1.0,
				ui_scale: 1.0,
				ui_dl: crate::ui::DrawList::default(),
				ui_hits: crate::ui::HitMap::default(),
				inv_screen: Default::default(),
				debug_hover: None,
				alch_screen: Default::default(),
				char_screen: Default::default(),
				hiscores: Default::default(),
				spellbook: Default::default(),
				menu,
				chat: crate::chat::Chat::default(),
				overlay_visible,
			},
			audio: AudioState {
				sink: None,
				water_audio: crate::water_audio::WaterAudio::default(),
				footsteps: crate::audio::Footsteps::default(),
				swim_strokes: crate::audio::SwimStrokes::default(),
				ambient: crate::audio::Ambient::default(),
				prospect: crate::audio::Prospect::default(),
				audio_scratch: Vec::new(),
				prev_player_pos: spawn,
				music_night: None,
				prev_wet: true,
				fall_from_y: spawn.y,
			},
			vfx: Vfx {
				confetti: Vec::new(),
				confetti_seq: 0,
				siphon: None,
				casts: Vec::new(),
				cast_seq: 0,
				kindled: Vec::new(),
				kindle_seq: 0,
				channel_env: 0.0,
				flash: 0.0,
				// The startup world marks its spawn too: the marker is the
				// pad's identity, and captures photograph it here.
				marker_sites: vec![Vec3::new(
					spawn.x,
					spawn.y - digblock_core::player::EYE_TO_FEET,
					spawn.z,
				)],
				fx_scratch: Vec::new(),
				marker_scratch: Vec::new(),
				view_bob: crate::viewmodel::Bob::default(),
				view_swing: crate::viewmodel::Swing::default(),
				view_cast: crate::viewmodel::Cast::default(),
				view_swap: crate::viewmodel::Swap::default(),
				toasts: Vec::new(),
				nametag_scratch: Vec::new(),
				nametag_count: 0,
				remote_scratch: Vec::new(),
				remote_instances: Vec::new(),
				held_instances: Vec::new(),
				history_scratch: Vec::new(),
				carried: Carried::default(),
			},
			// Assume wet: whatever the spawn turns out to be, arriving
			// there is not "entering the water".
			services: None,
			ranks: None,
			board: Default::default(),
			settings_store: None,
			clipboard: None,
			save: SaveSession {
				world_store: None,
				in_local_save: false,
				player_save_accum: 0.0,
				offline_waystones: digblock_core::waystones::Roster::new(seed),
				spawn_pos: spawn,
				teleport_cooldown: 0.0,
				spawn_return_pending: false,
				arrival_hold: None,
			},
			debug_sphere_pending: config.debug_sphere,
			sphere_climbs: Vec::new(),
			debug_shapes_pending: {
				let mut plan = if config.debug_shapes {
					debug_shapes_plan(spawn)
				} else {
					Vec::new()
				};
				if config.debug_doors {
					plan.extend(debug_doors_plan(spawn));
				}
				if config.debug_crack.is_some() {
					plan.extend(debug_crack_plan(spawn));
				}
				if config.debug_vault {
					plan.extend(debug_vault_plan(spawn, !config.debug_vault_dark));
				}
				if config.debug_ore > 0 || config.debug_node.is_some() {
					plan.extend(ore_scene.1.clone());
				}
				if config.debug_roof {
					plan.extend(debug_roof_plan(spawn));
				}
				if config.debug_tunnel {
					plan.extend(debug_tunnel_plan(spawn));
				}
				if config.debug_dive {
					plan.extend(debug_dive_plan(spawn));
				}
				if config.debug_river {
					plan.extend(debug_river_plan(spawn));
				}
				if config.debug_torches > 0 {
					plan.extend(debug_torches_plan(
						spawn,
						config.debug_torches,
						config.debug_torches_far,
						config.debug_torches_wall,
					));
				}
				plan
			},
			config,
			seed,
			renderer: None,
			pipeline,
			camera,
			player: Player::new(spawn),
			input: InputState {
				touch: TouchDriver::new(),
				ui_touch: None,
				hud_touches: Vec::new(),
				hud_jump_held: false,
				fullscreen_requested: false,
				safe_area: crate::ui::hud::SafeArea::default(),
				walking: true,
				fly_key_down: false,
				overlay_key_down: false,
				gi_drill_key_down: false,
				keys: HeldKeys::default(),
				look_accum: (0.0, 0.0),
				dig_held: false,
				click_place: false,
				tool_swap: crate::hotbar::ToolSwap::default(),
			},
			sky_tally: Vec::new(),
			sky_stage: Vec::new(),
			sky_cursor: 0,
			sky_origin: (0, 0),
			bench: BenchState {
				metrics,
				bench_reported: false,
				pending: FrameTimings::default(),
				frame_start: None,
				prep_start: None,
				bench_t: 0.0,
				fly_frames: 0,
				walk_origin: None,
				frames_seen: 0,
			},
			tick_accum: 0.0,
			sun,
			refused_cell: None,
			drawn_cell: None,
			dig: None,
			cast: None,
			arc_on: false,
			wisp_left: 0.0,
			causeway_left: 0.0,
			causeway: Default::default(),
			last_cast: None,
			dig_accum: 0.0,
			target: None,
			inventory: Inventory::default(),
			auth_essence: None,
			hotbar: Hotbar::new(),
			// The startup world is ephemeral (a backdrop, or a capture
			// scene) — no arrival lock; entering a NAMED save or a server
			// world replaces this per `world_session`.
			protected: Vec::new(),
			pending_stamp: Vec::new(),
			fluid: digblock_core::fluid::FluidSim::new(),
			world_jobs: digblock_core::worldjob::Scheduler::new(),
			presence: digblock_core::presence::RollingPeak::day(),
			flow_accum: 0.0,
			time_secs: 0.0,
			world_secs: 0.0,
			session: NetSession {
				client: None,
				net_events: Vec::new(),
				net_backlog: std::collections::VecDeque::new(),
				pending_ack: None,
				correction_offset: Vec3::ZERO,
				tick_lerp: crate::smoothing::TickLerp::new(spawn),
				waystones: Vec::new(),
				net_corrections: 0,
				net_error_accum: 0.0,
				net_log_timer: 0.0,
				feed: Vec::new(),
				feed_queue: std::collections::VecDeque::new(),
				feed_drip: 0.0,
				player_names: rustc_hash::FxHashMap::default(),
				my_player_id: None,
				last_layout_sent: None,
			},
			stream: StreamQueues {
				pending_uploads: std::collections::VecDeque::new(),
				pending_evicts: std::collections::VecDeque::new(),
				pending_evict_set: rustc_hash::FxHashSet::default(),
				gi_queue: VecDeque::new(),
				gi_queued: rustc_hash::FxHashSet::default(),
				upload_sort_center: ChunkPos::new(i32::MAX, 0, i32::MAX),
				evict_burst: false,
				stream_budget: digblock_core::stream_budget::StreamBudget::new(stream_floor),
			},
			ambient_height: None,
			skills: Default::default(),
			inv_grid: Default::default(),
		};
		// DIG_SCREEN: open a UI screen at startup, stocked, so
		// `--screenshot` can capture it. Screens are input-driven, which
		// left them the one part of the UI a headless capture could not
		// see — and a layout regression on the alchemy page shipped
		// because of exactly that.
		if engine.config.debug_doors {
			// A door is craft-only, so `unlock::pickup` refuses it below
			// its recipe and `dig_progress` draws NOTHING on a cell
			// `diggable` says no to — the same trap DIG_CRACK sidesteps by
			// standing a Dirt wall. Without the level this scene could not
			// photograph the outline it exists for. Read out of the unlock
			// table rather than written down, so retuning the timber set
			// moves the capture with it.
			engine.skills.set_total(
				digblock_core::skills::Skill::Alchemy,
				digblock_core::skills::total_xp_for(digblock_core::unlock::pickup(
					BlockType::LumberDoor,
				)),
			);
		}
		if engine.config.debug_ore > 0 {
			// The scene exists to photograph a tier, so it grants exactly
			// the level that reveals that tier — read out of the unlock
			// table rather than written down, so retuning a reveal moves
			// the capture with it. The sync below every scene is what then
			// hands the level to the pipeline and makes the stamp fire.
			let ore = digblock_core::worldgen::ore::ORES[engine.config.debug_ore.clamp(1, 4) - 1];
			let need = digblock_core::unlock::reveal(ore).unwrap_or(1);
			engine.skills.set_total(
				digblock_core::skills::Skill::Mining,
				digblock_core::skills::total_xp_for(need),
			);
		}
		if let Some(node) = engine.config.debug_node {
			// The surface twin of the DIG_ORE grant: exactly the Alchemy
			// level that reveals the kind, read out of the unlock table.
			// Leystone's chamber needs the ROCK wall cleared too, or the
			// capture stands in front of a find it cannot legally swing at.
			let need = digblock_core::unlock::alchemy_reveal(node).unwrap_or(1);
			engine.skills.set_total(
				digblock_core::skills::Skill::Alchemy,
				digblock_core::skills::total_xp_for(need),
			);
			engine.skills.set_total(
				digblock_core::skills::Skill::Mining,
				digblock_core::skills::total_xp_for(digblock_core::unlock::mine(node)),
			);
		}
		if engine.config.debug_mining > 0 {
			// Last, so an explicit level overrides the one `debug_ore`
			// grants for its tier: the two scenes compose (a chamber at a
			// tier, walked into with a late-game pick), and the one the
			// caller typed is the one they meant.
			engine.skills.set_total(
				digblock_core::skills::Skill::Mining,
				digblock_core::skills::total_xp_for(engine.config.debug_mining),
			);
		}
		if let Some(screen) = engine.config.debug_screen {
			for (block, n) in [
				(BlockType::Wood, 5u32),
				(BlockType::Stone, 11),
				(BlockType::Dirt, 64),
			] {
				engine.inventory.add(block, n);
			}
			engine.inventory.add(BlockType::Rubble, 24);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Vitae, 54);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Terra, 120);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Unda, 7);
			// Metal/Aether too, so the header's five columns photograph
			// populated — two dead zeros read as a layout bug in shots.
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Ferrum, 12);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Aether, 3);
			if let DebugScreen::Bucket { full: true } = screen {
				engine.inventory.add(BlockType::Water, 1);
			}
			// The tools — the bucket included, now that it is crafted
			// like the rest. A tool is never in the grid (§6: not
			// slottable) and the faded rail only draws once a SECOND one
			// is owned, so a stocked bar is the only way any of this —
			// the rail, the split before the tool slot, the authored
			// icons that replace a cube fallback — reaches a capture at
			// all.
			for tool in BlockType::ALL {
				if tool.is_tool() {
					engine.inventory.add(tool, 1);
				}
			}
			// Conditional, so no other capture's inventory grid moves.
			if screen == DebugScreen::Torch {
				engine.inventory.add(BlockType::Torch, 12);
			}
			digblock_core::inv_grid::reconcile_layout(
				&engine.inventory,
				&mut engine.hotbar,
				&mut engine.inv_grid,
			);
			match screen {
				// The menu opens itself when `config.menu` is set; the
				// hand wants a stocked bar and nothing over it.
				DebugScreen::Menu | DebugScreen::Hand => {}
				DebugScreen::Bucket { .. } => engine
					.hotbar
					.select_digit(digblock_core::hotbar::HOTBAR_SLOTS as u8),
				DebugScreen::Tool(want) => {
					engine
						.hotbar
						.select_digit(digblock_core::hotbar::HOTBAR_SLOTS as u8);
					// Reached by CYCLING, the way a player reaches it —
					// so the capture also proves the roster contains the
					// tool it was asked for.
					let owned = engine.owned_tools();
					for _ in 0..owned.len() {
						if engine.hotbar.active_tool() == Some(want) {
							break;
						}
						engine.hotbar.cycle_tool(&owned);
					}
					assert_eq!(engine.hotbar.active_tool(), Some(want), "DIG_SCREEN tool");
				}
				DebugScreen::Torch => {
					// Find the slot rather than writing a number down:
					// `reconcile_layout` files by block id, so retuning the
					// stocking list above would otherwise silently select
					// something else. No skill is needed — `unlock::pickup`
					// gates DIGGING a craft-only block, not holding one.
					let want = Some(HotbarItem::Block(BlockType::Torch));
					if let Some(i) = engine.hotbar.slots().iter().position(|s| *s == want) {
						engine.hotbar.select_digit(i as u8 + 1);
					}
				}
				DebugScreen::Inventory | DebugScreen::InventoryHover => {
					engine.ui.inv_screen.open();
					if screen == DebugScreen::InventoryHover {
						engine.ui.debug_hover = Some(crate::ui::InvId::Bar(0));
					}
				}
				DebugScreen::Alchemy
				| DebugScreen::AlchemyHover
				| DebugScreen::Transmute
				| DebugScreen::Transmuting => {
					// A mid-curve level so the header's bar, the pace,
					// and a mix of locked/unlocked rows all photograph.
					engine
						.skills
						.add_xp(digblock_core::skills::Skill::Alchemy, 20_000);
					engine.ui.alch_screen.open();
					if screen == DebugScreen::AlchemyHover {
						engine.ui.debug_hover = Some(crate::ui::InvId::Family(0));
					}
					if screen == DebugScreen::Transmuting {
						// Stocked well past the batch, so the row reads as
						// a batch running rather than as one about to be
						// cut short by the holdings.
						for e in digblock_core::block::Essence::ALL {
							engine.inventory.add_sub(e, 5_000);
						}
						engine
							.ui
							.alch_screen
							.debug_batch(BlockType::LumberSlab, 7, 0.45);
					}
				}
				DebugScreen::Skills => {
					// Mid-curve totals: fractional bars, real XP lines —
					// and a rank on every board, since no master answers a
					// capture.
					use digblock_core::skills::Skill;
					engine.skills.add_xp(Skill::Mining, 150_000);
					engine.skills.add_xp(Skill::Alchemy, 30_000);
					engine.ranks =
						Some(crate::ui::screens::character::sample_ranks(&engine.skills));
					engine.ui.char_screen.open();
				}
				DebugScreen::Hiscores => {
					// A full page with a page behind it, one row ours —
					// the sample, since no master answers a capture.
					engine
						.board
						.debug_fill(crate::ui::screens::hiscores::sample_board(None));
					engine.ui.hiscores.open();
				}
				DebugScreen::Spellbook | DebugScreen::SpellbookHover => {
					// Past the LAST spell rung the ladder opens below the
					// ports' tail, so every castable row and the next
					// teaser all photograph. XP derived from the rung it
					// must pass, never a magic total.
					use digblock_core::skills::Skill;
					engine.skills.add_xp(
						Skill::Arcana,
						digblock_core::skills::total_xp_for(digblock_core::unlock::CAUSEWAY_AT + 1),
					);
					// Funded, so the fares read as offers rather than as a
					// page of shortfalls — the short ink is a state worth
					// photographing, but not the only one.
					for e in digblock_core::block::Essence::ALL {
						engine.inventory.add_sub(e, 5_000);
					}
					engine.ui.spellbook.open();
					if screen == DebugScreen::SpellbookHover {
						// The slot comes from the page's own roster rather
						// than from a number written down here: `fares`
						// lists the rows that price themselves in essence,
						// in the order the page lays them out, so the
						// first is whatever the first priced spell is.
						let view = crate::ui::screens::spellbook::SpellView::build(
							&engine.skills,
							0,
							&engine.known_ports(),
							&engine.inventory,
							engine.arc_on,
							engine.causeway_left.ceil() as u32,
						);
						if let Some(&(slot, _)) = view.fares().first() {
							engine.ui.debug_hover = Some(crate::ui::InvId::SpellFare(slot));
						}
					}
				}
			}
		}
		// ONE sync, after every scene has had its say. The pipeline stamps nodes
		// at the levels it was last handed, and `nodes::revealed` answers at the
		// live ones, so a grant the pipeline never hears about meshes a cell as
		// its host while the dig path calls it a node — the mesh and the dig
		// disagreeing, which is the failure this whole layer exists to make
		// impossible.
		//
		// Here rather than in each block because it kept being forgotten: three
		// scenes called it and half the `DIG_SCREEN` roster did not. Every grant
		// is above this line, so a new scene is covered by standing where the
		// others do. Silent: a capture is not a discovery.
		engine.sync_node_reveal(false);
		// The same production rules the server registers, from the same
		// constructor — offline is not allowed to fork them. They only run
		// in local worlds; hosted, the results arrive as ordinary edits.
		for job in digblock_core::worldjob::production() {
			engine.register_world_job(job);
		}
		engine
	}

	/// Attach a multiplayer transport (loopback tests / tokenless paths).
	pub fn set_transport(&mut self, transport: Box<dyn digblock_net::transport::NetTransport>) {
		self.set_transport_with_token(transport, [0; 16]);
	}

	/// Attach a multiplayer transport carrying a master-minted one-time
	/// token. The world is swapped to the server's seed when the Welcome
	/// arrives; until then the local world keeps running.
	pub fn set_transport_with_token(
		&mut self,
		transport: Box<dyn digblock_net::transport::NetTransport>,
		token: [u8; 16],
	) {
		let name = self
			.config
			.player_name
			.clone()
			.unwrap_or_else(|| "player".into());
		// Hang up first if something is still attached. Not defensive
		// clutter: one account holds one live connection, so a replaced
		// connection the server has not noticed yet would make the JOIN
		// WE ARE ABOUT TO MAKE the duplicate, and refuse it.
		self.leave_server();
		self.session.client = Some(digblock_net::NetClient::new(
			transport,
			digblock_net::client::NetClientConfig {
				name,
				draw_dist: self.config.draw_dist,
				token,
				legacy_identity: self.config.legacy_identity,
			},
		));
		// Fresh connection: the layout dedup re-primes from the server's
		// join-sync Layout.
		self.session.last_layout_sent = None;
		// The server has no fly physics; walk is the only multiplayer mode.
		self.input.walking = true;
	}

	/// Wire the launcher's master-server services (list fetch, pings, token
	/// mint + connect). Enables the Multiplayer entry in the menu.
	pub fn set_menu_services(&mut self, services: crate::menu::MenuServices) {
		self.services = Some(services);
		self.ui.menu.set_services_available();
	}

	/// Inject the platform audio backend (launcher-owned, like
	/// `MenuServices`). Never called by bench/screenshot/drill runs.
	pub fn set_audio(&mut self, sink: Box<dyn crate::audio::AudioSink>) {
		self.audio.sink = Some(sink);
		self.audio.ambient.reset();
		self.audio.prospect.reset();
		// Persisted volumes apply from the very first sound.
		self.emit(crate::audio::AudioCmd::Volume {
			music: self.config.music_vol,
			sfx: self.config.sfx_vol,
		});
	}

	/// Inject the settings persistence hook (launcher-owned; desktop
	/// writes a data-dir file, web localStorage). Called with each change
	/// batch from the Settings screen.
	pub fn set_settings_store(&mut self, store: crate::settings::SettingsStore) {
		self.settings_store = Some(store);
	}

	/// Inject the launcher's clipboard writer (desktop: arboard; web:
	/// `navigator.clipboard`). Absent = Copy ID quietly does nothing,
	/// which is the right answer for capture runs and iOS.
	pub fn set_clipboard(&mut self, copy: ClipboardSink) {
		self.clipboard = Some(copy);
	}

	/// Inject a platform chunk-job pool (launcher-owned: the web's Web
	/// Workers; native uses its own threads and ignores this). Safe to
	/// call before the pool has finished starting — jobs run inline until
	/// it reports ready.
	pub fn set_job_dispatch(&mut self, dispatch: Box<dyn digblock_core::pipeline::JobDispatch>) {
		self.pipeline.set_dispatch(dispatch);
	}

	/// Swap in a fresh chunk pipeline, carrying any job pool across. Every world change
	/// (server join, return to title, loading a save) goes through here: workers cost real
	/// time to start and hold no world state, so respawning them per world would be pure
	/// loss — and `take_dispatch` is what stops the old world's in-flight columns landing
	/// as terrain in the new one.
	pub(super) fn install_pipeline(&mut self, pipeline: Pipeline) {
		let dispatch = self.pipeline.take_dispatch();
		self.pipeline = pipeline;
		if let Some(dispatch) = dispatch {
			self.pipeline.set_dispatch(dispatch);
		}
	}

	/// Inject the named-save backend (launcher-owned: desktop files, web
	/// localStorage). Root's "Play Local" becomes "Singleplayer".
	pub fn set_world_store(&mut self, store: Box<dyn digblock_persist::WorldStore>) {
		self.save.world_store = Some(store);
		self.ui.menu.set_store_available();
	}

	/// Window size, display density and the user's preference all feed
	/// the one UI scale. Every place any of the three can change routes
	/// through here — miss one and the settings slider appears to do
	/// nothing until the window is resized.
	pub(super) fn recompute_ui_scale(&mut self) {
		let was = self.ui.ui_scale;
		self.ui.ui_scale =
			crate::ui::scale::ui_scale(self.ui.size, self.ui.scale_factor, self.config.ui_scale);
		// Logged with its inputs: a UI the wrong size on one platform is a
		// surface the wrong size, and this line is what says which.
		if self.ui.ui_scale != was {
			log::info!(
				"ui scale {}: {}x{} px at {}x, preference {}",
				self.ui.ui_scale,
				self.ui.size.0,
				self.ui.size.1,
				self.ui.scale_factor,
				self.config.ui_scale
			);
		}
	}

	/// Apply a settings change batch at runtime, then persist it. SSR is a per-frame uniform
	/// bit; volumes ride the audio command stream; draw distance retunes the camera far plane,
	/// the streaming radius (rescan on next maintain), and — connected — the server's sync
	/// radius. The store sees every batch, including name-only changes from the join path.
	pub(super) fn apply_settings(&mut self, s: crate::settings::Settings) {
		self.config.ssr = s.ssr;
		self.config.touch_jump_button = s.touch_jump_button;
		if (s.ui_scale - self.config.ui_scale).abs() > f32::EPSILON {
			self.config.ui_scale = s.ui_scale;
			self.recompute_ui_scale();
		}
		if (s.music_vol - self.config.music_vol).abs() > f32::EPSILON
			|| (s.sfx_vol - self.config.sfx_vol).abs() > f32::EPSILON
		{
			self.config.music_vol = s.music_vol;
			self.config.sfx_vol = s.sfx_vol;
			self.emit(crate::audio::AudioCmd::Volume {
				music: s.music_vol,
				sfx: s.sfx_vol,
			});
		}
		if s.draw_dist != self.config.draw_dist {
			self.config.draw_dist = s.draw_dist;
			self.camera.far = far_plane(s.draw_dist);
			self.pipeline.set_draw_dist(s.draw_dist);
			if let Some(net) = self.session.client.as_mut()
				&& net.ready()
			{
				net.send_set_draw_dist(s.draw_dist);
			}
		}
		// Both render scales are applied by simply storing them: the
		// renderer keys its target rebuild on the scale it last built at,
		// so the next frame re-creates them. No explicit invalidation, and
		// nothing to keep in sync.
		self.config.render_scale = s.render_scale;
		self.config.gi_scale_div = s.gi_scale_div;
		// The light cap rides the frame into `flush_lights`, which tracks
		// it and re-flushes on a change — storing it IS applying it.
		self.config.max_lights = s.max_lights;
		if let Some(store) = &mut self.settings_store {
			store(&s);
		}
	}
}
