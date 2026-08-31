use digblock_core::worldgen::Seed;
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

use crate::camera::{FlyCamera, LookAngles};
use crate::renderer::{FrameScene, FrameUniforms, RenderConfig, Renderer};
use crate::ui::screens::inventory::InvAction;
use crate::voice::{Msg, Spell};

/// Who drives the camera pose. Presentation only: the player sim's
/// rules never change with the mode, and the server never hears of it.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum CameraMode {
	/// Camera = the player's presented eye (the shipping game).
	FirstPerson,
	/// Free 6-DoF debug camera; `player.pos` goes stale. F, offline only.
	DebugFly,
	/// A rig drives the camera; the player keeps simulating, standing
	/// still, and `fp_eye` stays the body anchor. Pure presentation.
	Cinematic(crate::camrig::Cinematic),
}

impl CameraMode {
	pub(crate) fn first_person(&self) -> bool {
		matches!(self, CameraMode::FirstPerson)
	}
}

mod config;
pub use config::{DebugScreen, EngineConfig, parse_held_item};
pub mod knobs;

mod actions;
mod audio_step;
mod causeway;
mod events;
mod frame;
mod input;
mod net_sync;
mod present;
mod progression;
mod setup;
mod stare;
mod streaming;
mod update;
mod world_session;
mod worldjobs;

mod carried;
use carried::{CARRIER_DEBUG, CARRIER_SELF, Carried, Remote, cell_center, pack_rgb, push_remote};
mod debug_plans;
pub use debug_plans::parse_node_kind;
use debug_plans::{
	debug_crack_plan, debug_dive_plan, debug_doors_plan, debug_node_plan, debug_ore_plan,
	debug_river_plan, debug_roof_plan, debug_shapes_plan, debug_torches_plan, debug_tunnel_plan,
	debug_vault_plan, quantized_facing,
};
pub use stare::STARE_FRAMES;

/// The launcher-injected clipboard writer, in the same shape as
/// [`crate::settings::SettingsStore`]: the engine raises an intent, the
/// launcher owns the platform call. Write-only — nothing in this project
/// pastes, and adding a read would mean giving the game a way to see what
/// the player copied somewhere else.
pub(crate) type ClipboardSink = Box<dyn FnMut(&str)>;

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
/// (`EngineConfig::stream_budget_ms`). The first half bounds the result drain
/// (chunk inserts + neighbor cascades), the rest bounds mesh uploads, leftovers
/// carried in `pending_uploads`. Workers keep producing at full speed, so this
/// trades a longer fill tail for streaming + render fitting one vsync interval.
const STREAM_BUDGET_MS: f32 = 3.0;
/// Uploads always make this much progress per frame even with the budget
/// exhausted, so a saturated drain can never starve the upload queue. Sized to a
/// chunk stack's worth of the player's own column: the guarantee only has to
/// cover where the player IS, and the queue is nearest-first, so anything beyond
/// that is the far ring choosing to wait rather than to hitch.
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
/// The same, once the backlog is a WORLD leaving rather than a rim crescent — see
/// [`rim_backlog`]. Still a cap and not a free hand: releasing a whole disc at once is thousands
/// of buffer destroys, which is a stall on the web rather than a bill that idles.
const EVICT_BURST: usize = 2048;

/// Evicted chunks a rim crescent can plausibly leave, above which a backlog is an ARRIVAL: a
/// teleport, a draw-distance cut, a world swap.
///
/// The per-frame cap exists because a retired buffer merely idles until it is dropped, which is
/// true of a crescent and false of a world: the old world's meshes stay live beside the new one's
/// for as long as the drain takes, the cull walks both every frame, and at [`EVICT_MIN`] a
/// full disc takes thousands of frames to clear. Crossing one boundary retires a column per row of
/// the disc, so a crescent is O(draw distance) columns where a disc is O(draw distance²); the
/// margin is several crossings' worth, since a diagonal run crosses on both axes and the queue is
/// drained lazily behind them.
fn rim_backlog(draw_dist: i32) -> usize {
	const CROSSINGS: usize = 4;
	let rows = 2 * draw_dist.max(0) as usize + 1;
	CROSSINGS * rows * digblock_core::chunk::WORLD_VCHUNK as usize
}
/// Share of one display interval `pump_net` may spend applying authoritative edits. A COUNT could
/// not express it: what one edit costs swings by more than an order of magnitude — whether its
/// chunk is still shared decides a whole-chunk copy-on-write — and the mesh jobs it queues are
/// charged to this same thread later in the same frame, Web Worker pool or not. Pricing the slice
/// in TIME makes the same code back off on the platform that cannot afford it.
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
const FEED_MAX: usize = 16;
/// How long a message stays lit before it fades out of the HUD. It stays
/// in `feed` afterwards with a non-positive ttl — see [`FEED_TTL_FLOOR`].
const FEED_TTL_SECS: f32 = 15.0;
/// Expired entries stop counting down here. Without a floor the ttl of a
/// long-lived message would run away into large negatives; nothing reads
/// the magnitude, only the sign, so it just needs to stay finite.
const FEED_TTL_FLOOR: f32 = -1.0;
/// How long between lines of a QUEUED burst. The admin catalog arrives as
/// a dozen replies in one drain and a wall of them is unreadable; nothing
/// else queues, so an answer to something you typed still lands at once.
const FEED_DRIP_SECS: f32 = 0.5;

pub struct Engine {
	config: EngineConfig,
	/// Resolved world seed (config.seed or a random one).
	seed: Seed,
	renderer: Option<Renderer>,
	pipeline: Pipeline,
	camera: FlyCamera,
	/// Who drives `camera` this frame (see [`CameraMode`]).
	mode: CameraMode,
	/// The player's facing — input writes here; modes copy it out.
	look: LookAngles,
	/// The player's PRESENTED eye this frame (what first person shows) —
	/// the body anchor a detached camera leaves behind.
	fp_eye: Vec3,

	/// The interactive walkthrough (armed on offline world entry).
	tutorial: crate::tutorial::Tutorial,
	/// Did the player jump off the ground this frame (tutorial poll)?
	tutorial_jumped: bool,
	/// The staged siphon reveal: cues of (seconds into the cinematic,
	/// subject cell) still owed a `remesh_near`, the clock they read,
	/// and whether the world-wide sweep still waits for the camera to
	/// come home. All idle outside the one scripted cinematic.
	reveal_cues: Vec<(f32, (i32, i32, i32))>,
	reveal_clock: f32,
	reveal_finale: bool,
	player: Player,
	/// See [`InputState`].
	input: InputState,
	/// The sky cull's column-top tally (`gi::sky_floor`'s input), rebuilt
	/// INCREMENTALLY: a few column walks per frame into the stage, adopted
	/// whole when a pass completes. Continuous because no cheap key says
	/// when the answer changed — residency counts slots before terrain
	/// lands, and edits change heights with no key at all — so streaming,
	/// digging and flying all register within one pass.
	sky_tally: Vec<i32>,
	sky_stage: Vec<i32>,
	/// Next cell of the sampling grid; a completed pass wraps it to zero.
	sky_cursor: usize,
	/// Block position the running pass samples around, fixed when the
	/// pass starts so a moving camera cannot shear one tally.
	sky_origin: (i32, i32),
	/// See [`BenchState`].
	bench: BenchState,
	tick_accum: f32,
	sun: Sun,
	/// A lightless character's held dusk — paints `sun` while active
	/// (see [`crate::dusk`]).
	dusk_hold: crate::dusk::DuskHold,
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
	/// In-progress hold-to-dig, advanced at the fixed sim rate while
	/// `dig_held` stays on a valid, unchanged target.
	dig: Option<DigProgress>,
	/// Seconds the wisp's self-light keeps burning; zero = no wisp.
	/// Session-local on purpose — a buff is not a possession, so nothing
	/// persists or syncs it, and a world swap puts it out.
	wisp_left: f32,
	/// Seconds the causeway keeps laying planks; zero = no road.
	/// Session-local for the wisp's reason — but the PLANKS are ordinary
	/// cells, so leaving a world takes the road up rather than dropping
	/// it, and `causeway` is what remembers which cells are ours to take.
	causeway_left: f32,
	causeway: digblock_core::causeway::Trail,
	/// The last spell SENT, so a `CastRefused` — which carries a reason
	/// and never a spell — can be aimed at the prediction it actually
	/// answers. Without it any refused cast takes the road up, and a
	/// kindle you could not afford drops you off your own causeway.
	last_cast: Option<CastTarget>,
	/// In-progress cast channel, advanced at the same fixed rate. A
	/// caster may walk while channelling; a dig hold or surface
	/// opening resets it — the decay mold is deliberately NOT borrowed
	/// here (that is a dig's, and later the damage-interrupt's).
	cast: Option<CastState>,
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
	/// Essence counts as of the last AUTHORITATIVE inventory (the
	/// server's base, before pending predictions are re-applied), so a
	/// rebase can name what a draw paid by diffing against the previous
	/// base. Diffing against the live inventory instead would toast a
	/// refused cast's fare coming back, since fares are predicted
	/// locally and never ride `pending`. `None` until the join sync's
	/// first inventory, which is state arriving rather than a find.
	auth_essence: Option<[u32; digblock_core::block::Essence::COUNT]>,
	/// Whether this session already fired the siphon-affordability nudge
	/// (which arms the siphon walkthrough chapter) — once per world
	/// session, and the ownership check silences it for good the moment
	/// one is made.
	siphon_nudged: bool,
	hotbar: Hotbar,
	/// The spawn structure still owed to an offline world: the stamp's
	/// writes, drained as their chunks become resident. Named saves only
	/// — a server stamps its own, and a backdrop world is ephemeral.
	pending_stamp: Vec<digblock_core::waystones::Write>,
	/// Protected arrival regions this engine ENFORCES — offline only,
	/// where the pipeline is the authority and the spawn sphere is derived
	/// at world entry. Empty in multiplayer on purpose: a server's regions
	/// are its own secret and refusals arrive as `DigRefused`.
	protected: Vec<digblock_core::protect::Region>,
	/// Edit-time water flow (digblock-core fluid rules); idle unless
	/// edits schedule cells.
	fluid: digblock_core::fluid::FluidSim,
	/// Clock-scheduled sweeps, run here only OFFLINE — connected, the
	/// server owns them and their results arrive as ordinary edits.
	world_jobs: digblock_core::worldjob::Scheduler,
	/// The day's head-count for the jobs that scale by it — one here,
	/// kept on the server's tracker so the rule does not fork.
	presence: digblock_core::presence::RollingPeak,
	flow_accum: f32,
	/// Shader animation clock, wrapped on a long period (carried in the
	/// otherwise unused light_color.w — no uniform layout change).
	time_secs: f32,
	/// Unwrapped world clock feeding the job scheduler. The shader clock
	/// above wraps on a period shorter than a game day, and a Schedule
	/// slot never advances again against a wrapped clock.
	world_secs: f64,
	/// See [`NetSession`].
	session: NetSession,
	/// See [`Vfx`].
	vfx: Vfx,
	/// See [`StreamQueues`].
	stream: StreamQueues,
	/// Memoized worldgen surface height for the ambient-zone test, keyed by
	/// the camera's block column.
	ambient_height: Option<((i32, i32), i32)>,
	/// See [`UiState`].
	ui: UiState,
	/// Per-skill XP totals: adopted from the server online
	/// (`Skills`/`XpGain` — never client-claimed), computed through the
	/// same core award functions offline.
	skills: digblock_core::skills::SkillSet,
	/// See [`SaveSession`].
	save: SaveSession,
	/// The inventory-grid arrangement behind the hotbar (slot
	/// assignments; counts stay in `inventory`). Kept consistent by
	/// `reconcile_layout` every frame.
	inv_grid: digblock_core::inv_grid::InvGrid,
	/// See [`AudioState`].
	audio: AudioState,
	/// Launcher-owned HTTP/probe services the menu talks to; None = no
	/// master URL configured (Multiplayer shows as disabled).
	services: Option<crate::menu::MenuServices>,
	/// The account's standing on the master's boards, fetched each time
	/// the character screen opens; `None` until an answer lands, and the
	/// screen simply says nothing about rank.
	ranks: Option<Vec<crate::menu::RankView>>,
	/// The hiscores screen's board: what is in view, the last answer, and
	/// whether one is outstanding. Fetched on open and on every tab or page.
	board: crate::ui::screens::hiscores::BoardModel,
	/// Launcher-injected settings persistence hook, called with each
	/// change batch; None (bench, drill, iOS) = settings don't persist.
	settings_store: Option<crate::settings::SettingsStore>,
	/// Launcher-injected clipboard writer; see `set_clipboard`.
	clipboard: Option<ClipboardSink>,
	/// DIG_SHAPES=1 showcase cells not yet placed (chunks stream in
	/// asynchronously; each update retries what's left). Empty = done
	/// or disabled.
	debug_shapes_pending: Vec<(i32, i32, i32, u16)>,
	/// DIG_SPHERE=<r>: a sphere still waiting for every cell it would touch
	/// to be resident. `None` = landed or disabled.
	debug_sphere_pending: Option<i32>,
	/// Every sphere climbing in this world — ours and each remote's, one
	/// per caster — stepped with the sim ticks, so every sphere's shell
	/// climbs at its own caster's pace and unwinds on their own break.
	sphere_climbs: Vec<SphereClimb>,
}

/// What an offline cast paid, settled at the seal — the blocks the climb declined come back —
/// or handed back whole on an unwind. Online the server's `InventoryFull` is the settlement, so
/// a connected climb carries none, and DIG_SPHERE paid nothing.
struct SphereBill {
	debited: u32,
	fares: [(digblock_core::block::Essence, u32); 2],
}

/// One sphere mid-climb: whose it is, the plan it is walking, how far
/// through that plan this end has got, and what an unwind hands back.
struct SphereClimb {
	/// The caster. `None` where no player ids exist at all — offline,
	/// where a climb can only be ours — so one comparison against
	/// `NetSession::my_player_id` answers "is this mine" on both sides
	/// of the wire.
	by: Option<digblock_net::protocol::PlayerId>,
	closing: digblock_core::sphere::Closing,
	head: (i32, i32, i32),
	/// The block the shell is built of — the caster's held block, carried by the wire for a
	/// remote's climb since a stranger's hand is not in the snapshot.
	block: digblock_core::block::BlockType,
	/// How much of the plan this end has handed to the world: a PREFIX
	/// length in the plan's own order, which is the currency
	/// `ServerMsg::SphereEnded` quotes its own count in — neither end can
	/// derive which cells the other declined.
	prefix: usize,
	/// Cells of the shell this end actually wrote — what the bill settles against.
	placed: u32,
	/// Only an offline cast carries one, so a refund can never answer for somebody else's sphere.
	bill: Option<SphereBill>,
}

impl SphereClimb {
	/// Ours? See [`SphereClimb::by`] for why one comparison covers both
	/// offline play and a connected world.
	fn own(&self, me: Option<digblock_net::protocol::PlayerId>) -> bool {
		self.by == me
	}

	/// The cell the shell is built of.
	fn wall(&self) -> u16 {
		digblock_core::chunk::make_cell(self.block, 0)
	}
}

/// Client-side hold-to-dig progress (mirrors the server's `DigState`;
/// purely predictive — the server re-times the interval itself).
/// Which surfaces were open last frame, plus the hotbar selection — the
/// delta drives every open/close/select cue from ONE comparison, so no
/// individual toggle path needs a sound hook.
#[derive(Clone, Copy, PartialEq)]
struct UiSnapshot {
	menu: bool,
	inv: bool,
	alch: bool,
	character: bool,
	hiscores: bool,
	spellbook: bool,
	chat: bool,
	hotbar: usize,
}

/// Everything the engine needs to decide what to play. The sink is
/// injected, and a `None` sink is what covers bench, screenshot,
/// drill and iOS runs.
struct AudioState {
	/// Injected audio backend; None (bench, iOS) makes every emit free.
	sink: Option<Box<dyn crate::audio::AudioSink>>,

	/// Waterfall loop-voice reconciler (fed by mesh impacts).
	water_audio: crate::water_audio::WaterAudio,

	/// Footstep cadence + ambient-bed state (pure helpers).
	footsteps: crate::audio::Footsteps,

	swim_strokes: crate::audio::SwimStrokes,

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

	/// Last frame's open-surface snapshot; None until seeded, so the
	/// surfaces a session starts on never chirp.
	ui_snapshot: Option<UiSnapshot>,

	/// Frame budgets for `react` cues, reset each `step_audio`.
	frame_oneshots: u8,
	frame_remote_edits: u8,
}

/// Work the pipeline has produced and the renderer has not yet taken.
/// Drained under one per-frame deadline, so streaming can never hitch
/// the render loop.
struct StreamQueues {
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

	/// The release queue is draining an ARRIVAL rather than a rim crescent ([`rim_backlog`]).
	/// A latch, not a re-test: it is what makes the GI mirror wipe fire ONCE per arrival instead
	/// of on every frame the backlog stays over the line.
	evict_burst: bool,

	/// Adaptive streaming-budget controller (web/inline builds; the fixed
	/// config budget elsewhere — see `EngineConfig::adaptive_stream`).
	stream_budget: digblock_core::stream_budget::StreamBudget,
}

/// Presentation-only motion: nothing here is read by gameplay, and
/// none of it is authoritative. Reset when the world under it is
/// replaced.
struct Vfx {
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

	/// Generic bursts (debris/splash/sparkle/poof) — confetti's contract
	/// exactly: engine-aged instances, each carrying its life in lane 6.
	bursts: Vec<crate::bursts::BurstInstance>,
	burst_seq: u32,

	/// Camera feel (shake/kick/pulse); applied on the FINAL view matrix.
	camera_fx: crate::camera_fx::CameraFx,

	/// The live essence draw, aged by `step_siphon`. World state for the
	/// same reason confetti is: it names a cell, and a world swap must
	/// forget it.
	siphon: Option<SiphonDraw>,

	/// Live teleport bursts (departure/arrival), aged in `update` — world
	/// state, exactly as confetti is.
	casts: Vec<TeleportFx>,

	/// Burst counter — the per-effect seed, confetti's arrangement.
	cast_seq: u32,

	/// Every kindled light still burning, each on its own countdown —
	/// yours and anyone's, a join's backlog included. World state like
	/// confetti; and the LIGHT half rides the carried list under its own
	/// key bank, so a light going out re-places the probes exactly as a
	/// torch put away does.
	kindled: Vec<KindledLight>,

	/// Key counter for [`Self::kindled`] — a STABLE carrier key per
	/// light, since an index would shift under every expiry and make
	/// `Carried` re-place lights that never moved.
	kindle_seq: u16,

	/// The channel aura's fade envelope: opens while a cast channels,
	/// dissolves when one ends or breaks — an interrupted channel fades
	/// rather than pops.
	channel_env: crate::anim::Envelope,

	/// The teleport instant's full-screen flash, decayed in `update`.
	flash: f32,

	/// The arrival pads this world marks with a standing spiral — spawn
	/// today, the waystones when they land. Set at world entry.
	marker_sites: Vec<Vec3>,

	/// Per-frame packed scratch for the two instanced effects (reused
	/// allocations, the nametag arrangement).
	fx_scratch: Vec<crate::teleport::FxInstance>,
	marker_scratch: Vec<crate::waystone_fx::MarkerInstance>,

	/// Held-item animation. Advanced in `update`, NOT in `step_audio` —
	/// that returns early with no audio sink attached (bench, screenshot,
	/// iOS), which would silently freeze the hand in exactly the
	/// configurations that photograph it.
	view_bob: crate::viewmodel::Bob,

	view_swing: crate::viewmodel::Swing,

	/// The casting gesture, strength-driven by the channel's envelope so
	/// hand and aura rise and dissolve together.
	view_cast: crate::viewmodel::Cast,

	/// Owns which item the hand is DRAWING — it lags `hotbar.selected`
	/// by half a swap, so the exchange happens below the frame.
	view_swap: crate::viewmodel::Swap,

	/// Live "+N SKILL" / "+N ESSENCE" toasts above the hotbar
	/// (coalescing per label).
	toasts: Vec<crate::ui::hud::Toast>,

	/// Per-frame name-tag scratch (reused allocation).
	/// Name tags, kept beyond `nametag_count` so their String buffers
	/// survive between frames instead of being reallocated every frame.
	nametag_scratch: Vec<crate::renderer::NameTag>,

	nametag_count: usize,

	/// Per-frame remote-player sampling scratch.
	remote_scratch: Vec<digblock_net::remote::RemoteDraw>,

	remote_instances: Vec<[f32; 8]>,

	held_instances: Vec<(crate::ui::hud::IconKind, [f32; 8])>,

	history_scratch: Vec<f32>,

	carried: Carried,
}

/// The local-world half: which save is open, when the next player
/// snapshot is owed, and the spawn-return cooldown. A rejoin resumes
/// in place, so the cooldown persists with the position.
struct SaveSession {
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

	/// Offline port anchors, resolved rung by rung as casts ask for them
	/// and kept for the session — the ring walks are pure but the far
	/// ones are not cheap, and a session casts to the same few places.
	offline_waystones: digblock_core::waystones::Roster,

	/// The world's spawn point (the server's after a Welcome swap):
	/// the return-to-spawn target.
	spawn_pos: Vec3,

	/// Seconds left on the teleport cooldown (0 = ready) — one clock over
	/// Return Home and every port, so hops cannot be chained. One gate
	/// for local and connected play; connected, the server also enforces
	/// its own copy.
	teleport_cooldown: crate::anim::Cooldown,

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
	arrival_hold: crate::anim::Timer,
}

/// Held keys, accumulated look, and the touch driver. While ANY UI
/// surface is open, cursor and left-button are synthesized from the
/// primary touch at the top of `handle_input`.
struct InputState {
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

	/// The live rebindable-key table (the persisted `Settings::bindings`,
	/// re-copied by `apply_settings`). Every key event resolves through it
	/// before the fixed fallback arm; the texts that say "press K" read it
	/// too, so a hint can never name a dead key.
	bindings: crate::bindings::Bindings,

	/// The screenshot bind fired; the shell drains it right after this
	/// input dispatch (`Game::take_screenshot_request`), inside the
	/// keypress's own task like the fullscreen request.
	screenshot_requested: bool,

	/// Shell-forwarded display cutouts the touch HUD keeps clear of.
	safe_area: crate::ui::hud::SafeArea,

	fly_key_down: bool,

	overlay_key_down: bool,

	/// F7 edge latch — the GI ratchet drill fires once per press.
	gi_drill_key_down: bool,

	keys: HeldKeys,

	look_accum: (f32, f32),

	/// Left mouse button currently held (hold-to-dig); right stays an edge.
	dig_held: bool,

	click_place: bool,

	/// The tool slot's handover animation — presentation only, and it
	/// watches `hotbar.active_tool()` rather than being told, so every
	/// way the tool can change animates the same way.
	tool_swap: crate::hotbar::ToolSwap,
}

/// The multiplayer session. `client` is `None` in a local world and
/// every field beside it is meaningless while it is, which is why
/// `reset_world_transients` clears them together.
struct NetSession {
	/// Multiplayer client; None = single-player (every connected branch
	/// below is dead in that case).
	client: Option<digblock_net::NetClient>,

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

	/// The server's resolved waystones for THIS world, from join sync —
	/// `(SurfaceKind index, eye)`. The spellbook's roster and the
	/// standing spirals' anchors; empty offline (ports derive lazily).
	waystones: Vec<(u8, [f32; 3])>,

	/// Reconciliation health, logged on a slow cadence while connected.
	net_corrections: u32,

	net_error_accum: f32,

	net_log_timer: f32,

	/// Server-message feed (join/leave/disconnect/chat) as
	/// (text, ttl, 0xAABBGGRR color), drawn by the overlay
	/// above the HUD; (text, seconds left) — fades over the last second.
	feed: Vec<(crate::voice::Msg, f32, u32)>,

	/// Lines waiting their turn — see [`FEED_DRIP_SECS`]. Separate from
	/// `feed` because an entry here has no ttl yet: it starts fading when
	/// it is SHOWN, not when it arrived.
	feed_queue: std::collections::VecDeque<(crate::voice::Msg, u32)>,

	/// Seconds until the next queued line is let in. Parked at zero while
	/// the queue is empty, so a burst's first line never waits.
	feed_drip: f32,

	/// id → name, learned from PlayerJoined (roster + live joins), so
	/// PlayerLeft (id-only on the wire) can be displayed by name.
	player_names: rustc_hash::FxHashMap<u8, String>,

	/// Our own id from Welcome. The roster never includes ourselves, so
	/// our chat echo resolves through this + config.player_name.
	my_player_id: Option<u8>,

	/// The last layout wire pair sent to the server — any difference
	/// triggers a `ClientMsg::Layout` (before this frame's edits, so the
	/// server's slot enforcement always sees the current arrangement).
	last_layout_sent: Option<(Vec<u16>, Vec<u16>)>,
}

/// Measurement, and the scripted flight `--fly`/`--walk` drive. None
/// of it runs in an ordinary session.
struct BenchState {
	metrics: Metrics,

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

	/// Scripted-flight clock (bench mode), in fixed sim ticks.
	bench_t: f32,

	/// Steps of the scripted flight already taken, against `config.fly`.
	fly_frames: u32,

	/// Where `config.walk`'s oscillation is anchored: the camera's pose at
	/// the first step, latched so `--fly` and `--warp` share it.
	walk_origin: Option<Vec3>,

	/// Frames through `update`, for `--gi-drill-at`.
	frames_seen: u32,

	/// Where `config.stare`'s stations are laid out from: the camera's
	/// pose at the first step, i.e. spawn.
	stare_origin: Option<Vec3>,

	/// The stare station in progress, so a change of station marks the
	/// metrics and re-arms the placement exactly once.
	stare_station: Option<u32>,

	/// Whether the current station's glass block has landed (see
	/// `stare_step` — the placement retries until the column exists).
	stare_placed: bool,
}

/// The screens and the frame's UI scratch. A frame has exactly one
/// text size and one panel colour over one dim; `ui::theme` owns both
/// and every screen is swept for them.
struct UiState {
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

	/// A capture's standing hover: the tile the cursor is parked on once
	/// a frame has laid it out (`DebugScreen::*Hover`). Cleared when fed.
	debug_hover: Option<crate::ui::InvId>,

	/// The alchemy screen (K toggles): block → sub-material conversion
	/// and shaped-item crafting.
	alch_screen: crate::ui::screens::alchemy::AlchScreen,

	/// The character screen (C toggles): per-skill levels + XP bars.
	char_screen: crate::ui::screens::character::CharScreen,

	/// The hiscores screen (H toggles): one page of one of the master's boards.
	hiscores: crate::ui::screens::hiscores::HiscoresScreen,

	/// The spellbook screen (J toggles): Arcana's casting surface.
	spellbook: crate::ui::screens::spellbook::SpellScreen,

	/// Startup/pause menu state (pure; overlay renders its view).
	menu: crate::menu::Menu,

	/// Chat input line (pure; overlay renders its view).
	chat: crate::chat::Chat,

	overlay_visible: bool,
}

/// What a channel is winding up toward.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CastTarget {
	Home,
	/// The port at this `SurfaceKind` declaration-order index.
	Waystone(u8),
	/// The timed self-light.
	Wisp,
	/// Conjured light in the cell the head is in at COMPLETION, on its own clock.
	Kindle,
	/// The shell of the held block about the head, climbing through the channel
	/// (`digblock_core::sphere`).
	Sphere,
	/// The road that lays itself under the feet for its span (`digblock_core::causeway`).
	Causeway,
}

impl CastTarget {
	/// Whether this spell MOVES the caster, and so rests on the shared
	/// teleport clock. Asked rather than listed at each gate, so a new
	/// travel spell cannot quietly arrive outside the cooldown.
	fn teleports(self) -> bool {
		match self {
			CastTarget::Home | CastTarget::Waystone(_) => true,
			CastTarget::Wisp | CastTarget::Kindle | CastTarget::Sphere | CastTarget::Causeway => {
				false
			}
		}
	}
}

struct CastState {
	target: CastTarget,
	ticks: u32,
}

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

/// One standing kindled light: its host cell, the seconds it has left,
/// and the carrier key its GI entry wears. Its clock is its own — the
/// server said how long, and nothing after that is consulted, which is
/// what lets the caster port away or leave.
struct KindledLight {
	cell: (i32, i32, i32),
	left: f32,
	/// Seconds since it was lit here, for the orb's fade-in alone.
	age: f32,
	key: u16,
}

/// One live teleport burst: where, which half of the jump, and how old.
/// The whole simulation is aging it — every mote derives in the shader.
struct TeleportFx {
	pos: Vec3,
	mode: crate::teleport::Mode,
	age: f32,
	seed: f32,
}

/// The essence draw the siphon is running, if any — presentation only, and the whole of what
/// [`crate::siphon`] needs to draw a stream. The geometry is kept through the fade-OUT rather than
/// dropped with the hold, which is the only reason this is a struct and not a query: a completed
/// extraction turns the cell to spent rock on the frame it lands, so by the time there is anything
/// to flare about there is no vein left to ask where the motes came from.
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

/// Milliseconds a frame may spend applying authoritative edits, given the measured display
/// interval — so the same queue takes proportionally less of a shorter frame (see
/// [`NET_EDIT_SHARE`]). A free function rather than a method because the scaling rule is the part
/// worth pinning and a test for it should not need a GPU. `refresh_ms` is a measured percentile
/// that reads zero until it settles; the nominal interval is the fallback.
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

/// What a snapshot's held-item wire id looks like in a remote's hand. Routes through
/// `hud::item_icon`, the same function the hotbar and your own viewmodel use, so a remote hand can
/// never show a different object from the one its owner is looking at. `None` = an empty hand, an
/// unknown id from a newer server, or an id no slot could legally hold.
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
		if !self.config.bench || self.bench.bench_reported {
			return;
		}
		self.bench.bench_reported = true;
		if let Some(line) = self.bench.metrics.bench_summary() {
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
		self.ui.size = size;
		self.recompute_ui_scale();
		self.build_renderer(gpu, surface_format);
		log::info!(
			"engine init: draw_dist={} format={surface_format:?}",
			self.config.draw_dist
		);
	}

	fn resize(&mut self, _gpu: &Gpu, size: (u32, u32)) {
		self.ui.size = size;
		self.recompute_ui_scale();
	}

	fn set_scale_factor(&mut self, scale_factor: f32) {
		self.ui.scale_factor = scale_factor;
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
			// Not density-derived, unlike the line above — the same gate
			// simply IS "no graphics preference", and the full list is the
			// slider's opt-in rather than where a session starts.
			let lights = crate::settings::LIGHT_CAP_DEFAULT;
			self.config.max_lights = lights;
			// Keep the settings screen showing what is actually in use.
			self.ui.menu.set_gi_scale(gi);
			self.ui.menu.set_max_lights(lights);
			log::info!("display scale {scale_factor}x: lighting detail /{gi}, {lights} lights");
		}
		self.recompute_ui_scale();
	}

	fn input(&mut self, event: InputEvent) {
		self.handle_input(event);
	}

	fn frame_started(&mut self) {
		self.bench.frame_start = Some(web_time::Instant::now());
	}

	fn set_host_memory(&mut self, wasm_bytes: u64, js_heap_bytes: u64) {
		self.bench.metrics.host_mem = digblock_core::metrics::HostMemory {
			wasm_bytes,
			js_heap_bytes,
		};
	}

	fn frame_acquired(&mut self, acquire_wait: Duration) {
		self.bench.pending.acquire_ms = acquire_wait.as_secs_f32() * 1000.0;
	}

	fn update(&mut self, dt: Duration) {
		self.step(dt);
	}

	fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32)) {
		self.draw(gpu, target, size);
	}

	/// A capture never takes the pointer. Without the first clause a capture answers YES on frame
	/// one — it opens with no menu, so no UI surface is open — and the desktop shell locks and
	/// hides the cursor for the run, taking the operator's mouse and swallowing everything they
	/// type into a window they never meant to play.
	fn wants_cursor_grab(&self) -> bool {
		self.config.interactive
			&& !self.ui.menu.is_open()
			&& !self.ui.inv_screen.is_open()
			&& !self.ui.alch_screen.is_open()
			&& !self.ui.char_screen.is_open()
			&& !self.ui.hiscores.is_open()
			&& !self.ui.spellbook.is_open()
	}

	fn wants_text_input(&self) -> bool {
		self.config.interactive && self.ui.chat.is_open()
	}

	fn take_fullscreen_request(&mut self) -> bool {
		std::mem::take(&mut self.input.fullscreen_requested)
	}

	fn take_screenshot_request(&mut self) -> bool {
		std::mem::take(&mut self.input.screenshot_requested)
	}

	fn text_entry_active(&self) -> bool {
		self.config.interactive && (self.ui.chat.is_open() || self.ui.menu.text_field_open())
	}

	// Chat first in both, mirroring `text_entry_active`: a menu cannot be
	// open over the chat line, but the shared order keeps the pair
	// answering about the same field even if that ever changes.
	fn text_field_contents(&self) -> String {
		if self.ui.chat.is_open() {
			self.ui.chat.line().to_string()
		} else {
			self.ui.menu.text_field_contents().to_string()
		}
	}

	fn text_replace(&mut self, text: &str) {
		if !self.config.interactive {
			return;
		}
		if self.ui.chat.is_open() {
			self.ui.chat.replace(text);
		} else {
			self.ui.menu.handle_text_replace(text);
		}
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
		self.bench.metrics.pause();
		self.stream.stream_budget.reset();
		// One cheap synchronous send — honors the on_suspend contract.
		self.emit(crate::audio::AudioCmd::Pause);
		// Named-save save-point: drain any same-frame edits, snapshot the
		// player, and flush — all synchronous by design (file fsync /
		// localStorage put), honoring the cheap-and-synchronous contract.
		let recorded = self.pipeline.take_recorded_edits();
		if let Some(store) = self.save.world_store.as_mut() {
			if self.save.in_local_save {
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
						yaw: self.look.yaw_deg,
						pitch: self.look.pitch_deg,
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
		// The old renderer's allocations are DESTROYED rather than left to
		// the drop, which frees nothing on the web (`gpu_ledger`). On a
		// live device — a format change, the drill's simulated loss — that
		// is the memory back now; after a real loss the handles belong to
		// the dead device and destroy is a no-op there, so one path serves
		// both. Then rebuild every device-lifetime object; meshes stream
		// back in from retained chunk data via the pipeline.
		if let Some(old) = self.renderer.take() {
			old.retire();
		}
		self.build_renderer(gpu, surface_format);
		log::info!("engine: renderer rebuilt (device loss), re-mesh queued");
	}
}

/// The fluid sim's view of the chunk pipeline: set_block already handles
/// copy-on-write and remesh invalidation, and returns None for unloaded
/// cells, which the sim treats as a dropped write.
struct PipelineWorld<'a>(&'a mut Pipeline, &'a [digblock_core::protect::Region]);

impl digblock_core::worldjob::CellSource for PipelineWorld<'_> {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		// Overlay fallback for evicted-but-edited cells, mirroring the
		// server sweep's journal fallback — the arms must answer alike
		// or one rule judges the same world two ways.
		self.0.cell_at(x, y, z).or_else(|| {
			let s = digblock_core::chunk::CHUNK_SIZE as i32;
			self.0.overlay().and_then(|o| {
				o.column(x.div_euclid(s), z.div_euclid(s))
					.and_then(|cells| cells.get(&(x, y, z)).copied())
			})
		})
	}

	fn edited_cells(&self, cx: i32, cz: i32, f: &mut dyn FnMut(i32, i32, i32)) {
		// The overlay is the offline world's whole edit record — and it
		// records the sweep's own writes, which is what keeps a spread
		// self-sustaining across runs. Multiplayer never reaches here.
		if let Some(cells) = self.0.overlay().and_then(|o| o.column(cx, cz)) {
			for &(x, y, z) in cells.keys() {
				f(x, y, z);
			}
		}
	}
}

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
		// `cur.props().solid` kept water out of it — a property of
		// whatever block happens to sit there, which is precisely what
		// the floor rule must not depend on. The sim reads `false` as
		// "didn't land" and declines to propagate onward. Offline the
		// engine's own arrival regions ride along; multiplayer passes an
		// empty set, because a server's regions are its own secret.
		if digblock_core::protect::immutable(x, y, z, self.1) {
			return false;
		}
		self.0.set_block(x, y, z, b).is_some()
	}
}

#[cfg(test)]
mod net_budget_tests;

#[cfg(test)]
mod worldjob_tests;

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
mod cast_tests;

#[cfg(test)]
mod ore_capture_tests;

#[cfg(test)]
mod causeway_tests;

#[cfg(test)]
mod ui_sound_tests;

#[cfg(test)]
mod page_nav_tests;

#[cfg(test)]
mod siphon_tutorial_tests;
