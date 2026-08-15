use std::collections::VecDeque;
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
	/// Start the player here instead of at the seed's spawn point
	/// (`--pos x y z`). The scenes this engine is actually slow in are
	/// NATURAL water — terraced beds, stepped descents, cascades — and no
	/// synthetic plan builds those: `debug_river_plan` digs a
	/// flat-bottomed box with vertical walls, where only the rim is bank
	/// and nothing ever falls, so it has no impact cells and its gather
	/// early-outs almost everywhere. Two water perf fixes were measured on
	/// it and both played badly. A pose flag is what lets a capture stand
	/// in a real river instead. The debug plans key off the spawn too, so
	/// they follow this point.
	pub start_pos: Option<[f32; 3]>,
	pub metrics_csv: Option<std::path::PathBuf>,
	/// Start with the debug overlay visible (` toggles at runtime).
	pub overlay: bool,
	/// Benchmark mode: inputs ignored, deterministic scripted camera
	/// flight, run summary logged on exit.
	pub bench: bool,
	/// Capture harness: run the bench flight for this many frames, then
	/// FREEZE the camera there. Every capture in this engine is a still,
	/// which is exactly the case where a temporal artifact has already
	/// converged away — so a report about motion could not be measured at
	/// all. Freezing at a pose the flight is a pure function of gives two
	/// runs the identical camera by construction, with only their history
	/// differing.
	pub fly: Option<u32>,
	/// Take all of `fly`'s steps on frame 0 instead of one per frame: the
	/// same pose with no motion behind it. `--fly N` vs `--warp N` is the
	/// A/B — walked there vs sat there.
	pub warp: bool,
	/// Fraction of the surface the WORLD renders at, before the upscale
	/// pass takes it to native (`--render-scale` / `?renderscale=`). The
	/// UI is never scaled by this. Capture paths hold it at 1.0.
	pub render_scale: f32,
	/// The GI resolve runs at `render_size / gi_scale_div`
	/// (`--gi-scale` / `?giscale=`). Must be one of
	/// `settings::GI_SCALE_DIVISORS` — see `RenderScale`.
	pub gi_scale_div: u32,
	/// No explicit lighting-detail preference (no flag, nothing saved), so
	/// derive `gi_scale_div` from display density at the first
	/// `set_scale_factor` — the first moment anything knows the
	/// backing scale, since the launcher resolves settings before a window
	/// exists. FALSE on every capture path, which is what keeps bench,
	/// screenshot and drill runs on the shipped values.
	pub gfx_defaults_auto: bool,
	/// Swap the path `fly`/`warp` follow for a ground walk. The bench
	/// flight is a 30 blocks/s flyover 12 blocks up, which photographs the
	/// fast, shallow-parallax end of screen motion; a player reports
	/// temporal artifacts from the other end — eye height, walking pace,
	/// ground a few blocks ahead, where one half-res texel covers far more
	/// world per pixel. Same harness, second regime.
	pub walk: bool,
	/// Flyover speed in blocks/s (`--bench-speed`). **Default 30.0**, which
	/// every BENCH.md baseline was taken at — changing the default silently
	/// invalidates all of them, so don't.
	///
	/// 30 blocks/s is 7.5x `WALK_SPEED`, and that is a different STREAMING
	/// REGIME, not just a faster one: at flyover speed the GI upload budget
	/// cannot keep up and the classify drain drops entries, so a cost that
	/// only appears there may be budget starvation rather than a real leak.
	/// Set this to 4 for a soak that matches how the game is actually
	/// played. (`--walk` is not the same thing — it oscillates one block
	/// either side of the origin, so it never brings new terrain in at all.)
	pub bench_speed: f32,
	/// Fire the F7 GI drill at this frame (`--gi-drill-at`), so the ratchet
	/// is measurable with no human at the keyboard. See `update`.
	pub gi_drill_at: Option<u32>,
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
	/// Debug: throw a level-up confetti burst every couple of seconds, a
	/// few blocks ahead. A burst is a rare, transient event, so it is
	/// otherwise invisible to headless capture — the same argument
	/// `debug_screen` makes for the input-only screens. Set by the
	/// desktop launcher from DIG_CONFETTI.
	pub debug_confetti: bool,
	/// Debug: pin the crack decal at this progress (0..1) on whatever the
	/// crosshair is holding, and stand a wall of blocks two cells ahead
	/// for it to hold. Digging is INPUT-driven and a hold lasts a second
	/// or two, so the crack is otherwise unphotographable — the same
	/// argument `debug_confetti` makes, and the reason two look-and-feel
	/// regressions shipped in the screens before `debug_screen` existed.
	/// A sweep of runs over 0..1 is what shows the break spreading. Set
	/// by the desktop launcher from DIG_CRACK.
	pub debug_crack: Option<f32>,
	/// Debug: open a UI screen at startup with a stocked inventory, so a
	/// headless `--screenshot` run can capture it. Set by the desktop
	/// launcher from DIG_SCREEN.
	pub debug_screen: Option<DebugScreen>,
	/// Debug: place a showcase of every shaped-block variant near spawn
	/// (both stair materials × 4 facings, doors × facings × open/closed)
	/// once chunks are Ready. Set by the desktop launcher from
	/// DIG_SHAPES — visual verification without gameplay wiring.
	pub debug_shapes: bool,
	/// Debug: stand a rank of doors three cells ahead of spawn, close
	/// enough that the crosshair lands on one. Set by the desktop
	/// launcher from DIG_DOORS. See [`debug_doors_plan`] for why
	/// `debug_shapes` does not cover this.
	pub debug_doors: bool,
	/// Debug: seal a stone vault around the spawn camera with one torch
	/// inside — the Stage-B flagship capture (a roofed room goes DARK and
	/// torchlight is what makes it readable; LIGHT-DESIGN.md P3). Set by
	/// the desktop launcher from DIG_VAULT.
	pub debug_vault: bool,
	/// Debug: hang a wide stone roof with NO walls over the ground ahead
	/// of spawn — the beam-veto capture (a slanting morning sun must
	/// reach under it exactly as the shadow map says; only the ambient
	/// dome dims). Set by the desktop launcher from DIG_ROOF.
	pub debug_roof: bool,
	/// Debug: flood an open-topped stone tank around the spawn camera so
	/// the eye starts SUBMERGED — the underwater capture (GI-DESIGN.md
	/// P4: sun transmittance through the column dims submerged terrain,
	/// the surface reads as a plane from below). Set by the desktop
	/// launcher from DIG_DIVE.
	pub debug_dive: bool,
	/// Debug: excavate a wide, deep basin AHEAD of the spawn camera and
	/// flood it — the "standing at a river's edge looking into it" scene
	/// (R17). Near water fills most of the frame from above, inside C0
	/// beside bank geometry, with a submerged floor behind it for the
	/// resolve's rays to cross — none of the other water captures put a
	/// SHADED surface on screen at all (DIG_DIVE is submerged; the sea
	/// at spawn is 100+ blocks out). Set by the desktop launcher from
	/// DIG_RIVER.
	pub debug_river: bool,
	/// Debug: how many torches `debug_torches_plan` lights its courtyard
	/// with, 0 = scene off. The local-light capture DIG_VAULT cannot be:
	/// a room deeper than `LIGHT_RANGE` and an emitter COUNT that varies
	/// with the geometry held fixed. Set by the desktop launcher from
	/// DIG_TORCHES.
	pub debug_torches: i32,
	/// Debug: extra torches parked 45 blocks BEHIND the camera, out of
	/// every visible surface's `LIGHT_RANGE` but inside the light list's
	/// reach — so they change the emitter COUNT without changing one
	/// pixel of direct light. Set from DIG_TORCHES_FAR.
	pub debug_torches_far: i32,
	/// Debug: screen the torch cluster in half, so emitters are occluded
	/// from each other — the only pose that can measure a resampled NEE's
	/// residual, which is entirely a visibility term. Set from
	/// DIG_TORCHES_WALL.
	pub debug_torches_wall: bool,
	/// Debug: shading terms to REMOVE this run (`DIG_STUB`, names in
	/// `stub::parse`). One build then attributes a whole frame across a
	/// handful of runs, instead of a rebuild per bisect — and, more to the
	/// point, instead of a guess. The mask rides `frame.misc.z` and the
	/// resolve's own uniform as a UNIFORM branch: zero cost at 0, and it
	/// can never diverge inside a wave, so a stubbed run measures the term
	/// it removed and nothing else.
	pub stub: u32,
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
	/// Mesh bytes handed to the queue per frame. Wall-clock alone cannot
	/// bound this on the web: `write_buffer` merely RECORDS the copy and
	/// Chrome's GPU process performs the staging transfer at submit, so the
	/// cost lands after everything this engine can measure.
	pub upload_bytes_max: usize,
	/// Debug: paint the voxel-window raymarch over the scene — the GI
	/// window's visual proof (`DIG_GI=window`).
	pub debug_gi: bool,
	/// Debug: the raymarch shades hits with the probe field instead of
	/// albedo (`DIG_GI=probes`) — the P2 gate view.
	pub debug_gi_probes: bool,
}

impl EngineConfig {
	pub fn desktop() -> Self {
		let workers = std::thread::available_parallelism().map_or(4, |n| (n.get() - 2).clamp(2, 4));
		Self {
			draw_dist: 20,
			workers,
			seed: None,
			tod: None,
			yaw: None,
			pitch: None,
			start_pos: None,
			metrics_csv: None,
			overlay: false,
			bench: false,
			fly: None,
			warp: false,
			walk: false,
			render_scale: 1.0,
			gfx_defaults_auto: false,
			gi_scale_div: crate::settings::GI_SCALE_DEFAULT,
			bench_speed: 30.0,
			gi_drill_at: None,
			ssr: true,
			ui_scale: 1.0,
			music_vol: 1.0,
			sfx_vol: 1.0,
			player_name: None,
			debug_playerbox: false,
			debug_confetti: false,
			debug_crack: None,
			debug_shapes: false,
			debug_doors: false,
			debug_vault: false,
			debug_roof: false,
			debug_dive: false,
			debug_river: false,
			debug_torches: 0,
			debug_torches_far: 0,
			debug_torches_wall: false,
			stub: 0,
			debug_screen: None,
			spray_droplets: 128,
			stream_budget_ms: STREAM_BUDGET_MS,
			adaptive_stream: false,
			chunk_slots_override: None,
			menu: false,
			auto_join: None,
			identity: [0; 16],
			spike_log: false,
			upload_bytes_max: UPLOAD_BYTES_MAX_NATIVE,
			debug_gi: false,
			debug_gi_probes: false,
		}
	}

	pub fn mobile() -> Self {
		Self {
			draw_dist: 10,
			workers: 2,
			seed: None,
			tod: None,
			yaw: None,
			pitch: None,
			start_pos: None,
			metrics_csv: None,
			// iOS has no keyboard toggle; flip this for device profiling.
			overlay: false,
			bench: false,
			fly: None,
			warp: false,
			walk: false,
			render_scale: 1.0,
			gfx_defaults_auto: false,
			gi_scale_div: crate::settings::GI_SCALE_DEFAULT,
			bench_speed: 30.0,
			gi_drill_at: None,
			ssr: false,
			ui_scale: 1.0,
			music_vol: 1.0,
			sfx_vol: 1.0,
			player_name: None,
			debug_playerbox: false,
			debug_confetti: false,
			debug_crack: None,
			debug_shapes: false,
			debug_doors: false,
			debug_vault: false,
			debug_roof: false,
			debug_dive: false,
			debug_river: false,
			debug_torches: 0,
			debug_torches_far: 0,
			debug_torches_wall: false,
			stub: 0,
			debug_screen: None,
			spray_droplets: 48,
			stream_budget_ms: STREAM_BUDGET_MS,
			adaptive_stream: false,
			chunk_slots_override: None,
			menu: false,
			auto_join: None,
			identity: [0; 16],
			spike_log: false,
			upload_bytes_max: UPLOAD_BYTES_MAX_CONSTRAINED,
			debug_gi: false,
			debug_gi_probes: false,
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
			draw_dist: self.draw_dist,
			spray_droplets: self.spray_droplets,
			chunk_slots_override: self.chunk_slots_override,
			debug_gi: self.debug_gi,
			debug_gi_probes: self.debug_gi_probes,
			stub: self.stub,
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

/// The [`UPLOAD_MIN`] twin for the RTGI voxel window. Higher because a GI
/// chunk is 4 KiB against a mesh's tens of KiB, and because a window the
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
/// Share of one display interval `pump_net` may spend applying
/// authoritative edits. Each costs a copy-on-write cell write plus up to
/// eight mesh-heap pushes, so a join sync's worth of them in one frame is
/// a visible stall; spread over frames it is invisible, and the world
/// simply fills in as fast as meshing can keep up anyway.
///
/// A COUNT could not express that, which is what this replaced. What one
/// edit costs swings by more than an order of magnitude — whether its
/// chunk is still shared decides a 16³ copy-on-write — and the mesh jobs
/// it queues run INLINE on this same thread on the web (`workers: 0`),
/// later in the same frame, so the apply is only half of what a burst
/// actually spends. The old 256 was sized against a 16.67 ms native frame
/// with worker threads behind it; at 120 Hz on the web the frame is
/// 8.33 ms and the meshing is not behind anything. Pricing the slice in
/// TIME makes the same code back off on the platform that cannot afford
/// it, with no constant to retune per display.
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
const _: () = assert!(NET_EDIT_MIN % NET_EDIT_CHECK == 0);

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
		out.push((x, by, bz + 2, make_cell(B::LumberStairs, stair_state(f))));
		for (dz, open) in [(8, false), (10, true)] {
			out.push((
				x,
				by,
				bz + dz,
				make_cell(B::LumberDoor, door_state(f, open, false)),
			));
			out.push((
				x,
				by + 1,
				bz + dz,
				make_cell(B::LumberDoor, door_state(f, open, true)),
			));
		}
	}
	// A torch row: one on the ground, one up on a wood plinth — the
	// emissive head is the thing to photograph at night.
	out.push((bx - 5, by, bz + 4, B::Torch as u16));
	out.push((bx - 5, by, bz + 6, B::Wood as u16));
	out.push((bx - 5, by + 1, bz + 6, B::Torch as u16));
	// Slabs: both materials × both states, then a walkable seam row —
	// 1/3 | 2/3 | full block | full + raised 1/3, every riser one
	// auto-step (also eyeballs boundary culling on the shared planes).
	// Ceiling slabs hang two cells up, where their underside is the
	// thing to look at.
	for (i, b) in [B::WoodSlab, B::StoneSlab, B::LumberSlab]
		.into_iter()
		.enumerate()
	{
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

/// DIG_VAULT=1 layout: a sealed stone vault enclosing the spawn camera,
/// one torch on the floor ahead and a stair to catch its falloff. The
/// Stage-B flagship capture: skylight is stripped by the roof (the
/// placement writes ride the ordinary edit hook), so the interior
/// photographs DARK except the torch pool — at noon.
fn debug_vault_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{stair_state, Facing};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// Shell-first, carve-second ordering doesn't matter — placement is
	// retried per cell and the light BFS settles under maintain either
	// way. Interior: x±3, y −2..=+2, z −2..=+8 around the eye cell.
	for x in (bx - 4)..=(bx + 4) {
		for y in (by - 3)..=(by + 3) {
			for z in (bz - 3)..=(bz + 9) {
				let interior = (bx - 3..=bx + 3).contains(&x)
					&& (by - 2..=by + 2).contains(&y)
					&& (bz - 2..=bz + 8).contains(&z);
				out.push((
					x,
					y,
					z,
					if interior {
						B::Air as u16
					} else {
						B::Stone as u16
					},
				));
			}
		}
	}
	// The torch, and something with depth for its light to model.
	out.push((bx, by - 2, bz + 6, B::Torch as u16));
	out.push((
		bx - 2,
		by - 2,
		bz + 5,
		make_cell(B::StoneStairs, stair_state(Facing::PosX)),
	));
	out.push((bx + 2, by - 2, bz + 4, B::Wood as u16));
	out
}

/// DIG_DIVE=1 layout: an open-topped stone tank flooded around the
/// spawn camera, so the eye starts one block below the surface — the
/// RTGI underwater capture. Open sky above is the light source: the
/// sun DDA crosses the water column (transmittance dims submerged
/// terrain), the probes see the pool through the same window, and the
/// surface overhead photographs as a plane from below (Snell window /
/// TIR mirror). Furniture on the floor gives the light something to
/// model.
fn debug_dive_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{stair_state, Facing};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// Shell x±4 / z −4..+10, floor at −4, walls up to +2; interior
	// carved then flooded to one block ABOVE the eye cell, so the
	// camera is submerged with the surface in view. No ceiling — the
	// sky lights the pool.
	for x in (bx - 4)..=(bx + 4) {
		for y in (by - 4)..=(by + 2) {
			for z in (bz - 4)..=(bz + 10) {
				let interior = (bx - 3..=bx + 3).contains(&x)
					&& (by - 3..=by + 2).contains(&y)
					&& (bz - 3..=bz + 9).contains(&z);
				let cell = if !interior {
					B::Stone as u16
				} else if y <= by + 1 {
					B::Water as u16
				} else {
					B::Air as u16
				};
				out.push((x, y, z, cell));
			}
		}
	}
	// Submerged furniture: sand strip, a stair, a wood pillar.
	for dz in 2..=4 {
		out.push((bx - 1, by - 3, bz + dz, B::Sand as u16));
	}
	out.push((
		bx + 1,
		by - 3,
		bz + 4,
		make_cell(B::StoneStairs, stair_state(Facing::PosX)),
	));
	out.push((bx + 2, by - 3, bz + 6, B::Wood as u16));
	out.push((bx + 2, by - 2, bz + 6, B::Wood as u16));
	out
}

/// DIG_RIVER=1 layout: a wide, deep flooded basin AHEAD of the spawn
/// camera (yaw 0 faces +z), its surface ~3.6 blocks below the eye — the
/// "standing at a river's edge looking into it" frame-cost scene (R17).
/// Pair with `--pitch -35` so near water fills most of the screen. All
/// of it sits inside C0 beside bank geometry (the expensive gather
/// path), and the submerged stone floor keeps the resolve's rays
/// crossing the water column, which is the other half of that scene's
/// cost. 49×49 surface, 7 deep.
fn debug_river_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for x in (bx - 25)..=(bx + 25) {
		for y in (by - 10)..=(by + 8) {
			for z in (bz + 2)..=(bz + 52) {
				let interior = (bx - 24..=bx + 24).contains(&x)
					&& y >= by - 9 && (bz + 3..=bz + 51).contains(&z);
				let cell = if y >= by - 2 {
					// Open air above the waterline — clears any hillside
					// so the whole surface is in view from the rim.
					B::Air as u16
				} else if interior {
					B::Water as u16
				} else {
					// Shell: floor and a one-block rim at water level, so
					// the pool cannot drain and the banks give C0 probes
					// geometry.
					B::Stone as u16
				};
				out.push((x, y, z, cell));
			}
		}
	}
	out
}

/// DIG_CRACK=&lt;p&gt; layout: a flat wall of blocks two cells ahead of the
/// spawn camera, tall and wide enough that the crosshair lands on one
/// whatever the eye height resolves to.
///
/// Dirt, not Stone: the decal only draws on a cell `diggable` says yes
/// to, and rock is behind the Mining gate, so a fresh capture profile
/// would photograph nothing at all. The neighbours either side are a
/// second material because the crack is seeded per (cell, face) — three
/// blocks in one frame is what shows the pattern is not one stamp.
fn debug_crack_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for x in (bx - 2)..=(bx + 2) {
		for y in (by - 3)..=(by + 2) {
			let b = if x == bx { B::Dirt } else { B::Lumber };
			out.push((x, y, bz + 2, b as u16));
		}
	}
	out
}

/// DIG_DOORS=1 layout: a rank of door pairs standing three cells ahead
/// of the spawn camera with nothing in front of them, plus a plain
/// Lumber column beside them as the tint reference.
///
/// DIG_SHAPES already stands every door variant, so this looks like a
/// duplicate and is not: it stands them EIGHT blocks out behind three
/// rows of stairs sharing their x columns, and from eye height a
/// one-block stair projects straight over the lower half of the door
/// behind it. The seam between a door's two cells is the one thing a
/// door has to be judged on — it is one object wearing two cells, so the
/// tint has to match across it and the outline has to wrap both — and it
/// is the one thing that scene cannot show. Three cells out also puts
/// the pair inside the interact ray, which is what makes the outline
/// photographable at all.
///
/// The reference column is deliberately the door's OWN material: the
/// leaf must read as one piece of timber down its own middle while still
/// differing from the timber beside it, and a capture that only proves
/// the first half of that would pass just as well if the tint had been
/// switched off.
fn debug_doors_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{door_state, Facing};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// NegZ faces the camera: the leaf sits against the cell face the
	// facing points at, and the camera is on the −z side.
	for (dx, f, open) in [
		(0, Facing::NegZ, false),
		(2, Facing::NegZ, true),
		(-2, Facing::PosX, false),
	] {
		for upper in [false, true] {
			out.push((
				bx + dx,
				by + i32::from(upper),
				bz + 3,
				make_cell(B::LumberDoor, door_state(f, open, upper)),
			));
		}
	}
	for y in by..=(by + 1) {
		out.push((bx - 3, y, bz + 3, B::Lumber as u16));
		out.push((bx + 3, y, bz + 3, B::Lumber as u16));
	}
	out
}

/// DIG_ROOF=1 layout: a wide stone plate hanging over the ground ahead
/// of spawn with NO walls — the beam-veto capture. Skylight under it
/// drops (lateral spread decays toward the middle), so this scene is
/// exactly where scaling direct sun by the nibble went wrong: a morning
/// sun slants in from the open side and must light the ground precisely
/// where the shadow map says, with only the ambient dome dimming.
fn debug_roof_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for x in (bx - 8)..=(bx + 8) {
		for z in (bz + 2)..=(bz + 18) {
			out.push((x, by + 3, z, B::Stone as u16));
		}
	}
	out
}

/// DIG_TORCHES=&lt;n&gt; layout: an open-topped stone courtyard ahead of
/// spawn with `n` torches clustered at its near end and forty blocks of
/// bare floor running away from them.
///
/// DIG_VAULT cannot photograph the remaining local-light noise, and the
/// two reasons are the whole point of this scene. Its room is eleven
/// blocks deep, so every surface in it sits inside `LIGHT_RANGE` and
/// well inside the near-field clamp — there is no "far from the torch"
/// band at all. And it holds exactly ONE torch, which is the single case
/// the resolve's dominant-light NEE covers completely: with one emitter
/// in range the deterministic ray is the whole direct term and the
/// gather's emitter-hit lottery is suppressed for it.
///
/// So: `n` is a knob (1 vs 8 with the geometry held fixed) and the floor
/// runs to +44 so a capture crosses the 24-block `LIGHT_RANGE` edge with
/// the same material and the same face code either side of it. No roof —
/// the report is about NIGHT, where the sky is a dim second source
/// rather than absent, so `--tod 180` is the pose and `--tod 0` is the
/// control that shows the sun masking it.
fn debug_torches_plan(spawn: Vec3, n: i32, far: i32, wall: bool) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// Floor at by−3 (DIG_VAULT's convention: the first air cell is by−2),
	// cleared to by+8 so nothing worldgen left overhead shadows the run.
	for x in (bx - 16)..=(bx + 16) {
		for z in (bz - 6)..=(bz + 46) {
			out.push((x, by - 3, z, B::Stone as u16));
			for y in (by - 2)..=(by + 8) {
				let wall = x == bx - 16 || x == bx + 16 || z == bz + 46 || z == bz - 6;
				out.push((
					x,
					y,
					z,
					if wall && y <= by + 2 {
						B::Stone as u16
					} else {
						B::Air as u16
					},
				));
			}
		}
	}
	// The cluster: up to 8 torches on 3-block centres around (bx, bz+17),
	// laid down in a fixed order so `n` = 1 keeps the same first torch in
	// the same cell as `n` = 8 and the A/B moves one variable.
	//
	// Seventeen blocks out, not two, because the report is about distance:
	// an eye-height camera at spawn then holds all three regimes in one
	// frame — bare floor from 0 to 12 blocks lit only by what reaches it
	// (the band the artifact is reported in, and the one a half-res texel
	// magnifies most), the pools themselves at 14-20, and floor running out
	// past `LIGHT_RANGE` behind them where no NEE fires at all.
	const SPOTS: [(i32, i32); 8] = [
		(0, 17),
		(3, 17),
		(-3, 17),
		(0, 20),
		(3, 20),
		(-3, 20),
		(0, 14),
		(3, 14),
	];
	for &(dx, dz) in SPOTS.iter().take(n.clamp(0, 8) as usize) {
		out.push((bx + dx, by - 2, bz + dz, B::Torch as u16));
	}
	// `wall` cuts the cluster in half with a solid screen, so a floor
	// point on either side sees some of the torches and not the others.
	//
	// That is the one case a resampled NEE cannot price: the draw is
	// proportional to UNSHADOWED contribution, so a point that can only
	// see half of what it samples gets a Bernoulli on the pool total. The
	// open courtyard cannot show it — with every emitter visible the
	// estimator is exact — so without this the residual would have to be
	// argued rather than measured.
	if wall {
		for dy in 0..4 {
			for dx in -7..=7 {
				out.push((bx + dx, by - 2 + dy, bz + 18, B::Stone as u16));
			}
		}
	}
	// `far` torches on a pad 45 blocks BEHIND the camera, outside the
	// courtyard wall. They cannot light one pixel of the capture — every
	// visible surface is past `LIGHT_RANGE` from them and the wall is in
	// the way — but they are well inside the 95-block reach `flush_lights`
	// keeps, so they land in the light LIST.
	//
	// That is the entire experiment. The resolve scans the list by
	// position, so it does not care; gi_probe's `nee` picks ONE entry
	// UNIFORMLY over the whole list and divides by 1/count, so every entry
	// that cannot contribute still takes its turn and multiplies the ones
	// that can. If flicker tracks `far` with the picture held fixed, the
	// probe cache's light selection is a source in its own right.
	for i in 0..far.clamp(0, 64) {
		let x = bx - 6 + (i % 8) * 3;
		let z = bz - 45 - (i / 8) * 3;
		out.push((x, by - 3, z, B::Stone as u16));
		out.push((x, by - 2, z, B::Torch as u16));
	}
	out
}

/// Camera far plane: draw_dist × 32 (Java World.updateDepth). One
/// definition — it is set at construction and again whenever the
/// settings screen retunes the draw distance.
fn far_plane(draw_dist: i32) -> f32 {
	(draw_dist * 32) as f32
}

fn ms_since(t: web_time::Instant) -> f32 {
	t.elapsed().as_secs_f32() * 1000.0
}

/// Milliseconds a frame may spend applying authoritative edits, given the
/// measured display interval — so the same queue takes half as much of a
/// 120 Hz frame as of a 60 Hz one (see [`NET_EDIT_SHARE`]).
///
/// A free function rather than a method because the scaling rule is the
/// part worth pinning, and a test for it should not need a GPU.
/// `refresh_ms` is a measured percentile that reads zero until it settles;
/// the 60 Hz interval is the fallback, which is also what every
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
		let far = far_plane(config.draw_dist);
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
		// `--pos` replaces the seed's spawn wholesale, debug plans and
		// return-to-spawn included: the point of standing somewhere is that
		// everything keyed on the spawn follows you there.
		let spawn = config
			.start_pos
			.map_or_else(|| digblock_core::player::spawn(seed), Vec3::from);
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
				render_scale: config.render_scale,
				gi_scale_div: config.gi_scale_div,
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
					plan.extend(debug_vault_plan(spawn));
				}
				if config.debug_roof {
					plan.extend(debug_roof_plan(spawn));
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
			touch: TouchDriver::new(),
			ui_touch: None,
			walking: true,
			fly_key_down: false,
			overlay_key_down: false,
			gi_drill_key_down: false,
			frames_seen: 0,
			tick_accum: 0.0,
			sun,
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
			confetti: Vec::new(),
			confetti_seq: 0,
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
			fly_frames: 0,
			walk_origin: None,
			pending_uploads: std::collections::VecDeque::new(),
			pending_evicts: std::collections::VecDeque::new(),
			pending_evict_set: rustc_hash::FxHashSet::default(),
			gi_queue: VecDeque::new(),
			gi_queued: rustc_hash::FxHashSet::default(),
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
			arrival_hold: None,
			inv_grid: Default::default(),
			last_layout_sent: None,
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
			// Metal/Aether too, so the header's five columns photograph
			// populated — two dead zeros read as a layout bug in shots.
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Metal, 12);
			engine
				.inventory
				.add_sub(digblock_core::block::Essence::Aether, 3);
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
			self.camera.far = far_plane(s.draw_dist);
			self.pipeline.set_draw_dist(s.draw_dist);
			if let Some(net) = self.net.as_mut() {
				if net.ready() {
					net.send_set_draw_dist(s.draw_dist);
				}
			}
		}
		// Both render scales are applied by simply storing them: the
		// renderer keys its target rebuild on the scale it last built at,
		// so the next frame re-creates them. No explicit invalidation, and
		// nothing to keep in sync.
		self.config.render_scale = s.render_scale;
		self.config.gi_scale_div = s.gi_scale_div;
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
		// Confetti is thrown at a world position; a new world has none of
		// them left hanging in the air.
		self.confetti.clear();
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

	/// Hold the world after an arrival — a join or a teleport — until the
	/// server reports the journaled columns around the destination sent
	/// (`NetEvent::AreaReady`), or the wait runs out.
	///
	/// Offline arrivals never hold: a local world's edits are already in
	/// the overlay before the pipeline classifies air, so there is nothing
	/// in flight to wait for and a cover would be a pause for its own sake.
	/// That is guaranteed by the CALL SITES — both are `pump_net` arms, and
	/// an offline session produces no `NetEvent`s at all. It must not be
	/// guarded on `self.net` here: `pump_net` owns the connection by value
	/// for the whole drain (`self.net.take()`), so `is_some()` reads false
	/// on exactly the two paths that need the hold. That guard armed
	/// nothing, and since `Welcome` no longer closes the Connecting screen
	/// itself, it stranded every join on "Joining world..." forever — the
	/// 5 s escape hatch cannot fire on a hold that was never set.
	fn begin_arrival_hold(&mut self) {
		self.arrival_hold = Some(digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS);
	}

	/// Release it, whichever way it ended. Also closes the join's
	/// Connecting screen, which is a no-op for a teleport (that path
	/// covers itself from the HUD) and for a hold that timed out with the
	/// menu already gone.
	fn end_arrival_hold(&mut self) {
		if self.arrival_hold.take().is_some() {
			self.menu.on_welcome();
			// The pose we are about to start simulating from is the one the
			// server gave us, now standing on the real world rather than on
			// worldgen. Re-anchor so the first drawn frame does not
			// interpolate out of wherever the held frames left the lerp.
			self.tick_lerp.reset(self.player.pos);
			self.prev_player_pos = self.player.pos;
		}
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
		// Edits are budgeted in TIME (see `NET_EDIT_SHARE`); everything
		// else in the queue is bookkeeping that costs nothing to run.
		// Stopping still parks the WHOLE remaining queue rather than
		// letting cheap events overtake — the ordering rule above is what
		// makes that mandatory, and it is unchanged by the budget's units.
		let edit_deadline = net_edit_budget_ms(self.metrics.refresh_ms());
		let t_edits = web_time::Instant::now();
		let mut edits_applied = 0usize;
		let mut drop_net = false;
		while let Some(next) = queue.front() {
			if matches!(next, NetEvent::Edit { .. }) {
				if edits_applied >= NET_EDIT_MAX {
					break;
				}
				// The floor lands before the clock is ever read, so a
				// frame that is already long still makes progress.
				if edits_applied >= NET_EDIT_MIN
					&& edits_applied % NET_EDIT_CHECK == 0
					&& ms_since(t_edits) >= edit_deadline
				{
					break;
				}
				edits_applied += 1;
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
					// NOT `menu.on_welcome()` yet: the Connecting screen IS
					// the join's loading cover, and closing it here is what
					// used to show the player pristine worldgen for the
					// second it took the journal to arrive. It closes when
					// the hold releases.
					self.begin_arrival_hold();
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
					// Recording the cell — for EVERY reason, not just the
					// ones we can name — is what stops `step_dig` opening
					// a fresh hold and re-sending DigStart each frame (a
					// reason-gated latch silently spun on any refusal this
					// build didn't know).
					if self.dig.as_ref().is_some_and(|d| d.cell == cell) {
						self.cancel_dig();
					}
					if self.refused_cell != Some(cell) {
						self.refused_cell = Some(cell);
						use digblock_net::messages::RejectReason as R;
						let msg = match reason {
							R::Protected => digblock_core::protect::REFUSE_DIG_MSG,
							R::SkillGated => digblock_core::unlock::Kind::Strength.message(),
							R::NotUnlocked => digblock_core::unlock::Kind::Recipe.message(),
							_ => "the server refused that dig",
						};
						self.push_feed(msg.into());
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
				NetEvent::LevelUp {
					player,
					skill,
					level,
				} => {
					// Someone else levelled: confetti over THEIR head. The
					// position is last frame's interpolated sample — the
					// same one their box was drawn at, which is the only
					// place the paper could honestly come from. A player
					// too far away to be sampled at all gets no burst,
					// which is exactly right: nobody can see them.
					if let Some(d) = self.remote_scratch.iter().find(|d| d.id == player) {
						let eye = d.pos;
						self.throw_confetti(eye);
					}
					if let Some(s) = digblock_core::skills::Skill::try_from_index(skill) {
						let who = self
							.player_names
							.get(&player)
							.cloned()
							.unwrap_or_else(|| format!("player {player}"));
						self.push_feed(format!("{who} reached {} level {level}.", s.display()));
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
				NetEvent::Teleported { pos, look } => {
					self.apply_teleport(pos, look);
					// A teleport is an arrival too — the destination's
					// journal is no more synced than a join's.
					self.begin_arrival_hold();
				}
				NetEvent::AreaReady => self.end_arrival_hold(),
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
		// The fresh renderer's GI window is empty; every resident chunk
		// re-uploads through the ordinary budgeted drain (the GI mirror
		// of the re-mesh above — device loss must never need a relight).
		self.pipeline.gi_redirty_all();
	}

	/// The GI's ratcheting state, or zeros before the renderer exists.
	fn renderer_gi_state(&self) -> (f32, u32) {
		self.renderer
			.as_ref()
			.map_or((0.0, 0), Renderer::gi_debug_state)
	}

	fn camera_chunk(&self) -> ChunkPos {
		ChunkPos::new(
			(self.camera.pos.x / 16.0).floor() as i32,
			0,
			(self.camera.pos.z / 16.0).floor() as i32,
		)
	}

	/// One tick of the scripted flight: a constant-speed line with a slow
	/// yaw sweep, advanced by a fixed 60Hz tick so the path is a pure
	/// function of how many times this has been called — identical
	/// workload run-to-run, and identical pose whether the steps are
	/// spread over frames (`--fly`) or taken at once (`--warp`).
	fn fly_step(&mut self) {
		const BENCH_YAW: f32 = 12.0; // degrees per path-second
							   // The walk: one block forward, one block back, at eye height with
							   // the ground a few blocks ahead — the repro exactly as reported.
							   // An oscillation rather than a line keeps the column under the feet
							   // constant, so the eye cannot step a whole block mid-path and
							   // invalidate the history for a reason that is not the one under
							   // test.
		const WALK_AMP: f32 = 1.0; // blocks either side of the origin
		const WALK_PERIOD: f32 = 1.5; // seconds — peak 4.2 blocks/s, a walk
		const WALK_PITCH: f32 = -25.0; // ground ~3.5 blocks ahead of the eye
		self.bench_t += TICK;
		if self.config.walk {
			// Anchored on wherever the camera started, so the walk happens
			// at spawn — which is where DIG_SHAPES/DIG_VAULT put geometry
			// worth casting a shadow.
			let o = *self.walk_origin.get_or_insert(self.camera.pos);
			let phase = std::f32::consts::TAU * self.bench_t / WALK_PERIOD;
			self.camera.pos = Vec3::new(o.x, o.y, o.z + WALK_AMP * phase.sin());
			self.camera.yaw_deg = 0.0;
			self.camera.pitch_deg = WALK_PITCH;
			return;
		}
		// Altitude follows the terrain so worldgen retunes can't put the
		// flight underground; still a pure function of the step count.
		let x = self.config.bench_speed * self.bench_t;
		let h = digblock_core::worldgen::terrain_height(x as i32, 8, self.seed);
		self.camera.pos = Vec3::new(x, h as f32 + 12.0, 8.0);
		self.camera.look(BENCH_YAW * TICK, 0.0);
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
		// RTGI: scroll the window to the camera (re-dirtying whatever
		// just entered coverage), pull freshly-dirty chunks into the
		// re-upload queue, then feed the voxel window under a byte budget
		// — a quarter of the mesh-upload ceiling, ~4 KiB a chunk — AND
		// the shared streaming deadline (GI-DESIGN.md).
		//
		// The clock matters as much as the bytes: `classify_gi` walks all
		// 4096 cells of a chunk, and the byte cap alone permits 512 of
		// those in one frame. This was the only streaming stage that
		// ignored `deadline`, and the only one charged to no phase — a
		// classify burst landed in `busy_ms` with nothing to attribute it
		// to. `GI_UPLOAD_MIN` is the same minimum-progress floor the mesh
		// loop uses, so a frame that is already over budget still makes
		// headway instead of starving the window forever.
		let t_gi = web_time::Instant::now();
		// The window AS BUILT, never the live draw distance. Raising draw
		// distance from the settings screen mutates `config.draw_dist`
		// while the GI window texture keeps its construction size, and
		// approving a chunk beyond that makes `upload_chunk`'s
		// `rem_euclid` wrap it onto a different cell than the gather
		// reads — one part of the world written into another's slot.
		let gi_window = renderer.gi_window_chunks().min(self.config.draw_dist);
		self.pipeline.gi_recenter(center, gi_window);
		for pos in self.pipeline.take_gi_dirty() {
			if self.gi_queued.insert(pos) {
				self.gi_queue.push_back(pos);
			}
		}
		for (x, y, z) in self.pipeline.take_gi_edits() {
			renderer.gi_invalidate([x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5]);
		}
		let mut gi_budget = self.config.upload_bytes_max / 4;
		let mut gi_uploads = 0u32;
		while gi_budget >= 4096 {
			if gi_uploads >= GI_UPLOAD_MIN && web_time::Instant::now() >= deadline {
				break;
			}
			let Some(pos) = self.gi_queue.pop_front() else {
				break;
			};
			self.gi_queued.remove(&pos);
			// Outside the window's half-extent: uploading would ALIAS onto
			// an in-window column — drop it; `gi_recenter` re-dirties it
			// the moment it enters coverage. Not resident anymore =
			// evicted while queued: same answer.
			if !self.pipeline.gi_uploadable(pos, gi_window) {
				continue;
			}
			if let Some(chunk) = self.pipeline.classify_gi(pos) {
				renderer.gi_upload(gpu, pos, &chunk);
				gi_budget -= 4096;
				gi_uploads += 1;
			}
		}
		self.pending.gi_ms = ms_since(t_gi);
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
			&& self.hotbar.active_tool() == BlockType::Bucket
	}

	/// The owned tool roster in deterministic block-id order: the
	/// always-owned bucket plus every tool id with a nonzero count.
	fn owned_tools(&self) -> Vec<BlockType> {
		BlockType::ALL
			.iter()
			.copied()
			.filter(|b| b.is_tool() && (b.always_owned() || self.inventory.count(*b) > 0))
			.collect()
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
		// The skill gates are fully client-predictable (we know our own
		// levels); the server's DigStart refusals are the backstop, and it
		// asks this same question.
		if digblock_core::unlock::dig_blocked(hit.block, &self.skills).is_some() {
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
			self.release_dig();
			return;
		}
		// Aiming somewhere new clears the refusal, so a fresh attempt on
		// a different block speaks again.
		if self.refused_cell.is_some() && self.refused_cell != self.target.map(|h| h.cell) {
			self.refused_cell = None;
		}
		// The locally-knowable refusals (the world floor, the two skill
		// gates). A server-side lock can't be predicted and arrives as
		// `DigRefused` instead. The latch keeps each refusal to one feed
		// line per aim, same as the server path.
		if let Some((cell, msg)) = self.target.and_then(|h| {
			if digblock_core::protect::immutable(h.cell.0, h.cell.1, h.cell.2, None) {
				Some((h.cell, digblock_core::protect::REFUSE_DIG_MSG))
			} else {
				digblock_core::unlock::dig_blocked(h.block, &self.skills)
					.map(|req| (h.cell, req.kind.message()))
			}
		}) {
			if self.refused_cell != Some(cell) {
				self.refused_cell = Some(cell);
				self.push_feed(msg.into());
			}
		}
		let valid = self.target.filter(|h| self.diggable(h));
		let Some(hit) = valid else {
			self.release_dig();
			return;
		};
		// Three cases, and only the first advances a tick this step: an
		// already-live hold on this exact cell, a DECAYING one on it
		// (resumed, inheriting whatever the decay left), or a fresh one.
		let same = self
			.dig
			.as_ref()
			.filter(|d| d.cell == hit.cell && d.cell_value == hit.cell_value)
			.map(|d| (d.idle == 0, d.banked()));
		if !same.is_some_and(|(live, _)| live) {
			// A resume keeps its banked progress; anything else drops what
			// it was holding (and tells the server, which is what stops it
			// verifying aim on a cell we walked away from). Either way the
			// hold re-opens from here, DigStart included — the server was
			// told to stop the moment the button came up, so a resume has
			// to ask it to start counting again.
			let carried = match same {
				Some((_, banked)) => banked,
				None => {
					self.cancel_dig();
					0
				}
			};
			// Family-scaled hold time (SIM v12) — the same `dig_required`
			// the server's completion gate runs.
			let required = hit
				.block
				.dig_hardness(hit.cell.1)
				.map(|h| {
					digblock_core::skills::dig_required(
						hit.block,
						h,
						self.skills.level(digblock_core::skills::Skill::Mining),
					) as u16
				})
				.unwrap_or(u16::MAX);
			self.dig = Some(DigProgress {
				cell: hit.cell,
				cell_value: hit.cell_value,
				ticks: carried,
				required,
				idle: 0,
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
			let d = self.dig.as_mut().expect("live hold checked");
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

	/// Stop digging, but KEEP what the hold banked: progress bleeds back
	/// to untouched over `skills::DIG_DECAY_TICKS` and a hold resumed
	/// inside that window inherits what is left, so a thumb slipping off
	/// the button no longer costs the whole block. Called once per 60 Hz
	/// step while not digging — that repetition is what advances the
	/// decay, and dropping the record at zero is what keeps a spent hold
	/// from being resumed for free.
	///
	/// The server is told once, on the transition, and runs the identical
	/// decay from ITS release to ITS resume. Those two windows are the
	/// same interval on a stable link — both ends are shifted by the same
	/// one-way delay — so only the JITTER between the cancel and the next
	/// start separates them, which is what `dig_grace` already exists to
	/// absorb.
	fn release_dig(&mut self) {
		let Some(d) = self.dig.as_mut() else {
			return;
		};
		let stopping = d.idle == 0;
		d.idle = d.idle.saturating_add(1);
		if d.banked() == 0 {
			self.dig = None;
		}
		if stopping {
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

	/// Discard any in-progress hold outright, decay and all — a retarget
	/// onto a different cell, a world swap, or a server refusal. The soft
	/// path (button up, target momentarily lost) is `release_dig`.
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
	/// crosshair toggles; otherwise placement against the target, gated
	/// on stock ("you can only place what you gathered").
	fn apply_place(&mut self) {
		if !std::mem::take(&mut self.click_place) {
			return;
		}
		let Some(hit) = self.target else { return };
		// Use: the pick ray narrow-phases against interaction boxes, so
		// this finds an OPEN door on its swung leaf too — there is no
		// second ray to walk.
		if hit.block.is_door() {
			self.toggle_door(hit);
			return;
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
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
			// A level that OPENS something speaks with the bigger voice —
			// derived from the gate tables, so the two sounds can't drift
			// from what the level actually did. A multi-level jump (a big
			// offline award, or a coalesced total) asks about every level
			// crossed, not just the one landed on.
			let unlocked =
				(old_level + 1..=new_level).any(|l| digblock_core::unlock::opens(skill, l));
			self.emit(crate::audio::AudioCmd::Sfx {
				sound: crate::audio::level_sfx(unlocked),
				pos: None,
				gain: crate::audio::LEVEL_GAIN,
			});
			// Your own confetti, over your own head — first-person, that
			// is paper falling past the camera. Everyone ELSE gets theirs
			// from the server's `LevelUp` broadcast.
			let eye = self.player.pos;
			self.throw_confetti(eye);
		}
	}

	/// Throw a confetti burst from a player's head. `eye` is the eye
	/// position (the authoritative `Player::pos` locally, the
	/// interpolated sample for a remote), and the burst sits just above
	/// it so it erupts over the head rather than out of the face.
	///
	/// Over [`crate::confetti::MAX_BURSTS`] the oldest is dropped: a
	/// celebration that can't be drawn is better lost than the newest
	/// one, which is the one somebody is watching.
	fn throw_confetti(&mut self, eye: Vec3) {
		const HEAD_CLEARANCE: f32 = 0.55;
		self.confetti_seq = self.confetti_seq.wrapping_add(1);
		if self.confetti.len() >= crate::confetti::MAX_BURSTS {
			self.confetti.remove(0);
		}
		self.confetti.push(crate::confetti::instance(
			[eye.x, eye.y + HEAD_CLEARANCE, eye.z],
			0.0,
			// A spread-out seed: consecutive bursts must not look like
			// the same paper thrown twice.
			self.confetti_seq as f32 * 7.919,
		));
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
	/// outline stays but the crack never appears. Empty, it marks the
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
		// The DECAYED progress, so a released hold's web visibly retreats
		// (and a resumed one picks up from the retreated web) rather than
		// snapping between full and gone.
		let progress = self.config.debug_crack.or_else(|| {
			self.dig
				.as_ref()
				.filter(|d| d.cell == hit.cell)
				.map(|d| (f32::from(d.banked()) / f32::from(d.required.max(1))).min(1.0))
		});
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

	fn frame_uniforms(&self) -> FrameUniforms {
		let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
		let vp = self.camera.view_proj(aspect);
		let caster = self.sun.caster();
		let atmos = digblock_core::sky::Atmosphere::sample(self.sun.elevation());
		// Directions toward the bodies in the sky (opposite the light).
		let sun = -self.sun.sun_dir();
		let moon = -self.sun.moon_dir();
		let cam = self.camera.pos;
		// The sky is the sky, submerged or not. The old model substituted
		// a hand-tuned deep-water palette for the atmosphere whenever the
		// eye went under, which meant the GI rays were told the sky was
		// green — the attenuation belongs on the PATH through the water
		// (the sun DDA's transmittance today, M2's participating medium
		// for the view ray), never in the radiance of the source.
		let submerged = self.eye_in_water();
		let (horizon, zenith) = (atmos.horizon, atmos.zenith);
		FrameUniforms {
			view_proj: vp.to_cols_array_2d(),
			inv_view_proj: vp.inverse().to_cols_array_2d(),
			light_dir: [caster.dir.x, caster.dir.y, caster.dir.z, caster.strength],
			light_color: [
				atmos.light_color[0],
				atmos.light_color[1],
				atmos.light_color[2],
				// Animation clock (water scroll/foam) in the spare lane.
				self.time_secs,
			],
			clear_color: [horizon[0], horizon[1], horizon[2], 0.0],
			cam_pos: [cam.x, cam.y, cam.z, 0.0],
			sun_dir: [sun.x, sun.y, sun.z, atmos.star_intensity],
			moon_dir: [moon.x, moon.y, moon.z, atmos.glow_strength],
			zenith: [zenith[0], zenith[1], zenith[2], 0.0],
			horizon: [horizon[0], horizon[1], horizon[2], 0.0],
			// The medium's rim distance: where the STREAMED CYLINDER ends,
			// which is draw_dist chunks of CHUNK_SIZE blocks — NOT the
			// camera far plane, which is `draw_dist * 32` for legacy
			// reasons and therefore sits at twice the distance any terrain
			// exists at. Passing the far plane here (M2 did) put the rim
			// ramp at 525-640 blocks in a world that stops at 320, so the
			// term never fired and the streaming edge was never covered.
			misc: [
				1.0,
				(self.config.draw_dist * digblock_core::chunk::CHUNK_SIZE as i32) as f32,
				// The cost-stub mask (DIG_STUB), exact in f32 well past the
				// bits it uses. Zero in every normal run.
				self.config.stub as f32,
				// The render:GI divisor chunk.wgsl's bilateral upsample
				// walks its 2x2 neighbourhood with.
				self.config.gi_scale_div as f32,
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
			// The renderer's GiDraw fills these (quantized caster, cascade
			// dims, window side) via patch_frame each frame.
			gi_sun: [0.0; 4],
			gi0: [0.0; 4],
			gi1: [0.0; 4],
			gi_misc: [0.0; 4],
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
		// Benchmark runs must be input-independent to stay comparable.
		if self.config.bench {
			return;
		}
		// UI surfaces speak pointer. While any is open, the PRIMARY touch
		// synthesizes cursor + left-button events at this one site, so
		// iOS gets menu taps, inventory drags, and the extract-slot drag
		// (design-doc M2 debt) without any screen learning about touch.
		// Gameplay touch (look/move) keeps its own arm below.
		if let InputEvent::Touch { id, phase, x, y } = event {
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
		// Overlay data is last frame's numbers (metrics.frame() runs at the
		// end of this fn) and GPU ms lags 2-3 frames — fine for a debug HUD.
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
			crate::ui::hud::tool_rail(
				ui.dl,
				&ui.style,
				size,
				&owned_tools,
				self.hotbar.active_tool(),
				self.inventory.bucket_full(),
			);
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
				confetti: &self.confetti,
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
mod net_budget_tests {
	use super::*;

	/// The whole reason the edit drain is priced in time rather than
	/// counted: the same queue must take half as much of a 120 Hz frame as
	/// of a 60 Hz one. The count it replaced (256) was sized against the
	/// 16.67 ms native frame and spent the same share of an 8.33 ms web
	/// frame, which is twice the interval it was allowed.
	#[test]
	fn net_edit_budget_scales_with_the_display_interval() {
		let at60 = net_edit_budget_ms(1000.0 / 60.0);
		let at120 = net_edit_budget_ms(1000.0 / 120.0);
		assert!(
			(at120 * 2.0 - at60).abs() < 1e-4,
			"120 Hz must get exactly half: {at120} vs {at60}"
		);
		// `refresh_ms` reads zero until the estimate settles; falling back
		// to anything but the 60 Hz interval would move every baseline.
		assert_eq!(net_edit_budget_ms(0.0), at60);
		assert_eq!(net_edit_budget_ms(-1.0), at60);
	}

	/// The budget is a slice of the frame, not the frame. Nothing here
	/// pins the exact share, but a drain allowed a quarter of the interval
	/// is no longer a budget — the render still has to fit.
	#[test]
	fn net_edit_budget_leaves_the_frame_its_interval() {
		for hz in [30.0, 60.0, 120.0, 144.0] {
			let interval = 1000.0 / hz;
			let budget = net_edit_budget_ms(interval);
			assert!(
				budget > 0.0 && budget < interval * 0.25,
				"{hz} Hz: {budget} of {interval}"
			);
		}
	}
}

#[cfg(test)]
mod arrival_hold_tests {
	use super::*;
	use digblock_net::messages::ServerMsg;
	use digblock_net::protocol::{encode_preamble, PROTOCOL_VERSION};
	use digblock_net::transport::NetTransport;

	fn frame(msg: &ServerMsg) -> Vec<u8> {
		let mut out = Vec::new();
		digblock_net::codec::write_frame(msg, &mut out).expect("encode");
		out
	}

	fn welcome() -> ServerMsg {
		ServerMsg::Welcome {
			protocol: PROTOCOL_VERSION,
			seed: 11.0,
			player_id: 1,
			spawn: [0.5, 140.5, 0.5],
			server_tick: 60,
			snapshot_divisor: 3,
			session_token: [0; 16],
			sim_version: digblock_core::SIM_VERSION,
			build: "test".into(),
			tod: 0.0,
			spawn_look: [0.0, 0.0, 0.0],
		}
	}

	/// The join's loading cover is the hold: `Welcome` no longer closes the
	/// Connecting screen itself, so a `Welcome` that arms nothing strands
	/// the player on "Joining world..." forever — the 5 s escape hatch
	/// cannot fire on a hold that was never set.
	///
	/// This shipped broken because `begin_arrival_hold` asked
	/// `self.net.is_some()`, and its only two call sites are `pump_net`
	/// arms — where the connection is owned by value (`self.net.take()`)
	/// and the field always reads `None`. So drive the real event path
	/// rather than calling the helper: a unit test of the helper alone
	/// passes against exactly the bug it is meant to catch.
	#[test]
	fn welcome_arms_the_arrival_hold_and_area_ready_releases_it() {
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);

		let (client_end, mut server_end) = digblock_net::loopback::loopback_pair();
		engine.set_transport_with_token(Box::new(client_end), [0; 16]);
		server_end.send_reliable(&encode_preamble(PROTOCOL_VERSION));
		server_end.send_reliable(&frame(&welcome()));
		engine.update(Duration::from_millis(16));
		assert_eq!(
			engine.seed, 11.0,
			"the Welcome was actually processed, not swallowed"
		);
		assert!(
			engine.arrival_hold.is_some(),
			"a join must hold the world until the server says the area is real"
		);

		server_end.send_reliable(&frame(&ServerMsg::AreaReady));
		engine.update(Duration::from_millis(16));
		assert!(
			engine.arrival_hold.is_none(),
			"the marker releases the hold"
		);
	}

	/// The escape hatch. A server that never sends the marker must not be
	/// able to strand a player on the loading screen.
	#[test]
	fn the_arrival_hold_times_out_without_the_marker() {
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);

		let (client_end, mut server_end) = digblock_net::loopback::loopback_pair();
		engine.set_transport_with_token(Box::new(client_end), [0; 16]);
		server_end.send_reliable(&encode_preamble(PROTOCOL_VERSION));
		server_end.send_reliable(&frame(&welcome()));
		engine.update(Duration::from_millis(16));
		assert!(engine.arrival_hold.is_some(), "held on arrival");

		// Just past `AREA_SYNC_TIMEOUT_SECS` worth of 16 ms frames.
		let frames = (digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS / 0.016).ceil() as u32 + 1;
		for _ in 0..frames {
			engine.update(Duration::from_millis(16));
		}
		assert!(
			engine.arrival_hold.is_none(),
			"the wait ran out and the world was entered anyway"
		);
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

		// §6 pinned the tool slot: a legacy layout that parked the tool
		// mid-bar migrates on decode — the parked spot empties and the
		// tool comes back at the end; blocks keep their slots.
		let b3 = BlockType::try_from_id(3).unwrap();
		assert_eq!(engine.hotbar.slots()[0], None, "parked spot empties");
		assert_eq!(engine.hotbar.slots()[4], Some(HotbarItem::Block(b3)));
		assert_eq!(
			engine.hotbar.slots()[HOTBAR_SLOTS - 1],
			Some(HotbarItem::WaterTool),
			"the tool slot is pinned to the end"
		);

		// Suspend hands the MIGRATED arrangement back to the store.
		engine.on_suspend();
		let mut migrated = [WIRE_EMPTY; HOTBAR_SLOTS];
		migrated[4] = 3;
		migrated[HOTBAR_SLOTS - 1] = WIRE_TOOL;
		assert_eq!(calls.borrow().saved_hotbar, migrated.to_vec());
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
	/// against. Both outline; neither ever draws the dig crack.
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

#[cfg(test)]
mod craft_gate_tests {
	use super::*;
	use digblock_core::skills::{total_xp_for, Skill};
	use digblock_core::unlock::Kind;

	/// A torch — craft-only, Alchemy 8 — two blocks ahead.
	fn torch_ahead() -> (Engine, (i32, i32, i32)) {
		block_ahead(BlockType::Torch)
	}

	/// A generated world with one block floating in open air two blocks
	/// ahead of the camera. Open sky well above spawn, so a worldgen
	/// retune can't move terrain into the shot.
	pub(super) fn block_ahead(block: BlockType) -> (Engine, (i32, i32, i32)) {
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
		let p = engine.player.pos;
		let cell = (
			p.x.floor() as i32,
			p.y.floor() as i32 + 20,
			p.z.floor() as i32,
		);
		assert_eq!(
			engine.pipeline.block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Air),
			"the shot must start empty"
		);
		engine
			.pipeline
			.set_block(cell.0, cell.1, cell.2, block)
			.expect("chunk ready");
		// Eye at the cell's mid-height, dead on the post's axis.
		engine.camera.pos = Vec3::new(
			cell.0 as f32 + 0.5,
			cell.1 as f32 + 0.5,
			cell.2 as f32 - 2.5,
		);
		engine.camera.yaw_deg = 0.0; // +z
		engine.camera.pitch_deg = 0.0;
		(engine, cell)
	}

	/// The craft gate, client half: a torch is somebody's crafted item, so
	/// a player who cannot make one cannot pick one up either — the hold
	/// never opens, the outline never draws, and holding the button says
	/// why exactly once. The gate is the recipe's own level, not a level
	/// near it.
	#[test]
	fn a_craft_only_block_refuses_the_dig_until_its_recipe_opens() {
		let (mut engine, cell) = torch_ahead();
		engine.update_target();
		assert_eq!(
			engine.target.map(|h| h.cell),
			Some(cell),
			"aimed at the torch"
		);
		assert!(
			!engine.diggable(&engine.target.unwrap()),
			"Alchemy 1 cannot make a torch, so it cannot take one"
		);
		assert_eq!(
			engine.dig_progress().map(|t| t.cell),
			None,
			"and it does not even outline"
		);

		// Holding the button explains itself, once per aim.
		engine.dig_held = true;
		engine.step_dig();
		engine.step_dig();
		assert!(engine.dig.is_none(), "no hold opened");
		let spoken = |e: &Engine| {
			e.feed
				.iter()
				.filter(|(t, ..)| t.as_str() == Kind::Recipe.message())
				.count()
		};
		assert_eq!(spoken(&engine), 1, "latched: {:?}", engine.feed);

		// Aiming away and back is what clears the latch in play.
		engine.dig_held = false;
		engine.refused_cell = None;

		engine.skills.set_total(Skill::Alchemy, total_xp_for(7));
		engine.update_target();
		assert!(
			!engine.diggable(&engine.target.unwrap()),
			"one level short of the recipe is still short"
		);

		engine.skills.set_total(Skill::Alchemy, total_xp_for(8));
		engine.update_target();
		assert!(
			engine.diggable(&engine.target.unwrap()),
			"Alchemy 8 makes torches, so it takes them too"
		);
		assert_eq!(
			engine.dig_progress().map(|t| t.cell),
			Some(cell),
			"and the outline is back"
		);
		engine.dig_held = true;
		engine.step_dig();
		assert_eq!(
			engine.dig.as_ref().map(|d| d.cell),
			Some(cell),
			"the hold opens"
		);
	}
}

#[cfg(test)]
mod dig_decay_tests {
	use super::craft_gate_tests::block_ahead;
	use super::*;
	use digblock_core::skills::DIG_DECAY_TICKS;

	/// Aimed at a diggable block with the button down, stepped `n` times.
	fn hold(engine: &mut Engine, n: u32) {
		engine.dig_held = true;
		for _ in 0..n {
			engine.update_target();
			engine.step_dig();
		}
	}

	/// Button up, stepped `n` times — the decay's only clock.
	fn release(engine: &mut Engine, n: u32) {
		engine.dig_held = false;
		for _ in 0..n {
			engine.update_target();
			engine.step_dig();
		}
	}

	/// The crack's own number, as `dig_progress` hands it to the
	/// highlight instance.
	fn web(engine: &Engine) -> Option<f32> {
		engine.dig_progress().and_then(|t| t.progress)
	}

	/// Letting go part-way no longer bins the hold: it bleeds back to
	/// untouched over a second, the crack retreats with it (same
	/// number, one source), and picking the button back up resumes from
	/// what is left instead of from zero.
	#[test]
	fn releasing_a_hold_bleeds_it_out_instead_of_binning_it() {
		let (mut engine, cell) = block_ahead(BlockType::Dirt);
		engine.update_target();
		assert_eq!(engine.target.map(|h| h.cell), Some(cell), "aimed at it");

		// Open the hold (the opening step banks nothing), then bank half
		// of what the block needs.
		hold(&mut engine, 1);
		let need = engine.dig.as_ref().expect("hold opened").required;
		assert!(need >= 8, "dirt should be a real hold, not {need} ticks");
		hold(&mut engine, u32::from(need) / 2);
		let peak = engine.dig.as_ref().unwrap().ticks;
		assert_eq!(peak, need / 2, "banked what was held");
		let full = web(&engine).expect("the web is drawn while digging");

		// Release: the record survives, and both the banked ticks and
		// the web fall together. (Ten ticks, not one — the decay
		// truncates down, so a block needing under 60 ticks loses
		// nothing on the very first.)
		release(&mut engine, 10);
		let d = engine.dig.as_ref().expect("the release kept the hold");
		assert_eq!(d.ticks, peak, "the peak is not rewritten");
		assert_eq!(d.idle, 10, "the decay started");
		let after = web(&engine).expect("the web is still drawn");
		assert!(after < full, "the web should retreat: {after} vs {full}");
		assert!(
			(after - f32::from(d.banked()) / f32::from(need)).abs() < 1e-6,
			"the web draws the decayed number, not a second one"
		);

		// A full window of release erases it — record and web together.
		release(&mut engine, DIG_DECAY_TICKS);
		assert!(engine.dig.is_none(), "a spent hold is dropped");
		assert_eq!(web(&engine), None, "and the block outlines unbroken");
	}

	/// Half a second off the button costs half a BAR, not half of what
	/// was banked — which is what makes one second the ceiling for every
	/// block rather than a flat wait — and the resumed hold completes in
	/// what is still owed.
	#[test]
	fn a_resumed_hold_keeps_what_the_decay_left() {
		let (mut engine, cell) = block_ahead(BlockType::Dirt);
		engine.update_target();
		hold(&mut engine, 1);
		let need = engine.dig.as_ref().expect("hold opened").required;
		hold(&mut engine, u32::from(need) - 1);
		assert_eq!(
			engine.dig.as_ref().unwrap().ticks,
			need - 1,
			"one tick from breaking"
		);

		let idle = DIG_DECAY_TICKS / 2;
		release(&mut engine, idle);
		let left = engine.dig.as_ref().expect("still held").banked();
		assert_eq!(
			u32::from(left),
			digblock_core::skills::dig_decayed(u32::from(need - 1), u32::from(need), idle),
			"the shared rule, not a second implementation"
		);
		assert!(left > 0 && left < need / 2 + 1, "about half a bar gone");

		// Resuming inherits it: the opening step banks nothing, so the
		// block breaks exactly `need - left` steps later.
		hold(&mut engine, 1);
		assert_eq!(
			engine.dig.as_ref().unwrap().ticks,
			left,
			"picked up at {left}"
		);
		hold(&mut engine, u32::from(need - left) - 1);
		assert_eq!(
			engine.pipeline.block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Dirt),
			"the resumed hold still owed a tick"
		);
		hold(&mut engine, 1);
		assert_eq!(
			engine.pipeline.block_at(cell.0, cell.1, cell.2),
			Some(BlockType::Air),
			"the resumed hold broke it"
		);
	}

	/// Aiming at a DIFFERENT block is not a release — the old hold is
	/// gone outright, because only one cell can be decaying and it must
	/// be the one under the crosshair.
	#[test]
	fn retargeting_drops_the_old_hold_outright() {
		let (mut engine, cell) = block_ahead(BlockType::Dirt);
		engine.update_target();
		hold(&mut engine, 10);
		assert!(engine.dig.as_ref().unwrap().ticks > 0, "banked something");

		// Put a second block in front of the first, so the same ray now
		// stops one cell short: a fresh hold at zero.
		let other = (cell.0, cell.1, cell.2 - 1);
		engine
			.pipeline
			.set_block(other.0, other.1, other.2, BlockType::Dirt)
			.expect("chunk ready");
		hold(&mut engine, 1);
		let d = engine.dig.as_ref().expect("the new hold opened");
		assert_eq!(d.cell, other, "on the other block");
		assert_eq!((d.ticks, d.idle), (0, 0), "from scratch");
	}
}
