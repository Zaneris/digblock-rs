use std::collections::VecDeque;
use std::time::Duration;

use digblock_core::BlockType;
use digblock_core::chunk::ChunkPos;
use digblock_core::hotbar::{Hotbar, HotbarItem};
use digblock_core::inventory::Inventory;
use digblock_core::metrics::{FrameTimings, Metrics, PipelineGauges};
use digblock_core::pipeline::Pipeline;
use digblock_core::player::{Player, PlayerIntent, TICK};
use digblock_core::raycast::{REACH, RayHit, raycast, raycast_fluid};
use digblock_core::sun::Sun;
use digblock_core::touch::{Phase, TouchDriver};
use digblock_platform::{Game, Gpu, InputEvent, Key, MouseButton, TouchPhase};
use glam::{Vec2, Vec3};

use crate::camera::FlyCamera;
use crate::renderer::{FrameScene, FrameUniforms, RenderConfig, Renderer};
use crate::ui::screens::inventory::InvAction;

mod config;
pub use config::{DebugScreen, EngineConfig, parse_held_item};

mod actions;
mod audio_step;
mod net_sync;
mod present;
mod progression;
mod setup;
mod streaming;
mod world_session;

mod carried;
use carried::{CARRIER_DEBUG, CARRIER_SELF, Carried, Remote, cell_center, pack_rgb, push_remote};
mod debug_plans;
pub use debug_plans::parse_node_kind;
use debug_plans::{
	debug_crack_plan, debug_dive_plan, debug_doors_plan, debug_node_plan, debug_ore_plan,
	debug_river_plan, debug_roof_plan, debug_shapes_plan, debug_torches_plan, debug_tunnel_plan,
	debug_vault_plan, quantized_facing,
};

/// The launcher-injected clipboard writer, in the same shape as
/// [`crate::settings::SettingsStore`]: the engine raises an intent, the
/// launcher owns the platform call. Write-only — nothing in this project
/// pastes, and adding a read would mean giving the game a way to see what
/// the player copied somewhere else.
pub type ClipboardSink = Box<dyn FnMut(&str)>;

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
/// streaming + render must fit inside one high-refresh vsync interval.
const STREAM_BUDGET_MS: f32 = 3.0;
/// Uploads always make this much progress per frame even with the budget
/// exhausted, so a saturated drain can never starve the upload queue.
/// Sized to a chunk stack's worth of the player's own column: the
/// guarantee only has to cover where the player IS, and the queue is
/// nearest-first, so anything beyond that is the far ring choosing to wait
/// rather than to hitch. The byte ceiling below still applies.
const UPLOAD_MIN: usize = 2;

/// The [`UPLOAD_MIN`] twin for the RTGI voxel window. Higher because a GI
/// chunk is a small fraction of a mesh's size, and because a window the
/// camera has outrun is not merely late — it traces rays against terrain
/// that is no longer there.
const GI_UPLOAD_MIN: u32 = 8;
/// Mesh bytes per frame on desktop — high enough to be a backstop against
/// pathological frames rather than a throttle on normal streaming.
const UPLOAD_BYTES_MAX_NATIVE: usize = 8 << 20;
/// …and on the web/mobile, where the staging copy lands in another
/// process at submit time and is invisible to `upload_ms`.
const UPLOAD_BYTES_MAX_CONSTRAINED: usize = 768 << 10;
/// Evicted chunks released per frame even with the budget spent, so a
/// player who keeps walking can't outrun the release queue.
const EVICT_MIN: usize = 8;
/// Share of one display interval `pump_net` may spend applying authoritative
/// edits. Each costs a copy-on-write cell write plus up to eight mesh-heap
/// pushes, so a join sync's worth of them in one frame is a visible stall;
/// spread over frames it is invisible.
///
/// A COUNT could not express that. What one edit costs swings by more than an
/// order of magnitude — whether its chunk is still shared decides a whole-chunk
/// copy-on-write — and the mesh jobs it queues are charged to this same thread
/// later in the same frame even with a Web Worker pool up, since feeding one
/// encodes and copies the padded snapshot and absorbing the result is
/// main-thread work by definition. With no pool they run here outright, and the
/// COUNT this replaced was sized against a native frame with worker threads
/// behind it. Pricing the slice in TIME makes the same code back off on the
/// platform that cannot afford it, with no constant to retune per display.
const NET_EDIT_SHARE: f32 = 0.15;
/// Edits that land before the clock is consulted at all — the guarantee
/// that a frame already over budget still drains rather than parking the
/// queue forever. Same shape as `EVICT_MIN` and the inline executor's
/// per-`maintain` floor.
const NET_EDIT_MIN: usize = 32;
/// Edits between deadline checks. `Instant::now` is `performance.now()`
/// on the web: cheap, but not free enough to call per cell.
const NET_EDIT_CHECK: usize = 32;
/// Hard ceiling however cheap the edits measure, so one frame on a fast
/// machine cannot run away with the whole queue and spend the interval
/// the render still needs. This is the count the budget replaced.
const NET_EDIT_MAX: usize = 256;

// Relationships between the four, asserted at BUILD time because they are
// arithmetic on constants: a test could only restate them. The floor must
// be reachable and under the ceiling, and the check stride must divide it
// — otherwise the first deadline read lands past the floor and the floor
// is not the floor.
const _: () = assert!(NET_EDIT_MIN > 0 && NET_EDIT_MIN <= NET_EDIT_MAX);
const _: () = assert!(NET_EDIT_MIN.is_multiple_of(NET_EDIT_CHECK));

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
	/// The primary touch currently driving UI-surface pointer synthesis
	/// (see the Touch arm at the top of `input`).
	ui_touch: Option<u64>,
	/// Touches claimed by HUD buttons (`touch_hud`), consumed to their
	/// end so a held button never becomes look or movement. A Vec, not a
	/// single slot: a place tap while the jump button is held is two
	/// simultaneous claims.
	hud_touches: Vec<(u64, crate::ui::hud::HudButton)>,
	/// The on-screen jump button is held (Button jump mode only).
	hud_jump_held: bool,
	/// The menu's Fullscreen row fired; the shell drains it right after
	/// this input dispatch, while the gesture is still on the stack.
	fullscreen_requested: bool,
	/// Shell-forwarded display cutouts the touch HUD keeps clear of.
	safe_area: crate::ui::hud::SafeArea,
	/// Walk (player physics) vs debug fly camera; F toggles.
	walking: bool,
	fly_key_down: bool,
	overlay_key_down: bool,
	/// F7 edge latch — the GI ratchet drill fires once per press.
	gi_drill_key_down: bool,
	/// Frames through `update`, for `--gi-drill-at`.
	frames_seen: u32,
	tick_accum: f32,
	sun: Sun,
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
	/// Cell a siphon draw just emptied, held until the button comes up.
	///
	/// A draw leaves the mountain standing — the cell turns to the tier's
	/// spent rock — so the very next tick the same hold is aimed at an
	/// ordinary block and used to roll straight on into quarrying it. Two
	/// actions on one press, and the second one destroys the trophy the
	/// first one earned. Drawing the NEXT seam still needs no release,
	/// because the latch names one cell and an extraction is never
	/// refused by it.
	drawn_cell: Option<(i32, i32, i32)>,
	click_place: bool,
	/// In-progress hold-to-dig, advanced at the fixed sim rate while
	/// `dig_held` stays on a valid, unchanged target.
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
	/// The tool slot's handover animation — presentation only, and it
	/// watches `hotbar.active_tool()` rather than being told, so every
	/// way the tool can change animates the same way.
	tool_swap: crate::hotbar::ToolSwap,
	/// Edit-time water flow (digblock-core fluid rules); idle unless
	/// edits schedule cells.
	fluid: digblock_core::fluid::FluidSim,
	flow_accum: f32,
	/// Shader animation clock, wrapped on a long period (carried in the
	/// otherwise unused light_color.w — no uniform layout change).
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
	/// decaying over a fraction of a second so corrections nudge instead of
	/// teleport.
	correction_offset: Vec3,
	/// Visual-only sampling of the fixed-rate player sim: the camera
	/// draws between the last two tick states instead of snapping to the
	/// newest one. Without it frame-pacing jitter IS position jitter —
	/// measurably the dominant stutter source, well ahead of anything the
	/// network does.
	tick_lerp: crate::smoothing::TickLerp,
	/// Per-frame remote-player sampling scratch.
	remote_scratch: Vec<digblock_net::remote::RemoteDraw>,
	remote_instances: Vec<[f32; 8]>,
	held_instances: Vec<(crate::ui::hud::IconKind, [f32; 8])>,
	carried: Carried,
	/// Live level-up confetti bursts, in the flat instance layout the
	/// shader reads (origin + age + seed). World state, so a world swap
	/// forgets them; the flakes' whole motion derives from these three
	/// numbers in confetti.wgsl, which is why aging them is the entire
	/// simulation.
	confetti: Vec<crate::confetti::BurstInstance>,
	/// Burst counter — the per-burst seed, so two celebrations on the
	/// same spot don't throw identical paper. Deliberately a counter
	/// rather than an RNG: nothing here needs to be unpredictable.
	confetti_seq: u32,
	/// The live essence draw, aged by `step_siphon`. World state for the
	/// same reason confetti is: it names a cell, and a world swap must
	/// forget it.
	siphon: Option<SiphonDraw>,
	/// Reconciliation health, logged on a slow cadence while connected.
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
	/// The bench summary has been emitted; a second call would report a
	/// different (fallback) line, because `bench_summary` consumes the run
	/// history. Two exits can reach it — `Game::on_exit` and `Drop` — and
	/// which one fires depends on the platform.
	bench_reported: bool,
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
	/// Scripted-flight clock (bench mode), in fixed sim ticks.
	bench_t: f32,
	/// Steps of the scripted flight already taken, against `config.fly`.
	fly_frames: u32,
	/// Where `config.walk`'s oscillation is anchored: the camera's pose at
	/// the first step, latched so `--fly` and `--warp` share it.
	walk_origin: Option<Vec3>,
	/// Meshes drained but not yet uploaded (upload budget carry-over).
	pending_uploads: std::collections::VecDeque<digblock_core::pipeline::MeshOutput>,
	/// Evicted chunks whose GPU buffers are still to be released, drained
	/// under the frame's streaming budget.
	pending_evicts: std::collections::VecDeque<ChunkPos>,
	/// Positions with a release still outstanding. Doubles as the cancel
	/// set: an upload for a queued position clears it, so a chunk that
	/// comes back before its release is reached keeps its new mesh.
	pending_evict_set: rustc_hash::FxHashSet<ChunkPos>,
	/// RTGI voxel-window re-upload queue (chunk positions from
	/// `Pipeline::take_gi_dirty`, drained under a per-frame byte
	/// budget; the set half dedupes re-dirtied entries).
	gi_queue: VecDeque<ChunkPos>,
	gi_queued: rustc_hash::FxHashSet<ChunkPos>,
	/// Camera chunk the upload queue was last ordered for.
	upload_sort_center: ChunkPos,
	size: (u32, u32),
	/// Device pixels per logical pixel (shell-reported; unscaled until told).
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
	/// Seconds left holding the world after an arrival — a join or a
	/// teleport — while the server ships the journaled columns around the
	/// destination (`ServerMsg::AreaReady`). `None` = not holding.
	///
	/// Counts DOWN rather than storing a deadline so it needs no clock of
	/// its own, and so a suspended tab resumes with its full wait rather
	/// than an expired one. Zero releases exactly as the marker does:
	/// a hold that cannot time out is worse than the artifact it removes.
	arrival_hold: Option<f32>,
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
	/// The prospector's sense's cadence (SKILLS-DESIGN §5.5).
	prospect: crate::audio::Prospect,
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
	/// Launcher-injected clipboard writer; see `set_clipboard`.
	clipboard: Option<ClipboardSink>,
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
	/// Ticks since the hold stopped advancing. Zero while the button is
	/// down and on target; counting up while `ticks` bleeds back toward
	/// untouched (`skills::dig_decayed`). `ticks` itself is never
	/// rewritten by the decay — it is the banked peak, and the decay is
	/// derived from it, so client and server compute the same number
	/// from the same two counts instead of two rounding chains.
	idle: u16,
}

impl DigProgress {
	/// Progress after the decay so far: what the crack draws, what a
	/// resumed hold inherits, and what the server's gate charges.
	fn banked(&self) -> u16 {
		digblock_core::skills::dig_decayed(
			u32::from(self.ticks),
			u32::from(self.required),
			u32::from(self.idle),
		) as u16
	}
}

/// The essence draw the siphon is running, if any — presentation only,
/// and the whole of what [`crate::siphon`] needs to draw a stream.
///
/// The geometry is kept through the fade-OUT rather than dropped with the
/// hold, which is the only reason this is a struct and not a query. A
/// completed extraction turns the cell to spent rock on the frame it
/// lands, so by the time there is anything to flare about there is no
/// vein left to ask where the motes were coming from.
#[derive(Clone, Copy)]
struct SiphonDraw {
	/// Centre of the face the motes lift off: the cell's own centre,
	/// pushed out half a block along the struck normal.
	source: Vec3,
	/// The face itself, as the raycast's unit normal.
	normal: (i32, i32, i32),
	/// The vein's own colour (`BlockType::ore_flecks`), so a tier's motes
	/// and its cast light are the same statement.
	tint: [f32; 3],
	/// How far through the hold, 0..1.
	progress: f32,
	/// Fade in, fade out, and the flare a completed draw ends on — see
	/// [`crate::siphon::FLARE`].
	envelope: f32,
}

/// Camera far plane, a fixed multiple of the draw distance in chunks
/// (Java World.updateDepth). One definition — it is set at construction
/// and again whenever the settings screen retunes the draw distance.
fn far_plane(draw_dist: i32) -> f32 {
	(draw_dist * 32) as f32
}

fn ms_since(t: web_time::Instant) -> f32 {
	t.elapsed().as_secs_f32() * 1000.0
}

/// Milliseconds a frame may spend applying authoritative edits, given the
/// measured display interval — so the same queue takes proportionally less
/// of a shorter frame (see [`NET_EDIT_SHARE`]).
///
/// A free function rather than a method because the scaling rule is the
/// part worth pinning, and a test for it should not need a GPU.
/// `refresh_ms` is a measured percentile that reads zero until it settles;
/// the nominal interval is the fallback, which is also what every
/// pre-existing baseline was taken at.
fn net_edit_budget_ms(refresh_ms: f32) -> f32 {
	let interval = if refresh_ms > 0.0 {
		refresh_ms
	} else {
		1000.0 / 60.0
	};
	interval * NET_EDIT_SHARE
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
pub(super) fn player_color(id: u8) -> [f32; 3] {
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

/// What a snapshot's held-item wire id looks like in a remote's hand.
///
/// Routes through `hud::item_icon`, the same function the hotbar and
/// your own viewmodel use, so a remote hand can never show a different
/// object from the one its owner is looking at. `None` = an empty hand,
/// an unknown id from a newer server, or an id no slot could legally
/// hold.
pub(super) fn held_icon(held: u16) -> Option<crate::ui::hud::IconKind> {
	use digblock_core::hotbar::{HotbarItem, WIRE_EMPTY, WIRE_TOOL};
	let (item, full) = match held {
		WIRE_EMPTY => return None,
		WIRE_TOOL => (HotbarItem::WaterTool, false),
		digblock_net::messages::HELD_TOOL_FULL => (HotbarItem::WaterTool, true),
		// Still a wire-trust gate, not a nicety: a stale or hostile
		// server can name Water here, which no slot can hold and which
		// would otherwise draw as an item. A TOOL id is legitimate now
		// though — `Hotbar::to_wire` sends the active tool as its own id
		// and only the bucket as `WIRE_TOOL` — so tools are admitted
		// explicitly rather than by relaxing `is_slottable`, which is
		// answering a different question (may a GRID slot hold this).
		// `item_icon` routes them on to `tool_icon`, so a remote's pick
		// draws its authored geometry exactly as its owner's hand does.
		id => (
			HotbarItem::Block(
				BlockType::try_from_id(id).filter(|b| b.is_slottable() || b.is_tool())?,
			),
			false,
		),
	};
	Some(crate::ui::hud::item_icon(item, full).0)
}

impl Engine {
	/// Emit the one-line run summary, from whichever exit reaches it
	/// first. On the web that is `Game::on_exit`, because winit's exit
	/// unwinds through JS and `Drop` may never run — which is why `?bench`
	/// produced no output there at all before this existed.
	fn report_bench(&mut self) {
		if !self.config.bench || self.bench_reported {
			return;
		}
		self.bench_reported = true;
		if let Some(line) = self.metrics.bench_summary() {
			log::info!("{line}");
		}
	}
}

impl Drop for Engine {
	fn drop(&mut self) {
		self.report_bench();
	}
}

impl Game for Engine {
	fn init(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat, size: (u32, u32)) {
		self.size = size;
		self.recompute_ui_scale();
		self.build_renderer(gpu, surface_format);
		log::info!(
			"engine init: draw_dist={} format={surface_format:?}",
			self.config.draw_dist
		);
	}

	fn resize(&mut self, _gpu: &Gpu, size: (u32, u32)) {
		self.size = size;
		self.recompute_ui_scale();
	}

	fn set_scale_factor(&mut self, scale_factor: f32) {
		self.scale_factor = scale_factor;
		// The density-derived graphics defaults, applied here because this
		// is the FIRST point anything knows the display's backing scale —
		// the launcher resolves settings before a window exists.
		// `gfx_defaults_auto` means "the player has expressed no
		// preference": no flag, nothing saved. It is false on every
		// capture path, so baselines keep the shipped values.
		if self.config.gfx_defaults_auto {
			self.config.gfx_defaults_auto = false;
			let gi = crate::settings::density_gi_div(scale_factor);
			self.config.gi_scale_div = gi;
			// Keep the settings screen showing what is actually in use.
			self.menu.set_gi_scale(gi);
			log::info!("display scale {scale_factor}x: lighting detail /{gi}");
		}
		self.recompute_ui_scale();
	}

	fn input(&mut self, event: InputEvent) {
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
		// The F7 GI drill on a timer, so the ratchet is measurable WITHOUT a
		// human at the keyboard. Pair with `--fly N --frames M` (N < drill <
		// M): the camera freezes at N, the drill fires mid-freeze, and the
		// CSV rows either side are the same pose in the same thermal state
		// with only the GI's derived age differing — which is the only way
		// to separate "the ceiling ratcheted" from "the view got busier".
		self.frames_seen += 1;
		if self.config.gi_drill_at == Some(self.frames_seen) {
			let (sky, emit) = self.renderer_gi_state();
			if let Some(r) = self.renderer.as_mut() {
				r.gi_clear_derived_state();
			}
			self.pipeline.gi_redirty_all();
			log::info!(
				"GI drill at frame {}: cleared (sky_y {sky:.0}, {emit} emitters)",
				self.frames_seen
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
			if self.config.fly.is_none_or(|n| self.fly_frames < n) {
				self.fly_step();
				self.fly_frames += 1;
			}
			self.advance_hand(dt);
			self.pending.update_ms = ms_since(t_update);
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
			while self.fly_frames < n {
				self.fly_step();
				self.fly_frames += 1;
				if !self.config.warp {
					break;
				}
			}
			self.advance_hand(dt);
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
		if self.menu.take_disconnect() && self.leave_server() {
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
		// Return to spawn: one shared cooldown gates the menu row.
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
				// The account's name becomes the session name, so the Hello
				// introduces us consistently. It is a courtesy only: the
				// server overwrites it with the master's answer, which is
				// now the same string by construction.
				if matches!(cmd, crate::menu::MenuCommand::Join { .. }) {
					self.config.player_name = self.menu.account().and_then(|a| a.name.clone());
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
		if let Some(net) = self.net.as_mut()
			&& net.ready()
		{
			net.predictor.active_slot = self.hotbar.selected_index() as u8;
			// First send only after the join-sync `ServerMsg::Layout`
			// primed the dedup — reporting a pre-sync default would
			// overwrite the identity's stored arrangement.
			if let Some((last_bar, last_grid)) = self.last_layout_sent.as_ref() {
				// `to_wire` returns arrays, so the comparison is free;
				// only an actual change pays for the Vecs.
				let bar = self.hotbar.to_wire();
				let grid = self.inv_grid.to_wire();
				if last_bar.as_slice() != bar.as_slice() || last_grid.as_slice() != grid.as_slice()
				{
					net.send_layout(&bar, &grid);
					self.last_layout_sent = Some((bar.to_vec(), grid.to_vec()));
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
		// Confetti: the age in lane 3 IS the simulation — every flake's
		// arc is a pure function of it in the shader. A burst is dropped
		// the moment it reaches the life the shader fades to zero at.
		for b in &mut self.confetti {
			b[3] += dt;
		}
		self.confetti.retain(|b| b[3] < crate::confetti::LIFE);
		// DIG_CONFETTI=1: a burst every couple of seconds a few blocks
		// ahead, so a capture run can photograph one at every age.
		if self.config.debug_confetti && self.confetti.is_empty() {
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
		if self.alch_screen.is_open() || self.alch_screen.busy() {
			let view = crate::ui::screens::alchemy::AlchView::build(&self.inventory, &self.skills);
			if let Some(act) = self.alch_screen.tick(dt, &view) {
				self.apply_alchemy_action(act);
			}
		}
		if let Some(net) = self.net.as_ref() {
			// Reconciliation health line, on the metrics cadence.
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

		// Crosshair target → placement edge → hold-to-dig stepping (at the
		// fixed sim rate, so offline and online dig timing share the
		// server's tick domain, in walk AND fly mode). All outside the physics
		// `block_at` borrow: raycast (immutable) → set_block (mutable) →
		// player tick (immutable), strictly sequential.
		self.update_target();
		self.apply_place();
		// A stationary touch hold sends no events, so its promotion to
		// digging has to be polled — ahead of the dig ticks that read it.
		self.touch.tick(web_time::Instant::now());
		self.dig_accum = (self.dig_accum + dt).min(0.25);
		while self.dig_accum >= TICK {
			self.dig_accum -= TICK;
			self.step_dig();
		}
		// The siphon's stream reads the state those ticks just left, so it
		// ages after them and on the FRAME clock rather than the sim one:
		// it is presentation, and its envelope should not step in ticks.
		self.step_siphon(dt);

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

		// Arrival hold: run the wait down and give up on it if the marker
		// never lands. A server that wedges mid-sync, or one whose outbound
		// queue never opens the room join sync reserves, must not be able
		// to strand a player on a loading screen — the fallback is exactly
		// the behaviour this replaced.
		if let Some(left) = self.arrival_hold {
			let left = left - dt;
			// A connection that ended can never deliver the marker, and the
			// world we fall back to playing offline is generated, not
			// synced — so release now rather than making the player watch
			// the timeout run out over a server that is already gone. This
			// covers every drop path at once (menu disconnect, refusal,
			// timeout, `return_to_title`) instead of one hook each.
			if self.net.is_none() {
				self.end_arrival_hold();
			} else if left <= 0.0 {
				log::warn!(
					"arrival hold: no AreaReady in {:.0}s — entering the world anyway",
					digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS
				);
				self.end_arrival_hold();
			} else {
				self.arrival_hold = Some(left);
			}
		}

		if self.walking {
			// Fixed-rate player simulation (Java feel was tuned per-frame at
			// that same rate; the accumulator preserves it at any display rate).
			// Always drained, honoured only in double-tap mode — an undrained
			// edge would fire the moment the setting flipped back.
			let touch_jump = self.touch.take_jump();
			let jump = self.keys.up
				|| if self.config.touch_jump_button {
					self.hud_jump_held
				} else {
					touch_jump
				};
			let key_axis = Vec2::new(
				(self.keys.d as i32 - self.keys.a as i32) as f32,
				(self.keys.w as i32 - self.keys.s as i32) as f32,
			);
			let intent = PlayerIntent {
				move_axis: key_axis + self.touch.move_axis(),
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
			let world_ready = column_loaded && self.arrival_hold.is_none();
			if !world_ready {
				self.tick_accum = 0.0;
				// Zeroing the accumulator parks the lerp at alpha = 0, which
				// would draw a whole tick BACKWARDS; hold in place instead.
				self.tick_lerp.reset(self.player.pos);
			}
			match self.net.as_mut() {
				_ if !world_ready => {}
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
			// Sub-second exponential decay; snapped off once imperceptible.
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
		// At the TAIL on the interactive path, after `reconcile_layout` and
		// `pump_net`: a slot whose count reached zero is only released by
		// the reconcile above, so reading the bar ahead of it drew — and
		// LIT — a torch from a hand that had just placed its last one. The
		// scripted-flight arms call this before their own returns instead,
		// because the hand is part of the frame they capture.
		self.advance_hand(dt);
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
		// Remote players, sampled an interpolation delay in the past;
		// the instance list rides FrameScene and re-uploads every frame.
		self.remote_instances.clear();
		self.held_instances.clear();
		self.carried.begin();
		// Your own hand has no world position — the viewmodel is authored
		// in view space — so the LIGHT is body-anchored exactly like
		// everyone else's (`gi::carried_origin`). Keyed off the item the
		// renderer DRAWS rather than the hotbar's selection, so the light
		// and the visible torch arrive and leave together through the swap
		// dip; and read unconditionally, because opening the inventory
		// puts the hand away, not the torch out.
		self.carried.push(
			CARRIER_SELF,
			self.camera.pos,
			self.view_swap.shown().and_then(crate::ui::hud::icon_block),
			self.skills.level(digblock_core::skills::Skill::Mining),
		);
		if let Some(net) = self.net.as_ref() {
			net.remotes.sample(&mut self.remote_scratch);
			for i in 0..self.remote_scratch.len() {
				let d = self.remote_scratch[i];
				push_remote(
					&mut self.remote_instances,
					&mut self.held_instances,
					&mut self.carried,
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
				&mut self.remote_instances,
				&mut self.held_instances,
				&mut self.carried,
				&Remote {
					key: CARRIER_DEBUG,
					id: 2,
					pos: p,
					yaw,
					held,
				},
			);
		}
		self.carried.finish(self.camera.pos);
		let frame = self.frame_uniforms();
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
		// Overlay data is last frame's numbers (metrics.frame() runs at the
		// end of this fn) and GPU ms lags the readback ring — fine for a
		// debug HUD.
		let overlay_stats = if self.overlay_visible {
			self.metrics
				.history_tail(crate::overlay::GRAPH_FRAMES, &mut self.history_scratch);
			let mut stats = self.metrics.overlay_stats();
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
		let menu_view = self.menu.is_open().then(|| {
			self.menu.view(
				size,
				self.ui_scale,
				((self.time_secs * 2.0) as u32).is_multiple_of(2),
			)
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
			// The menu's back control on a touch device — the same corner
			// button the in-game screens close with, so one control means
			// one thing everywhere. Drawn after the panel so it sits over
			// the dim, and only where Escape has somewhere to go.
			if self.config.touch_hud && self.menu.can_go_back() {
				crate::ui::hud::corner_button(
					&mut self.ui_dl,
					self.ui_scale,
					size,
					&self.safe_area,
					true,
				);
			}
		}
		if in_game {
			let hotbar_view =
				crate::hotbar::HotbarView::build(&self.hotbar, &self.inventory, &self.tool_swap);
			let owned_tools = self.owned_tools();
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
				self.alch_screen.build(&mut ui, size, &view, daylight);
			} else if self.char_screen.is_open() {
				let view = crate::ui::screens::character::CharView::build(
					&self.skills,
					self.camera.pos.y.floor() as i32,
				);
				self.char_screen.build(&mut ui, size, &view, daylight);
			} else {
				crate::ui::hud::crosshair(ui.dl, size, ui.style.scale);
			}
			crate::ui::hud::hotbar(&mut ui, size, &hotbar_view, |i| {
				crate::ui::InvId::Bar(i as u8)
			});
			crate::ui::hud::xp_toasts(ui.dl, &ui.style, size, &self.xp_toasts);
			// The background bench's tell: only while a job runs with the
			// screen CLOSED — open, the screen's own progress row is the
			// richer version of the same fact.
			if !self.alch_screen.is_open()
				&& let Some(job) = self.alch_screen.job()
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
				self.tool_swap.shown(),
				self.inventory.bucket_full(),
				self.tool_swap.present(),
			);
			// The touch HUD draws last so its corner button sits over an
			// open screen's dim — it is that screen's close control.
			if self.config.touch_hud {
				let screen_open = self.inv_screen.is_open()
					|| self.alch_screen.is_open()
					|| self.char_screen.is_open();
				crate::ui::hud::touch_buttons(
					ui.dl,
					ui.style.scale,
					size,
					&self.safe_area,
					screen_open,
					self.config.touch_jump_button,
				);
				if !screen_open && let Some((anchor, finger)) = self.touch.joystick() {
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
		if self.arrival_hold.is_some() && in_game {
			crate::ui::hud::arrival_cover(
				&mut self.ui_dl,
				&crate::ui::Style::new(self.ui_scale),
				size,
				"Loading world...",
			);
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
			// A carried light that has travelled — or has just gone out —
			// re-places itself through the same hook a block edit uses, so
			// the probes around it re-trace at full weight instead of
			// decaying from a stale value. See `Carried`. `commit` runs
			// only once they have actually been queued.
			for &(_, p) in &self.carried.pending {
				r.gi_invalidate_moving(p);
			}
			self.carried.commit();
			let t_encode = web_time::Instant::now();
			let scene = FrameScene {
				uniforms: frame,
				view_proj,
				size,
				scale: crate::renderer::RenderScale {
					render: self.config.render_scale,
					gi_div: self.config.gi_scale_div,
				},
				ui_scale: self.ui_scale,
				players: &self.remote_instances,
				held: &self.held_instances,
				carried: &self.carried.lights,
				confetti: &self.confetti,
				// The stream ends where the tool's own light hangs, and
				// the destination is handed over rather than derived in
				// the shader so the two cannot drift — see `siphon`.
				siphon: self.siphon.map(|s| {
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
					.map(|s| (s, self.history_scratch.as_slice())),
				feed: &self.feed,
				target: highlight_target,
				viewmodel,
				ui: &self.ui_dl,
				chat: (in_game && self.chat.is_open()).then(|| {
					self.chat
						.view(((self.time_secs * 2.0) as u32).is_multiple_of(2))
				}),
				nametags: &self.nametag_scratch[..self.nametag_count],
			};
			let stats = r.render(gpu, target, &scene);
			self.pending.encode_ms = ms_since(t_encode);
			self.metrics.render = stats;
			self.metrics.gpu = r.gpu_pass_ms();
			self.metrics.gpu_buffer_bytes = r.gpu_buffer_bytes;
		}
		// Occupancy: the whole handler span minus the vsync wait that blocks
		// inside it on native (and is effectively absent on the web). Closed
		// here rather than by the platform after `render` so it reaches this
		// frame's record instead of the next one's.
		self.pending.busy_ms = self
			.frame_start
			.take()
			.map(|t| (ms_since(t) - self.pending.acquire_ms).max(0.0))
			.unwrap_or_default();
		// total_ms is measured inside frame() from the frame-to-frame delta.
		self.metrics.frame(std::mem::take(&mut self.pending));
	}

	/// A capture never takes the pointer. Without the first clause a
	/// capture answered YES on frame one — it opens with no menu, so no
	/// UI surface is open, so the desktop shell locked the cursor and
	/// hid it for the length of the run. The operator lost their mouse
	/// to a window they never meant to play, and everything they typed
	/// in the meantime landed in the world.
	fn wants_cursor_grab(&self) -> bool {
		self.config.interactive
			&& !self.menu.is_open()
			&& !self.inv_screen.is_open()
			&& !self.alch_screen.is_open()
			&& !self.char_screen.is_open()
	}

	fn wants_text_input(&self) -> bool {
		self.config.interactive && self.chat.is_open()
	}

	fn take_fullscreen_request(&mut self) -> bool {
		std::mem::take(&mut self.fullscreen_requested)
	}

	fn text_entry_active(&self) -> bool {
		self.config.interactive && (self.chat.is_open() || self.menu.text_field_open())
	}

	/// The window closed, the page is unloading, or a capture ran out its
	/// frames. Hang up properly so the server frees the session now rather
	/// than at its idle timeout — otherwise a player who quits and
	/// relaunches is refused by their own still-live connection — and emit
	/// the bench summary, which on the web has no other exit to ride.
	fn on_exit(&mut self) {
		if self.leave_server() {
			log::info!("engine: on_exit (said goodbye to the server)");
		}
		self.report_bench();
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
		// The world floor, refused exactly where the server's `StoreWorld`
		// refuses it — offline this pipeline IS the authority, so the two
		// have to answer alike or a local save diverges from the same
		// world hosted. Refusing the WRITE rather than filtering the
		// schedule is what covers every direction of approach:
		// `schedule_around` always enqueues the floor row too, and until now only
		// `cur.props().collides` kept water out of it — a property of
		// whatever block happens to sit there, which is precisely what
		// the floor rule must not depend on. The sim reads `false` as
		// "didn't land" and declines to propagate onward. A spawn lock is
		// server-only knowledge and stays unpredicted, hence `None`.
		if digblock_core::protect::immutable(x, y, z, None) {
			return false;
		}
		self.0.set_block(x, y, z, b).is_some()
	}
}

#[cfg(test)]
mod net_budget_tests;

#[cfg(test)]
mod stream_budget_gate_tests;

#[cfg(test)]
mod arrival_hold_tests;

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod water_tool_tests;

#[cfg(test)]
mod craft_gate_tests;

#[cfg(test)]
mod touch_hud_tests;

#[cfg(test)]
mod dig_decay_tests;

#[cfg(test)]
mod ore_capture_tests;
