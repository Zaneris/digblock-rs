use std::time::Duration;

use digblock_core::chunk::ChunkPos;
use digblock_core::hotbar::{Hotbar, HotbarItem};
use digblock_core::inventory::Inventory;
use digblock_core::metrics::{FrameTimings, Metrics, PipelineGauges};
use digblock_core::pipeline::Pipeline;
use digblock_core::player::{Player, PlayerIntent, TICK};
use digblock_core::raycast::{raycast, raycast_fluid, RayHit, REACH};
use digblock_core::sun::Sun;
use digblock_core::touch::{Phase, TouchDriver};
use digblock_core::BlockType;
use digblock_platform::{Game, Gpu, InputEvent, Key, MouseButton, TouchPhase};
use glam::{Vec2, Vec3};

use crate::camera::FlyCamera;
use crate::renderer::{FrameScene, FrameUniforms, RenderConfig, Renderer};

pub struct EngineConfig {
	/// Draw distance in chunks: 20 desktop, 10 mobile (Java Config).
	pub draw_dist: i32,
	/// Shadow map resolution: 2048 desktop, 1024 mobile.
	pub shadow_size: u32,
	/// Chunk worker threads.
	pub workers: usize,
	pub seed: Option<f64>,
	/// Fixed sun angle in degrees for deterministic captures. Filled by
	/// dev-build flags (or bench internally); wins over the server's
	/// world clock because `update()` re-asserts it every frame.
	pub tod: Option<f32>,
	/// Initial camera yaw/pitch in degrees for deterministic captures
	/// (yaw 0 faces +z, 90 faces +x; pitch defaults to −20).
	pub yaw: Option<f32>,
	pub pitch: Option<f32>,
	pub metrics_csv: Option<std::path::PathBuf>,
	/// Start with the debug overlay visible (` toggles at runtime).
	pub overlay: bool,
	/// Benchmark mode: inputs ignored, deterministic scripted camera
	/// flight, run summary logged on exit.
	pub bench: bool,
	/// Screen-space reflections on water (desktop default; the mobile
	/// config keeps the procedural-sky fallback only).
	pub ssr: bool,
	/// Multiplayer display name; the LAUNCHER fills this (e.g. from
	/// $USER) — the engine itself must stay env-free for wasm.
	pub player_name: Option<String>,
	/// Debug: park a fake remote player 5 blocks ahead (GPU-path check
	/// without a second client). Set by the desktop launcher from
	/// DIG_PLAYERBOX.
	pub debug_playerbox: bool,
	/// Spray droplets per waterfall-impact instance: 64 desktop, 24
	/// mobile (was inferred in the renderer from draw_dist <= 10).
	pub spray_droplets: u32,
	/// Per-frame wall-clock budget for main-thread streaming (see the
	/// STREAM_BUDGET_MS doc below, which is the default).
	pub stream_budget_ms: f32,
	/// Distance fog band as fractions of the far plane.
	pub fog_start_frac: f32,
	pub fog_end_frac: f32,
	/// Force the initial chunk-origin UBO capacity in slots. The LAUNCHER
	/// fills this from DIG_CHUNK_SLOTS (engine stays env-free for wasm);
	/// tiny values exercise the growth path in a normal run.
	pub chunk_slots_override: Option<u32>,
	/// Start with the menu open (desktop interactive runs). Bench,
	/// screenshot, drill, and iOS keep this false — those paths must not
	/// sit on a menu screen.
	pub menu: bool,
	/// Scripted join (a server-name filter; empty string = first listed):
	/// once menu services arrive, the menu walks the exact user join path
	/// unattended. Web `?join` and scripted flows.
	pub auto_join: Option<String>,
	/// Persistent 16-byte identity secret keying the server-side persisted
	/// inventory; all zeros = anonymous. The LAUNCHER fills this (desktop:
	/// data-dir file; web: localStorage) — the engine stays env-free.
	pub identity: [u8; 16],
}

impl EngineConfig {
	pub fn desktop() -> Self {
		let workers = std::thread::available_parallelism().map_or(4, |n| (n.get() - 2).clamp(2, 4));
		Self {
			draw_dist: 20,
			shadow_size: 2048,
			workers,
			seed: None,
			tod: None,
			yaw: None,
			pitch: None,
			metrics_csv: None,
			overlay: false,
			bench: false,
			ssr: true,
			player_name: None,
			debug_playerbox: false,
			spray_droplets: 64,
			stream_budget_ms: STREAM_BUDGET_MS,
			fog_start_frac: 0.70,
			fog_end_frac: 0.95,
			chunk_slots_override: None,
			menu: false,
			auto_join: None,
			identity: [0; 16],
		}
	}

	pub fn mobile() -> Self {
		Self {
			draw_dist: 10,
			shadow_size: 1024,
			workers: 2,
			seed: None,
			tod: None,
			yaw: None,
			pitch: None,
			metrics_csv: None,
			// iOS has no keyboard toggle; flip this for device profiling.
			overlay: false,
			bench: false,
			ssr: false,
			player_name: None,
			debug_playerbox: false,
			spray_droplets: 24,
			stream_budget_ms: STREAM_BUDGET_MS,
			fog_start_frac: 0.70,
			fog_end_frac: 0.95,
			chunk_slots_override: None,
			menu: false,
			auto_join: None,
			identity: [0; 16],
		}
	}

	/// Browser client: full desktop quality (Chrome-on-WebGPU hardware
	/// handles it — verified 120fps at draw 20 + SSR), but jobs run
	/// inline on the main thread (workers: 0 — wasm has no std threads)
	/// and the menu opens by default like a desktop interactive run.
	/// URL params (?draw= ?ssr=0 ?shadow=) opt weaker hardware down.
	pub fn web() -> Self {
		Self {
			workers: 0,
			menu: true,
			..Self::desktop()
		}
	}

	/// The renderer's slice of the config. `wide_slice_draws` has no
	/// per-platform knob yet; the renderer default is the tuned value.
	pub(crate) fn render_config(&self) -> RenderConfig {
		RenderConfig {
			shadow_size: self.shadow_size,
			draw_dist: self.draw_dist,
			spray_droplets: self.spray_droplets,
			wide_slice_draws: crate::renderer::WIDE_SLICE_DRAWS,
			chunk_slots_override: self.chunk_slots_override,
		}
	}
}

#[derive(Default)]
struct HeldKeys {
	w: bool,
	a: bool,
	s: bool,
	d: bool,
	up: bool,
	down: bool,
}

/// Default per-frame wall-clock budget for main-thread streaming
/// (`EngineConfig::stream_budget_ms`) — the modern form of the Java
/// one-queue-item-per-frame rule. The first half bounds the result
/// drain (chunk inserts + neighbor cascades); the rest bounds mesh
/// uploads, with leftovers carried in `pending_uploads` to later frames.
/// Workers keep producing at full speed — undrained results wait in the
/// channel — so this trades a longer fill tail for even frame pacing:
/// streaming + render must fit a 120Hz vsync frame (8.3ms).
const STREAM_BUDGET_MS: f32 = 3.0;
/// Uploads always make this much progress per frame even with the budget
/// exhausted, so a saturated drain can never starve the upload queue.
const UPLOAD_MIN: usize = 4;

pub struct Engine {
	config: EngineConfig,
	/// Resolved world seed (config.seed or a random one).
	seed: f64,
	renderer: Option<Renderer>,
	pipeline: Pipeline,
	camera: FlyCamera,
	player: Player,
	touch: TouchDriver,
	/// Walk (player physics) vs debug fly camera; F toggles.
	walking: bool,
	fly_key_down: bool,
	overlay_key_down: bool,
	tick_accum: f32,
	sun: Sun,
	wide_due: bool,
	keys: HeldKeys,
	look_accum: (f32, f32),
	/// Left mouse button currently held (hold-to-dig); right stays an edge.
	dig_held: bool,
	click_place: bool,
	/// In-progress hold-to-dig, advanced at 60 Hz while `dig_held` stays
	/// on a valid, unchanged target.
	dig: Option<DigProgress>,
	/// Fixed-rate accumulator for the dig state machine — independent of
	/// the walk/fly tick loops so digging works in both modes.
	dig_accum: f32,
	/// The block the crosshair rests on, recomputed every update (drives
	/// the highlight overlay and the click/dig paths). Fluid-aware when
	/// the water tool is selected.
	target: Option<RayHit>,
	/// Predicted block counts; the server's InventoryFull + per-edit
	/// confirms/rejects keep it honest. Offline it is simply the truth.
	inventory: Inventory,
	hotbar: Hotbar,
	/// Edit-time water flow (digblock-core fluid rules); idle unless
	/// edits schedule cells.
	fluid: digblock_core::fluid::FluidSim,
	flow_accum: f32,
	/// Shader animation clock, wrapped at 600s (carried in the otherwise
	/// unused light_color.w — no uniform layout change).
	time_secs: f32,
	/// Multiplayer client; None = single-player (every connected branch
	/// below is dead in that case).
	net: Option<digblock_net::NetClient>,
	net_events: Vec<digblock_net::NetEvent>,
	/// Snapshot ack carried from the net drain to the walk branch (the
	/// reconcile needs the physics `block_at` borrow).
	pending_ack: Option<(u32, digblock_core::player::PlayerState)>,
	/// Visual-only reconciliation smoothing: camera = player + offset,
	/// decaying ~200 ms so corrections nudge instead of teleport.
	correction_offset: Vec3,
	/// Per-frame remote-player sampling scratch.
	remote_scratch: Vec<digblock_net::remote::RemoteDraw>,
	remote_instances: Vec<[f32; 8]>,
	/// Reconciliation health, logged every ~5 s while connected.
	net_corrections: u32,
	net_error_accum: f32,
	net_log_timer: f32,
	/// Server-message feed (join/leave/disconnect/chat) as
	/// (text, ttl, 0xAABBGGRR color), drawn by the overlay
	/// above the HUD; (text, seconds left) — fades over the last second.
	feed: Vec<(String, f32, u32)>,
	/// id → name, learned from PlayerJoined (roster + live joins), so
	/// PlayerLeft (id-only on the wire) can be displayed by name.
	player_names: rustc_hash::FxHashMap<u8, String>,
	/// Our own id from Welcome. The roster never includes ourselves, so
	/// our chat echo resolves through this + config.player_name.
	my_player_id: Option<u8>,
	metrics: Metrics,
	/// Phase timings for the in-progress frame, handed to metrics at the
	/// end of render().
	pending: FrameTimings,
	overlay_visible: bool,
	history_scratch: Vec<f32>,
	/// Scripted-flight clock (bench mode), in fixed 60Hz ticks.
	bench_t: f32,
	/// Meshes drained but not yet uploaded (upload budget carry-over).
	pending_uploads: std::collections::VecDeque<digblock_core::pipeline::MeshOutput>,
	size: (u32, u32),
	/// Startup/pause menu state (pure; overlay renders its view).
	menu: crate::menu::Menu,
	/// Chat input line (pure; overlay renders its view).
	chat: crate::chat::Chat,
	/// Injected audio backend; None (bench, iOS) makes every emit free.
	audio: Option<Box<dyn crate::audio::AudioSink>>,
	/// Waterfall loop-voice reconciler (fed by mesh impacts).
	water_audio: crate::water_audio::WaterAudio,
	/// Footstep cadence + ambient-bed state (pure helpers).
	footsteps: crate::audio::Footsteps,
	ambient: crate::audio::Ambient,
	/// Scratch for batched audio commands (water voices).
	audio_scratch: Vec<crate::audio::AudioCmd>,
	/// Player eye position last frame (footstep distance).
	prev_player_pos: Vec3,
	/// Per-frame name-tag scratch (reused allocation).
	nametag_scratch: Vec<crate::renderer::NameTag>,
	/// Launcher-owned HTTP/probe services the menu talks to; None = no
	/// master URL configured (Multiplayer shows as disabled).
	services: Option<crate::menu::MenuServices>,
	/// The world's spawn point (the server's after a Welcome swap):
	/// the return-to-spawn target.
	spawn_pos: Vec3,
	/// Seconds left on the return-to-spawn cooldown (0 = ready). One
	/// gate for local and connected play; connected, the server also
	/// enforces its own copy.
	spawn_cooldown: f32,
}

/// Client-side hold-to-dig progress (mirrors the server's `DigState`;
/// purely predictive — the server re-times the interval itself).
struct DigProgress {
	cell: (i32, i32, i32),
	block: BlockType,
	ticks: u16,
	required: u16,
}

fn ms_since(t: web_time::Instant) -> f32 {
	t.elapsed().as_secs_f32() * 1000.0
}

/// Stable, distinct color per player id: golden-ratio hue walk, flat
/// saturation/value via a cheap HSV→RGB.
fn player_color(id: u8) -> [f32; 3] {
	let h = (id as f32 * 0.618_034) % 1.0 * 6.0;
	let x = 1.0 - (h % 2.0 - 1.0).abs();
	let (r, g, b) = match h as u32 {
		0 => (1.0, x, 0.0),
		1 => (x, 1.0, 0.0),
		2 => (0.0, 1.0, x),
		3 => (0.0, x, 1.0),
		4 => (x, 0.0, 1.0),
		_ => (1.0, 0.0, x),
	};
	// Lift toward pastel so the lambert shading stays readable.
	[0.35 + 0.55 * r, 0.35 + 0.55 * g, 0.35 + 0.55 * b]
}

/// Pack linear RGB into the overlay's 0xAABBGGRR vertex color (opaque).
fn pack_rgb(rgb: [f32; 3]) -> u32 {
	let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
	0xFF00_0000 | (c(rgb[2]) << 16) | (c(rgb[1]) << 8) | c(rgb[0])
}

/// World-space center of a block cell (positional-audio anchor).
fn cell_center(cell: (i32, i32, i32)) -> [f32; 3] {
	[
		cell.0 as f32 + 0.5,
		cell.1 as f32 + 0.5,
		cell.2 as f32 + 0.5,
	]
}

impl Engine {
	pub fn new(config: EngineConfig) -> Self {
		let mut metrics = Metrics::new(config.metrics_csv.as_deref());
		metrics.workers = config.workers as u32;
		if config.bench {
			metrics.record_run_history();
		}
		// Java: seed = Math.random() * 10000, a domain offset for the noise.
		// Clock-derived instead of `rand`: good enough for a default world
		// seed, and it keeps getrandom (and its wasm backend cfg) out of
		// the dependency tree.
		let seed = config.seed.unwrap_or_else(|| {
			let nanos = web_time::SystemTime::now()
				.duration_since(web_time::UNIX_EPOCH)
				.map_or(0, |d| d.subsec_nanos());
			nanos as f64 / u32::MAX as f64 * 10000.0
		});
		let far = (config.draw_dist * 32) as f32;
		log::info!("world seed: {seed} | {} chunk workers", config.workers);
		let pipeline = Pipeline::new(seed, config.draw_dist, config.workers);
		let sun = config.tod.map_or_else(Sun::new, Sun::with_tod);
		let overlay_visible = config.overlay;
		let spawn = digblock_core::player::spawn(seed);
		let mut camera = FlyCamera::new(far, spawn);
		if let Some(yaw) = config.yaw {
			camera.yaw_deg = yaw;
		}
		if let Some(pitch) = config.pitch {
			camera.pitch_deg = pitch.clamp(-89.0, 89.0);
		}
		// The name field starts empty unless an explicit, valid override
		// (`?name=`, `--name`) was supplied — entering one is mandatory.
		let menu = crate::menu::Menu::new(
			config.menu,
			config
				.player_name
				.clone()
				.filter(|n| digblock_net::protocol::validate_name(n).is_ok())
				.unwrap_or_default(),
			config.auto_join.clone(),
		);
		Self {
			menu,
			chat: crate::chat::Chat::default(),
			audio: None,
			water_audio: crate::water_audio::WaterAudio::default(),
			footsteps: crate::audio::Footsteps::default(),
			ambient: crate::audio::Ambient::default(),
			audio_scratch: Vec::new(),
			prev_player_pos: spawn,
			nametag_scratch: Vec::new(),
			services: None,
			spawn_pos: spawn,
			spawn_cooldown: 0.0,
			config,
			seed,
			renderer: None,
			pipeline,
			camera,
			player: Player::new(spawn),
			touch: TouchDriver::new(),
			walking: true,
			fly_key_down: false,
			overlay_key_down: false,
			tick_accum: 0.0,
			sun,
			wide_due: false,
			keys: HeldKeys::default(),
			look_accum: (0.0, 0.0),
			dig_held: false,
			click_place: false,
			dig: None,
			dig_accum: 0.0,
			target: None,
			inventory: Inventory::default(),
			hotbar: Hotbar::new(),
			fluid: digblock_core::fluid::FluidSim::new(),
			flow_accum: 0.0,
			time_secs: 0.0,
			net: None,
			net_events: Vec::new(),
			pending_ack: None,
			correction_offset: Vec3::ZERO,
			remote_scratch: Vec::new(),
			remote_instances: Vec::new(),
			net_corrections: 0,
			net_error_accum: 0.0,
			net_log_timer: 0.0,
			feed: Vec::new(),
			player_names: rustc_hash::FxHashMap::default(),
			my_player_id: None,
			metrics,
			pending: FrameTimings::default(),
			overlay_visible,
			history_scratch: Vec::new(),
			bench_t: 0.0,
			pending_uploads: std::collections::VecDeque::new(),
			size: (1, 1),
		}
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
		self.net = Some(digblock_net::NetClient::new(
			transport,
			digblock_net::client::NetClientConfig {
				name,
				draw_dist: self.config.draw_dist,
				token,
				identity: self.config.identity,
			},
		));
		// The server has no fly physics; walk is the only multiplayer mode.
		self.walking = true;
	}

	/// Wire the launcher's master-server services (list fetch, pings, token
	/// mint + connect). Enables the Multiplayer entry in the menu.
	pub fn set_menu_services(&mut self, services: crate::menu::MenuServices) {
		self.services = Some(services);
		self.menu.set_services_available();
	}

	/// Inject the platform audio backend (launcher-owned, like
	/// `MenuServices`). Never called by bench/screenshot/drill runs.
	pub fn set_audio(&mut self, sink: Box<dyn crate::audio::AudioSink>) {
		self.audio = Some(sink);
		self.ambient.reset();
	}

	/// Per-frame audio upkeep, after the camera settles: listener pose,
	/// footsteps, waterfall voices, ambient bed. One early-out branch
	/// when no sink is attached.
	fn step_audio(&mut self, dt: f32) {
		use crate::audio::{ambient_sfx, step_sfx, AmbientZone, AudioCmd, LOOP_AMBIENT};
		if self.audio.is_none() {
			return;
		}
		let cam = self.camera.pos;
		self.emit(AudioCmd::Listener {
			pos: cam.to_array(),
			forward: self.camera.dir().to_array(),
		});

		// Footsteps: walk mode only, driven by actual horizontal motion.
		let submerged = self.eye_in_water();
		if self.walking {
			let d = self.player.pos - self.prev_player_pos;
			let moved = Vec3::new(d.x, 0.0, d.z).length();
			let feet = self.player.pos.y - digblock_core::player::EYE_TO_FEET;
			let (bx, bz) = (
				self.player.pos.x.floor() as i32,
				self.player.pos.z.floor() as i32,
			);
			let below = self.pipeline.block_at(bx, (feet - 0.05).floor() as i32, bz);
			let grounded = below.is_some_and(|b| b.props().collides);
			let feet_wet = self
				.pipeline
				.block_at(bx, feet.floor() as i32, bz)
				.is_some_and(|b| b.is_water());
			if let Some((m, gain)) =
				self.footsteps
					.advance(moved, grounded, below, submerged || feet_wet)
			{
				self.emit(AudioCmd::Sfx {
					sound: step_sfx(m),
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
		}
		self.prev_player_pos = self.player.pos;

		// Waterfall voices.
		let mut cmds = std::mem::take(&mut self.audio_scratch);
		self.water_audio.update(dt, cam, &mut cmds);
		for cmd in cmds.drain(..) {
			self.emit(cmd);
		}
		self.audio_scratch = cmds;

		// Ambient bed: underwater beats cave beats surface. Cave = well
		// below the pure worldgen surface (dug pits near grade don't
		// count — that's fine, they're open to the sky anyway).
		let zone = if submerged {
			AmbientZone::Underwater
		} else {
			let h = digblock_core::worldgen::terrain_height(
				cam.x.floor() as i32,
				cam.z.floor() as i32,
				self.seed,
			);
			if cam.y < h as f32 - 6.0 {
				AmbientZone::Cave
			} else {
				AmbientZone::Surface
			}
		};
		if let Some(zone) = self.ambient.advance(zone, dt) {
			self.emit(AudioCmd::LoopStart {
				id: LOOP_AMBIENT,
				sound: ambient_sfx(zone),
				pos: None,
				gain: crate::audio::AMBIENT_GAIN,
			});
		}
	}

	/// Send one audio command if a sink is attached (free otherwise).
	fn emit(&mut self, cmd: crate::audio::AudioCmd) {
		if let Some(a) = self.audio.as_mut() {
			a.send(cmd);
		}
	}

	/// Welcome handshake: replace the locally-seeded world with the
	/// server's. Cheap at startup — nothing meaningful has streamed yet.
	fn swap_world(&mut self, seed: f64, spawn: Vec3) {
		log::info!("joining server world: seed {seed}");
		self.player_names.clear();
		self.push_feed("Connected to server.".into());
		self.seed = seed;
		self.spawn_pos = spawn;
		self.pipeline = Pipeline::new(seed, self.config.draw_dist, self.config.workers);
		self.player = Player::new(spawn);
		self.camera.pos = spawn;
		// The server's InventoryFull (right behind JournalDone) is the
		// authoritative base; local single-player stock must not leak in.
		self.inventory = Inventory::default();
		self.dig = None;
		self.target = None;
		self.fluid = digblock_core::fluid::FluidSim::new();
		self.flow_accum = 0.0;
		self.tick_accum = 0.0;
		self.pending_ack = None;
		self.correction_offset = Vec3::ZERO;
		self.pending_uploads.clear();
		if let Some(r) = self.renderer.as_mut() {
			r.clear_chunks();
		}
		// Old-world sounds don't survive the swap.
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_DIG,
		});
		let mut cmds = std::mem::take(&mut self.audio_scratch);
		self.water_audio.clear(&mut cmds);
		for cmd in cmds.drain(..) {
			self.emit(cmd);
		}
		self.audio_scratch = cmds;
		self.ambient.reset();
		self.prev_player_pos = spawn;
	}

	/// Land a spawn teleport: hard-set the player and camera — a 200 ms
	/// smoothed slide across half the map would fight streaming — and
	/// drop everything that describes the pre-teleport position (the
	/// decaying correction offset, a stashed ack, any dig in progress,
	/// the footstep odometer).
	fn apply_teleport(&mut self, pos: Vec3) {
		self.player.set_state(digblock_core::player::PlayerState {
			pos,
			vel: Vec3::ZERO,
		});
		self.camera.pos = pos;
		self.correction_offset = Vec3::ZERO;
		self.pending_ack = None;
		self.prev_player_pos = pos;
		self.dig = None;
		self.push_feed("Returned to spawn.".into());
	}

	/// Apply an authoritative block value; the Pipeline holds edits whose
	/// chunk hasn't generated yet and applies them pre-mesh when it lands.
	fn apply_remote_edit(&mut self, cell: (i32, i32, i32), block: BlockType) {
		self.pipeline
			.apply_authoritative(cell.0, cell.1, cell.2, block);
	}

	/// Newest-last message feed; oldest drops when full so a join wave
	/// can't grow the overlay unboundedly.
	fn push_feed(&mut self, text: String) {
		self.push_feed_colored(text, 0xFFFF_FFFF);
	}

	/// Feed line in a specific 0xAABBGGRR color (chat is tinted per
	/// player, matching the sender's body color).
	fn push_feed_colored(&mut self, text: String, color: u32) {
		const FEED_MAX: usize = 8;
		const FEED_TTL_SECS: f32 = 8.0;
		if self.feed.len() >= FEED_MAX {
			self.feed.remove(0);
		}
		self.feed.push((text, FEED_TTL_SECS, color));
	}

	/// Drain the network: remote edits into the pipeline, acks stashed
	/// for the walk branch, disconnects fall back to single-player.
	fn pump_net(&mut self, dt: f32) {
		let Some(mut net) = self.net.take() else {
			return;
		};
		net.remotes.advance(dt);
		let mut events = std::mem::take(&mut self.net_events);
		events.clear();
		net.poll(&mut events, dt);
		let mut drop_net = false;
		for ev in events.drain(..) {
			use digblock_net::NetEvent;
			match ev {
				NetEvent::Welcome {
					seed,
					spawn,
					player_id,
					tod,
					..
				} => {
					self.my_player_id = Some(player_id);
					self.menu.on_welcome();
					self.swap_world(seed, spawn);
					// The server owns the world clock. A dev-build --tod
					// freeze still wins: update() re-asserts config.tod
					// every frame.
					self.sun = Sun::with_tod(tod);
				}
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => self.pending_ack = Some((acked_input_tick, me)),
				NetEvent::Edit {
					cell,
					block,
					superseded,
					remote_author,
				} => {
					// Another PLAYER's dig/place gets a quiet cue; fluid
					// results (SERVER_ID) and join sync stay silent.
					if let Some(author) = remote_author {
						if author != digblock_net::protocol::SERVER_ID {
							let prev = self.pipeline.block_at(cell.0, cell.1, cell.2);
							let sound = if block == BlockType::Air {
								prev.filter(|p| !p.is_water() && *p != BlockType::Air)
									.map(|p| crate::audio::break_sfx(crate::audio::material_of(p)))
							} else if !block.is_water() {
								Some(crate::audio::Sfx::Place)
							} else {
								None
							};
							if let Some(sound) = sound {
								self.emit(crate::audio::AudioCmd::Sfx {
									sound,
									pos: Some(cell_center(cell)),
									gain: 0.5,
								});
							}
						}
					}
					self.apply_remote_edit(cell, block);
					// Our predictions on this cell lost to authority: the
					// cell is owned by the broadcast value, but the server
					// never applied our inventory deltas — take them back.
					for e in superseded {
						self.revert_inv_delta(e.inv_delta);
					}
				}
				NetEvent::EditRejected(e) => {
					// Revert the predicted edit: the cell AND the
					// inventory delta (dig credit / place debit).
					self.pipeline
						.set_block(e.cell.0, e.cell.1, e.cell.2, e.prev);
					self.revert_inv_delta(e.inv_delta);
				}
				NetEvent::Inventory { counts } => {
					// Authoritative base + our still-pending predictions.
					let mut inv = Inventory::default();
					for (b, n) in counts {
						inv.add(b, n);
					}
					for e in net.edits.iter() {
						match e.inv_delta {
							Some((b, n)) if n > 0 => inv.add(b, n as u32),
							Some((b, n)) if n < 0 => {
								let _ = inv.try_take(b, (-n) as u32);
							}
							_ => {}
						}
					}
					for (b, _) in inv.iter_nonzero() {
						self.hotbar.auto_assign(b);
					}
					self.inventory = inv;
				}
				NetEvent::PlayerJoined { id, name } => {
					self.push_feed(format!("{name} joined the server."));
					self.player_names.insert(id, name);
				}
				NetEvent::PlayerLeft { id } => {
					let name = self
						.player_names
						.remove(&id)
						.unwrap_or_else(|| format!("player {id}"));
					self.push_feed(format!("{name} left the server."));
				}
				NetEvent::Teleported { pos } => self.apply_teleport(pos),
				// Hard-set: inter-beat drift is microscopic, and after a
				// paused clock (backgrounded tab) a snap is what we want.
				NetEvent::TimeOfDay { tod } => self.sun.tod = tod,
				NetEvent::Chat { from, text } => {
					// Our own echo: the roster never carries ourselves, so
					// resolve through the session name instead.
					let name = if Some(from) == self.my_player_id {
						self.config
							.player_name
							.clone()
							.unwrap_or_else(|| "me".into())
					} else {
						self.player_names
							.get(&from)
							.cloned()
							.unwrap_or_else(|| format!("player {from}"))
					};
					let c = player_color(from);
					self.push_feed_colored(format!("{name}: {text}"), pack_rgb(c));
				}
				NetEvent::Refused { reason } => {
					self.push_feed(format!("Server refused: {reason}."));
					log::warn!("server refused: {reason}; continuing single-player");
					self.menu.on_net_failed(&reason);
					drop_net = true;
				}
				NetEvent::Disconnected { reason } => {
					self.push_feed(format!("Disconnected: {reason}."));
					log::warn!("disconnected: {reason}; continuing single-player");
					self.menu.on_net_failed(&reason);
					drop_net = true;
				}
			}
		}
		self.net_events = events;

		if !drop_net {
			self.net = Some(net);
		} else {
			self.pending_ack = None;
			self.correction_offset = Vec3::ZERO;
			self.my_player_id = None;
			// No one left to talk to: discard any half-typed chat line.
			if self.chat.is_open() {
				self.chat.handle_input(&InputEvent::Key {
					key: Key::Escape,
					pressed: true,
				});
			}
		}
	}

	/// The ONLY way Engine builds a Renderer — anything uploaded to the
	/// renderer outside `upload_chunk` must be re-sent here. Shared by
	/// first init and device-loss rebuild so the two paths can't drift:
	/// meshes stream back in via the pipeline re-mesh, and spray/player
	/// state rides the same uploads (spray via mesh impacts, players
	/// re-uploaded every frame).
	fn build_renderer(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat) {
		self.renderer = Some(Renderer::new(
			gpu,
			surface_format,
			&self.config.render_config(),
		));
		self.pipeline.remesh_all();
	}

	/// Camera far plane = light rig depth = draw_dist × 32 (Java World.updateDepth).
	fn depth(&self) -> f32 {
		(self.config.draw_dist * 32) as f32
	}

	fn camera_chunk(&self) -> ChunkPos {
		ChunkPos::new(
			(self.camera.pos.x / 16.0).floor() as i32,
			0,
			(self.camera.pos.z / 16.0).floor() as i32,
		)
	}

	fn pump_pipeline(&mut self, gpu: &Gpu) {
		let center = self.camera_chunk();
		let Some(renderer) = self.renderer.as_mut() else {
			return;
		};
		let t_maintain = web_time::Instant::now();
		let budget_ms = self.config.stream_budget_ms;
		let full_deadline = t_maintain + Duration::from_secs_f32(budget_ms / 1000.0);
		let drain_deadline = t_maintain + Duration::from_secs_f32(budget_ms / 2000.0);
		let m = self
			.pipeline
			.maintain(center, usize::MAX, Some(drain_deadline));
		self.pending.maintain_ms = ms_since(t_maintain);
		let t_upload = web_time::Instant::now();
		self.metrics.evicted += m.evicted.len() as u32;
		if !m.evicted.is_empty() {
			self.pending_uploads
				.retain(|out| !m.evicted.contains(&out.pos));
			for pos in m.evicted {
				renderer.remove_chunk(pos);
				self.water_audio.remove_chunk(pos);
			}
		}
		self.pending_uploads.extend(m.meshed);
		// Nearest-first: on fast travel the queue backs up, and the chunks
		// that matter are the ones at and around the player. UPLOAD_MIN ≥
		// stack height, so the column the player is in/above always lands
		// the frame it drains — only the far ring is allowed to lag.
		if self.pending_uploads.len() > 1 {
			self.pending_uploads.make_contiguous().sort_by_key(|out| {
				let dx = out.pos.x - center.x;
				let dz = out.pos.z - center.z;
				dx * dx + dz * dz
			});
		}
		let mut uploads = 0u32;
		while let Some(out) = self.pending_uploads.pop_front() {
			renderer.upload_chunk(gpu, out.pos, &out.mesh);
			// Waterfall audio follows the same impacts spray renders.
			self.water_audio
				.set_chunk(out.pos, out.pos.origin(), &out.mesh.impacts);
			uploads += 1;
			if uploads as usize >= UPLOAD_MIN && web_time::Instant::now() >= full_deadline {
				break;
			}
		}
		self.pending.upload_ms = ms_since(t_upload);
		self.metrics.chunks_generated += m.generated;
		self.metrics.meshes_uploaded += uploads;
		self.metrics.gen_jobs.merge(m.gen_jobs);
		self.metrics.mesh_jobs.merge(m.mesh_jobs);
		self.metrics.stale_dropped += m.stale_dropped;
		self.metrics.requeued += m.requeued;
		self.metrics.upload_saturated += m.saturated as u32;
		self.metrics.gauges = PipelineGauges {
			gen_queue: m.gen_queue,
			mesh_queue: m.mesh_queue,
			in_flight: m.in_flight,
			resident: self.pipeline.resident() as u32,
		};
		// First full quiesce = initial world fill complete (the bench's
		// primary streaming-throughput metric).
		if m.gen_queue == 0
			&& m.mesh_queue == 0
			&& m.in_flight == 0
			&& self.pending_uploads.is_empty()
		{
			self.metrics.mark_streamed();
		}
	}

	/// True while the water tool is the selected hotbar item — it changes
	/// both the picking ray (fluid-aware) and what a dig/place means.
	fn water_tool_selected(&self) -> bool {
		self.hotbar.selected_item() == Some(HotbarItem::WaterTool)
	}

	/// Recompute the crosshair target (every update — it drives the
	/// highlight overlay, the dig state machine, and placement).
	fn update_target(&mut self) {
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let pipeline = &self.pipeline;
		let block_at = |x: i32, y: i32, z: i32| pipeline.block_at(x, y, z);
		self.target = if self.water_tool_selected() {
			raycast_fluid(origin, dir, REACH, &block_at)
		} else {
			raycast(origin, dir, REACH, &block_at)
		};
	}

	/// Is `hit` something the current tool can dig?
	fn diggable(&self, hit: &RayHit) -> bool {
		if self.water_tool_selected() {
			hit.block.is_fluid_source()
		} else {
			hit.block.props().collides && hit.block.dig_hardness(hit.cell.1).is_some()
		}
	}

	/// One 60 Hz step of the hold-to-dig state machine. Purely
	/// predictive online — the server runs its own timer and aim checks.
	fn step_dig(&mut self) {
		if !self.dig_held {
			self.cancel_dig();
			return;
		}
		let valid = self.target.filter(|h| self.diggable(h));
		let Some(hit) = valid else {
			self.cancel_dig();
			return;
		};
		let on_same = self
			.dig
			.as_ref()
			.is_some_and(|d| d.cell == hit.cell && d.block == hit.block);
		if !on_same {
			self.cancel_dig();
			let required = hit.block.dig_hardness(hit.cell.1).unwrap_or(u16::MAX);
			self.dig = Some(DigProgress {
				cell: hit.cell,
				block: hit.block,
				ticks: 0,
				required,
			});
			if let Some(net) = self.net.as_mut() {
				if net.ready() {
					net.send_dig_start(hit.cell);
				}
			}
			self.emit(crate::audio::AudioCmd::LoopStart {
				id: crate::audio::LOOP_DIG,
				sound: crate::audio::dig_sfx(crate::audio::material_of(hit.block)),
				pos: Some(cell_center(hit.cell)),
				gain: 1.0,
			});
			return;
		}
		let (done, pitch) = {
			let d = self.dig.as_mut().expect("on_same checked");
			d.ticks += 1;
			// Pitch ramp at 10 Hz: the loop climbs toward completion.
			let pitch = (d.ticks % 6 == 0 && d.required > 0)
				.then(|| 1.0 + 0.5 * d.ticks as f32 / d.required as f32);
			(d.ticks >= d.required, pitch)
		};
		if let Some(rate) = pitch {
			self.emit(crate::audio::AudioCmd::LoopPitch {
				id: crate::audio::LOOP_DIG,
				rate,
			});
		}
		if done {
			self.dig = None;
			self.complete_dig(hit);
		}
	}

	/// Abandon any in-progress hold (release, retarget, invalid target).
	fn cancel_dig(&mut self) {
		if self.dig.take().is_some() {
			if let Some(net) = self.net.as_mut() {
				if net.ready() {
					net.send_dig_cancel();
				}
			}
			self.emit(crate::audio::AudioCmd::LoopStop {
				id: crate::audio::LOOP_DIG,
			});
		}
	}

	/// The hold finished: apply the predicted dig/collect and credit the
	/// drop. The server confirms (no-op) or rejects (cell + credit revert).
	fn complete_dig(&mut self, hit: RayHit) {
		let Some(prev) =
			self.pipeline
				.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Air)
		else {
			return;
		};
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_DIG,
		});
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: crate::audio::break_sfx(crate::audio::material_of(prev)),
			pos: Some(cell_center(hit.cell)),
			gain: 1.0,
		});
		let drop = prev.dig_drop();
		if let Some(d) = drop {
			self.inventory.add(d, 1);
			self.hotbar.auto_assign(d);
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		match self.net.as_mut() {
			Some(net) if net.ready() => {
				let kind = if self.hotbar.selected_item() == Some(HotbarItem::WaterTool) {
					digblock_net::messages::EditKind::Collect
				} else {
					digblock_net::messages::EditKind::Dig
				};
				let tick = net.predictor.current_tick();
				let seq =
					net.edits
						.begin(hit.cell, prev, BlockType::Air, tick, drop.map(|d| (d, 1)));
				net.send_edit(seq, tick, origin, dir, kind, hit.cell, (0, 0, 0));
			}
			_ => {
				self.fluid
					.schedule_around(hit.cell.0, hit.cell.1, hit.cell.2);
			}
		}
	}

	/// Resolve a pending right-click against the current target. Placement
	/// is gated on stock — "you can only place what you gathered".
	fn apply_place(&mut self) {
		if !std::mem::take(&mut self.click_place) {
			return;
		}
		let Some(hit) = self.target else { return };
		if hit.normal == (0, 0, 0) {
			return;
		}
		let block = match self.hotbar.selected_item() {
			Some(HotbarItem::Block(b)) => b,
			Some(HotbarItem::WaterTool) => BlockType::Water,
			None => return,
		};
		let target = (
			hit.cell.0 + hit.normal.0,
			hit.cell.1 + hit.normal.1,
			hit.cell.2 + hit.normal.2,
		);
		// Never place inside the player — the shared occupied-cells
		// contract (identical to the server's InsidePlayer scan).
		let mut occupied = digblock_core::player::occupied_cells(self.camera.pos);
		if occupied.any(|c| c == target) {
			return;
		}
		// Stock gate BEFORE the world mutation: no stock, no prediction.
		if !self.inventory.try_take(block, 1) {
			return;
		}
		let Some(prev) = self.pipeline.set_block(target.0, target.1, target.2, block) else {
			self.inventory.add(block, 1); // refund: nothing happened
			return;
		};
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: crate::audio::Sfx::Place,
			pos: Some(cell_center(target)),
			gain: 0.9,
		});
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		match self.net.as_mut() {
			Some(net) if net.ready() => {
				let tick = net.predictor.current_tick();
				let seq = net
					.edits
					.begin(target, prev, block, tick, Some((block, -1)));
				net.send_edit(
					seq,
					tick,
					origin,
					dir,
					digblock_net::messages::EditKind::Place {
						block: block as u16,
					},
					hit.cell,
					hit.normal,
				);
			}
			_ => {
				self.fluid.schedule_around(target.0, target.1, target.2);
			}
		}
	}

	/// Undo one pending edit's predicted inventory delta (rejection or
	/// authoritative supersede).
	fn revert_inv_delta(&mut self, delta: Option<(BlockType, i32)>) {
		match delta {
			Some((b, n)) if n > 0 => {
				// A credited dig didn't happen: take the credit back.
				let _ = self.inventory.try_take(b, n as u32);
			}
			Some((b, n)) if n < 0 => {
				// A debited place didn't happen: refund.
				self.inventory.add(b, (-n) as u32);
			}
			_ => {}
		}
	}

	/// Crosshair cell + dig progress for the highlight overlay: `None`
	/// progress = idle outline.
	pub fn dig_progress(&self) -> Option<((i32, i32, i32), Option<f32>)> {
		let hit = self.target.as_ref()?;
		if !self.diggable(hit) {
			return None;
		}
		let progress = self
			.dig
			.as_ref()
			.filter(|d| d.cell == hit.cell)
			.map(|d| (d.ticks as f32 / d.required.max(1) as f32).min(1.0));
		Some((hit.cell, progress))
	}

	pub fn inventory(&self) -> &Inventory {
		&self.inventory
	}

	pub fn hotbar(&self) -> &Hotbar {
		&self.hotbar
	}

	/// Camera eye below the VISIBLE water surface? (None = unloaded =
	/// dry.) The cell test alone flips at the lattice boundary, but the
	/// rendered surface of a resting source sits ~0.111 below the cell
	/// top (water.wgsl `DROPS`) — without the offset the underwater
	/// atmosphere pops in while the camera is still visibly above water.
	/// The flip happens FLIP_EARLY above the visible surface: the near
	/// plane clips the surface pane before the eye reaches it, and
	/// without the margin there are a few frames of neither-above-nor-
	/// below shading while crossing.
	fn eye_in_water(&self) -> bool {
		const SURFACE_DROP: f32 = digblock_core::player::WATER_SURFACE_DROP;
		const FLIP_EARLY: f32 = 0.1;
		let c = self.camera.pos;
		let (x, y, z) = (c.x.floor() as i32, c.y.floor() as i32, c.z.floor() as i32);
		if !self
			.pipeline
			.block_at(x, y, z)
			.is_some_and(|b| b.is_water())
		{
			return false;
		}
		// Water above too → fully submerged; otherwise this is the
		// surface cell and the drop (minus the crossing margin) applies.
		self.pipeline
			.block_at(x, y + 1, z)
			.is_some_and(|b| b.is_water())
			|| c.y - y as f32 <= 1.0 - SURFACE_DROP + FLIP_EARLY
	}

	/// Visible height of the water surface directly above the camera
	/// (top of the contiguous water column, minus the rendered drop) —
	/// the shaders' path-length clamp. Only meaningful while submerged.
	fn water_surface_above(&self) -> f32 {
		let c = self.camera.pos;
		let (x, z) = (c.x.floor() as i32, c.z.floor() as i32);
		let mut y = c.y.floor() as i32;
		let top = y + 256;
		while y < top
			&& self
				.pipeline
				.block_at(x, y + 1, z)
				.is_some_and(|b| b.is_water())
		{
			y += 1;
		}
		y as f32 + 1.0 - digblock_core::player::WATER_SURFACE_DROP
	}

	fn frame_uniforms(&self, near_vp: glam::Mat4) -> FrameUniforms {
		let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
		let vp = self.camera.view_proj(aspect);
		let caster = self.sun.caster();
		let atmos = digblock_core::sky::Atmosphere::sample(self.sun.elevation());
		// Directions toward the bodies in the sky (opposite the light).
		let sun = -self.sun.sun_dir();
		let moon = -self.sun.moon_dir();
		let far = self.depth();
		let (mut fog_start, mut fog_end) = (
			self.config.fog_start_frac * far,
			self.config.fog_end_frac * far,
		);
		let cam = self.camera.pos;
		// Underwater: substitute the atmosphere the shaders read (the
		// clear color and sky pass follow automatically) — no shader
		// branches needed. The palette is DERIVED from the atmosphere
		// sample, not hardcoded, so day/night falls out naturally.
		// Tunables (shader partners: the WATER_* block in
		// frame_common.wgsl):
		// Mean albedo of the water tile texture — the biggest hue lever
		// for matching submerged fog to the from-above deep-water look.
		const UW_ALBEDO: [f32; 3] = [0.12, 0.27, 0.35];
		// Day-arc-average direct-light factor folded into the deep color.
		const UW_DIRECT: f32 = 0.6;
		// Light funnels down through the surface: the submerged sky
		// gradient brightens overhead (complements the Snell window).
		const UW_ZENITH_BOOST: f32 = 1.8;
		// Per-channel ambient survival underwater (red dies first, like
		// WATER_EXTINCTION) — submerged terrain isn't lit by open air.
		const UW_AMBIENT: [f32; 3] = [0.45, 0.75, 0.92];
		let submerged = self.eye_in_water();
		let (mut horizon, mut zenith) = (atmos.horizon, atmos.zenith);
		let mut ambient = atmos.ambient;
		if submerged {
			// Lit deep-water color: what water.wgsl's `scatter` converges
			// to from above (albedo x (ambient + direct)). The submerged
			// fog and the below-horizon sky both converge HERE, which is
			// what makes above/below views of the same water agree.
			let uw =
				|i: usize| UW_ALBEDO[i] * (atmos.ambient[i] + atmos.light_color[i] * UW_DIRECT);
			horizon = [uw(0), uw(1), uw(2)];
			zenith = [
				horizon[0] * UW_ZENITH_BOOST,
				horizon[1] * UW_ZENITH_BOOST,
				horizon[2] * UW_ZENITH_BOOST,
			];
			// The band parameters are ignored by the submerged exp fog
			// (apply_fog keys off water_misc.x) but stay sane for
			// anything else reading them.
			fog_start = 2.0;
			fog_end = 28.0;
			ambient = [
				ambient[0] * UW_AMBIENT[0],
				ambient[1] * UW_AMBIENT[1],
				ambient[2] * UW_AMBIENT[2],
			];
		}
		FrameUniforms {
			view_proj: vp.to_cols_array_2d(),
			inv_view_proj: vp.inverse().to_cols_array_2d(),
			light_near: near_vp.to_cols_array_2d(),
			// Placeholder: the renderer owns this slot — it overwrites it
			// every frame with the ping-pong's active_vp (the map actually
			// rendered, which may lag this frame's wide_vp by one job).
			light_wide: glam::Mat4::IDENTITY.to_cols_array_2d(),
			light_dir: [caster.dir.x, caster.dir.y, caster.dir.z, caster.strength],
			light_color: [
				atmos.light_color[0],
				atmos.light_color[1],
				atmos.light_color[2],
				// Animation clock (water scroll/foam) in the spare lane.
				self.time_secs,
			],
			ambient: [ambient[0], ambient[1], ambient[2], 0.0],
			fog_color: [horizon[0], horizon[1], horizon[2], fog_start],
			cam_pos: [cam.x, cam.y, cam.z, 1.0 / (fog_end - fog_start)],
			sun_dir: [sun.x, sun.y, sun.z, atmos.star_intensity],
			moon_dir: [moon.x, moon.y, moon.z, atmos.glow_strength],
			zenith: [zenith[0], zenith[1], zenith[2], 0.0],
			horizon: [horizon[0], horizon[1], horizon[2], 0.0],
			misc: [
				1.0,
				1.0,
				1.0 / self.config.shadow_size as f32,
				// Wide-cascade texel size in world units (extent / resolution),
				// the chunk shader's normal-offset distance.
				far / self.config.shadow_size as f32,
			],
			water_misc: [
				f32::from(submerged),
				f32::from(self.config.ssr),
				// Water-surface height above the camera: the shaders'
				// underwater path-length clamp (unused while dry).
				if submerged {
					self.water_surface_above()
				} else {
					0.0
				},
				0.0,
			],
		}
	}
}

impl Drop for Engine {
	fn drop(&mut self) {
		if self.config.bench {
			if let Some(line) = self.metrics.bench_summary() {
				log::info!("{line}");
			}
		}
	}
}

impl Game for Engine {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, size: (u32, u32)) {
		self.size = size;
		self.build_renderer(gpu, surface_format);
		log::info!(
			"engine init: draw_dist={} shadow={}px format={surface_format:?}",
			self.config.draw_dist,
			self.config.shadow_size
		);
	}

	fn resize(&mut self, _gpu: &Gpu, size: (u32, u32)) {
		self.size = size;
	}

	fn input(&mut self, event: InputEvent) {
		// Benchmark runs must be input-independent to stay comparable.
		if self.config.bench {
			return;
		}
		// Menu open: every input belongs to the menu — gameplay keys must
		// not leak (typing a name would also walk). FocusLost still clears
		// held state.
		if self.menu.is_open() {
			if matches!(event, InputEvent::FocusLost | InputEvent::PointerLockLost) {
				self.keys = HeldKeys::default();
				self.touch.clear();
			} else {
				self.menu.handle_input(&event, self.size);
			}
			return;
		}
		// Chat open: same containment as the menu, but the pointer stays
		// grabbed (the shell delivers Text via wants_text_input).
		if self.chat.is_open() {
			if matches!(event, InputEvent::FocusLost | InputEvent::PointerLockLost) {
				self.keys = HeldKeys::default();
				self.touch.clear();
				// Losing the lock also surfaces the pause menu; a chat
				// draft doesn't survive that.
				self.chat.handle_input(&InputEvent::Key {
					key: Key::Escape,
					pressed: true,
				});
				if matches!(event, InputEvent::PointerLockLost) {
					self.menu.open();
				}
			} else if let Some(crate::chat::ChatAction::Send(text)) = self.chat.handle_input(&event)
			{
				if let Some(net) = self.net.as_mut() {
					net.send_chat(&text);
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
						}
						log::info!("mode: {}", if self.walking { "walk" } else { "fly" });
					}
					self.fly_key_down = pressed;
				}
				Key::Digit(n) => {
					if pressed {
						self.hotbar.select_digit(n);
					}
				}
				Key::Backquote => {
					if pressed && !self.overlay_key_down {
						self.overlay_visible = !self.overlay_visible;
					}
					self.overlay_key_down = pressed;
				}
				Key::Escape => {
					if pressed {
						self.dig_held = false;
						self.menu.open();
					}
				}
				// Chat is multiplayer-only — there is no one to hear a
				// single-player message.
				Key::T | Key::Enter => {
					if pressed && self.net.is_some() {
						self.keys = HeldKeys::default();
						self.dig_held = false;
						self.chat.open();
					}
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
					if pressed {
						self.click_place = true;
					}
				}
				MouseButton::Other => {}
			},
			InputEvent::FocusLost => {
				self.keys = HeldKeys::default();
				self.touch.clear();
				self.dig_held = false;
			}
			InputEvent::PointerLockLost => {
				self.keys = HeldKeys::default();
				self.touch.clear();
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
				let phase = match phase {
					TouchPhase::Started => Phase::Started,
					TouchPhase::Moved => Phase::Moved,
					TouchPhase::Ended => Phase::Ended,
					TouchPhase::Cancelled => Phase::Cancelled,
				};
				self.touch.handle(id, phase, x, y);
			}
		}
	}

	fn frame_acquired(&mut self, acquire_wait: Duration) {
		self.pending.acquire_ms = acquire_wait.as_secs_f32() * 1000.0;
	}

	fn update(&mut self, dt: Duration) {
		let t_update = web_time::Instant::now();
		let dt = dt.as_secs_f32();
		self.time_secs = (self.time_secs + dt) % 600.0;
		// A fixed --tod freezes the sun angle but keeps the wide-cascade
		// refresh cadence (the map must still track streamed-in chunks).
		let fixed_tod = self.config.tod;
		self.wide_due |= self.sun.update(dt, self.camera.pos);
		if let Some(tod) = fixed_tod {
			self.sun.tod = tod;
		}
		// Scripted benchmark flight: constant-speed line with a slow yaw
		// sweep, advanced one fixed 60Hz tick per frame so the path depends
		// only on the frame index — identical workload run-to-run.
		if self.config.bench {
			const BENCH_SPEED: f32 = 30.0; // blocks per path-second
			const BENCH_YAW: f32 = 12.0; // degrees per path-second
								// Altitude follows the terrain so worldgen retunes can't put the
								// flight underground; still a pure function of the frame index.
			self.bench_t += TICK;
			let x = BENCH_SPEED * self.bench_t;
			let h = digblock_core::worldgen::terrain_height(x as i32, 8, self.seed);
			self.camera.pos = Vec3::new(x, h as f32 + 12.0, 8.0);
			self.camera.look(BENCH_YAW * TICK, 0.0);
			self.pending.update_ms = ms_since(t_update);
			return;
		}

		const LOOK_SENS: f32 = 0.12; // degrees per raw mouse unit
		let (lx, ly) = std::mem::take(&mut self.look_accum);
		let touch_look = self.touch.take_look();
		self.camera
			.look(lx * LOOK_SENS + touch_look.x, ly * LOOK_SENS + touch_look.y);

		// Menu plumbing: service responses in, commands out, disconnect
		// intents applied. The world (and any live connection) keeps
		// running underneath the open menu.
		self.menu.set_connected(self.net.is_some());
		let responses: Vec<crate::menu::MenuResponse> = self
			.services
			.as_ref()
			.map(|s| s.resp_rx.try_iter().collect())
			.unwrap_or_default();
		for resp in responses {
			if let Some((transport, token)) = self.menu.on_response(resp) {
				self.set_transport_with_token(transport, token);
			}
		}
		if self.menu.take_disconnect() && self.net.take().is_some() {
			self.pending_ack = None;
			self.correction_offset = Vec3::ZERO;
			self.my_player_id = None;
			self.push_feed("Left the server.".into());
		}
		// Return to spawn: one shared 60s cooldown gates the menu row.
		// Connected, the server teleports us (Teleported lands in
		// pump_net); local play lands immediately, stepping up past any
		// blocks built at the spawn point.
		self.spawn_cooldown = (self.spawn_cooldown - dt).max(0.0);
		self.menu
			.set_spawn_cooldown(self.spawn_cooldown.ceil() as u32);
		if self.menu.take_return_spawn() && self.spawn_cooldown == 0.0 {
			self.spawn_cooldown = digblock_net::protocol::RETURN_SPAWN_COOLDOWN_SECS as f32;
			match self.net.as_mut() {
				Some(net) if net.ready() => net.send_return_to_spawn(),
				_ => {
					let pipeline = &self.pipeline;
					let pos = digblock_core::player::ascend_clear(self.spawn_pos, &|x, y, z| {
						pipeline.block_at(x, y, z)
					});
					self.apply_teleport(pos);
				}
			}
		}
		if let Some(services) = &self.services {
			for cmd in self.menu.take_commands() {
				// The menu-typed name becomes the session name, so the
				// Hello introduces us consistently (the master's verified
				// name still wins server-side).
				if let crate::menu::MenuCommand::Join { name, .. } = &cmd {
					self.config.player_name = Some(name.clone());
				}
				let _ = services.cmd_tx.send(cmd);
			}
		}

		self.pump_net(dt);
		for m in &mut self.feed {
			m.1 -= dt;
		}
		self.feed.retain(|m| m.1 > 0.0);
		if let Some(net) = self.net.as_ref() {
			// Reconciliation health line, ~5 s cadence (matches metrics).
			self.net_log_timer += dt;
			if self.net_log_timer >= 5.0 && net.ready() {
				let avg = if self.net_corrections > 0 {
					self.net_error_accum / self.net_corrections as f32
				} else {
					0.0
				};
				log::info!(
					"net: {} corrections (avg {:.3} blocks), {} pending edits, {} deferred",
					self.net_corrections,
					avg,
					net.edits.len(),
					self.pipeline.deferred_edits(),
				);
				self.net_log_timer = 0.0;
				self.net_corrections = 0;
				self.net_error_accum = 0.0;
			}
		}

		// Crosshair target → placement edge → hold-to-dig stepping (fixed
		// 60 Hz so offline and online dig timing share the server's tick
		// domain, in walk AND fly mode). All outside the physics
		// `block_at` borrow: raycast (immutable) → set_block (mutable) →
		// player tick (immutable), strictly sequential.
		self.update_target();
		self.apply_place();
		self.dig_accum = (self.dig_accum + dt).min(0.25);
		while self.dig_accum >= TICK {
			self.dig_accum -= TICK;
			self.step_dig();
		}

		// Water flow runs on its own world-tick cadence, independent of
		// the walk/fly camera mode; idle (empty queue) it costs nothing.
		// Connected, the server owns fluid — its results arrive as edits.
		if self.net.is_none() {
			use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
			self.flow_accum = (self.flow_accum + dt).min(1.0);
			while self.flow_accum >= FLOW_STEP_SECS {
				self.flow_accum -= FLOW_STEP_SECS;
				let mut world = PipelineWorld(&mut self.pipeline);
				self.fluid.step(&mut world, FLOW_BUDGET);
			}
		}

		if self.walking {
			// Fixed 60 Hz player simulation (Java feel was tuned per-frame
			// at 60fps; the accumulator preserves it at any display rate).
			let touch_jump = self.touch.take_jump();
			let key_axis = Vec2::new(
				(self.keys.d as i32 - self.keys.a as i32) as f32,
				(self.keys.w as i32 - self.keys.s as i32) as f32,
			);
			let intent = PlayerIntent {
				move_axis: key_axis + self.touch.move_axis(),
				jump: self.keys.up || touch_jump,
			};
			let dir = self.camera.dir();
			self.tick_accum = (self.tick_accum + dt).min(0.25);
			let pipeline = &self.pipeline;
			let block_at = |x: i32, y: i32, z: i32| pipeline.block_at(x, y, z);
			match self.net.as_mut() {
				Some(net) if net.ready() => {
					// Reconcile against the latest snapshot before predicting
					// this frame's ticks; corrections land as a decaying
					// visual offset, not a camera snap.
					if let Some((tick, state)) = self.pending_ack.take() {
						use digblock_net::predict::Reconciled;
						if let Reconciled::Replayed { error } =
							net.predictor
								.reconcile(&mut self.player, tick, state, &block_at)
						{
							self.correction_offset += error;
							self.net_corrections += 1;
							self.net_error_accum += error.length();
						}
					}
					while self.tick_accum >= TICK {
						self.tick_accum -= TICK;
						net.predictor
							.predict(&mut self.player, intent, dir, &block_at);
					}
					net.send_inputs();
				}
				_ => {
					while self.tick_accum >= TICK {
						self.tick_accum -= TICK;
						self.player.tick(TICK, dir, intent, &block_at);
					}
				}
			}
			// ~200 ms exponential decay; snapped off below a millimeter.
			self.correction_offset *= (-dt / 0.2).exp();
			if self.correction_offset.length_squared() < 1e-6 {
				self.correction_offset = Vec3::ZERO;
			}
			self.camera.pos = self.player.pos + self.correction_offset;
		} else {
			// Debug fly camera.
			let speed = 24.0 * dt;
			let dir = self.camera.dir();
			let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
			let right = flat.cross(Vec3::Y);
			let mut mv = Vec3::ZERO;
			if self.keys.w {
				mv += flat;
			}
			if self.keys.s {
				mv -= flat;
			}
			if self.keys.d {
				mv += right;
			}
			if self.keys.a {
				mv -= right;
			}
			if self.keys.up {
				mv += Vec3::Y;
			}
			if self.keys.down {
				mv -= Vec3::Y;
			}
			self.camera.pos += mv.normalize_or_zero() * speed;
		}
		self.step_audio(dt);
		self.pending.update_ms = ms_since(t_update);
	}

	fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32)) {
		self.size = size;
		// Streaming happens here (not update) because uploads need the GPU.
		self.pump_pipeline(gpu);
		// Remote players, sampled ~100 ms in the past between snapshots;
		// the instance list rides FrameScene and re-uploads every frame.
		self.remote_instances.clear();
		if let Some(net) = self.net.as_ref() {
			net.remotes.sample(&mut self.remote_scratch);
			for d in &self.remote_scratch {
				let c = player_color(d.id);
				self.remote_instances
					.push([d.pos.x, d.pos.y, d.pos.z, d.yaw, c[0], c[1], c[2], 0.0]);
			}
		}
		// DIG_PLAYERBOX=1: park a fake remote player 5 blocks ahead —
		// a GPU-path check that needs no second client.
		if self.config.debug_playerbox {
			let p = self.camera.pos + self.camera.dir() * 5.0;
			let c = player_color(2);
			self.remote_instances
				.push([p.x, p.y, p.z, 0.8, c[0], c[1], c[2], 0.0]);
		}
		let d = self.depth();
		let texels = self.config.shadow_size as f32;
		let near_vp = self.sun.near_vp(self.camera.pos, d, texels);
		let wide_vp = self.sun.wide_vp(d, texels);
		let frame = self.frame_uniforms(near_vp);
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		let view_proj = self.camera.view_proj(aspect);
		// Name tags: project each nearby remote's head-top to screen px.
		// No depth (overlay) — tags read through walls within range.
		const NAMETAG_RANGE: f32 = 32.0;
		self.nametag_scratch.clear();
		for d in &self.remote_scratch {
			if self.net.is_none() || d.pos.distance(self.camera.pos) > NAMETAG_RANGE {
				continue;
			}
			// Remote pos is the eye; the box top is +0.3, tag floats above.
			let clip = view_proj * (d.pos + Vec3::Y * 0.55).extend(1.0);
			if clip.w <= 0.0 {
				continue;
			}
			let ndc = clip.truncate() / clip.w;
			if ndc.x.abs() > 1.2 || ndc.y.abs() > 1.2 {
				continue;
			}
			let text = match self.player_names.get(&d.id) {
				Some(n) => n.clone(),
				None => format!("player {}", d.id),
			};
			self.nametag_scratch.push(crate::renderer::NameTag {
				x: (ndc.x * 0.5 + 0.5) * size.0 as f32,
				y: (1.0 - (ndc.y * 0.5 + 0.5)) * size.1 as f32,
				color: pack_rgb(player_color(d.id)),
				text,
			});
		}
		let wide_due = std::mem::take(&mut self.wide_due);
		// Overlay data is last frame's numbers (metrics.frame() runs at the
		// end of this fn) and GPU ms lags 2-3 frames — fine for a debug HUD.
		let overlay_stats = if self.overlay_visible {
			self.metrics
				.history_tail(crate::overlay::GRAPH_FRAMES, &mut self.history_scratch);
			Some(self.metrics.overlay_stats())
		} else {
			None
		};
		let menu_view = self
			.menu
			.is_open()
			.then(|| self.menu.view(size, (self.time_secs * 2.0) as u32 % 2 == 0));
		// HUD state gates with the crosshair: hidden under the menu and in
		// bench captures' default state comparability is preserved (bench
		// never digs, hotbar renders its fixed default row).
		let in_game = menu_view.is_none();
		let hotbar_view =
			in_game.then(|| crate::hotbar::HotbarView::build(&self.hotbar, &self.inventory));
		let highlight_target = if in_game { self.dig_progress() } else { None };
		if let Some(r) = self.renderer.as_mut() {
			let t_encode = web_time::Instant::now();
			let scene = FrameScene {
				uniforms: frame,
				view_proj,
				near_vp,
				wide_vp,
				wide_due,
				size,
				players: &self.remote_instances,
				overlay: overlay_stats
					.as_ref()
					.map(|s| (s, self.history_scratch.as_slice())),
				feed: &self.feed,
				menu: menu_view.as_ref(),
				target: highlight_target,
				hotbar: hotbar_view.as_ref(),
				chat: (in_game && self.chat.is_open())
					.then(|| self.chat.view((self.time_secs * 2.0) as u32 % 2 == 0)),
				nametags: &self.nametag_scratch,
			};
			let stats = r.render(gpu, target, &scene);
			self.pending.encode_ms = ms_since(t_encode);
			self.metrics.render = stats;
			self.metrics.gpu = r.gpu_pass_ms();
			self.metrics.gpu_buffer_bytes = r.gpu_buffer_bytes;
		}
		// total_ms is measured inside frame() from the frame-to-frame delta.
		self.metrics.frame(std::mem::take(&mut self.pending));
	}

	fn wants_cursor_grab(&self) -> bool {
		!self.menu.is_open()
	}

	fn wants_text_input(&self) -> bool {
		self.chat.is_open()
	}

	fn on_suspend(&mut self) {
		log::info!("engine: on_suspend (state save point)");
		self.metrics.pause();
		// One cheap synchronous send — honors the on_suspend contract.
		self.emit(crate::audio::AudioCmd::Pause);
	}

	fn on_resume(&mut self) {
		log::info!("engine: on_resume");
		self.emit(crate::audio::AudioCmd::Resume);
	}

	fn rebuild_gpu_resources(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat) {
		// Device loss: rebuild every device-lifetime object; meshes
		// stream back in from retained chunk data via the pipeline.
		self.build_renderer(gpu, surface_format);
		log::info!("engine: renderer rebuilt (device loss), re-mesh queued");
	}
}

/// The fluid sim's view of the chunk pipeline: set_block already handles
/// copy-on-write and remesh invalidation, and returns None for unloaded
/// cells, which the sim treats as a dropped write.
struct PipelineWorld<'a>(&'a mut Pipeline);

impl digblock_core::fluid::FluidWorld for PipelineWorld<'_> {
	fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		self.0.block_at(x, y, z)
	}

	fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool {
		self.0.set_block(x, y, z, b).is_some()
	}
}
