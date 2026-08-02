use std::time::Duration;

use digblock_core::chunk::ChunkPos;
use digblock_core::metrics::{FrameTimings, Metrics, PipelineGauges};
use digblock_core::pipeline::Pipeline;
use digblock_core::player::{Player, PlayerIntent, TICK};
use digblock_core::raycast::{raycast, REACH};
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
	/// Fixed sun angle in degrees for deterministic captures.
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
	/// Click edges pending for the next update (left = dig, right = place).
	click_dig: bool,
	click_place: bool,
	/// What right-click places: the last block dug (default dirt), or a
	/// water source armed via the B debug key.
	held_block: BlockType,
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
	/// Server-message feed (join/leave/disconnect), drawn by the overlay
	/// above the HUD; (text, seconds left) — fades over the last second.
	feed: Vec<(String, f32)>,
	/// id → name, learned from PlayerJoined (roster + live joins), so
	/// PlayerLeft (id-only on the wire) can be displayed by name.
	player_names: rustc_hash::FxHashMap<u8, String>,
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
}

fn ms_since(t: std::time::Instant) -> f32 {
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

impl Engine {
	pub fn new(config: EngineConfig) -> Self {
		let mut metrics = Metrics::new(config.metrics_csv.as_deref());
		metrics.workers = config.workers as u32;
		if config.bench {
			metrics.record_run_history();
		}
		// Java: seed = Math.random() * 10000, a domain offset for the noise.
		let seed = config
			.seed
			.unwrap_or_else(|| rand::random::<f64>() * 10000.0);
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
		Self {
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
			click_dig: false,
			click_place: false,
			held_block: BlockType::Dirt,
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
			metrics,
			pending: FrameTimings::default(),
			overlay_visible,
			history_scratch: Vec::new(),
			bench_t: 0.0,
			pending_uploads: std::collections::VecDeque::new(),
			size: (1, 1),
		}
	}

	/// Attach a multiplayer transport (desktop `--connect`). The world is
	/// swapped to the server's seed when the Welcome arrives; until then
	/// the local world keeps running.
	pub fn set_transport(&mut self, transport: Box<dyn digblock_net::transport::NetTransport>) {
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
			},
		));
		// The server has no fly physics; walk is the only multiplayer mode.
		self.walking = true;
	}

	/// Welcome handshake: replace the locally-seeded world with the
	/// server's. Cheap at startup — nothing meaningful has streamed yet.
	fn swap_world(&mut self, seed: f64, spawn: Vec3) {
		log::info!("joining server world: seed {seed}");
		self.player_names.clear();
		self.push_feed("Connected to server.".into());
		self.seed = seed;
		self.pipeline = Pipeline::new(seed, self.config.draw_dist, self.config.workers);
		self.player = Player::new(spawn);
		self.camera.pos = spawn;
		self.fluid = digblock_core::fluid::FluidSim::new();
		self.flow_accum = 0.0;
		self.tick_accum = 0.0;
		self.pending_ack = None;
		self.correction_offset = Vec3::ZERO;
		self.pending_uploads.clear();
		if let Some(r) = self.renderer.as_mut() {
			r.clear_chunks();
		}
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
		const FEED_MAX: usize = 6;
		const FEED_TTL_SECS: f32 = 8.0;
		if self.feed.len() >= FEED_MAX {
			self.feed.remove(0);
		}
		self.feed.push((text, FEED_TTL_SECS));
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
				NetEvent::Welcome { seed, spawn, .. } => self.swap_world(seed, spawn),
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => self.pending_ack = Some((acked_input_tick, me)),
				NetEvent::Edit { cell, block } => self.apply_remote_edit(cell, block),
				NetEvent::EditRejected(e) => {
					// Revert the predicted edit; held_block stays (cosmetic).
					self.pipeline
						.set_block(e.cell.0, e.cell.1, e.cell.2, e.prev);
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
				NetEvent::Refused { reason } => {
					self.push_feed(format!("Server refused: {reason}."));
					log::warn!("server refused: {reason}; continuing single-player");
					drop_net = true;
				}
				NetEvent::Disconnected { reason } => {
					self.push_feed(format!("Disconnected: {reason}."));
					log::warn!("disconnected: {reason}; continuing single-player");
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
		let t_maintain = std::time::Instant::now();
		let budget_ms = self.config.stream_budget_ms;
		let full_deadline = t_maintain + Duration::from_secs_f32(budget_ms / 1000.0);
		let drain_deadline = t_maintain + Duration::from_secs_f32(budget_ms / 2000.0);
		let m = self
			.pipeline
			.maintain(center, usize::MAX, Some(drain_deadline));
		self.pending.maintain_ms = ms_since(t_maintain);
		let t_upload = std::time::Instant::now();
		self.metrics.evicted += m.evicted.len() as u32;
		if !m.evicted.is_empty() {
			self.pending_uploads
				.retain(|out| !m.evicted.contains(&out.pos));
			for pos in m.evicted {
				renderer.remove_chunk(pos);
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
			uploads += 1;
			if uploads as usize >= UPLOAD_MIN && std::time::Instant::now() >= full_deadline {
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

	/// Resolve pending dig/place clicks against the world. Runs in update,
	/// outside the physics `block_at` borrow: raycast (immutable) →
	/// set_block (mutable) → player tick (immutable), strictly sequential.
	fn apply_clicks(&mut self) {
		let dig = std::mem::take(&mut self.click_dig);
		let place = std::mem::take(&mut self.click_place);
		if !dig && !place {
			return;
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let hit = {
			let pipeline = &self.pipeline;
			raycast(origin, dir, REACH, &|x, y, z| pipeline.block_at(x, y, z))
		};
		let Some(hit) = hit else { return };
		if dig {
			if let Some(prev) =
				self.pipeline
					.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Air)
			{
				self.held_block = prev;
				match self.net.as_mut() {
					Some(net) if net.ready() => {
						// Predicted: the server confirms (no-op re-apply) or
						// rejects (revert to prev via EditRejected).
						let tick = net.predictor.current_tick();
						let seq = net.edits.begin(hit.cell, prev, BlockType::Air, tick);
						net.send_edit(
							seq,
							tick,
							origin,
							dir,
							digblock_net::messages::EditKind::Dig,
							hit.cell,
							(0, 0, 0),
						);
					}
					_ => {
						self.fluid
							.schedule_around(hit.cell.0, hit.cell.1, hit.cell.2);
					}
				}
			}
		} else if hit.normal != (0, 0, 0) {
			let target = (
				hit.cell.0 + hit.normal.0,
				hit.cell.1 + hit.normal.1,
				hit.cell.2 + hit.normal.2,
			);
			// Never place inside the player — the shared occupied-cells
			// contract (identical to the server's InsidePlayer scan).
			let [eye, body] = digblock_core::player::occupied_cells(self.camera.pos);
			if target != eye && target != body {
				if let Some(prev) =
					self.pipeline
						.set_block(target.0, target.1, target.2, self.held_block)
				{
					match self.net.as_mut() {
						Some(net) if net.ready() => {
							let tick = net.predictor.current_tick();
							let seq = net.edits.begin(target, prev, self.held_block, tick);
							net.send_edit(
								seq,
								tick,
								origin,
								dir,
								digblock_net::messages::EditKind::Place {
									block: self.held_block as u16,
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
			}
		}
	}

	/// Camera eye inside a water block? (None = unloaded = dry.)
	fn eye_in_water(&self) -> bool {
		let c = self.camera.pos;
		self.pipeline
			.block_at(c.x.floor() as i32, c.y.floor() as i32, c.z.floor() as i32)
			.is_some_and(|b| b.is_water())
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
		// Underwater: substitute the atmosphere the shaders read — short
		// blue-green fog, water-colored sky gradient (the clear color and
		// sky pass follow automatically) — no shader branches needed.
		// Daylight still scales everything so night dives stay dark.
		let submerged = self.eye_in_water();
		let (mut horizon, mut zenith) = (atmos.horizon, atmos.zenith);
		if submerged {
			let day = (atmos.light_color[1]).clamp(0.08, 1.0);
			horizon = [0.05 * day, 0.22 * day, 0.30 * day];
			zenith = [0.02 * day, 0.10 * day, 0.16 * day];
			fog_start = 2.0;
			fog_end = 28.0;
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
			ambient: [atmos.ambient[0], atmos.ambient[1], atmos.ambient[2], 0.0],
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
			water_misc: [f32::from(submerged), f32::from(self.config.ssr), 0.0, 0.0],
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
				Key::B => {
					if pressed {
						self.held_block = BlockType::Water;
						log::info!("held block: water source (debug)");
					}
				}
				Key::Backquote => {
					if pressed && !self.overlay_key_down {
						self.overlay_visible = !self.overlay_visible;
					}
					self.overlay_key_down = pressed;
				}
				_ => {}
			},
			InputEvent::MouseDelta { dx, dy } => {
				self.look_accum.0 += dx as f32;
				self.look_accum.1 += dy as f32;
			}
			InputEvent::MouseButton { button, pressed } => {
				if pressed {
					match button {
						MouseButton::Left => self.click_dig = true,
						MouseButton::Right => self.click_place = true,
						MouseButton::Other => {}
					}
				}
			}
			InputEvent::FocusLost => {
				self.keys = HeldKeys::default();
				self.touch.clear();
			}
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
		let t_update = std::time::Instant::now();
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

		self.apply_clicks();

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
		if let Some(r) = self.renderer.as_mut() {
			let t_encode = std::time::Instant::now();
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

	fn on_suspend(&mut self) {
		log::info!("engine: on_suspend (state save point)");
		self.metrics.pause();
	}

	fn on_resume(&mut self) {
		log::info!("engine: on_resume");
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
