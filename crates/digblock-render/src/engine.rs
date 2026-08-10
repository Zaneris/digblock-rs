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
use crate::ui::screens::inventory::InvAction;

/// A UI screen [`EngineConfig::debug_screen`] can open at startup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DebugScreen {
	/// The startup menu. Needs `menu: true` as well, which the launcher
	/// forces — captures otherwise run menu-less on purpose.
	Menu,
	Inventory,
	Alchemy,
	/// Not a screen: stock the inventory and open NOTHING, so the
	/// first-person viewmodel is on screen for `--screenshot`. It is
	/// here for the same reason the screens are — an empty starting
	/// inventory means an empty hand, which made the held item just as
	/// invisible to a headless capture as the pages were.
	Hand,
	/// Also not a screen: select the water tool, empty (`bucket`) or
	/// charged (`bucket-full`). The charge is the one item state in the
	/// game, it decides what both mouse buttons do, and it shows in two
	/// places at once — so both states need to be photographable.
	Bucket {
		full: bool,
	},
	/// The character screen, with mid-curve XP stocked so the bars and
	/// detail lines photograph as they play, not empty.
	Skills,
	/// The alchemy screen's second tab (craft rows, locked rows, costs).
	Transmute,
}

impl DebugScreen {
	/// Parse the launcher's DIG_SCREEN value. Unknown names are `None`
	/// rather than an error: this is a dev knob, not a wire format.
	pub fn parse(s: &str) -> Option<Self> {
		match s.trim().to_ascii_lowercase().as_str() {
			"menu" => Some(Self::Menu),
			"inventory" | "inv" => Some(Self::Inventory),
			"alchemy" | "alch" => Some(Self::Alchemy),
			"hand" => Some(Self::Hand),
			"bucket" => Some(Self::Bucket { full: false }),
			"bucket-full" => Some(Self::Bucket { full: true }),
			"skills" | "character" => Some(Self::Skills),
			"transmute" => Some(Self::Transmute),
			_ => None,
		}
	}
}

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
	/// User multiplier on the derived UI scale (settings slider).
	pub ui_scale: f32,
	/// User volume multipliers (settings sliders): 1.0 = authored gains.
	pub music_vol: f32,
	pub sfx_vol: f32,
	/// Multiplayer display name; the LAUNCHER fills this (e.g. from
	/// $USER) — the engine itself must stay env-free for wasm.
	pub player_name: Option<String>,
	/// Debug: park a fake remote player 5 blocks ahead (GPU-path check
	/// without a second client). Set by the desktop launcher from
	/// DIG_PLAYERBOX.
	pub debug_playerbox: bool,
	/// Debug: open a UI screen at startup with a stocked inventory, so a
	/// headless `--screenshot` run can capture it. Set by the desktop
	/// launcher from DIG_SCREEN.
	pub debug_screen: Option<DebugScreen>,
	/// Debug: place a showcase of every shaped-block variant near spawn
	/// (both stair materials × 4 facings, doors × facings × open/closed)
	/// once chunks are Ready. Set by the desktop launcher from
	/// DIG_SHAPES — visual verification without gameplay wiring.
	pub debug_shapes: bool,
	/// Spray droplets per waterfall-impact instance: 128 desktop, 48
	/// mobile (was inferred in the renderer from draw_dist <= 10). Each
	/// is a sub-tenth-block alpha billboard, so the cost is overdraw on
	/// a handful of culled emitters, not geometry.
	pub spray_droplets: u32,
	/// Per-frame wall-clock budget for main-thread streaming (see the
	/// STREAM_BUDGET_MS doc below, which is the default).
	pub stream_budget_ms: f32,
	/// Grow the streaming budget from measured frame headroom, scaled
	/// by how near the closest pending job is (the inline-executor web
	/// build; see `stream_budget`) — rim-only churn while walking stays
	/// at the fixed floor. Only effective with `workers == 0` and
	/// outside bench — captures must stay deterministic, and worker
	/// pools already produce across frames.
	pub adaptive_stream: bool,
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
	/// Log a per-phase breakdown of every over-budget frame. The LAUNCHER
	/// fills this (desktop: `DIG_SPIKELOG`; web: `?spikelog`) — reading the
	/// environment inside core answers None forever on wasm, which is
	/// exactly the platform whose spikes most need attributing.
	pub spike_log: bool,
	/// Chunk draws encoded per wide-shadow refresh slice. Lower on the web,
	/// where the same slice costs more to encode and the refresh sawtooth
	/// is a bigger share of the frame; the refresh just spans more frames,
	/// and the sun moves imperceptibly over the difference.
	pub wide_slice_draws: u32,
	/// Mesh bytes handed to the queue per frame. Wall-clock alone cannot
	/// bound this on the web: `write_buffer` merely RECORDS the copy and
	/// Chrome's GPU process performs the staging transfer at submit, so the
	/// cost lands after everything this engine can measure.
	pub upload_bytes_max: usize,
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
			ui_scale: 1.0,
			music_vol: 1.0,
			sfx_vol: 1.0,
			player_name: None,
			debug_playerbox: false,
			debug_shapes: false,
			debug_screen: None,
			spray_droplets: 128,
			stream_budget_ms: STREAM_BUDGET_MS,
			adaptive_stream: false,
			fog_start_frac: 0.70,
			fog_end_frac: 0.95,
			chunk_slots_override: None,
			menu: false,
			auto_join: None,
			identity: [0; 16],
			spike_log: false,
			wide_slice_draws: crate::renderer::WIDE_SLICE_DRAWS,
			upload_bytes_max: UPLOAD_BYTES_MAX_NATIVE,
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
			ui_scale: 1.0,
			music_vol: 1.0,
			sfx_vol: 1.0,
			player_name: None,
			debug_playerbox: false,
			debug_shapes: false,
			debug_screen: None,
			spray_droplets: 48,
			stream_budget_ms: STREAM_BUDGET_MS,
			adaptive_stream: false,
			fog_start_frac: 0.70,
			fog_end_frac: 0.95,
			chunk_slots_override: None,
			menu: false,
			auto_join: None,
			identity: [0; 16],
			spike_log: false,
			wide_slice_draws: WIDE_SLICE_DRAWS_CONSTRAINED,
			upload_bytes_max: UPLOAD_BYTES_MAX_CONSTRAINED,
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
			adaptive_stream: true,
			// Everything the main thread does is serialized with the render
			// loop here, so the per-frame ceilings that desktop can afford
			// to leave generous have to be real.
			wide_slice_draws: WIDE_SLICE_DRAWS_CONSTRAINED,
			upload_bytes_max: UPLOAD_BYTES_MAX_CONSTRAINED,
			..Self::desktop()
		}
	}

	/// The adaptive budget's effective enable (see `adaptive_stream`).
	fn adaptive_effective(&self) -> bool {
		self.adaptive_stream && self.workers == 0 && !self.bench
	}

	pub(crate) fn render_config(&self) -> RenderConfig {
		RenderConfig {
			shadow_size: self.shadow_size,
			draw_dist: self.draw_dist,
			spray_droplets: self.spray_droplets,
			wide_slice_draws: self.wide_slice_draws,
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
/// Sized to a chunk stack's worth of the player's own column: the
/// guarantee only has to cover where the player IS, and the queue is
/// nearest-first, so anything beyond that is the far ring choosing to wait
/// rather than to hitch. The byte ceiling below still applies.
const UPLOAD_MIN: usize = 2;
/// Mesh bytes per frame on desktop — high enough to be a backstop against
/// pathological frames rather than a throttle on normal streaming.
const UPLOAD_BYTES_MAX_NATIVE: usize = 8 << 20;
/// …and on the web/mobile, where the staging copy lands in another
/// process at submit time and is invisible to `upload_ms`.
const UPLOAD_BYTES_MAX_CONSTRAINED: usize = 768 << 10;
/// Wide-cascade slice size where encode is expensive (web, mobile).
const WIDE_SLICE_DRAWS_CONSTRAINED: u32 = 448;
/// Evicted chunks released per frame even with the budget spent, so a
/// player who keeps walking can't outrun the release queue.
const EVICT_MIN: usize = 8;
/// Authoritative edits applied per frame. Each costs a copy-on-write cell
/// write plus up to eight mesh-heap pushes, so a join sync's worth of them
/// in one frame is a visible stall; spread over frames it is invisible,
/// and the world simply fills in as fast as meshing can keep up anyway.
const NET_EDIT_BUDGET: usize = 256;

/// Messages the feed keeps. Also the scrollback: entries are no longer
/// dropped when they expire, so this is how far back the chat box can
/// show while you type.
const FEED_MAX: usize = 12;
/// How long a message stays lit before it fades out of the HUD. It stays
/// in `feed` afterwards with a non-positive ttl — see [`FEED_TTL_FLOOR`].
const FEED_TTL_SECS: f32 = 15.0;
/// Expired entries stop counting down here. Without a floor the ttl of a
/// long-lived message would run away into large negatives; nothing reads
/// the magnitude, only the sign, so it just needs to stay finite.
const FEED_TTL_FLOOR: f32 = -1.0;

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
	/// Cell whose dig was refused — locally (world floor) or by the
	/// server (a spawn lock it never told us about). Held until the aim
	/// moves elsewhere, and load-bearing in two ways: it explains itself
	/// once instead of every frame, and it stops `step_dig` immediately
	/// re-opening a hold the server just refused, which would re-send
	/// `DigStart` every frame for as long as the button was down.
	refused_cell: Option<(i32, i32, i32)>,
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
	/// Inbound events past this frame's edit budget, applied in order on
	/// later frames (see `NET_EDIT_BUDGET`).
	net_backlog: std::collections::VecDeque<digblock_net::NetEvent>,
	/// Snapshot ack carried from the net drain to the walk branch (the
	/// reconcile needs the physics `block_at` borrow).
	pending_ack: Option<(u32, digblock_core::player::PlayerState)>,
	/// Visual-only reconciliation smoothing: camera = player + offset,
	/// decaying ~200 ms so corrections nudge instead of teleport.
	correction_offset: Vec3,
	/// Visual-only sampling of the fixed 60 Hz player sim: the camera
	/// draws between the last two tick states instead of snapping to the
	/// newest one. Without it frame-pacing jitter IS position jitter —
	/// measurably the dominant stutter source, well ahead of anything the
	/// network does (see NET-AUDIT.md).
	tick_lerp: crate::smoothing::TickLerp,
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
	/// Adaptive streaming-budget controller (web/inline builds; the fixed
	/// config budget elsewhere — see `EngineConfig::adaptive_stream`).
	stream_budget: digblock_core::stream_budget::StreamBudget,
	/// Phase timings for the in-progress frame, handed to metrics at the
	/// end of render().
	pending: FrameTimings,
	/// Redraw-handler entry, opened by `frame_started` and closed in
	/// `render` to yield `busy_ms`.
	frame_start: Option<web_time::Instant>,
	/// Start of the post-streaming, pre-encode span (see `prep_ms`).
	prep_start: Option<web_time::Instant>,
	/// Memoized worldgen surface height for the ambient-zone test, keyed by
	/// the camera's block column.
	ambient_height: Option<((i32, i32), i32)>,
	overlay_visible: bool,
	history_scratch: Vec<f32>,
	/// Scripted-flight clock (bench mode), in fixed 60Hz ticks.
	bench_t: f32,
	/// Meshes drained but not yet uploaded (upload budget carry-over).
	pending_uploads: std::collections::VecDeque<digblock_core::pipeline::MeshOutput>,
	/// Evicted chunks whose GPU buffers are still to be released, drained
	/// under the frame's streaming budget.
	pending_evicts: std::collections::VecDeque<ChunkPos>,
	/// Positions with a release still outstanding. Doubles as the cancel
	/// set: an upload for a queued position clears it, so a chunk that
	/// comes back before its release is reached keeps its new mesh.
	pending_evict_set: rustc_hash::FxHashSet<ChunkPos>,
	/// Camera chunk the upload queue was last ordered for.
	upload_sort_center: ChunkPos,
	size: (u32, u32),
	/// Device pixels per logical pixel (shell-reported; 1.0 until told).
	scale_factor: f32,
	/// Whole-UI scale derived from `size` + `scale_factor` each time
	/// either changes.
	ui_scale: f32,
	/// Per-frame UI draw commands (HUD, screens), rebuilt in `render`.
	ui_dl: crate::ui::DrawList,
	/// Hit regions from the same emission pass (hotbar slots double as
	/// the inventory screen's drop targets).
	ui_hits: crate::ui::HitMap<crate::ui::InvId>,
	/// The drag-and-drop overflow inventory screen (I toggles).
	inv_screen: crate::ui::screens::inventory::InvScreen,
	/// The alchemy screen (K toggles): block → sub-material conversion
	/// and shaped-item crafting.
	alch_screen: crate::ui::screens::alchemy::AlchScreen,
	/// The character screen (C toggles): per-skill levels + XP bars.
	char_screen: crate::ui::screens::character::CharScreen,
	/// Per-skill XP totals: adopted from the server online
	/// (`Skills`/`XpGain` — never client-claimed), computed through the
	/// same core award functions offline.
	skills: digblock_core::skills::SkillSet,
	/// Live "+N SKILL" toasts above the hotbar (coalescing per skill).
	xp_toasts: Vec<crate::ui::hud::XpToast>,
	/// H pressed in-game: merged with the menu's return-spawn request.
	return_spawn_requested: bool,
	/// A return-to-spawn is outstanding, so the next teleport that lands
	/// is ours and says so. Kept apart from the teleport's `look`, which
	/// a server with `/set-spawn-dir` set fills in on this path too.
	spawn_return_pending: bool,
	/// The inventory-grid arrangement behind the hotbar (slot
	/// assignments; counts stay in `inventory`). Kept consistent by
	/// `reconcile_layout` every frame.
	inv_grid: digblock_core::inv_grid::InvGrid,
	/// The last layout wire pair sent to the server — any difference
	/// triggers a `ClientMsg::Layout` (before this frame's edits, so the
	/// server's slot enforcement always sees the current arrangement).
	last_layout_sent: Option<(Vec<u16>, Vec<u16>)>,
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
	swim_strokes: crate::audio::SwimStrokes,
	/// Held-item animation. Advanced in `update`, NOT in `step_audio` —
	/// that returns early with no audio sink attached (bench, screenshot,
	/// iOS), which would silently freeze the hand in exactly the
	/// configurations that photograph it.
	view_bob: crate::viewmodel::Bob,
	view_swing: crate::viewmodel::Swing,
	/// Owns which item the hand is DRAWING — it lags `hotbar.selected`
	/// by half a swap, so the exchange happens below the frame.
	view_swap: crate::viewmodel::Swap,
	ambient: crate::audio::Ambient,
	/// Scratch for batched audio commands (water voices).
	audio_scratch: Vec<crate::audio::AudioCmd>,
	/// Player eye position last frame (footstep distance).
	prev_player_pos: Vec3,
	/// Last music pool announced to the filler; `None` until the first
	/// frame, so startup always sends one.
	music_night: Option<bool>,
	/// Were the feet in water last frame? Rising edge = one splash.
	prev_wet: bool,
	/// Highest foot height since the feet were last supported — the fall
	/// height a splash is scaled by. Reset on ground and in water, so
	/// walking down a hillside into a lake isn't a plunge.
	fall_from_y: f32,
	/// Per-frame name-tag scratch (reused allocation).
	/// Name tags, kept beyond `nametag_count` so their String buffers
	/// survive between frames instead of being reallocated every frame.
	nametag_scratch: Vec<crate::renderer::NameTag>,
	nametag_count: usize,
	/// Launcher-owned HTTP/probe services the menu talks to; None = no
	/// master URL configured (Multiplayer shows as disabled).
	services: Option<crate::menu::MenuServices>,
	/// Launcher-injected settings persistence hook, called with each
	/// change batch; None (bench, drill, iOS) = settings don't persist.
	settings_store: Option<crate::settings::SettingsStore>,
	/// Launcher-injected named-save backend; None (bench, drill,
	/// scripted joins, iOS) keeps the ephemeral "Play Local" flow.
	world_store: Option<Box<dyn digblock_persist::WorldStore>>,
	/// True while playing an opened named save: recorded edits stream to
	/// the store and player state snapshots on a timer. False for the
	/// startup backdrop world (which still has an overlay, so eviction
	/// is safe, but nothing persists).
	in_local_save: bool,
	/// Seconds since the last player/inventory snapshot handoff.
	player_save_accum: f32,
	/// The world's spawn point (the server's after a Welcome swap):
	/// the return-to-spawn target.
	spawn_pos: Vec3,
	/// Seconds left on the return-to-spawn cooldown (0 = ready). One
	/// gate for local and connected play; connected, the server also
	/// enforces its own copy.
	spawn_cooldown: f32,
	/// DIG_SHAPES=1 showcase cells not yet placed (chunks stream in
	/// asynchronously; each update retries what's left). Empty = done
	/// or disabled.
	debug_shapes_pending: Vec<(i32, i32, i32, u16)>,
}

/// Client-side hold-to-dig progress (mirrors the server's `DigState`;
/// purely predictive — the server re-times the interval itself).
struct DigProgress {
	cell: (i32, i32, i32),
	/// Full cell value at hold start: any content change (id OR state —
	/// e.g. another player growing a slab mid-hold) restarts the hold.
	cell_value: u16,
	ticks: u16,
	required: u16,
}

/// Stair/door placement facing from the look direction: the dominant
/// horizontal axis, ascending/facing AWAY from the placer. The server
/// accepts the client-chosen facing (validated by `state_valid`), so no
/// re-derivation can disagree.
fn quantized_facing(dir: Vec3) -> digblock_core::shapes::Facing {
	use digblock_core::shapes::Facing;
	if dir.x.abs() >= dir.z.abs() {
		if dir.x >= 0.0 {
			Facing::PosX
		} else {
			Facing::NegX
		}
	} else if dir.z >= 0.0 {
		Facing::PosZ
	} else {
		Facing::NegZ
	}
}

/// DIG_SHAPES=1 layout: every shaped variant in rows floating at eye
/// height near spawn (render verification needs no ground) — both stair
/// materials × 4 facings, then closed and open doors.
fn debug_shapes_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{door_state, slab_state, stair_state, Facing};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for (i, f) in Facing::ALL.into_iter().enumerate() {
		let x = bx + 2 * i as i32 - 3;
		out.push((x, by, bz + 4, make_cell(B::WoodStairs, stair_state(f))));
		out.push((x, by, bz + 6, make_cell(B::StoneStairs, stair_state(f))));
		for (dz, open) in [(8, false), (10, true)] {
			out.push((
				x,
				by,
				bz + dz,
				make_cell(B::WoodDoor, door_state(f, open, false)),
			));
			out.push((
				x,
				by + 1,
				bz + dz,
				make_cell(B::WoodDoor, door_state(f, open, true)),
			));
		}
	}
	// Slabs: both materials × both states, then a walkable seam row —
	// 1/3 | 2/3 | full block | full + raised 1/3, every riser one
	// auto-step (also eyeballs boundary culling on the shared planes).
	// Ceiling slabs hang two cells up, where their underside is the
	// thing to look at.
	for (i, b) in [B::WoodSlab, B::StoneSlab].into_iter().enumerate() {
		let x = bx + 2 * i as i32 - 3;
		out.push((x, by, bz + 12, make_cell(b, slab_state(false, false))));
		out.push((x, by, bz + 14, make_cell(b, slab_state(true, false))));
		out.push((x, by + 2, bz + 12, make_cell(b, slab_state(false, true))));
		out.push((x, by + 2, bz + 14, make_cell(b, slab_state(true, true))));
	}
	let base = [
		make_cell(B::WoodSlab, slab_state(false, false)),
		make_cell(B::WoodSlab, slab_state(true, false)),
		B::Wood as u16,
		B::Wood as u16,
	];
	for (dx, cell) in base.into_iter().enumerate() {
		out.push((bx + 1 + dx as i32, by, bz + 16, cell));
	}
	out.push((
		bx + 4,
		by + 1,
		bz + 16,
		make_cell(B::WoodSlab, slab_state(false, false)),
	));
	out
}

fn ms_since(t: web_time::Instant) -> f32 {
	t.elapsed().as_secs_f32() * 1000.0
}

/// Coarse "last played" age for the world list.
fn age_label(now_ms: u64, then_ms: u64) -> String {
	let secs = now_ms.saturating_sub(then_ms) / 1000;
	match secs {
		0..=59 => "just now".into(),
		60..=3599 => format!("{}m ago", secs / 60),
		3600..=86_399 => format!("{}h ago", secs / 3600),
		_ => format!("{}d ago", secs / 86_400),
	}
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

/// Texture-array layer for a snapshot's held-item wire id (`None` =
/// empty hand, unknown id from a newer server, or nothing to draw).
fn held_tex_layer(held: u16) -> Option<u16> {
	use digblock_core::hotbar::{WIRE_EMPTY, WIRE_TOOL};
	match held {
		WIRE_EMPTY => None,
		// The remote hand is one small cube, so the bucket's charge is
		// all it can say: wood for an empty pail, water for a full one.
		WIRE_TOOL => Some(BlockType::Wood.props().tex_side),
		digblock_net::messages::HELD_TOOL_FULL => Some(BlockType::Water.props().tex_side),
		id => BlockType::try_from_id(id).map(|b| b.props().tex_side),
	}
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
		// Pay the sub-block variant bakes here rather than in whichever
		// frame first meshes a door (see `shapes::prewarm`).
		digblock_core::shapes::prewarm();
		let mut metrics = Metrics::new(config.metrics_csv.as_deref(), config.spike_log);
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
		// Even the ephemeral backdrop world gets an overlay: local edits
		// must survive chunk eviction within a session, saved or not.
		let pipeline = Pipeline::with_overlay(
			seed,
			config.draw_dist,
			config.workers,
			digblock_core::overlay::EditOverlay::default(),
		);
		let sun = config.tod.map_or_else(Sun::new, Sun::with_tod);
		let overlay_visible = config.overlay;
		let stream_floor = config.stream_budget_ms;
		let spawn = digblock_core::player::spawn(seed);
		let mut camera = FlyCamera::new(far, spawn);
		if let Some(yaw) = config.yaw {
			camera.yaw_deg = yaw;
		}
		if let Some(pitch) = config.pitch {
			camera.pitch_deg = pitch.clamp(-89.0, 89.0);
		}
		// The name field starts empty unless a valid override was supplied
		// (`?name=`/`--name`, or the persisted last-used name the launcher
		// loaded into the config).
		let menu = crate::menu::Menu::new(
			config.menu,
			crate::settings::Settings {
				draw_dist: config.draw_dist,
				music_vol: config.music_vol,
				sfx_vol: config.sfx_vol,
				ssr: config.ssr,
				ui_scale: config.ui_scale,
				name: config
					.player_name
					.clone()
					.filter(|n| digblock_net::protocol::validate_name(n).is_ok())
					.unwrap_or_default(),
			},
			config.auto_join.clone(),
		);
		let mut engine = Self {
			menu,
			chat: crate::chat::Chat::default(),
			audio: None,
			water_audio: crate::water_audio::WaterAudio::default(),
			footsteps: crate::audio::Footsteps::default(),
			swim_strokes: crate::audio::SwimStrokes::default(),
			view_bob: crate::viewmodel::Bob::default(),
			view_swing: crate::viewmodel::Swing::default(),
			view_swap: crate::viewmodel::Swap::default(),
			ambient: crate::audio::Ambient::default(),
			audio_scratch: Vec::new(),
			prev_player_pos: spawn,
			music_night: None,
			// Assume wet: whatever the spawn turns out to be, arriving
			// there is not "entering the water".
			prev_wet: true,
			fall_from_y: spawn.y,
			nametag_scratch: Vec::new(),
			nametag_count: 0,
			services: None,
			settings_store: None,
			world_store: None,
			in_local_save: false,
			player_save_accum: 0.0,
			spawn_pos: spawn,
			spawn_cooldown: 0.0,
			debug_shapes_pending: if config.debug_shapes {
				debug_shapes_plan(spawn)
			} else {
				Vec::new()
			},
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
			refused_cell: None,
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
			net_backlog: std::collections::VecDeque::new(),
			pending_ack: None,
			correction_offset: Vec3::ZERO,
			tick_lerp: crate::smoothing::TickLerp::new(spawn),
			remote_scratch: Vec::new(),
			remote_instances: Vec::new(),
			net_corrections: 0,
			net_error_accum: 0.0,
			net_log_timer: 0.0,
			feed: Vec::new(),
			player_names: rustc_hash::FxHashMap::default(),
			my_player_id: None,
			metrics,
			stream_budget: digblock_core::stream_budget::StreamBudget::new(stream_floor),
			pending: FrameTimings::default(),
			frame_start: None,
			prep_start: None,
			ambient_height: None,
			overlay_visible,
			history_scratch: Vec::new(),
			bench_t: 0.0,
			pending_uploads: std::collections::VecDeque::new(),
			pending_evicts: std::collections::VecDeque::new(),
			pending_evict_set: rustc_hash::FxHashSet::default(),
			upload_sort_center: ChunkPos::new(i32::MAX, 0, i32::MAX),
			size: (1, 1),
			scale_factor: 1.0,
			ui_scale: 1.0,
			ui_dl: crate::ui::DrawList::default(),
			ui_hits: crate::ui::HitMap::default(),
			inv_screen: Default::default(),
			alch_screen: Default::default(),
			char_screen: Default::default(),
			skills: Default::default(),
			xp_toasts: Vec::new(),
			return_spawn_requested: false,
			spawn_return_pending: false,
			inv_grid: Default::default(),
			last_layout_sent: None,
		};
		// DIG_SCREEN: open a UI screen at startup, stocked, so
		// `--screenshot` can capture it. Screens are input-driven, which
		// left them the one part of the UI a headless capture could not
		// see — and a layout regression on the alchemy page shipped
		// because of exactly that.
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
				.add_sub(digblock_core::block::Essence::Flora, 54);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Earth, 120);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Aqua, 7);
			if let DebugScreen::Bucket { full: true } = screen {
				engine.inventory.add(BlockType::Water, 1);
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
				DebugScreen::Inventory => engine.inv_screen.open(),
				DebugScreen::Alchemy | DebugScreen::Transmute => {
					// A mid-curve level so the header's bar, the pace,
					// and a mix of locked/unlocked rows all photograph.
					engine
						.skills
						.add_xp(digblock_core::skills::Skill::Alchemy, 20_000);
					engine.alch_screen.open();
					if screen == DebugScreen::Transmute {
						engine
							.alch_screen
							.set_tab(crate::ui::screens::alchemy::AlchTab::Transmute);
					}
				}
				DebugScreen::Skills => {
					// Mid-curve totals: fractional bars, real XP lines.
					use digblock_core::skills::Skill;
					engine.skills.add_xp(Skill::Mining, 150_000);
					engine.skills.add_xp(Skill::Alchemy, 30_000);
					engine.char_screen.open();
				}
			}
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
		self.net = Some(digblock_net::NetClient::new(
			transport,
			digblock_net::client::NetClientConfig {
				name,
				draw_dist: self.config.draw_dist,
				token,
				identity: self.config.identity,
			},
		));
		// Fresh connection: the layout dedup re-primes from the server's
		// join-sync Layout.
		self.last_layout_sent = None;
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

	/// Inject the named-save backend (launcher-owned: desktop files, web
	/// localStorage). Root's "Play Local" becomes "Singleplayer".
	pub fn set_world_store(&mut self, store: Box<dyn digblock_persist::WorldStore>) {
		self.world_store = Some(store);
		self.menu.set_store_available();
	}

	/// Window size, display density and the user's preference all feed
	/// the one UI scale. Every place any of the three can change routes
	/// through here — miss one and the settings slider appears to do
	/// nothing until the window is resized.
	fn recompute_ui_scale(&mut self) {
		self.ui_scale =
			crate::ui::scale::ui_scale(self.size, self.scale_factor, self.config.ui_scale);
	}

	/// Apply a settings change batch at runtime, then persist it. SSR is
	/// a per-frame uniform bit; volumes ride the audio command stream;
	/// draw distance retunes the camera far plane, the streaming radius
	/// (rescan on next maintain), and — connected — the server's sync
	/// radius. The store sees every batch, including name-only changes
	/// from the join path.
	fn apply_settings(&mut self, s: crate::settings::Settings) {
		self.config.ssr = s.ssr;
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
			self.camera.far = (s.draw_dist * 32) as f32;
			self.pipeline.set_draw_dist(s.draw_dist);
			if let Some(net) = self.net.as_mut() {
				if net.ready() {
					net.send_set_draw_dist(s.draw_dist);
				}
			}
		}
		if let Some(store) = &mut self.settings_store {
			store(&s);
		}
	}

	/// The block just under the feet and whether it holds the player up.
	/// Shared by the footstep cadence and the held item's walk bob, which
	/// have to agree: the hand keeps time with the feet, so anything that
	/// stops the steps sounding stops the hand swinging. Cheap enough to
	/// call twice a frame — it is one `block_at`.
	fn ground_below(&self) -> (Option<BlockType>, bool) {
		let feet = self.player.pos.y - digblock_core::player::EYE_TO_FEET;
		let below = self.pipeline.block_at(
			self.player.pos.x.floor() as i32,
			(feet - 0.05).floor() as i32,
			self.player.pos.z.floor() as i32,
		);
		(below, below.is_some_and(|b| b.props().collides))
	}

	/// Per-frame audio upkeep, after the camera settles: listener pose,
	/// footsteps, waterfall voices, ambient bed. One early-out branch
	/// when no sink is attached.
	///
	/// `prev_player_pos` is deliberately NOT advanced here — `update`
	/// owns it, because the viewmodel's bob reads the same delta and
	/// this function returns early on a silent run.
	fn step_audio(&mut self, dt: f32) {
		use crate::audio::{ambient_sfx, step_sfx, AmbientZone, AudioCmd, LOOP_AMBIENT};
		/// Splash gain: a step into the shallows, plus this much per block
		/// of the fall that preceded it, capped.
		const SPLASH_GAIN_MIN: f32 = 0.3;
		const SPLASH_GAIN_PER_BLOCK: f32 = 0.06;
		const SPLASH_GAIN_MAX: f32 = 0.9;
		if self.audio.is_none() {
			return;
		}
		let cam = self.camera.pos;
		self.emit(AudioCmd::Listener {
			pos: cam.to_array(),
			forward: self.camera.dir().to_array(),
		});

		// Footsteps and the water-entry splash, both driven by the feet.
		let submerged = self.eye_in_water();
		let d = self.player.pos - self.prev_player_pos;
		let feet = self.player.pos.y - digblock_core::player::EYE_TO_FEET;
		let (bx, bz) = (
			self.player.pos.x.floor() as i32,
			self.player.pos.z.floor() as i32,
		);
		let (below, grounded) = self.ground_below();
		let feet_wet = self
			.pipeline
			.block_at(bx, feet.floor() as i32, bz)
			.is_some_and(|b| b.is_water());
		let wet = submerged || feet_wet;
		if self.walking {
			let moved = Vec3::new(d.x, 0.0, d.z).length();
			if let Some((m, gain)) = self.footsteps.advance(moved, grounded, below, wet) {
				self.emit(AudioCmd::Sfx {
					sound: step_sfx(m),
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
			// One splash on the rising edge, scaled by the fall that got
			// you there — a step off a bank barely registers, a plunge
			// from a cliff does. Fly mode is excluded: the body isn't
			// being simulated, so drifting through a surface isn't an
			// entry.
			if wet && !self.prev_wet {
				let drop = (self.fall_from_y - feet).max(0.0);
				let gain = (SPLASH_GAIN_MIN + drop * SPLASH_GAIN_PER_BLOCK).min(SPLASH_GAIN_MAX);
				self.emit(AudioCmd::Sfx {
					sound: crate::audio::Sfx::Splash,
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
			// Strokes while actually swimming — in the water and off the
			// bottom. Wading is grounded, so it keeps its footsteps and
			// the two can never double up.
			if let Some(gain) = self.swim_strokes.advance(moved, wet && !grounded) {
				self.emit(AudioCmd::Sfx {
					sound: crate::audio::Sfx::Swim,
					pos: Some([self.player.pos.x, feet, self.player.pos.z]),
					gain,
				});
			}
		}
		self.prev_wet = wet;
		self.fall_from_y = if grounded || wet || !self.walking {
			feet
		} else {
			self.fall_from_y.max(feet)
		};

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
			// `terrain_height` runs the whole continentalness/roughness/
			// ridge/detail stack, which is far too much to spend every
			// frame on a question whose answer is a pure function of the
			// column — and the camera changes column a few times a second
			// at a walk.
			let col = (cam.x.floor() as i32, cam.z.floor() as i32);
			let h = match self.ambient_height {
				Some((c, h)) if c == col => h,
				_ => {
					let h = digblock_core::worldgen::terrain_height(col.0, col.1, self.seed);
					self.ambient_height = Some((col, h));
					h
				}
			};
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

		// Music pool follows the world clock. `tod` is the sun's angle
		// from straight down (0 = noon, ±90 = the horizons, 180 =
		// midnight), so hour = 12 - tod/15 and the 18:00-06:00 night
		// window is exactly "sun below the horizon". No hysteresis
		// needed: tod is monotonic, and the filler only acts on this at
		// the next track boundary anyway.
		let night = self.sun.tod.abs() >= 90.0;
		if self.music_night != Some(night) {
			self.music_night = Some(night);
			self.emit(AudioCmd::MusicSet { night });
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
		self.close_local_save();
		self.player_names.clear();
		self.push_feed("Connected to server.".into());
		self.seed = seed;
		self.spawn_pos = spawn;
		// No overlay: the server's journal owns this world's edits.
		self.pipeline = Pipeline::new(seed, self.config.draw_dist, self.config.workers);
		self.player = Player::new(spawn);
		self.camera.pos = spawn;
		// The server's InventoryFull (right behind JournalDone) is the
		// authoritative base; local single-player stock must not leak in.
		self.inventory = Inventory::default();
		self.reset_world_transients(spawn, true);
	}

	/// The world-swap tail shared by server joins, save entry, and the
	/// leave-server backdrop: kill in-flight interaction and streaming
	/// state, and silence the old world.
	fn reset_world_transients(&mut self, pos: Vec3, world_replaced: bool) {
		self.dig = None;
		self.target = None;
		self.fluid = digblock_core::fluid::FluidSim::new();
		self.flow_accum = 0.0;
		self.tick_accum = 0.0;
		self.pending_ack = None;
		self.correction_offset = Vec3::ZERO;
		// Parked net events belong to the session that queued them. `pump_net`
		// clears the backlog when IT observes a disconnect, but a menu-driven
		// Leave Server drops `self.net` outright, so `pump_net` returns early
		// and the queue simply freezes — thousands of a half-streamed join
		// sync's edits waiting to be applied to whatever world comes next.
		// (A stale `Welcome` in there re-swaps the world to the old seed; a
		// stale `Disconnected` kills the fresh connection on arrival.)
		self.net_backlog.clear();
		// Likewise session state: a cell one server refused stays refused at
		// the same coordinates in the next world otherwise, because the only
		// other clear needs the dig button held AND aimed elsewhere.
		self.refused_cell = None;
		self.tick_lerp.reset(pos);
		// A reused pipeline (entering the world already streamed behind
		// the menu) keeps its meshes and waterfall voices — the world is
		// identical, clearing would only force a full re-upload.
		if world_replaced {
			self.pending_uploads.clear();
			// `clear_chunks` already dropped every GPU mesh, so queued
			// releases are not just redundant but dangerous: those
			// positions exist in the NEW world too, and releasing one later
			// would delete a chunk that legitimately belongs there.
			self.pending_evicts.clear();
			self.pending_evict_set.clear();
			self.upload_sort_center = ChunkPos::new(i32::MAX, 0, i32::MAX);
			if let Some(r) = self.renderer.as_mut() {
				r.clear_chunks();
			}
			let mut cmds = std::mem::take(&mut self.audio_scratch);
			self.water_audio.clear(&mut cmds);
			for cmd in cmds.drain(..) {
				self.emit(cmd);
			}
			self.audio_scratch = cmds;
		}
		// Old-world sounds don't survive the swap.
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_DIG,
		});
		self.ambient.reset();
		self.swim_strokes.reset();
		self.view_bob.reset();
		self.view_swing.reset();
		self.view_swap.reset();
		self.prev_player_pos = pos;
		self.prev_wet = true;
		self.fall_from_y = pos.y;
	}

	/// Flush and detach the active named save (leaving for a server,
	/// another save, or a backdrop world). Drains this frame's recorded
	/// edits and flushes WITH the live overlay before closing — a
	/// snapshot-based store (web) serializes from that overlay, and this
	/// is its last chance to see it before the pipeline is replaced.
	fn close_local_save(&mut self) {
		let recorded = self.pipeline.take_recorded_edits();
		if let Some(store) = self.world_store.as_mut() {
			if self.in_local_save {
				for (x, y, z, cell) in recorded {
					store.append_edit(x, y, z, cell);
				}
				store.flush(self.pipeline.overlay());
				store.close_world();
			}
		}
		self.in_local_save = false;
	}

	/// Swap to a fresh backdrop world and relock the startup menu — the
	/// shared tail of leaving a server and leaving a local world. Don't
	/// keep phantom-playing the world we just left: a new clock-derived
	/// seed means the menu's backdrop is visibly not the save behind it.
	/// The caller detaches whatever it was attached to first
	/// ([`Self::close_local_save`] / dropping the transport).
	fn return_to_title(&mut self) {
		let nanos = web_time::SystemTime::now()
			.duration_since(web_time::UNIX_EPOCH)
			.map_or(0, |d| d.subsec_nanos());
		let seed = nanos as f64 / u32::MAX as f64 * 10000.0;
		self.seed = seed;
		let spawn = digblock_core::player::spawn(seed);
		self.spawn_pos = spawn;
		self.pipeline = Pipeline::with_overlay(
			seed,
			self.config.draw_dist,
			self.config.workers,
			digblock_core::overlay::EditOverlay::default(),
		);
		self.player = Player::new(spawn);
		self.camera.pos = spawn;
		self.inventory = Inventory::default();
		self.skills = Default::default();
		self.xp_toasts.clear();
		self.player_names.clear();
		self.reset_world_transients(spawn, true);
		self.in_local_save = false;
		self.menu.on_returned_to_title();
	}

	/// Enter an opened named save: rebuild the world around its seed +
	/// edit overlay, restore inventory and (when present) the player's
	/// last position and view.
	fn enter_local_world(&mut self, save: digblock_persist::WorldSave) {
		log::info!(
			"entering local world '{}' (seed {})",
			save.meta.name,
			save.meta.seed
		);
		self.player_names.clear();
		let spawn = digblock_core::player::spawn(save.meta.seed);
		self.spawn_pos = spawn;
		let overlay: digblock_core::overlay::EditOverlay = save.edits.into_iter().collect();
		// Same world already streamed in behind the menu (Create adopts
		// the backdrop's seed to hit this deliberately), no edits on
		// either side: keep the pipeline — and its generated chunks and
		// uploaded meshes — instead of regenerating from scratch. That
		// window is when the player would otherwise stand over an empty
		// world (held by the walk gate, seconds long on web's inline
		// executor).
		let reuse = self.net.is_none()
			&& save.meta.seed == self.seed
			&& overlay.is_empty()
			&& self.pipeline.overlay().is_some_and(|o| o.is_empty());
		self.seed = save.meta.seed;
		if !reuse {
			self.pipeline = Pipeline::with_overlay(
				save.meta.seed,
				self.config.draw_dist,
				self.config.workers,
				overlay,
			);
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
		// The save's skill totals (empty for pre-skills saves = level 1);
		// stale toasts belong to whatever world we just left.
		self.skills = digblock_core::skills::SkillSet::from_wire(&save.skills);
		self.xp_toasts.clear();
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
		self.in_local_save = true;
		self.player_save_accum = 0.0;
		self.menu.on_world_entered();
	}

	/// Handle one menu world intent against the injected store.
	fn handle_world_intent(&mut self, intent: crate::menu::WorldIntent) {
		use crate::menu::WorldIntent;
		match intent {
			WorldIntent::Refresh => self.push_world_list(),
			WorldIntent::Create { name } => {
				// On a backdrop world (not in a save, not on a server),
				// adopt the backdrop's seed: the world behind the menu is
				// exactly the one being created, so `enter_local_world`
				// keeps the already-streamed pipeline instead of
				// regenerating it from scratch. Otherwise a fresh
				// clock-derived seed (no `rand` — wasm cleanliness).
				let seed = if self.net.is_none() && !self.in_local_save {
					self.seed
				} else {
					let nanos = web_time::SystemTime::now()
						.duration_since(web_time::UNIX_EPOCH)
						.map_or(0, |d| d.subsec_nanos());
					nanos as f64 / u32::MAX as f64 * 10000.0
				};
				let opened = self.world_store.as_mut().map(|store| {
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
						self.menu.set_worlds_status(e);
						self.push_world_list();
					}
					None => {}
				}
			}
			WorldIntent::Enter { id } => {
				// Close the current save BEFORE opening the next (single
				// active world per store).
				self.close_local_save();
				let opened = self.world_store.as_mut().map(|s| s.open_world(&id));
				match opened {
					Some(Ok(save)) => self.enter_local_world(save),
					Some(Err(e)) => self.menu.set_worlds_status(e),
					None => {}
				}
			}
			WorldIntent::Delete { id } => {
				if let Some(store) = self.world_store.as_mut() {
					// The store drops the active world itself if this is it.
					store.delete_world(&id);
				}
				self.push_world_list();
			}
		}
	}

	/// Send the store's world list to the menu, ages formatted.
	fn push_world_list(&mut self) {
		let Some(store) = self.world_store.as_mut() else {
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
		self.menu.set_world_list(rows);
	}

	/// Adopt an authoritative facing: yaw from the horizontal direction,
	/// pitch levelled. Shared by teleports and the spawn facing an admin
	/// set, so the two can never disagree about which way `look` points.
	fn face(&mut self, look: Vec3) {
		self.camera.yaw_deg = look.x.atan2(look.z).to_degrees();
		self.camera.pitch_deg = 0.0;
	}

	/// Land a teleport (return to spawn, admin /tp): hard-set the player
	/// and camera — a 200 ms smoothed slide across half the map would
	/// fight streaming — and drop everything that describes the
	/// pre-teleport position (the decaying correction offset, a stashed
	/// ack, any dig in progress, the footstep odometer). A non-zero
	/// `look` adopts that facing (level pitch); zero keeps the view.
	///
	/// The feed line keys on `spawn_return_pending` — what WE asked for —
	/// not on whether a facing came back: a server with `/set-spawn-dir`
	/// set answers a return-to-spawn with a look, and reading that as
	/// "an admin teleported me" got the wording backwards.
	fn apply_teleport(&mut self, pos: Vec3, look: Vec3) {
		self.player.set_state(digblock_core::player::PlayerState {
			pos,
			vel: Vec3::ZERO,
		});
		self.camera.pos = pos;
		self.correction_offset = Vec3::ZERO;
		self.pending_ack = None;
		self.prev_player_pos = pos;
		self.prev_wet = true;
		self.fall_from_y = pos.y;
		self.tick_lerp.reset(pos);
		self.dig = None;
		if look != Vec3::ZERO {
			self.face(look);
		}
		let line = if std::mem::take(&mut self.spawn_return_pending) {
			"Returned to spawn."
		} else {
			"Teleported."
		};
		self.push_feed(line.into());
	}

	/// Apply an authoritative block value; the Pipeline holds edits whose
	/// chunk hasn't generated yet and applies them pre-mesh when it lands.
	fn apply_remote_edit(&mut self, cell: (i32, i32, i32), cell_value: u16) {
		self.pipeline
			.apply_authoritative(cell.0, cell.1, cell.2, cell_value);
	}

	/// Newest-last message feed; oldest drops when full so a join wave
	/// can't grow the overlay unboundedly.
	fn push_feed(&mut self, text: String) {
		self.push_feed_colored(text, 0xFFFF_FFFF);
	}

	/// Feed line in a specific 0xAABBGGRR color (chat is tinted per
	/// player, matching the sender's body color).
	fn push_feed_colored(&mut self, text: String, color: u32) {
		if self.feed.len() >= FEED_MAX {
			self.feed.remove(0);
		}
		self.feed.push((text, FEED_TTL_SECS, color));
	}

	/// Drain the network: remote edits into the pipeline, acks stashed
	/// for the walk branch, disconnects fall back to single-player.
	fn pump_net(&mut self, dt: f32) {
		let Some(mut net) = self.net.take() else {
			// Single player: no connection to report on. Without this the
			// overlay would keep showing the last session's numbers.
			self.metrics.net = None;
			return;
		};
		net.remotes.advance(dt);
		let mut events = std::mem::take(&mut self.net_events);
		events.clear();
		net.poll(&mut events, dt);
		use digblock_net::NetEvent;
		// Carry-over first, then this frame's fresh events. Applying edits
		// is bounded per frame, but the ORDER of the whole stream is not
		// negotiable — an Edit followed by a Layout must stay that way — so
		// hitting the cap parks the entire remaining queue rather than
		// letting cheap events overtake the expensive ones.
		//
		// Join sync is what forces this: the server streams its whole
		// journal as one Edit per cell, and applying thousands of them in
		// the frame they arrive is a multi-hundred-millisecond stall right
		// at the moment the world appears. Fluid broadcasts are the same
		// shape, smaller and recurring.
		let mut queue = std::mem::take(&mut self.net_backlog);
		for ev in events.drain(..) {
			match ev {
				// Acks are the one event with NO ordering dependency on
				// edits (they ride datagrams, edits ride the reliable
				// stream — already unordered relative to each other), and
				// they must never wait behind a parked backlog. The
				// predictor's buffer only reaches back `BUF_CAP` ticks
				// (2.1 s): an ack delayed past that falls out of the
				// buffer, so EVERY later snapshot takes the forced-replay
				// path — measured at 18 replays/s with 1.6-block
				// corrections. Join sync on a heavily edited world is
				// enough to get there at NET_EDIT_BUDGET per frame.
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => self.pending_ack = Some((acked_input_tick, me)),
				other => queue.push_back(other),
			}
		}
		let mut edit_budget = NET_EDIT_BUDGET;
		let mut drop_net = false;
		while let Some(next) = queue.front() {
			if matches!(next, NetEvent::Edit { .. }) {
				if edit_budget == 0 {
					break;
				}
				edit_budget -= 1;
			}
			let ev = queue.pop_front().expect("front() just succeeded");
			match ev {
				NetEvent::Welcome {
					seed,
					spawn,
					player_id,
					tod,
					spawn_look,
					..
				} => {
					self.my_player_id = Some(player_id);
					self.menu.on_welcome();
					self.swap_world(seed, spawn);
					// The admin's `/set-spawn-dir`, sent only when this
					// join actually placed us at spawn — a resume in place
					// sends zero and keeps the view we arrived with.
					if spawn_look != Vec3::ZERO {
						self.face(spawn_look);
					}
					// The server owns the world clock. A dev-build --tod
					// freeze still wins: update() re-asserts config.tod
					// every frame.
					self.sun = Sun::with_tod(tod);
				}
				// Unreachable: acks are dispatched above, ahead of the
				// queue. Kept as a backstop so a future event that does
				// get queued still lands rather than being dropped.
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => self.pending_ack = Some((acked_input_tick, me)),
				NetEvent::Edit {
					cell,
					cell_value,
					superseded,
					remote_author,
				} => {
					let block = BlockType::from_id(digblock_core::chunk::cell_id(cell_value));
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
					self.apply_remote_edit(cell, cell_value);
					// Our predictions on this cell lost to authority: the
					// cell is owned by the broadcast value, but the server
					// never applied our inventory deltas — take them back.
					for e in superseded {
						self.revert_inv_delta(e.inv_delta);
					}
				}
				NetEvent::EditRejected { edit: e, reason } => {
					// Revert the predicted edit: the cell AND the
					// inventory delta (dig credit / place debit).
					self.pipeline.set_cell(e.cell.0, e.cell.1, e.cell.2, e.prev);
					self.revert_inv_delta(e.inv_delta);
					// A protected cell is the one rejection a player can
					// act on, and the only one they can't see coming: the
					// world floor is predicted locally, but a spawn lock
					// is server-side knowledge.
					//
					// `inv_delta` is the dig/place discriminator (+1 = dig
					// credit, -1 = place debit) — NOT `placed == Air`,
					// which a slab dig breaks by writing its shrunk value.
					// A multi-cell action carries the delta on its first
					// entry only, so this speaks once per action rather
					// than once per cell.
					if reason == Some(digblock_net::messages::RejectReason::Protected) {
						match e.inv_delta {
							Some((_, n)) if n > 0 => {
								self.push_feed(digblock_core::protect::REFUSE_DIG_MSG.into())
							}
							Some((_, n)) if n < 0 => {
								self.push_feed(digblock_core::protect::REFUSE_PLACE_MSG.into())
							}
							_ => {}
						}
					}
				}
				NetEvent::DigRefused { cell, reason } => {
					// Abandon the hold the moment the server says no, so
					// the progress bar stops instead of running the full
					// dig time out against a block that can't break.
					// Recording the cell is what stops `step_dig` opening
					// a fresh hold on it next frame.
					if self.dig.as_ref().is_some_and(|d| d.cell == cell) {
						self.cancel_dig();
					}
					if reason == digblock_net::messages::RejectReason::Protected
						&& self.refused_cell != Some(cell)
					{
						self.refused_cell = Some(cell);
						self.push_feed(digblock_core::protect::REFUSE_DIG_MSG.into());
					}
				}
				NetEvent::Inventory { counts, pending } => {
					// Authoritative base (strict wire decode, sub-material
					// families included) + the predictions that were still in
					// flight WHEN THE SERVER'S COUNTS WERE DECODED. Reading
					// the live pending set here instead would be a different
					// moment — see `NetEvent::Inventory`.
					let mut inv = Inventory::from_wire(&counts);
					for (b, n) in pending {
						match n {
							n if n > 0 => inv.add(b, n as u32),
							n if n < 0 => {
								let _ = inv.try_take(b, (-n) as u32);
							}
							_ => {}
						}
					}
					self.inventory = inv;
					// New types file bar-first then grid; stale grid slots
					// clear — same derivation the server runs.
					digblock_core::inv_grid::reconcile_layout(
						&self.inventory,
						&mut self.hotbar,
						&mut self.inv_grid,
					);
				}
				NetEvent::Layout { hotbar, grid } => {
					// The identity's stored arrangement (join sync).
					// Sanitizing decode, keep the live selection, then
					// reconcile against the counts.
					use digblock_core::hotbar::{HOTBAR_SLOTS, WIRE_EMPTY};
					let mut bar_wire = [WIRE_EMPTY; HOTBAR_SLOTS];
					for (d, s) in bar_wire.iter_mut().zip(hotbar.iter()) {
						*d = *s;
					}
					let selected = self.hotbar.selected_index();
					self.hotbar = Hotbar::from_wire(&bar_wire);
					self.hotbar.select_digit(selected as u8 + 1);
					self.inv_grid = digblock_core::inv_grid::InvGrid::from_wire(&grid);
					// Prime the dedup with what the server holds (its
					// sanitized copy); the reconcile below normally lands on
					// exactly this, and any divergence gets reported by the
					// per-frame layout sync.
					self.last_layout_sent = Some((
						self.hotbar.to_wire().to_vec(),
						self.inv_grid.to_wire().to_vec(),
					));
					digblock_core::inv_grid::reconcile_layout(
						&self.inventory,
						&mut self.hotbar,
						&mut self.inv_grid,
					);
				}
				NetEvent::Skills { xp } => {
					// Join sync: adopt wholesale, no toasts — this is
					// state arriving, not an award happening.
					self.skills = digblock_core::skills::SkillSet::from_wire(&xp);
				}
				NetEvent::XpGain { skill, total } => {
					if let Some(s) = digblock_core::skills::Skill::try_from_index(skill) {
						self.apply_xp_total(s, total);
					}
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
				NetEvent::Teleported { pos, look } => self.apply_teleport(pos, look),
				// Hard-set: inter-beat drift is microscopic, and after a
				// paused clock (backgrounded tab) a snap is what we want.
				NetEvent::TimeOfDay { tod } => self.sun.tod = tod,
				NetEvent::AdminCommands { commands } => self.chat.set_commands(commands),
				NetEvent::Chat { from, text } => {
					if from == digblock_net::protocol::SERVER_ID {
						// Server-authored line (admin command replies):
						// bracketed, neutral white — not a player voice.
						self.push_feed(format!("[server] {text}"));
						continue;
					}
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
		// A disconnect voids everything still queued; keeping it would
		// apply a dead session's edits to whatever world comes next.
		if drop_net {
			queue.clear();
		}
		self.metrics.gauges.net_backlog = queue.len() as u32;
		self.net_backlog = queue;

		// Cleared on the way out, so the overlay reads "offline" the moment
		// a session ends rather than freezing on its last numbers.
		self.metrics.net = (!drop_net).then(|| net.stats());
		if !drop_net {
			self.net = Some(net);
		} else {
			self.pending_ack = None;
			self.correction_offset = Vec3::ZERO;
			self.my_player_id = None;
			self.last_layout_sent = None;
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
		// The adaptive controller feeds on the PREVIOUS frame's wall delta
		// and MEASURED occupancy — both complete by now; this frame's own
		// timings aren't. At the floor every value below matches the fixed
		// path.
		let adaptive = self.config.adaptive_effective();
		let budget_ms = if adaptive {
			// Urgency reads last frame's heaps (this frame's scan hasn't
			// run) — one frame of lag, and the floor batch still runs
			// regardless.
			self.stream_budget.on_frame(
				self.metrics.last.total_ms,
				self.metrics.last.busy_ms,
				self.pipeline.stream_urgency(),
			)
		} else {
			self.config.stream_budget_ms
		};
		self.metrics.stream_budget_ms = budget_ms;
		// ONE deadline for jobs, drain, evictions and uploads together.
		// Previously each stage got its own slice of the budget and they
		// summed to ~1.5× it; now they consume from the same clock, and the
		// per-stage minimum-progress floors keep any single stage from
		// being starved by an earlier one that overran.
		let deadline = t_maintain + Duration::from_secs_f32(budget_ms / 1000.0);
		let allowance = adaptive.then(|| Duration::from_secs_f32(budget_ms / 1000.0));
		let m = self
			.pipeline
			.maintain(center, usize::MAX, Some(deadline), allowance);
		self.pending.maintain_ms = ms_since(t_maintain);
		// Overlay-restored water resumes flowing (the client twin of the
		// sim's ReadyColumn handling); server worlds have no overlay.
		if self.net.is_none() {
			for &(x, y, z) in &m.overlaid_water {
				self.fluid.schedule_around(x, y, z);
			}
		}
		let t_upload = web_time::Instant::now();
		self.metrics.evicted += m.evicted.len() as u32;
		if !m.evicted.is_empty() {
			// A boundary crossing evicts a whole rim arc at once (600+
			// chunks at draw 20). Membership has to be a hash lookup, not a
			// linear scan of the evicted Vec per pending element, and the
			// GPU-side release is spread over frames below: buffers merely
			// idle until dropped, so nothing is gained by freeing them all
			// inside one frame.
			for pos in m.evicted {
				if self.pending_evict_set.insert(pos) {
					self.pending_evicts.push_back(pos);
				}
			}
			let gone = &self.pending_evict_set;
			self.pending_uploads.retain(|out| !gone.contains(&out.pos));
		}
		let appended = !m.meshed.is_empty();
		self.pending_uploads.extend(m.meshed);
		// Nearest-first: on fast travel the queue backs up, and the chunks
		// that matter are the ones at and around the player. The queue is
		// already ordered, so re-sort only when new work arrived or the
		// player changed column — not every frame it happens to be
		// non-empty, which is exactly when frames are tightest.
		if self.pending_uploads.len() > 1 && (appended || center != self.upload_sort_center) {
			self.upload_sort_center = center;
			self.pending_uploads.make_contiguous().sort_by_key(|out| {
				let dx = out.pos.x - center.x;
				let dz = out.pos.z - center.z;
				dx * dx + dz * dz
			});
		}
		// Release evicted GPU buffers under the same clock as everything
		// else, always making some progress so the queue can't grow without
		// bound while the player keeps moving.
		let mut released = 0usize;
		while let Some(pos) = self.pending_evicts.pop_front() {
			// Cancelled: the chunk came back (walked around the rim, or a
			// teleport) and re-uploaded before its removal was reached.
			// Releasing it now would delete the FRESH mesh and leave a hole
			// until something happened to re-dirty the chunk.
			if !self.pending_evict_set.remove(&pos) {
				continue;
			}
			renderer.remove_chunk(pos);
			self.water_audio.remove_chunk(pos);
			released += 1;
			if released >= EVICT_MIN && web_time::Instant::now() >= deadline {
				break;
			}
		}
		// Uploads are bounded by BYTES as well as by the clock: on the web
		// `write_buffer` only records the copy and Chrome's GPU process
		// performs the staging transfer at submit, so a frame that hands
		// over several megabytes looks free to `upload_ms` and hitches
		// anyway.
		let mut uploads = 0u32;
		let mut bytes = 0usize;
		while let Some(out) = self.pending_uploads.pop_front() {
			// Cancels any queued release for this position (see above).
			self.pending_evict_set.remove(&out.pos);
			// Both vertex streams are u32 words (see `mesher::pack`).
			bytes += (out.mesh.solid.len() + out.mesh.water.len()) * 4;
			renderer.upload_chunk(gpu, out.pos, &out.mesh);
			// Waterfall audio follows the same impacts spray renders.
			self.water_audio
				.set_chunk(out.pos, out.pos.origin(), &out.mesh.impacts);
			uploads += 1;
			if uploads as usize >= UPLOAD_MIN
				&& (bytes >= self.config.upload_bytes_max || web_time::Instant::now() >= deadline)
			{
				break;
			}
		}
		self.pending.upload_ms = ms_since(t_upload);
		self.metrics.chunks_generated += m.generated;
		self.metrics.meshes_uploaded += uploads;
		self.metrics.upload_kb += (bytes / 1024) as u32;
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
			upload_backlog: self.pending_uploads.len() as u32,
			evict_backlog: self.pending_evicts.len() as u32,
			// Written by pump_net earlier this frame; preserved here
			// because the gauges are replaced wholesale.
			net_backlog: self.net_backlog.len() as u32,
			// The bytes the loop above just handed over — the number the
			// byte cap actually governs, and the only per-frame signal for
			// a staging transfer `upload_ms` structurally cannot time.
			uploaded: uploads,
			upload_bytes: bytes as u32,
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

	/// True while the water tool is the selected hotbar item.
	fn water_tool_selected(&self) -> bool {
		self.hotbar.selected_item() == Some(HotbarItem::WaterTool)
	}

	/// True while the tool is out AND empty — the scooping state. This is
	/// the one that changes the picking ray, because it is the only state
	/// in which the thing you want to hit is a fluid source. Charged, the
	/// bucket aims at ordinary blocks like every other item, so it picks
	/// with the ordinary ray and the outline lands on the face you're
	/// about to pour against.
	fn scoop_ready(&self) -> bool {
		self.water_tool_selected() && !self.inventory.bucket_full()
	}

	/// Where the collidable body actually is, for occupancy questions
	/// ("would this placement trap me?"). Deliberately NOT `camera.pos`:
	/// the camera carries the tick interpolation and the decaying
	/// reconciliation offset, so it can sit up to `POS_TOLERANCE` from the
	/// body — and the server answers the same question from its own
	/// simulated position, so a camera-based answer mispredicts. Aim rays
	/// still come from the camera: you hit what you see.
	fn body_pos(&self) -> Vec3 {
		if self.walking {
			self.player.pos
		} else {
			self.camera.pos // fly leaves player.pos stale
		}
	}

	/// May a placement write this cell? Resident, and holding a block the
	/// shared core predicate calls replaceable. The server asks exactly
	/// this (`Sim::cell_is_replaceable`), so a place predicted here is
	/// never one the server turns around and rejects.
	fn cell_is_replaceable(&self, cell: (i32, i32, i32)) -> bool {
		self.pipeline
			.cell_at(cell.0, cell.1, cell.2)
			.is_some_and(|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_replaceable())
	}

	/// Recompute the crosshair target (every update — it drives the
	/// highlight overlay, the dig state machine, and placement).
	fn update_target(&mut self) {
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let pipeline = &self.pipeline;
		let cell_at = |x: i32, y: i32, z: i32| pipeline.cell_at(x, y, z);
		self.target = if self.scoop_ready() {
			raycast_fluid(origin, dir, REACH, &cell_at)
		} else {
			raycast(origin, dir, REACH, &cell_at)
		};
	}

	/// Is `hit` something the current tool can dig?
	/// The local world's spawn: the same edit-aware resolve the server
	/// runs, against this client's own edit overlay.
	///
	/// A world with no overlay (an unsaved session) has no recorded edits
	/// to reason about, so the anchor stands; `ascend_clear` still steps
	/// over anything built there this session.
	fn local_spawn(&self) -> Vec3 {
		let resolved = match self.pipeline.overlay() {
			Some(o) => digblock_core::spawn::resolve(self.spawn_pos, self.seed, &|x, y, z| {
				o.column(x.div_euclid(16), z.div_euclid(16))
					.and_then(|cells| cells.get(&(x, y, z)).copied())
			}),
			None => self.spawn_pos,
		};
		let pipeline = &self.pipeline;
		digblock_core::player::ascend_clear(resolved, &|x, y, z| pipeline.cell_at(x, y, z))
	}

	fn diggable(&self, hit: &RayHit) -> bool {
		// Permanently protected cells never start a hold. Only the world
		// floor is knowable client-side — a server's spawn lock is its own
		// secret, so a cell it has already refused is remembered instead.
		if digblock_core::protect::immutable(hit.cell.0, hit.cell.1, hit.cell.2, None)
			|| self.refused_cell == Some(hit.cell)
		{
			return false;
		}
		if self.water_tool_selected() {
			// A charged bucket digs nothing at all: it has one action
			// left, and that action is on the other button.
			self.scoop_ready() && hit.block.is_fluid_source()
		} else {
			hit.block.props().collides && hit.block.dig_hardness(hit.cell.1).is_some()
		}
	}

	/// One 60 Hz step of the hold-to-dig state machine. Purely
	/// predictive online — the server runs its own timer and aim checks.
	fn step_dig(&mut self) {
		// A charged bucket has no left-click action at all — not even the
		// "you can't dig that" complaint below, which belongs to a dig.
		if !self.dig_held || (self.water_tool_selected() && !self.scoop_ready()) {
			self.cancel_dig();
			return;
		}
		// Aiming somewhere new clears the refusal, so a fresh attempt on
		// a different block speaks again.
		if self.refused_cell.is_some() && self.refused_cell != self.target.map(|h| h.cell) {
			self.refused_cell = None;
		}
		// The locally-knowable refusal (the world floor). A server-side
		// lock can't be predicted and arrives as `DigRefused` instead.
		if let Some(h) = self
			.target
			.filter(|h| digblock_core::protect::immutable(h.cell.0, h.cell.1, h.cell.2, None))
		{
			if self.refused_cell != Some(h.cell) {
				self.refused_cell = Some(h.cell);
				self.push_feed(digblock_core::protect::REFUSE_DIG_MSG.into());
			}
		}
		let valid = self.target.filter(|h| self.diggable(h));
		let Some(hit) = valid else {
			self.cancel_dig();
			return;
		};
		let on_same = self
			.dig
			.as_ref()
			.is_some_and(|d| d.cell == hit.cell && d.cell_value == hit.cell_value);
		if !on_same {
			self.cancel_dig();
			// Level-scaled hold time (SIM v11) — the same `dig_required`
			// the server's completion gate runs.
			let required = hit
				.block
				.dig_hardness(hit.cell.1)
				.map(|h| {
					digblock_core::skills::dig_required(
						h,
						self.skills.level(digblock_core::skills::Skill::Mining),
					) as u16
				})
				.unwrap_or(u16::MAX);
			self.dig = Some(DigProgress {
				cell: hit.cell,
				cell_value: hit.cell_value,
				ticks: 0,
				required,
			});
			if let Some(net) = self.net.as_mut() {
				if net.ready() {
					net.send_dig_start(hit.cell);
				}
			}
			// A scoop is a swing, not a mining hold: one stroke, no
			// grinding loop. (Water has no sound material of its own, so
			// the loop resolved to `Material::Earth` and collecting a
			// spring played the dirt-digging sample.) The arc starts here,
			// with the hold it measures — its length is the scoop's, so
			// the hand comes back up exactly as the water lands in it.
			if self.water_tool_selected() {
				self.view_swing.strike();
				self.emit(crate::audio::AudioCmd::Sfx {
					sound: crate::audio::Sfx::Swim,
					pos: Some(cell_center(hit.cell)),
					gain: 0.7,
				});
			} else {
				self.emit(crate::audio::AudioCmd::LoopStart {
					id: crate::audio::LOOP_DIG,
					sound: crate::audio::dig_sfx(crate::audio::material_of(hit.block)),
					pos: Some(cell_center(hit.cell)),
					gain: 1.0,
				});
			}
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
	/// Digging either half of a door clears BOTH cells under one seq.
	fn complete_dig(&mut self, hit: RayHit) {
		let air = BlockType::Air as u16;
		let Some(prev_cell) = self.pipeline.cell_at(hit.cell.0, hit.cell.1, hit.cell.2) else {
			return;
		};
		let prev = BlockType::from_id(digblock_core::chunk::cell_id(prev_cell));
		// (cell, prev) list this dig clears — a door adds its partner.
		let mut cleared = vec![(hit.cell, prev_cell)];
		if prev.is_door() {
			let partner = digblock_core::shapes::door_partner(
				hit.cell,
				digblock_core::chunk::cell_state(prev_cell),
			);
			if let Some(pc) = self.pipeline.cell_at(partner.0, partner.1, partner.2) {
				if BlockType::from_id(digblock_core::chunk::cell_id(pc)).is_door() {
					cleared.push((partner, pc));
				}
			}
		}
		// Slabs shrink one third per completed dig (2/3 → 1/3 → Air);
		// everything else clears. Door partners always clear.
		let new_val = if prev.is_slab() {
			digblock_core::shapes::slab_shrunk(prev_cell)
		} else {
			air
		};
		if self
			.pipeline
			.set_cell(hit.cell.0, hit.cell.1, hit.cell.2, new_val)
			.is_none()
		{
			return;
		}
		for &(c, _) in &cleared[1..] {
			self.pipeline.set_cell(c.0, c.1, c.2, air);
		}
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_DIG,
		});
		// A filled bucket splashes; a broken block breaks. Water's
		// material class is Earth (the table's total fallback), which is
		// right for nothing it does.
		let tool = self.water_tool_selected();
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: if tool {
				crate::audio::Sfx::Splash
			} else {
				crate::audio::break_sfx(crate::audio::material_of(prev))
			},
			pos: Some(cell_center(hit.cell)),
			gain: if tool { 0.5 } else { 1.0 },
		});
		// No swing is started here: the arc that began with this hold is
		// running out on this very frame, and re-striking it is what used
		// to bolt a whole second stroke onto a finished scoop.
		// One credit even for the two-cell door.
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
				let entries: Vec<_> = cleared
					.iter()
					.enumerate()
					.map(|(i, &(c, p))| (c, p, if i == 0 { new_val } else { air }))
					.collect();
				let seq = net.edits.begin_multi(&entries, tick, drop.map(|d| (d, 1)));
				let slot = self.hotbar.selected_index() as u8;
				net.send_edit(seq, tick, origin, dir, kind, hit.cell, (0, 0, 0), slot);
			}
			_ => {
				for &(c, _) in &cleared {
					self.fluid.schedule_around(c.0, c.1, c.2);
				}
				// Offline, the engine IS the award authority — the same
				// table the server's dig-credit chokepoint reads.
				self.award_xp_offline(
					digblock_core::skills::Skill::Mining,
					digblock_core::skills::dig_xp(prev),
				);
			}
		}
	}

	/// Resolve a pending right-click: USE beats PLACE — a door under the
	/// interact ray toggles; otherwise placement against the plain
	/// target, gated on stock ("you can only place what you gathered").
	fn apply_place(&mut self) {
		if !std::mem::take(&mut self.click_place) {
			return;
		}
		// Use: the interact ray also finds an OPEN door's swung leaf,
		// which the plain target ray passes through.
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let pipeline = &self.pipeline;
		let interact = digblock_core::raycast::raycast_interact(origin, dir, REACH, &|x, y, z| {
			pipeline.cell_at(x, y, z)
		});
		if let Some(h) = interact {
			if h.block.is_door() {
				self.toggle_door(h);
				return;
			}
		}
		let Some(hit) = self.target else { return };
		if hit.normal == (0, 0, 0) {
			return;
		}
		let block = match self.hotbar.selected_item() {
			Some(HotbarItem::Block(b)) => b,
			// The charged bucket pours its one source. It works here, and
			// only here, because a charged bucket picks with the ORDINARY
			// ray: the fluid ray stops INSIDE a source, leaving no face
			// normal to build against, which is why right-clicking water
			// with the old collect-only tool did nothing at all.
			Some(HotbarItem::WaterTool) if self.inventory.bucket_full() => BlockType::Water,
			// An empty bucket has nothing to give. Doors above still toggle.
			Some(HotbarItem::WaterTool) | None => return,
		};
		// Stairs carry the placer-derived facing (ascending away along
		// the look); doors place as a lower+upper pair.
		use digblock_core::chunk::{cell_id, make_cell};
		use digblock_core::shapes::{
			door_state, slab_grow_normal, slab_grown, slab_place_state, stair_state,
		};
		// A slab onto a same-material slab's OPEN face — the top of a
		// floor slab, the underside of a ceiling one — grows the hit cell
		// in place (1/3 → 2/3 → base block) instead of writing the
		// neighbor. The wire still carries the canonical state-0 slab —
		// the server re-derives both the grow and the anchor from its own
		// raycast.
		let grow = block.is_slab()
			&& cell_id(hit.cell_value) == block as u16
			&& hit.normal == slab_grow_normal(hit.cell_value);
		let (target, placed_cell) = if grow {
			(hit.cell, slab_grown(hit.cell_value))
		} else {
			let target = (
				hit.cell.0 + hit.normal.0,
				hit.cell.1 + hit.normal.1,
				hit.cell.2 + hit.normal.2,
			);
			let placed = if block.is_stair() {
				make_cell(block, stair_state(quantized_facing(dir)))
			} else if block.is_door() {
				make_cell(block, door_state(quantized_facing(dir), false, false))
			} else if block.is_slab() {
				// A fresh 1/3, hanging from the ceiling it was built
				// against or sitting on anything else.
				make_cell(block, slab_place_state(hit.normal))
			} else {
				block as u16
			};
			(target, placed)
		};
		// The target must be loaded and replaceable — same core predicate the
		// server validates with, so a predicted place is never one the
		// server then rejects. `grow` skips this: it rewrites the hit cell
		// itself, which is a slab, not a free cell.
		if !grow && !self.cell_is_replaceable(target) {
			return;
		}
		// Never place into the player — the shared shape-aware contract
		// (identical to the server's InsidePlayer scan). Doors check
		// both halves.
		let mut placements = vec![(target, placed_cell)];
		if block.is_door() {
			let upper = (target.0, target.1 + 1, target.2);
			// The upper cell, same rule (mirrors the server's gate).
			if !self.cell_is_replaceable(upper) {
				return;
			}
			placements.push((
				upper,
				make_cell(block, door_state(quantized_facing(dir), false, true)),
			));
		}
		if placements
			.iter()
			.any(|&(c, v)| digblock_core::player::placement_blocked(self.body_pos(), c, v))
		{
			return;
		}
		// Stock gate BEFORE the world mutation: no stock, no prediction.
		// One debit covers a door's two cells.
		if !self.inventory.try_take(block, 1) {
			return;
		}
		let mut entries = Vec::new();
		for &(c, v) in &placements {
			match self.pipeline.set_cell(c.0, c.1, c.2, v) {
				Some(prev) => entries.push((c, prev, v)),
				None if entries.is_empty() => {
					self.inventory.add(block, 1); // refund: nothing happened
					return;
				}
				None => {}
			}
		}
		// A poured source splashes; everything else lands with a thunk.
		let poured = block == BlockType::Water;
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: if poured {
				crate::audio::Sfx::Splash
			} else {
				crate::audio::Sfx::Place
			},
			pos: Some(cell_center(target)),
			gain: if poured { 0.6 } else { 0.9 },
		});
		match self.net.as_mut() {
			Some(net) if net.ready() => {
				let tick = net.predictor.current_tick();
				let seq = net.edits.begin_multi(&entries, tick, Some((block, -1)));
				// Slabs always travel as the canonical state-0 cell: the
				// server derives the grow AND the anchor from the normal
				// it verified, so neither is a state a client can assert.
				let wire_cell = if grow || block.is_slab() {
					block as u16
				} else {
					placed_cell
				};
				net.send_edit(
					seq,
					tick,
					origin,
					dir,
					digblock_net::messages::EditKind::Place { block: wire_cell },
					hit.cell,
					hit.normal,
					self.hotbar.selected_index() as u8,
				);
			}
			_ => {
				for &(c, _, _) in &entries {
					self.fluid.schedule_around(c.0, c.1, c.2);
				}
			}
		}
	}

	/// Predict a door toggle on both halves and send the Interact (the
	/// server re-raycasts and flips authoritatively). Offline it IS the
	/// authority. Never consumes stock.
	fn toggle_door(&mut self, hit: RayHit) {
		use digblock_core::chunk::{cell_id, cell_state};
		use digblock_core::shapes::{door_partner, door_toggled};
		let cell = hit.cell;
		let Some(cur) = self.pipeline.cell_at(cell.0, cell.1, cell.2) else {
			return;
		};
		if !BlockType::from_id(cell_id(cur)).is_door() {
			return;
		}
		let partner = door_partner(cell, cell_state(cur));
		let mut entries = vec![(cell, cur, door_toggled(cur))];
		if let Some(pc) = self.pipeline.cell_at(partner.0, partner.1, partner.2) {
			if BlockType::from_id(cell_id(pc)).is_door() {
				entries.push((partner, pc, door_toggled(pc)));
			}
		}
		// Closing must not trap anyone — locally gate on ourselves; the
		// server gates on every player.
		let closing = digblock_core::chunk::cell_state(cur) & 0b0100 != 0;
		if closing
			&& entries
				.iter()
				.any(|&(c, _, v)| digblock_core::player::placement_blocked(self.body_pos(), c, v))
		{
			return;
		}
		for &(c, _, v) in &entries {
			self.pipeline.set_cell(c.0, c.1, c.2, v);
		}
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: crate::audio::Sfx::Place,
			pos: Some(cell_center(cell)),
			gain: 0.6,
		});
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		if let Some(net) = self.net.as_mut() {
			if net.ready() {
				let tick = net.predictor.current_tick();
				let seq = net.edits.begin_multi(&entries, tick, None);
				net.send_edit(
					seq,
					tick,
					origin,
					dir,
					digblock_net::messages::EditKind::Interact,
					hit.cell,
					hit.normal,
					self.hotbar.selected_index() as u8,
				);
			}
		}
	}

	/// Apply one inventory-screen drop to the hotbar/grid pair, then
	/// reconcile so a moved type never shows in two places. The per-frame
	/// layout sync mirrors the result to the server before the next edit.
	fn apply_inv_action(&mut self, act: InvAction) {
		use digblock_core::hotbar::HotbarItem;
		match act {
			InvAction::Assign { slot, block } => {
				let _ = self.hotbar.assign(slot, block);
			}
			InvAction::SwapBar { a, b } => self.hotbar.swap(a, b),
			InvAction::PlaceGrid {
				bar_slot,
				grid_slot,
			} => {
				if let Some(Some(HotbarItem::Block(b))) = self.hotbar.slots().get(bar_slot).copied()
				{
					let occupant = self.inv_grid.get(grid_slot);
					if self.hotbar.clear_slot(bar_slot) {
						self.inv_grid.place(grid_slot, b);
						// The displaced occupant takes the vacated bar slot.
						if let Some(x) = occupant {
							let _ = self.hotbar.assign(bar_slot, x);
						}
					}
				}
			}
			InvAction::MoveGrid { from, to } => self.inv_grid.swap(from, to),
			InvAction::ClearBar { slot } => {
				if let Some(Some(HotbarItem::Block(b))) = self.hotbar.slots().get(slot).copied() {
					if self.hotbar.clear_slot(slot) {
						self.inv_grid.file(b);
					}
				}
			}
		}
		digblock_core::inv_grid::reconcile_layout(
			&self.inventory,
			&mut self.hotbar,
			&mut self.inv_grid,
		);
	}

	/// Apply an alchemy action: offline it IS the authority (the same
	/// `apply_alchemy` rule the server runs); online it goes to the
	/// server un-predicted — the `InventoryFull` echo is the rebase. The
	/// screen already clamped to affordable counts, so online sends are
	/// never no-ops.
	fn apply_alchemy_action(&mut self, act: crate::ui::screens::alchemy::AlchAction) {
		use crate::ui::screens::alchemy::AlchAction;
		use digblock_core::inventory::{apply_alchemy, AlchemyOp};
		let (core_op, wire_op) = match act {
			AlchAction::Convert { block, count } => (
				AlchemyOp::Convert { block, count },
				digblock_net::messages::AlchemyOp::Convert {
					block: block as u16,
					count,
				},
			),
			AlchAction::Craft { item, count } => (
				AlchemyOp::Craft { item, count },
				digblock_net::messages::AlchemyOp::Craft {
					item: item as u16,
					count,
				},
			),
		};
		match self.net.as_mut() {
			Some(net) if net.ready() => net.send_alchemy(wire_op),
			_ => {
				let level = self.skills.level(digblock_core::skills::Skill::Alchemy);
				if apply_alchemy(&mut self.inventory, core_op, level) {
					if let AlchAction::Craft { item, .. } = act {
						self.hotbar.auto_assign(item);
					}
					self.award_xp_offline(
						digblock_core::skills::Skill::Alchemy,
						digblock_core::skills::alchemy_xp(&core_op),
					);
				}
			}
		}
	}

	/// Adopt a new authoritative XP total for one skill: coalesce the
	/// delta into a toast, and word + sound a level-up. Totals only
	/// grow, so a replayed or reordered frame is a no-op. Both award
	/// paths land here — the server's `XpGain` online, and
	/// [`Self::award_xp_offline`] offline — so the presentation can't
	/// drift between the two.
	fn apply_xp_total(&mut self, skill: digblock_core::skills::Skill, total: u64) {
		let old = self.skills.xp(skill);
		if total <= old {
			return;
		}
		self.skills.set_total(skill, total);
		let delta = total - old;
		match self.xp_toasts.iter_mut().find(|t| t.skill == skill) {
			Some(t) => {
				t.amount += delta;
				t.ttl = crate::ui::hud::XP_TOAST_TTL;
			}
			None => self.xp_toasts.push(crate::ui::hud::XpToast {
				skill,
				amount: delta,
				ttl: crate::ui::hud::XP_TOAST_TTL,
			}),
		}
		let (old_level, new_level) = (
			digblock_core::skills::level_for_xp(old),
			digblock_core::skills::level_for_xp(total),
		);
		if new_level > old_level {
			self.push_feed(format!("{} is now level {new_level}.", skill.display()));
			self.emit(crate::audio::AudioCmd::Sfx {
				sound: crate::audio::Sfx::Place,
				pos: None,
				gain: 0.9,
			});
		}
	}

	/// Offline XP award, through the same core award tables the server
	/// runs. Online this is never called — the server's `XpGain` is the
	/// only award path (XP is not client-predicted).
	fn award_xp_offline(&mut self, skill: digblock_core::skills::Skill, amount: u64) {
		if amount > 0 {
			let total = self.skills.xp(skill).saturating_add(amount);
			self.apply_xp_total(skill, total);
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

	/// Crosshair target for the highlight overlay: the cell, the box the
	/// outline wraps (the shape's own, so a slab or a door is boxed at
	/// its own size), and dig progress — `None` = idle outline.
	///
	/// The water tool never reports progress, in either state, so the
	/// outline stays but the crack web never appears. Empty, it marks the
	/// source the scoop will take — chipping a block apart is not what is
	/// happening, and the swinging hand is the feedback instead. Charged,
	/// it marks the ordinary block the pour will land against, which is
	/// why the outline survives `diggable` saying no.
	pub(crate) fn dig_progress(&self) -> Option<crate::highlight::HighlightTarget> {
		let hit = self.target.as_ref()?;
		let boxed =
			|progress| crate::highlight::HighlightTarget::new(hit.cell, hit.cell_value, progress);
		if self.water_tool_selected() {
			return (self.inventory.bucket_full() || self.diggable(hit)).then(|| boxed(None));
		}
		if !self.diggable(hit) {
			return None;
		}
		let progress = self
			.dig
			.as_ref()
			.filter(|d| d.cell == hit.cell)
			.map(|d| (d.ticks as f32 / d.required.max(1) as f32).min(1.0));
		Some(boxed(progress))
	}

	/// This frame's held item, if the hand has anything in it. The pose
	/// is authored in view space by `viewmodel.rs`, so all this does is
	/// pick the item and hand over the frame's light rig.
	fn viewmodel_frame(&self, frame: &FrameUniforms) -> Option<crate::viewmodel::ViewmodelFrame> {
		// The SWAP's item, not the hotbar's: mid-swap the hand is still
		// holding the one it hasn't put away yet.
		let kind = self.view_swap.shown()?;
		// The light rig is world-space; the item lives in view space, so
		// rotate the direction through the same view matrix the scene
		// used. Translation is irrelevant for a direction.
		let view = self.camera.view();
		let d = frame.light_dir;
		let light_dir_view = view.transform_vector3(Vec3::new(d[0], d[1], d[2]));
		Some(crate::viewmodel::ViewmodelFrame {
			kind,
			pose: [
				self.view_bob.pose(),
				self.view_swing.pose(),
				self.view_swap.pose(),
			]
			.into_iter()
			.fold(Default::default(), crate::viewmodel::combine),
			light_dir_view,
			light_strength: d[3],
			ambient: [frame.ambient[0], frame.ambient[1], frame.ambient[2]],
			light_color: [
				frame.light_color[0],
				frame.light_color[1],
				frame.light_color[2],
			],
		})
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
	/// top (water.wgsl sinks a corner d/9) — without the offset the underwater
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
		self.recompute_ui_scale();
		self.build_renderer(gpu, surface_format);
		log::info!(
			"engine init: draw_dist={} shadow={}px format={surface_format:?}",
			self.config.draw_dist,
			self.config.shadow_size
		);
	}

	fn resize(&mut self, _gpu: &Gpu, size: (u32, u32)) {
		self.size = size;
		self.recompute_ui_scale();
	}

	fn set_scale_factor(&mut self, scale_factor: f32) {
		self.scale_factor = scale_factor;
		self.recompute_ui_scale();
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
			// The menu can surface underneath the inventory (disconnect,
			// leave-server) — the screens yield rather than stacking.
			self.inv_screen.close();
			self.alch_screen.close();
			self.char_screen.close();
			if matches!(event, InputEvent::FocusLost | InputEvent::PointerLockLost) {
				self.keys = HeldKeys::default();
				self.touch.clear();
			} else {
				self.menu.handle_input(&event, self.size, self.ui_scale);
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
					self.touch.clear();
					self.dig_held = false;
					self.inv_screen.cancel_drag();
				}
				InputEvent::Key {
					key: Key::I | Key::Escape,
					pressed: true,
				} => self.inv_screen.close(),
				_ => {
					let bar = crate::hotbar::HotbarView::build(&self.hotbar, &self.inventory);
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
					self.touch.clear();
					self.dig_held = false;
					let bar = crate::hotbar::HotbarView::build(&self.hotbar, &self.inventory);
					let view =
						crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
					self.alch_screen.handle_input(
						&InputEvent::FocusLost,
						&self.ui_hits,
						&crate::ui::Style::new(self.ui_scale),
						&view,
						&self.inv_grid,
						&bar,
					);
				}
				InputEvent::Key {
					key: Key::K | Key::Escape,
					pressed: true,
				} => self.alch_screen.close(),
				_ => {
					let bar = crate::hotbar::HotbarView::build(&self.hotbar, &self.inventory);
					let view =
						crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
					let style = crate::ui::Style::new(self.ui_scale);
					let act = self.alch_screen.handle_input(
						&event,
						&self.ui_hits,
						&style,
						&view,
						&self.inv_grid,
						&bar,
					);
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
					self.touch.clear();
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

	fn frame_started(&mut self) {
		self.frame_start = Some(web_time::Instant::now());
	}

	fn frame_acquired(&mut self, acquire_wait: Duration) {
		self.pending.acquire_ms = acquire_wait.as_secs_f32() * 1000.0;
	}

	fn update(&mut self, dt: Duration) {
		let t_update = web_time::Instant::now();
		let dt = dt.as_secs_f32();
		self.time_secs = (self.time_secs + dt) % 600.0;
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
		for intent in self.menu.take_world_intents() {
			self.handle_world_intent(intent);
		}
		if self.menu.take_disconnect() && self.net.take().is_some() {
			self.pending_ack = None;
			self.correction_offset = Vec3::ZERO;
			self.my_player_id = None;
			self.push_feed("Left the server.".into());
			// Without a store there is no save list to return to, so a
			// store-less run keeps playing the server's seed offline
			// rather than dropping into a menu it can't leave.
			if self.world_store.is_some() {
				self.return_to_title();
			}
		}
		// The local-world counterpart: flush the save first, then take the
		// same road back to the title. Unconditional — a store-less
		// ephemeral "Play Local" world has a title menu to go back to.
		if self.menu.take_leave_world() {
			self.close_local_save();
			self.push_feed("Left the world.".into());
			self.return_to_title();
		}
		// Return to spawn: one shared 60s cooldown gates the menu row.
		// Connected, the server teleports us (Teleported lands in
		// pump_net); local play resolves and lands immediately.
		self.spawn_cooldown = (self.spawn_cooldown - dt).max(0.0);
		self.menu
			.set_spawn_cooldown(self.spawn_cooldown.ceil() as u32);
		// The menu's Inventory/Alchemy rows: the menu closed itself; the
		// screen takes over the free cursor seamlessly.
		if self.menu.take_inventory() {
			self.inv_screen.open();
		}
		if self.menu.take_alchemy() {
			self.alch_screen.open();
		}
		if self.menu.take_character() {
			self.char_screen.open();
		}
		let spawn_requested =
			self.menu.take_return_spawn() | std::mem::take(&mut self.return_spawn_requested);
		if spawn_requested && self.spawn_cooldown == 0.0 {
			self.spawn_cooldown = digblock_net::protocol::RETURN_SPAWN_COOLDOWN_SECS as f32;
			// Set before the arms: it is what the teleport that lands —
			// this frame locally, or a round trip later — words itself by.
			self.spawn_return_pending = true;
			match self.net.as_mut() {
				Some(net) if net.ready() => net.send_return_to_spawn(),
				_ => {
					// Same rule the server runs: relocate sideways out of
					// anything dug into the spawn, never descend into it.
					// The overlay — not `pipeline.cell_at` — is the edit
					// view: the pipeline answers None for unloaded columns,
					// which the support test would read as void.
					let pos = self.local_spawn();
					self.apply_teleport(pos, Vec3::ZERO);
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
		if let Some(s) = self.menu.take_settings_change() {
			self.apply_settings(s);
		}

		let t_net = web_time::Instant::now();
		self.pump_net(dt);
		self.pending.net_ms = ms_since(t_net);
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
		if let Some(net) = self.net.as_mut() {
			if net.ready() {
				net.predictor.active_slot = self.hotbar.selected_index() as u8;
				// First send only after the join-sync `ServerMsg::Layout`
				// primed the dedup — reporting a pre-sync default would
				// overwrite the identity's stored arrangement.
				if let Some((last_bar, last_grid)) = self.last_layout_sent.as_ref() {
					// `to_wire` returns arrays, so the comparison is free;
					// only an actual change pays for the Vecs.
					let bar = self.hotbar.to_wire();
					let grid = self.inv_grid.to_wire();
					if last_bar.as_slice() != bar.as_slice()
						|| last_grid.as_slice() != grid.as_slice()
					{
						net.send_layout(&bar, &grid);
						self.last_layout_sent = Some((bar.to_vec(), grid.to_vec()));
					}
				}
			}
		}
		// Expired messages are kept, not dropped: the chat box shows recent
		// history while you type, and there is nothing to show if the only
		// copy was thrown away the moment it faded. `feed` is capped at
		// FEED_MAX either way, so this is a fixed-size ring.
		for m in &mut self.feed {
			m.1 = (m.1 - dt).max(FEED_TTL_FLOOR);
		}
		// XP toasts fade out and drop (unlike the feed, nothing rereads
		// an expired toast).
		for t in &mut self.xp_toasts {
			t.ttl -= dt;
		}
		self.xp_toasts.retain(|t| t.ttl > 0.0);
		// The bench drip: while the extraction slot is occupied, each
		// completed cycle sends one ordinary single-block Convert (the
		// server paces the same rhythm with its extraction bucket).
		if self.alch_screen.is_open() {
			let level = self.skills.level(digblock_core::skills::Skill::Alchemy);
			if let Some(act) = self.alch_screen.tick(dt, level, &self.inventory) {
				self.apply_alchemy_action(act);
			}
		}
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
		let t_sim = web_time::Instant::now();
		if self.net.is_none() {
			use digblock_core::fluid::{FLOW_BUDGET, FLOW_STEP_SECS};
			self.flow_accum = (self.flow_accum + dt).min(1.0);
			while self.flow_accum >= FLOW_STEP_SECS {
				self.flow_accum -= FLOW_STEP_SECS;
				let mut world = PipelineWorld(&mut self.pipeline);
				self.fluid.step(&mut world, FLOW_BUDGET);
			}
		}
		self.pending.sim_ms = ms_since(t_sim);

		// Named-save persistence, after every world mutation this frame
		// (dig, place, fluid): forward recorded edits, throttle player
		// snapshots, and give the store its bounded upkeep step. The
		// backdrop world's recordings are drained and dropped.
		let t_persist = web_time::Instant::now();
		let recorded = self.pipeline.take_recorded_edits();
		if let Some(store) = self.world_store.as_mut() {
			if self.in_local_save && self.net.is_none() {
				for (x, y, z, cell) in recorded {
					store.append_edit(x, y, z, cell);
				}
				self.player_save_accum += dt;
				if self.player_save_accum >= 5.0 {
					self.player_save_accum = 0.0;
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
		self.pending.persist_ms = ms_since(t_persist);

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
			if !column_loaded {
				self.tick_accum = 0.0;
				// Zeroing the accumulator parks the lerp at alpha = 0, which
				// would draw a whole tick BACKWARDS; hold in place instead.
				self.tick_lerp.reset(self.player.pos);
			}
			match self.net.as_mut() {
				_ if !column_loaded => {}
				Some(net) if net.ready() => {
					// Reconcile against the latest snapshot before predicting
					// this frame's ticks; corrections land as a decaying
					// visual offset, not a camera snap.
					if let Some((tick, state)) = self.pending_ack.take() {
						use digblock_net::predict::Reconciled;
						let outcome =
							net.predictor
								.reconcile(&mut self.player, tick, state, &cell_at);
						if let Reconciled::Replayed { error } = outcome {
							self.correction_offset += error;
							// The rewind moved the player by -error; the lerp
							// anchor has to travel with it or the next frame
							// interpolates from the pre-rewind position and
							// re-injects the jump the offset exists to hide.
							self.tick_lerp.rewind(error);
							self.net_corrections += 1;
							self.net_error_accum += error.length();
						}
						// The ack that drove this reconcile is also the RTT
						// sample — the overlay's ping comes from here.
						net.note_reconciled(outcome);
					}
					while self.tick_accum >= TICK {
						self.tick_accum -= TICK;
						self.tick_lerp.before_tick(self.player.pos);
						net.predictor
							.predict(&mut self.player, intent, dir, &cell_at);
					}
					net.send_inputs();
				}
				_ => {
					while self.tick_accum >= TICK {
						self.tick_accum -= TICK;
						self.tick_lerp.before_tick(self.player.pos);
						self.player.tick(TICK, dir, intent, &cell_at);
					}
				}
			}
			// ~200 ms exponential decay; snapped off below a millimeter.
			self.correction_offset *= (-dt / 0.2).exp();
			if self.correction_offset.length_squared() < 1e-6 {
				self.correction_offset = Vec3::ZERO;
			}
			// Presentation only: `player.pos` stays the authority for
			// occupancy, the wire, and the save.
			self.camera.pos = self.tick_lerp.render_pos(self.player.pos, self.tick_accum)
				+ self.correction_offset;
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
		// Held-item animation and the audio both read this frame's
		// movement, so they sit together and `prev_player_pos` advances
		// once, after both — `step_audio` returns early on a silent run
		// and would otherwise starve the bob of every frame's delta.
		let moved = self.player.pos - self.prev_player_pos;
		self.view_bob.advance(
			Vec3::new(moved.x, 0.0, moved.z).length(),
			dt,
			self.ground_below().1,
		);
		// The scoop's arc is started by the hold that opens it, in
		// `step_dig` — one stroke per bucket, so there is nothing to
		// re-arm here.
		self.view_swing.advance(dt);
		let selected = self
			.hotbar
			.selected_item()
			.map(|i| crate::ui::hud::item_icon(i, self.inventory.bucket_full()).0);
		self.view_swap.advance(dt, selected);
		self.step_audio(dt);
		self.prev_player_pos = self.player.pos;
		self.pending.update_ms = ms_since(t_update);
	}

	fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32)) {
		self.size = size;
		self.recompute_ui_scale();
		// Streaming happens here (not update) because uploads need the GPU.
		self.pump_pipeline(gpu);
		// Everything from here to the encode — remote sampling, name tags,
		// menu/UI rebuild, frame uniforms — used to be untimed, so a spike
		// in any of it showed up in `total_ms` and in no phase at all.
		self.prep_start = Some(web_time::Instant::now());
		// Remote players, sampled ~100 ms in the past between snapshots;
		// the instance list rides FrameScene and re-uploads every frame.
		self.remote_instances.clear();
		if let Some(net) = self.net.as_ref() {
			net.remotes.sample(&mut self.remote_scratch);
			for d in &self.remote_scratch {
				let c = player_color(d.id);
				self.remote_instances
					.push([d.pos.x, d.pos.y, d.pos.z, d.yaw, c[0], c[1], c[2], 0.0]);
				// Held-item companion: a small textured cube in hand
				// (instance mode = texture layer + 1; empty hand = none).
				if let Some(layer) = held_tex_layer(d.held) {
					self.remote_instances.push([
						d.pos.x,
						d.pos.y,
						d.pos.z,
						d.yaw,
						1.0,
						1.0,
						1.0,
						layer as f32 + 1.0,
					]);
				}
			}
		}
		// DIG_PLAYERBOX=1: park a fake remote player 5 blocks ahead —
		// a GPU-path check that needs no second client (held cube: dirt).
		if self.config.debug_playerbox {
			let p = self.camera.pos + self.camera.dir() * 5.0;
			let c = player_color(2);
			self.remote_instances
				.push([p.x, p.y, p.z, 0.8, c[0], c[1], c[2], 0.0]);
			let layer = BlockType::Dirt.props().tex_side as f32;
			self.remote_instances
				.push([p.x, p.y, p.z, 0.8, 1.0, 1.0, 1.0, layer + 1.0]);
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
		// Entries past `tags` keep their String buffers for next frame; the
		// live slice is `[..tags]`.
		let mut tags = 0usize;
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
			// Reuse the slot's String rather than allocating a fresh one
			// per visible player per frame.
			if tags == self.nametag_scratch.len() {
				self.nametag_scratch.push(crate::renderer::NameTag {
					x: 0.0,
					y: 0.0,
					color: 0,
					text: String::new(),
				});
			}
			let tag = &mut self.nametag_scratch[tags];
			tag.x = (ndc.x * 0.5 + 0.5) * size.0 as f32;
			tag.y = (1.0 - (ndc.y * 0.5 + 0.5)) * size.1 as f32;
			tag.color = pack_rgb(player_color(d.id));
			tag.text.clear();
			match self.player_names.get(&d.id) {
				Some(n) => tag.text.push_str(n),
				None => {
					use std::fmt::Write;
					let _ = write!(tag.text, "player {}", d.id);
				}
			}
			tags += 1;
		}
		self.nametag_count = tags;
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
		let menu_view = self.menu.is_open().then(|| {
			self.menu
				.view(size, self.ui_scale, (self.time_secs * 2.0) as u32 % 2 == 0)
		});
		// HUD state gates with the crosshair: hidden under the menu and in
		// bench captures' default state comparability is preserved (bench
		// never digs, hotbar renders its fixed default row).
		let in_game = menu_view.is_none();
		self.ui_dl.clear();
		self.ui_hits.clear();
		// The menus' full-screen dim fades with the world's own darkness
		// (night scenes shouldn't be double-darkened); the sample is pure
		// and cheap, so re-deriving it here beats plumbing it out of
		// frame_uniforms.
		let daylight =
			1.0 - digblock_core::sky::Atmosphere::sample(self.sun.elevation()).star_intensity;
		if let Some(view) = &menu_view {
			view.emit(
				&mut self.ui_dl,
				&crate::ui::Style::new(self.ui_scale),
				size,
				daylight,
			);
		}
		if in_game {
			let hotbar_view = crate::hotbar::HotbarView::build(&self.hotbar, &self.inventory);
			let mut ui = crate::ui::Ui {
				dl: &mut self.ui_dl,
				hits: &mut self.ui_hits,
				style: crate::ui::Style::new(self.ui_scale),
			};
			if self.inv_screen.is_open() {
				// Screen first: its dim/panel sit under the bar's quads.
				self.inv_screen
					.build(&mut ui, size, &self.inv_grid, &self.inventory, daylight);
			} else if self.alch_screen.is_open() {
				let view =
					crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
				self.alch_screen.build(
					&mut ui,
					size,
					&view,
					&self.inv_grid,
					&self.inventory,
					daylight,
				);
			} else if self.char_screen.is_open() {
				let view = crate::ui::screens::character::CharView::build(&self.skills);
				self.char_screen.build(&mut ui, size, &view, daylight);
			} else {
				crate::ui::hud::crosshair(ui.dl, size, ui.style.scale);
			}
			crate::ui::hud::hotbar(&mut ui, size, &hotbar_view, |i| {
				crate::ui::InvId::Bar(i as u8)
			});
			crate::ui::hud::xp_toasts(ui.dl, &ui.style, size, &self.xp_toasts);
		}
		let highlight_target = if in_game { self.dig_progress() } else { None };
		// The hand is part of playing, not part of reading a screen: a
		// menu, the inventory or the alchemy page all put it away.
		let viewmodel = (in_game
			&& !self.inv_screen.is_open()
			&& !self.alch_screen.is_open()
			&& !self.char_screen.is_open())
		.then(|| self.viewmodel_frame(&frame))
		.flatten();
		self.pending.prep_ms = self.prep_start.take().map(ms_since).unwrap_or_default();
		if let Some(r) = self.renderer.as_mut() {
			let t_encode = web_time::Instant::now();
			let scene = FrameScene {
				uniforms: frame,
				view_proj,
				near_vp,
				wide_vp,
				wide_due,
				size,
				ui_scale: self.ui_scale,
				players: &self.remote_instances,
				overlay: overlay_stats
					.as_ref()
					.map(|s| (s, self.history_scratch.as_slice())),
				feed: &self.feed,
				target: highlight_target,
				viewmodel,
				ui: &self.ui_dl,
				chat: (in_game && self.chat.is_open())
					.then(|| self.chat.view((self.time_secs * 2.0) as u32 % 2 == 0)),
				nametags: &self.nametag_scratch[..self.nametag_count],
			};
			let stats = r.render(gpu, target, &scene);
			self.pending.encode_ms = ms_since(t_encode);
			self.metrics.render = stats;
			self.metrics.gpu = r.gpu_pass_ms();
			self.metrics.gpu_buffer_bytes = r.gpu_buffer_bytes;
		}
		// Occupancy: the whole handler span minus the vsync wait that blocks
		// inside it on native (and is ~0 on the web). Closed here rather
		// than by the platform after `render` so it reaches this frame's
		// record instead of the next one's.
		self.pending.busy_ms = self
			.frame_start
			.take()
			.map(|t| (ms_since(t) - self.pending.acquire_ms).max(0.0))
			.unwrap_or_default();
		// total_ms is measured inside frame() from the frame-to-frame delta.
		self.metrics.frame(std::mem::take(&mut self.pending));
	}

	fn wants_cursor_grab(&self) -> bool {
		!self.menu.is_open()
			&& !self.inv_screen.is_open()
			&& !self.alch_screen.is_open()
			&& !self.char_screen.is_open()
	}

	fn wants_text_input(&self) -> bool {
		self.chat.is_open()
	}

	fn on_suspend(&mut self) {
		log::info!("engine: on_suspend (state save point)");
		self.metrics.pause();
		self.stream_budget.reset();
		// One cheap synchronous send — honors the on_suspend contract.
		self.emit(crate::audio::AudioCmd::Pause);
		// Named-save save-point: drain any same-frame edits, snapshot the
		// player, and flush — all synchronous by design (file fsync /
		// localStorage put), honoring the cheap-and-synchronous contract.
		let recorded = self.pipeline.take_recorded_edits();
		if let Some(store) = self.world_store.as_mut() {
			if self.in_local_save {
				for (x, y, z, cell) in recorded {
					store.append_edit(x, y, z, cell);
				}
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
			store.flush(self.pipeline.overlay());
		}
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

#[cfg(test)]
mod store_tests {
	use super::*;
	use std::cell::RefCell;
	use std::rc::Rc;

	/// Records WorldStore calls; open returns a save with one edit and a
	/// player position.
	#[derive(Default)]
	struct Calls {
		appended: Vec<(i32, i32, i32, u16)>,
		saved_player: u32,
		saved_hotbar: Vec<u16>,
		/// Arrangement `open_world` hands back (empty = legacy save).
		open_hotbar: Vec<u16>,
		maintained: u32,
		flushed: u32,
		closed: u32,
	}

	struct MockStore(Rc<RefCell<Calls>>);

	impl digblock_persist::WorldStore for MockStore {
		fn list_worlds(&mut self) -> Vec<digblock_persist::WorldMeta> {
			Vec::new()
		}
		fn create_world(
			&mut self,
			name: &str,
			seed: f64,
		) -> Result<digblock_persist::WorldMeta, String> {
			Ok(digblock_persist::WorldMeta {
				id: "w1".into(),
				name: name.into(),
				seed,
				created_ms: 0,
				last_played_ms: 0,
				compatible: true,
			})
		}
		fn delete_world(&mut self, _id: &str) {}
		fn open_world(&mut self, id: &str) -> Result<digblock_persist::WorldSave, String> {
			Ok(digblock_persist::WorldSave {
				meta: digblock_persist::WorldMeta {
					id: id.into(),
					name: "W".into(),
					seed: 7.0,
					created_ms: 0,
					last_played_ms: 0,
					compatible: true,
				},
				edits: vec![((1, 100, 1), 0)],
				inventory: vec![(3, 5)],
				hotbar: self.0.borrow().open_hotbar.clone(),
				grid: Vec::new(),
				skills: Vec::new(),
				player: Some(digblock_persist::PlayerPersist {
					pos: [1.5, 140.0, 2.5],
					yaw: 90.0,
					pitch: -10.0,
				}),
			})
		}
		fn append_edit(&mut self, x: i32, y: i32, z: i32, cell: u16) {
			self.0.borrow_mut().appended.push((x, y, z, cell));
		}
		fn save_player(
			&mut self,
			_inventory: &[(u16, u32)],
			hotbar: &[u16],
			_grid: &[u16],
			_skills: &[(u8, u64)],
			_player: &digblock_persist::PlayerPersist,
		) {
			let mut state = self.0.borrow_mut();
			state.saved_player += 1;
			state.saved_hotbar = hotbar.to_vec();
		}
		fn maintain(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {
			self.0.borrow_mut().maintained += 1;
		}
		fn flush(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {
			self.0.borrow_mut().flushed += 1;
		}
		fn close_world(&mut self) {
			self.0.borrow_mut().closed += 1;
		}
	}

	#[test]
	fn create_enter_edits_and_snapshots_flow_to_store() {
		let calls = Rc::new(RefCell::new(Calls::default()));
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0; // deterministic inline executor
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);
		engine.set_world_store(Box::new(MockStore(calls.clone())));

		engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
		assert!(engine.in_local_save, "entered the created world");
		assert_eq!(engine.seed, 7.0, "world seed from the save");
		assert_eq!(
			engine.player.pos.to_array(),
			[1.5, 140.0, 2.5],
			"restored position"
		);
		assert!(engine.pipeline.overlay().is_some(), "overlay installed");

		// Headless: pump_pipeline (render path) never runs, so generate
		// the world directly until the edit's chunk is Ready.
		let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
		for _ in 0..10_000 {
			let m = engine.pipeline.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}

		// A local edit + >5s of updates: the edit reaches the store and a
		// player snapshot fires; maintain runs every frame.
		let new = if engine.pipeline.block_at(1, 101, 1) == Some(BlockType::Stone) {
			BlockType::Dirt
		} else {
			BlockType::Stone
		};
		engine
			.pipeline
			.set_block(1, 101, 1, new)
			.expect("chunk ready for the test edit");
		for _ in 0..400 {
			engine.update(Duration::from_millis(16));
		}
		let c = calls.borrow();
		assert!(c.maintained > 300, "store maintained per frame");
		assert!(c.saved_player >= 1, "player snapshot fired after 5s");
		assert!(
			c.appended.contains(&(1, 101, 1, new as u16)),
			"edit forwarded: {:?}",
			c.appended
		);
		drop(c);

		// Suspend: final snapshot + flush, synchronously.
		engine.on_suspend();
		let c = calls.borrow();
		assert!(c.flushed >= 1, "suspend flushes");
		assert!(c.saved_player >= 2, "suspend snapshots the player");
	}

	#[test]
	fn saved_hotbar_arrangement_restores_and_round_trips() {
		use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS, WIRE_EMPTY, WIRE_TOOL};

		let calls = Rc::new(RefCell::new(Calls::default()));
		// Custom arrangement: tool moved to slot 0, the inventory's block
		// (id 3) parked in slot 4.
		let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		wire[0] = WIRE_TOOL;
		wire[4] = 3;
		calls.borrow_mut().open_hotbar = wire.to_vec();

		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);
		engine.set_world_store(Box::new(MockStore(calls.clone())));
		engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });

		let b3 = BlockType::try_from_id(3).unwrap();
		assert_eq!(engine.hotbar.slots()[0], Some(HotbarItem::WaterTool));
		assert_eq!(engine.hotbar.slots()[4], Some(HotbarItem::Block(b3)));
		assert_eq!(
			engine.hotbar.slots()[HOTBAR_SLOTS - 1],
			None,
			"auto-assign respects the saved layout (id 3 already placed)"
		);

		// Suspend hands the same arrangement back to the store.
		engine.on_suspend();
		assert_eq!(calls.borrow().saved_hotbar, wire.to_vec());
	}

	/// The walk sim holds while the player's column is ungenerated —
	/// unloaded cells are non-solid, so ticking would free-fall the
	/// player through the world (the web underground-spawn bug).
	#[test]
	fn walk_sim_holds_until_player_column_generates() {
		let calls = Rc::new(RefCell::new(Calls::default()));
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);
		engine.set_world_store(Box::new(MockStore(calls)));
		// MockStore's save has seed 7.0 ≠ backdrop 3.0: the pipeline is
		// rebuilt from scratch, so nothing is generated at entry.
		engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
		let start = engine.player.pos;

		// Headless (no render pump): the world never generates — the
		// player must hold at the restored position instead of falling.
		for _ in 0..60 {
			engine.update(Duration::from_millis(16));
		}
		assert_eq!(engine.player.pos, start, "held over ungenerated world");

		// Once the world lands, the sim resumes (gravity takes over).
		let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
		for _ in 0..10_000 {
			let m = engine.pipeline.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}
		for _ in 0..60 {
			engine.update(Duration::from_millis(16));
		}
		assert!(engine.player.pos.y < start.y, "sim resumed after gen");
	}

	/// Echoes `create_world`'s seed back through `open_world` with a
	/// pristine save, the way real stores do — Create must then keep the
	/// backdrop pipeline (same seed, no edits) instead of regenerating.
	struct EchoStore(f64);

	impl digblock_persist::WorldStore for EchoStore {
		fn list_worlds(&mut self) -> Vec<digblock_persist::WorldMeta> {
			Vec::new()
		}
		fn create_world(
			&mut self,
			name: &str,
			seed: f64,
		) -> Result<digblock_persist::WorldMeta, String> {
			self.0 = seed;
			Ok(digblock_persist::WorldMeta {
				id: "w1".into(),
				name: name.into(),
				seed,
				created_ms: 0,
				last_played_ms: 0,
				compatible: true,
			})
		}
		fn delete_world(&mut self, _id: &str) {}
		fn open_world(&mut self, id: &str) -> Result<digblock_persist::WorldSave, String> {
			Ok(digblock_persist::WorldSave {
				meta: digblock_persist::WorldMeta {
					id: id.into(),
					name: "W".into(),
					seed: self.0,
					created_ms: 0,
					last_played_ms: 0,
					compatible: true,
				},
				edits: Vec::new(),
				inventory: Vec::new(),
				hotbar: Vec::new(),
				grid: Vec::new(),
				skills: Vec::new(),
				player: None,
			})
		}
		fn append_edit(&mut self, _x: i32, _y: i32, _z: i32, _cell: u16) {}
		fn save_player(
			&mut self,
			_inventory: &[(u16, u32)],
			_hotbar: &[u16],
			_grid: &[u16],
			_skills: &[(u8, u64)],
			_player: &digblock_persist::PlayerPersist,
		) {
		}
		fn maintain(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {}
		fn flush(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {}
		fn close_world(&mut self) {}
	}

	#[test]
	fn create_reuses_streamed_backdrop_world() {
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);
		engine.set_world_store(Box::new(EchoStore(0.0)));

		// Stream the backdrop in around its spawn, like the menu does.
		let spawn = digblock_core::player::spawn(3.0);
		let center = digblock_core::chunk::ChunkPos::new(
			spawn.x.floor() as i32 / 16,
			0,
			spawn.z.floor() as i32 / 16,
		);
		for _ in 0..10_000 {
			let m = engine.pipeline.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}
		let ground = (
			spawn.x.floor() as i32,
			(spawn.y - digblock_core::player::EYE_TO_FEET - 0.5).floor() as i32,
			spawn.z.floor() as i32,
		);
		assert!(engine
			.pipeline
			.cell_at(ground.0, ground.1, ground.2)
			.is_some());

		engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
		assert!(engine.in_local_save);
		assert_eq!(engine.seed, 3.0, "create adopted the backdrop seed");
		// Retained pipeline: the spawn column is queryable with no
		// further maintain — the player stands instead of waiting.
		assert!(
			engine
				.pipeline
				.cell_at(ground.0, ground.1, ground.2)
				.is_some(),
			"backdrop chunks survived world entry"
		);
		for _ in 0..60 {
			engine.update(Duration::from_millis(16));
		}
		assert!(
			(engine.player.pos.y - spawn.y).abs() < 2.0,
			"player settled at spawn, not underground (y {} vs spawn {})",
			engine.player.pos.y,
			spawn.y
		);
	}
}

#[cfg(test)]
mod water_tool_tests {
	use super::*;

	/// A generated world with the camera parked in a corridor: a water
	/// source two blocks ahead, then a gap, then a stone wall. Aiming at
	/// the source and at the wall is the same look — which ray is cast
	/// decides which one the crosshair finds.
	fn corridor() -> (Engine, (i32, i32, i32), (i32, i32, i32)) {
		let mut config = EngineConfig::desktop();
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);
		let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
		for _ in 0..10_000 {
			let m = engine.pipeline.maintain(center, usize::MAX, None, None);
			if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
				break;
			}
		}
		// Open sky well above spawn: nothing here depends on terrain, so a
		// worldgen retune can't quietly move the corridor into rock.
		let p = engine.player.pos;
		let (x, y, z) = (
			p.x.floor() as i32,
			p.y.floor() as i32 + 20,
			p.z.floor() as i32,
		);
		let (source, wall) = ((x, y, z), (x, y, z + 2));
		for c in [source, wall, (x, y, z + 1)] {
			assert_eq!(
				engine.pipeline.block_at(c.0, c.1, c.2),
				Some(BlockType::Air),
				"the corridor must start empty at {c:?}"
			);
		}
		engine
			.pipeline
			.set_block(source.0, source.1, source.2, BlockType::Water)
			.expect("chunk ready");
		engine
			.pipeline
			.set_block(wall.0, wall.1, wall.2, BlockType::Stone)
			.expect("chunk ready");
		engine.camera.pos = Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 - 2.5);
		engine.camera.yaw_deg = 0.0; // +z
		engine.camera.pitch_deg = 0.0;
		engine
			.hotbar
			.select_digit(digblock_core::hotbar::HOTBAR_SLOTS as u8);
		assert!(engine.water_tool_selected());
		(engine, source, wall)
	}

	/// The charge decides which ray is cast, and so what the crosshair
	/// is even pointing at: an empty bucket sees the source it can scoop,
	/// a charged one sees straight through it to the block it can pour
	/// against. Both outline; neither ever draws the dig crack web.
	#[test]
	fn the_charge_switches_the_ray_and_the_outline() {
		let (mut engine, source, wall) = corridor();

		engine.update_target();
		assert!(engine.scoop_ready());
		assert_eq!(engine.target.map(|h| h.cell), Some(source));
		assert!(
			engine.diggable(&engine.target.unwrap()),
			"a scoopable source"
		);
		assert_eq!(
			engine.dig_progress().map(|t| (t.cell, t.progress)),
			Some((source, None)),
			"outline, no cracks"
		);

		engine.inventory.add(BlockType::Water, 1);
		engine.update_target();
		assert!(!engine.scoop_ready());
		assert_eq!(
			engine.target.map(|h| h.cell),
			Some(wall),
			"a charged bucket picks with the ordinary ray"
		);
		assert!(
			!engine.diggable(&engine.target.unwrap()),
			"and left-click has nothing to do"
		);
		assert_eq!(
			engine.dig_progress().map(|t| (t.cell, t.progress)),
			Some((wall, None)),
			"the wall still outlines — that is where the water goes"
		);
	}

	/// Right-click pours, and only when there is something to pour. The
	/// charge is spent by the ordinary stock debit, which is what makes
	/// the pour roll back with the edit if a server refuses it.
	#[test]
	fn right_click_pours_the_charge_and_only_the_charge() {
		let (mut engine, _source, wall) = corridor();
		let target = (wall.0, wall.1, wall.2 - 1);

		// Empty: the click is consumed and nothing happens.
		engine.update_target();
		engine.click_place = true;
		engine.apply_place();
		assert_eq!(
			engine.pipeline.block_at(target.0, target.1, target.2),
			Some(BlockType::Air),
			"an empty bucket poured"
		);

		engine.inventory.add(BlockType::Water, 1);
		engine.update_target();
		engine.click_place = true;
		engine.apply_place();
		assert_eq!(
			engine.pipeline.block_at(target.0, target.1, target.2),
			Some(BlockType::Water),
			"the charged bucket poured against the wall"
		);
		assert!(!engine.inventory.bucket_full(), "and emptied itself");
	}
}
