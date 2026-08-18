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

/// The launcher-injected clipboard writer, in the same shape as
/// [`crate::settings::SettingsStore`]: the engine raises an intent, the
/// launcher owns the platform call. Write-only — nothing in this project
/// pastes, and adding a read would mean giving the game a way to see what
/// the player copied somewhere else.
pub type ClipboardSink = Box<dyn FnMut(&str)>;

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
	/// One of the CRAFTED tools, owned and selected. Each carries
	/// authored geometry — a tool has no texture layers of its own, so
	/// the cube fallback draws it as a block of whatever sits at layer 0
	/// — and that geometry shows up in three places at once (the hand,
	/// the tool slot, the faded rail), none of which a capture can reach
	/// without being told which tool to hold: the bucket is the only one
	/// a fresh player owns.
	Tool(BlockType),
	/// Also not a screen: stock and SELECT a torch, so the one held item
	/// that is also a LIGHT is photographable. `Hand` cannot serve —
	/// `reconcile_layout` files by block id, so its first slot is Dirt —
	/// and a torch in hand differs from every other item in three places
	/// at once: the viewmodel draws it fullbright, a remote's hand draws
	/// its real post, and the GI light list carries it.
	Torch,
	/// The character screen, with mid-curve XP stocked so the bars and
	/// detail lines photograph as they play, not empty.
	Skills,
	/// The alchemy screen's second tab (craft rows, locked rows, costs).
	Transmute,
	/// The same tab with a BATCH on the bench: the progress row under the
	/// button, part-way through a cycle. A paced craft is over in seconds
	/// and needs a press to start, so this is the only way it reaches a
	/// capture at all.
	Transmuting,
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
			"siphon" => Some(Self::Tool(BlockType::EssenceSiphon)),
			"pick" => Some(Self::Tool(BlockType::ResonantPick)),
			"sigil" => Some(Self::Tool(BlockType::MastersSigil)),
			"torch" => Some(Self::Torch),
			"skills" | "character" => Some(Self::Skills),
			"transmute" => Some(Self::Transmute),
			"transmuting" => Some(Self::Transmuting),
			_ => None,
		}
	}
}

/// Parse `DIG_PLAYERBOX`'s value: what the fake remote holds, in HOTBAR
/// WIRE terms.
///
/// Lives here rather than in the launcher for the same reason
/// [`DebugScreen::parse`] does — the launcher owns the environment, the
/// engine owns what the names mean. A wire id rather than a block so the
/// scene walks the real `held_icon` decode, tool sentinels included.
/// `None` is an unknown name, which the launcher makes fatal.
pub fn parse_held_item(s: &str) -> Option<u16> {
	use digblock_core::hotbar::{WIRE_EMPTY, WIRE_TOOL};
	Some(match s.trim().to_ascii_lowercase().as_str() {
		// Any value used to mean "on", and on meant a dirt cube — "0"
		// and "off" included, because the switch was presence-only
		// (`var_os`), exactly as `DIG_VAULT=0` still means vault ON.
		"" | "0" | "1" | "on" | "off" | "yes" | "no" | "true" | "false" | "dirt" => {
			BlockType::Dirt as u16
		}
		"torch" => BlockType::Torch as u16,
		"stairs" => BlockType::WoodStairs as u16,
		"slab" => BlockType::WoodSlab as u16,
		"door" => BlockType::LumberDoor as u16,
		"bucket" => WIRE_TOOL,
		"bucket-full" => digblock_net::messages::HELD_TOOL_FULL,
		// The crafted tools, by their own block ids — which is exactly
		// what `Client::held_wire` sends for them, the two sentinels
		// being the bucket's charges. Named the same way `DIG_SCREEN`
		// names them, and worth having because two of them now GLOW and
		// CAST: a remote's siphon lighting the room is a second code path
		// (players.wgsl's own ink decode) and this is the only way to
		// photograph it.
		"siphon" => BlockType::EssenceSiphon as u16,
		"pick" => BlockType::ResonantPick as u16,
		"sigil" => BlockType::MastersSigil as u16,
		"empty" => WIRE_EMPTY,
		_ => return None,
	})
}

pub struct EngineConfig {
	/// Draw distance in chunks; the mobile config ships a shorter one
	/// (Java Config).
	pub draw_dist: i32,
	/// Chunk worker threads.
	pub workers: usize,
	pub seed: Option<f64>,
	/// Fixed sun angle in degrees for deterministic captures. Filled by
	/// dev-build flags (or bench internally); wins over the server's
	/// world clock because `update()` re-asserts it every frame.
	pub tod: Option<f32>,
	/// Initial camera yaw/pitch in degrees for deterministic captures
	/// (yaw 0 faces +z, 90 faces +x; an absent pitch takes a default
	/// downward tilt).
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
	/// Is a PERSON at this keyboard? False for every capture path —
	/// bench, `--screenshot`, `--drill` — and it governs two things that
	/// have to move together: whether [`Engine::input`] listens at all,
	/// and whether the shell is allowed to take the mouse pointer.
	///
	/// The pointer half is the one that bit. A capture sets `menu:
	/// false`, so no UI surface is open, so `wants_cursor_grab` answered
	/// true on the first frame and the desktop shell locked the cursor
	/// and hid it — for the whole run, on a window the operator never
	/// asked to play. Every keystroke they made meanwhile went into the
	/// world: a stray `E` opened the inventory into the shot, a stray
	/// mouse move spun the camera off the pose. `bench` alone did not
	/// cover it, because `--screenshot`/`--frames`/`--exit` runs are
	/// captures that never set it — which is exactly the class of run a
	/// person sits and watches.
	///
	/// The launcher already computes this predicate to decide the menu,
	/// audio, the master URL and whether saved settings load; this field
	/// is that same answer reaching the engine, not a second opinion.
	pub interactive: bool,
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
	/// UI is never scaled by this. Capture paths hold it at native.
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
	/// flight is a fast flyover well above the ground, which photographs the
	/// fast, shallow-parallax end of screen motion; a player reports
	/// temporal artifacts from the other end — eye height, walking pace,
	/// ground a few blocks ahead, where one half-res texel covers far more
	/// world per pixel. Same harness, second regime.
	pub walk: bool,
	/// Flyover speed in blocks/s (`--bench-speed`). Every BENCH.md baseline
	/// was taken at the default — changing it silently invalidates all of
	/// them, so don't.
	///
	/// The default is several times `WALK_SPEED`, and that is a different
	/// STREAMING REGIME, not just a faster one: at flyover speed the GI
	/// upload budget cannot keep up and the classify drain drops entries, so
	/// a cost that only appears there may be budget starvation rather than a
	/// real leak. Drop this to walking pace for a soak that matches how the
	/// game is actually played. (`--walk` is not the same thing — it
	/// oscillates about a fixed origin, so it never brings new terrain in at
	/// all.)
	pub bench_speed: f32,
	/// Fire the F7 GI drill at this frame (`--gi-drill-at`), so the ratchet
	/// is measurable with no human at the keyboard. See `update`.
	pub gi_drill_at: Option<u32>,
	/// Screen-space reflections on water (desktop default; the mobile
	/// config keeps the procedural-sky fallback only).
	pub ssr: bool,
	/// User multiplier on the derived UI scale (settings slider).
	pub ui_scale: f32,
	/// User volume multipliers (settings sliders); unity = authored gains.
	pub music_vol: f32,
	pub sfx_vol: f32,
	/// Multiplayer display name; the LAUNCHER fills this (e.g. from
	/// $USER) — the engine itself must stay env-free for wasm.
	pub player_name: Option<String>,
	/// Debug: park a fake remote player a few blocks ahead, holding this item
	/// (GPU-path check without a second client). A held-item WIRE id, not
	/// a block, so the scene walks `held_icon` end to end and is a real
	/// check of that decode rather than a second code path — which is
	/// also how it can photograph a remote's lit torch. Set by the
	/// desktop launcher from DIG_PLAYERBOX.
	pub debug_playerbox: Option<u16>,
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
	/// Debug: pin the essence draw at this progress (0..1) on whatever
	/// vein the crosshair is holding. `DIG_CRACK`'s argument exactly — a
	/// draw is input-driven and over in a second or two — with one
	/// difference that shapes the flag: this one cannot stand its own
	/// scene, because a vein is derived worldgen and there is no placing
	/// one. It rides `DIG_ORE`, which already hollows a chamber around a
	/// deposit of a tier and grants the level that reveals it, and the
	/// launcher refuses the pair unset rather than capturing an empty
	/// wall. Set by the desktop launcher from DIG_SIPHON.
	pub debug_siphon: Option<f32>,
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
	/// inside — the NEE capture, and any look call about a dim room (a
	/// roofed room goes DARK and torchlight is what makes it readable;
	/// GI-DESIGN.md). Set by the desktop launcher from DIG_VAULT.
	pub debug_vault: bool,
	/// DIG_NODE=<heartwood|leystone|springstone>: pose at the nearest
	/// node of that kind with its Alchemy reveal granted — the surface
	/// counterpart of `DIG_ORE`, and like it a SCENE rather than a pose,
	/// since a node is derived and invisible below its level. Validated
	/// at the launcher; an unknown kind is fatal there.
	pub debug_node: Option<BlockType>,
	/// DIG_ORE=<tier 1..=4>: hollow a chamber around the nearest deposit
	/// of that tier and stand in it, with the Mining level that reveals
	/// it. Ore is derived and invisible below its level, so there is no
	/// pose that photographs it — see [`debug_ore_plan`].
	pub debug_ore: usize,
	/// DIG_MINING=<level>: start at that Mining level, 0 = leave it
	/// alone. A level is not a pose and not a scene, and everything it
	/// gates is invisible without it: the strength wall's refusals, dig
	/// speed, and the Resonant Pick's two ranks — the second of which
	/// brightens the tool's own glow, so a capture at level 1 photographs
	/// the wrong light. Applied AFTER `debug_ore`'s own grant, so asking
	/// for a level explicitly wins.
	pub debug_mining: u32,
	/// Debug: build DIG_VAULT's room with NO torch on its floor, so the
	/// only source in it is the one the player is CARRYING. Set by the
	/// desktop launcher from DIG_VAULT_DARK. A modifier rather than a
	/// re-spelling of DIG_VAULT, because `DIG_VAULT=0` already means
	/// "vault on" and silently inverting a value someone may have typed
	/// costs an afternoon.
	pub debug_vault_dark: bool,
	/// Debug: hang a wide stone roof with NO walls over the ground ahead
	/// of spawn — the beam-veto capture (a slanting morning sun must
	/// reach under it exactly as the shadow map says; only the ambient
	/// dome dims). Set by the desktop launcher from DIG_ROOF.
	pub debug_roof: bool,
	/// Debug: seal a long stone tunnel ahead of the spawn camera, no
	/// torch — the unproven-daylight capture (GI-DESIGN.md R32). Its far
	/// end sits past the probe cascades' reach and past the gather ray's
	/// length, which is exactly where every "answer sky on no data"
	/// fallback fires; a vault is too small to reach any of them. Pair
	/// with `--pos` at a low y — the fade the scene photographs is
	/// depth-gated, and the plan keys off wherever the spawn is. Set by
	/// the desktop launcher from DIG_TUNNEL.
	pub debug_tunnel: bool,
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
	/// at spawn is far out of frame). Set by the desktop launcher from
	/// DIG_RIVER.
	pub debug_river: bool,
	/// Debug: how many torches `debug_torches_plan` lights its courtyard
	/// with, 0 = scene off. The local-light capture DIG_VAULT cannot be:
	/// a room deeper than `LIGHT_RANGE` and an emitter COUNT that varies
	/// with the geometry held fixed. Set by the desktop launcher from
	/// DIG_TORCHES.
	pub debug_torches: i32,
	/// Debug: extra torches parked well BEHIND the camera, out of
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
	/// Spray droplets per waterfall-impact instance; the mobile config
	/// ships fewer (it used to be inferred in the renderer from the draw
	/// distance). Each is a sub-tenth-block alpha billboard, so the cost
	/// is overdraw on a handful of culled emitters, not geometry.
	pub spray_droplets: u32,
	/// Per-frame wall-clock budget for main-thread streaming (see the
	/// STREAM_BUDGET_MS doc below, which is the default).
	pub stream_budget_ms: f32,
	/// Grow the streaming budget from measured frame headroom, scaled
	/// by how near the closest pending job is (the web build; see
	/// `stream_budget`) — rim-only churn while walking stays at the
	/// fixed floor. Effective outside bench only: captures must stay
	/// deterministic. It is NOT conditioned on the executor — see
	/// `adaptive_effective`.
	pub adaptive_stream: bool,
	/// Force the initial chunk-origin UBO capacity in slots. The LAUNCHER
	/// fills this from DIG_CHUNK_SLOTS (engine stays env-free for wasm);
	/// tiny values exercise the growth path in a normal run.
	pub chunk_slots_override: Option<u32>,
	/// Start with the menu open (desktop interactive runs). Bench,
	/// screenshot, drill, and iOS keep this false — those paths must not
	/// sit on a menu screen.
	pub menu: bool,
	/// Draw the touch HUD (the corner menu button, the thumb-cluster
	/// buttons, tappable hotbar) and claim touches that land on it. Set by
	/// the configs whose primary pointer is a finger; the touch VERBS stay
	/// unconditional, since they only ever fire from touch events.
	pub touch_hud: bool,
	/// The touch jump verb is the on-screen button instead of the
	/// double-tap. A persisted user setting (like `ssr`), mirrored here so
	/// the frame's intent read needs no settings lookup.
	pub touch_jump_button: bool,
	/// The shell can toggle browser fullscreen (the web launcher sets it
	/// from the Fullscreen API's availability): the menu offers the row.
	pub fullscreen_button: bool,
	/// There is no Fullscreen API here, but there IS a way to play without
	/// the browser's chrome: install the page as a home-screen app. iPhone
	/// Safari, in a tab. The menu says so where the row would have been,
	/// because a player who wants fullscreen looks there and otherwise
	/// finds nothing at all — twice, in this project's case.
	pub fullscreen_hint: bool,
	/// Scripted join (a server-name filter; empty string = first listed):
	/// once menu services arrive, the menu walks the exact user join path
	/// unattended. Web `?join` and scripted flows.
	pub auto_join: Option<String>,
	/// The signed-in account, if the launcher found a stored session. The
	/// engine never reads or writes the session TOKEN — that lives in the
	/// launcher's services thread, which is the only thing that talks to
	/// the master. This is display state: who to say you are on the
	/// account screen, and whether the menu offers Multiplayer or Sign In.
	pub account: Option<crate::menu::AccountView>,
	/// The key this machine's pre-account character was filed under, if an
	/// old identity file is still on disk — see
	/// `NetClientConfig::legacy_identity`. The LAUNCHER fills this, reading
	/// the file and hashing it, so the secret itself never enters the
	/// engine and there is nothing here to leak.
	///
	/// Read-only, and never created: a machine that has no old identity has
	/// nothing to migrate, and minting one would invent a character out of
	/// thin air.
	pub legacy_identity: Option<[u8; 16]>,
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
			// Launchers narrow this; the DEFAULT has to be the safe one.
			// A capture that forgets to set it merely accepts input it
			// will not get; an interactive run that forgets is a game
			// that ignores its player.
			interactive: true,
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
			debug_playerbox: None,
			debug_confetti: false,
			debug_crack: None,
			debug_siphon: None,
			debug_shapes: false,
			debug_doors: false,
			debug_vault: false,
			debug_node: None,
			debug_ore: 0,
			debug_mining: 0,
			debug_vault_dark: false,
			debug_roof: false,
			debug_tunnel: false,
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
			touch_hud: false,
			touch_jump_button: false,
			fullscreen_button: false,
			fullscreen_hint: false,
			auto_join: None,
			account: None,
			legacy_identity: None,
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
			// Launchers narrow this; the DEFAULT has to be the safe one.
			// A capture that forgets to set it merely accepts input it
			// will not get; an interactive run that forgets is a game
			// that ignores its player.
			interactive: true,
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
			debug_playerbox: None,
			debug_confetti: false,
			debug_crack: None,
			debug_siphon: None,
			debug_shapes: false,
			debug_doors: false,
			debug_vault: false,
			debug_node: None,
			debug_ore: 0,
			debug_mining: 0,
			debug_vault_dark: false,
			debug_roof: false,
			debug_tunnel: false,
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
			touch_hud: true,
			touch_jump_button: false,
			fullscreen_button: false,
			fullscreen_hint: false,
			auto_join: None,
			account: None,
			legacy_identity: None,
			spike_log: false,
			upload_bytes_max: UPLOAD_BYTES_MAX_CONSTRAINED,
			debug_gi: false,
			debug_gi_probes: false,
		}
	}

	/// Browser client: full desktop quality (Chrome-on-WebGPU hardware
	/// handles it — verified at the desktop draw distance with SSR on), and
	/// the menu opens by default like a desktop interactive run. URL params
	/// (?draw= ?ssr=0 ?shadow=) opt weaker hardware down.
	///
	/// `workers: 0` here is the FLOOR, not the shipped setting: wasm has
	/// no std threads, so a pipeline built from this config runs jobs
	/// inline. The launcher overwrites the field with what it intends to
	/// spawn and attaches a Web Worker pool afterwards (`apps/web/jobs.rs`),
	/// which the pipeline upgrades to when its first worker answers.
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

	/// Browser client on a phone or tablet: the `mobile()` quality knobs —
	/// same hardware class as the iOS build — over the `web()` plumbing,
	/// plus the touch HUD. Detection (and the `?mobile` override) lives in
	/// the launcher; saved settings and URL params still win, in that
	/// order, exactly as they do over `web()`.
	pub fn web_mobile() -> Self {
		Self {
			draw_dist: 10,
			ssr: false,
			gi_scale_div: 3, // the Settings screen's "Medium" notch
			spray_droplets: 48,
			touch_hud: true,
			touch_jump_button: false,
			fullscreen_button: false,
			fullscreen_hint: false,
			..Self::web()
		}
	}

	/// The adaptive budget's effective enable (see `adaptive_stream`).
	///
	/// Deliberately NOT gated on the executor. What this enables is ONE
	/// deadline covering jobs, drain, eviction and uploads together — and
	/// moving jobs to a pool moves only the jobs. A `workers == 0`
	/// condition here therefore snapped the drain and the uploads back to
	/// the fixed floor the instant web workers were switched on: jobs got
	/// faster, absorption got slower, and the machines with headroom to
	/// spend lost it. The JOB half is gated where it belongs instead —
	/// `Pipeline::maintain` spends the allowance only when jobs are
	/// actually running on this thread.
	fn adaptive_effective(&self) -> bool {
		self.adaptive_stream && !self.bench
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
/// Share of one display interval `pump_net` may spend applying
/// authoritative edits. Each costs a copy-on-write cell write plus up to
/// eight mesh-heap pushes, so a join sync's worth of them in one frame is
/// a visible stall; spread over frames it is invisible, and the world
/// simply fills in as fast as meshing can keep up anyway.
///
/// A COUNT could not express that, which is what this replaced. What one
/// edit costs swings by more than an order of magnitude — whether its
/// chunk is still shared decides a whole-chunk copy-on-write — and the mesh
/// jobs it queues are charged to this same thread later in the same frame
/// even with a Web Worker pool up — feeding one encodes and copies the
/// padded snapshot, and absorbing the result is main-thread work by
/// definition — so the apply is only half of what a burst actually spends.
/// With no pool (a 1–2 core device, a browser that refused workers,
/// `?workers=0`) they run here outright. The COUNT it replaced was sized
/// against a native frame with worker threads behind it, while a
/// high-refresh web frame is a fraction of that. Pricing the slice in TIME
/// makes the same code back off on the platform that cannot afford it, with
/// no constant to retune per display.
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
	/// network does (see NET-AUDIT.md).
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

/// Every authored block of one shape family, in material order — the
/// showcase scenes walk this so a new material × shape pair photographs
/// itself. The old scenes hand-listed `[WoodStairs, StoneStairs,
/// LumberStairs]`, which is exactly the kind of mirror §7.2's grid exists
/// to delete: two timbers landed and neither appeared in any capture.
fn shaped_roster(s: digblock_core::block::ShapeFamily) -> Vec<BlockType> {
	digblock_core::material::Material::ALL
		.into_iter()
		.filter_map(|m| BlockType::from_grid(m, s))
		.collect()
}

/// DIG_SHAPES=1 layout: every shaped variant in rows floating at eye
/// height near spawn (render verification needs no ground) — every stair
/// material × 4 facings, then closed and open doors in every material,
/// then the slabs. Rows run back in z, one material per row, so the rank
/// grows backward as the catalog does instead of off the sides of the
/// frame.
fn debug_shapes_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::{BlockType as B, ShapeFamily};
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{Facing, door_state, slab_state, stair_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	let stairs = shaped_roster(ShapeFamily::Stairs);
	let doors = shaped_roster(ShapeFamily::Door);
	let slabs = shaped_roster(ShapeFamily::Slab);
	// One z cursor, walked front to back, so a row added anywhere pushes
	// what follows instead of colliding with it. Nearest first is what the
	// camera actually frames: the FURNITURE rides right behind the stairs
	// because it is the newest thing to look at.
	let mut z = bz + 2;
	for (j, &b) in stairs.iter().enumerate() {
		for (i, f) in Facing::ALL.into_iter().enumerate() {
			out.push((
				bx + 2 * i as i32 - 3,
				by,
				z + 2 * j as i32,
				make_cell(b, stair_state(f)),
			));
		}
	}
	z += 2 * stairs.len() as i32;
	// Furniture × 4 facings: the table is stateless, so its row is four
	// identical tops — which is the point, since pushed together they read
	// as one surface. The chair and the bed turn.
	for (j, b) in [B::Table, B::Chair, B::Bed].into_iter().enumerate() {
		for (i, f) in Facing::ALL.into_iter().enumerate() {
			let state = if b.takes_facing() { stair_state(f) } else { 0 };
			out.push((
				bx + 2 * i as i32 - 3,
				by,
				z + 2 * j as i32,
				make_cell(b, state),
			));
		}
	}
	z += 6;
	// Doors take two z rows each (closed, then open), at the two heights a
	// leaf spans, so both states of every material are in one capture.
	for (j, &b) in doors.iter().enumerate() {
		for (i, f) in Facing::ALL.into_iter().enumerate() {
			for (dz, open) in [(0, false), (2, true)] {
				for upper in [false, true] {
					out.push((
						bx + 2 * i as i32 - 3,
						by + i32::from(upper),
						z + 4 * j as i32 + dz,
						make_cell(b, door_state(f, open, upper)),
					));
				}
			}
		}
	}
	z += 4 * doors.len() as i32;
	let slab_z = z;
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
	for (i, b) in slabs.into_iter().enumerate() {
		let x = bx + 2 * i as i32 - 3;
		out.push((x, by, slab_z, make_cell(b, slab_state(false, false))));
		out.push((x, by, slab_z + 2, make_cell(b, slab_state(true, false))));
		out.push((x, by + 2, slab_z, make_cell(b, slab_state(false, true))));
		out.push((x, by + 2, slab_z + 2, make_cell(b, slab_state(true, true))));
	}
	let base = [
		make_cell(B::WoodSlab, slab_state(false, false)),
		make_cell(B::WoodSlab, slab_state(true, false)),
		B::Wood as u16,
		B::Wood as u16,
	];
	for (dx, cell) in base.into_iter().enumerate() {
		out.push((bx + 1 + dx as i32, by, slab_z + 4, cell));
	}
	out.push((
		bx + 4,
		by + 1,
		slab_z + 4,
		make_cell(B::WoodSlab, slab_state(false, false)),
	));
	out
}

/// DIG_ORE=<tier 1..=4>: a chamber hollowed out around the nearest
/// deposit of that tier, the vein left standing in its wall, and a torch
/// on the floor to see it by. Returns the plan and where to stand.
///
/// Ore is DERIVED and invisible below its level, which makes it the least
/// photographable thing in the game: there is no cell to teleport to,
/// because the cell is Stone until someone with the level looks at it.
/// Hence a scene rather than a pose — and hence the one rule that makes
/// the scene work at all: **the carve must not touch the vein**. An edited
/// cell never shows ore (that is the anti-dupe), so hollowing the box
/// naively would delete the very thing being photographed. Every cell the
/// deposit claims is left exactly as worldgen made it.
fn debug_ore_plan(spawn: Vec3, seed: f64, want: BlockType) -> (Vec3, Vec<(i32, i32, i32, u16)>) {
	use digblock_core::worldgen::ore;
	let (sx, sz) = (spawn.x.floor() as i32, spawn.z.floor() as i32);
	// Nearest deposit of the tier, horizontally — a search box wide
	// enough that even Aetherock (one deposit in fifteen, and only in the
	// bottom two grid rows) is found from any spawn.
	let mut best: Option<(i64, ore::Deposit)> = None;
	for d in ore::deposits_near((sx - 320, 1, sz - 320), (sx + 320, 200, sz + 320), seed) {
		if d.ore != want {
			continue;
		}
		let (dx, dz) = (i64::from(d.x - sx), i64::from(d.z - sz));
		let d2 = dx * dx + dz * dz;
		if best.is_none_or(|(b, _)| d2 < b) {
			best = Some((d2, d));
		}
	}
	let Some((_, d)) = best else {
		log::warn!("DIG_ORE: no {want:?} within reach of spawn");
		return (spawn, Vec::new());
	};
	// The chamber: a box around the vein, minus the vein itself. Stood
	// off far enough on -z that the camera is outside it looking in.
	const R: i32 = 5;
	let mut plan = Vec::new();
	for y in (d.y - R)..=(d.y + R) {
		for z in (d.z - R)..=(d.z + R) {
			for x in (d.x - R)..=(d.x + R) {
				if y <= digblock_core::block::BEDROCK_Y || ore::ore_at(x, y, z, seed).is_some() {
					continue;
				}
				plan.push((x, y, z, BlockType::Air as u16));
			}
		}
	}
	// A floor two below the vein, so a standing player's EYE lands level
	// with it — physics drops the camera onto that floor within a frame
	// or two of a capture starting, and a pose that has to be looked up
	// at is a pose nobody frames correctly.
	//
	// Guarded exactly like the carve, and for the same reason: this plate
	// is laid THROUGH the blob. A max-radius deposit reaches
	// `ore::MAX_RADIUS` straight down, so the cell right under the
	// centre sits on the rim and `rim_open` keeps it more often than not —
	// and the plan is applied through `pipeline.set_cell`, which records
	// into the `EditOverlay`, after which `ore::revealed` answers None for
	// that cell forever. An unguarded plate quietly deletes the vein it
	// was laid down to photograph.
	//
	// Dropping the whole plane clear of the deposit instead (`d.y -
	// MAX_RADIUS - 1`) would not replace this guard and would cost the
	// pose: `ore_at` answers for every deposit in the 3×3×3 apron and not
	// just this one, so a neighbour's blob can cross whatever plane we
	// pick, and a floor a block lower puts the eye UNDER the vein — the
	// one thing this scene exists to get right. Where a vein does cross,
	// the cell keeps its pristine worldgen value, which is the ore-bearing
	// Stone itself: the floor stays walkable and the seam simply runs
	// through it. Only a cave cell a deposit happens to claim leaves a
	// real gap, and that is the same hazard the carve above already
	// accepts.
	// Never at or below the world floor. `deposit_in` now keeps a whole
	// BLOB clear of `BEDROCK_Y + MAX_RADIUS`, so this `max` cannot bite
	// today — it is here because that is a coupling two modules apart,
	// and the plate must not silently start writing the one plane the
	// game promises is untouchable if the ore constraint is ever
	// loosened. Clamped rather than skipped so the plate stays whole: a
	// floor with holes in it is a worse capture than a floor one block
	// high.
	let floor = (d.y - 2).max(digblock_core::block::BEDROCK_Y + 1);
	for z in (d.z - R)..=(d.z + R) {
		for x in (d.x - R)..=(d.x + R) {
			if ore::ore_at(x, floor, z, seed).is_some() {
				continue;
			}
			plan.push((x, floor, z, BlockType::Stone as u16));
		}
	}
	// A chamber this deep is lit by nothing at all and photographs as
	// black; the torch is off to one side so the vein is raked rather
	// than flattened by a light behind the camera.
	plan.push((d.x - 3, floor + 1, d.z - 3, BlockType::Torch as u16));
	log::info!("DIG_ORE: {want:?} vein at {},{},{}", d.x, d.y, d.z);
	(
		Vec3::new(
			d.x as f32 + 0.5,
			floor as f32 + 1.0 + digblock_core::player::EYE_TO_FEET,
			d.z as f32 - 4.5,
		),
		plan,
	)
}

/// `DIG_NODE`'s launcher-side parser, beside [`parse_held_item`] for the
/// same reason: the launchers do not link core, and the roster belongs
/// with the scene that consumes it.
pub fn parse_node_kind(v: &str) -> Option<BlockType> {
	Some(match v {
		"heartwood" => BlockType::Heartwood,
		"leystone" => BlockType::Leystone,
		"springstone" => BlockType::Springstone,
		_ => return None,
	})
}

/// DIG_NODE=<heartwood|leystone|springstone>: pose the camera at the
/// nearest node of that kind, with the reveal granted so the stamp shows
/// it. Leystone rides the deposit stream and reuses the ore chamber; the
/// two surface kinds are outdoor finds, so their scenes WRITE nothing —
/// a node is exactly as photographable as the world makes it, which is
/// the point of the capture.
fn debug_node_plan(spawn: Vec3, seed: f64, node: BlockType) -> (Vec3, Vec<(i32, i32, i32, u16)>) {
	use digblock_core::worldgen::nodes;
	let (sx, sz) = (spawn.x.floor() as i32, spawn.z.floor() as i32);
	let eye = digblock_core::player::EYE_TO_FEET;
	match node {
		BlockType::Leystone => debug_ore_plan(spawn, seed, BlockType::Leystone),
		BlockType::Heartwood => {
			// The nearest heart-bearing trunk cell: trunks are a pure
			// candidate rule, so this is a column walk over the memoized
			// decoration cells — a one-time startup cost.
			let mut best: Option<(i64, (i32, i32, i32))> = None;
			for z in (sz - 160)..(sz + 160) {
				for x in (sx - 160)..(sx + 160) {
					let Some(t) = digblock_core::decoration::trunk_at(x, z, seed) else {
						continue;
					};
					if !nodes::heartwood_at(x, t.y0 + 1, z, seed) {
						continue;
					}
					let (dx, dz) = (i64::from(x - sx), i64::from(z - sz));
					let d2 = dx * dx + dz * dz;
					if best.is_none_or(|(b, _)| d2 < b) {
						best = Some((d2, (x, t.y0 + 1, z)));
					}
				}
			}
			let Some((_, (x, y, z))) = best else {
				log::warn!("DIG_NODE: no heartwood within reach of spawn");
				return (spawn, Vec::new());
			};
			log::info!("DIG_NODE: heartwood at {x},{y},{z}");
			// Stand a few blocks south of the trunk on the tree's own
			// ground, eye level with the heart.
			let ground = digblock_core::worldgen::terrain_height(x, z - 3, seed);
			(
				Vec3::new(x as f32 + 0.5, ground as f32 + 1.0 + eye, z as f32 - 2.5),
				Vec::new(),
			)
		}
		BlockType::Springstone => {
			let mut best: Option<(i64, (i32, i32, i32))> = None;
			for z in (sz - 512)..(sz + 512) {
				for x in (sx - 512)..(sx + 512) {
					// The rate first — it prunes all but a few percent
					// before any field evaluation runs.
					let shape = digblock_core::worldgen::column_shape(x, z, seed);
					if shape.water <= shape.height {
						continue;
					}
					if !nodes::springstone_at(x, shape.height, z, seed) {
						continue;
					}
					let (dx, dz) = (i64::from(x - sx), i64::from(z - sz));
					let d2 = dx * dx + dz * dz;
					if best.is_none_or(|(b, _)| d2 < b) {
						best = Some((d2, (x, shape.height, z)));
					}
				}
			}
			let Some((_, (x, y, z))) = best else {
				log::warn!("DIG_NODE: no springstone within reach of spawn");
				return (spawn, Vec::new());
			};
			log::info!("DIG_NODE: springstone at {x},{y},{z}");
			// Stand on the nearest dry column shoreward of the spring,
			// looking over the water at the glint in the bed.
			let stand = (1..=12)
				.map(|d| (x, z - d))
				.find(|&(cx, cz)| {
					let s = digblock_core::worldgen::column_shape(cx, cz, seed);
					s.height >= s.water
				})
				.unwrap_or((x, z - 4));
			let ground = digblock_core::worldgen::terrain_height(stand.0, stand.1, seed);
			(
				Vec3::new(
					stand.0 as f32 + 0.5,
					ground as f32 + 1.0 + eye,
					stand.1 as f32 + 0.5,
				),
				Vec::new(),
			)
		}
		other => {
			log::warn!("DIG_NODE: {other:?} is not a surface node kind");
			(spawn, Vec::new())
		}
	}
}

/// DIG_VAULT=1 layout: a sealed stone vault enclosing the spawn camera,
/// one torch on the floor ahead and a stair to catch its falloff. The
/// NEE capture: skylight is stripped by the roof (the placement writes
/// ride the ordinary edit hook), so the interior photographs DARK except
/// the torch pool — at noon.
fn debug_vault_plan(spawn: Vec3, torch: bool) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{Facing, stair_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// Shell-first, carve-second ordering doesn't matter — placement is
	// retried per cell and the light BFS settles under maintain either
	// way. The interior is a box around the eye cell, reaching further
	// AHEAD in z than behind so the torch pool has room to fall off.
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
	// The torch, and something with depth for its light to model. The
	// torch is optional (DIG_VAULT_DARK) because a room whose ONLY source
	// is the one being carried is the capture a held light needs — with
	// the floor torch present the two pools cannot be told apart. Keeping
	// it is the other half of the pair: same lamp, placed and carried, in
	// one frame, so a calibration drift between them is visible.
	if torch {
		out.push((bx, by - 2, bz + 6, B::Torch as u16));
	}
	out.push((
		bx - 2,
		by - 2,
		bz + 5,
		make_cell(B::StoneStairs, stair_state(Facing::PosX)),
	));
	out.push((bx + 2, by - 2, bz + 4, B::Wood as u16));
	out
}

/// DIG_TUNNEL=1 layout: a long sealed stone tunnel running ahead of the
/// spawn camera, no torch — the unproven-daylight capture (GI-DESIGN.md
/// R32). The length is the scene: the far end sits beyond the probe
/// cascades' coverage and beyond the gather ray's reach, so every
/// "answer sky on no data" fallback fires along it, which no room-sized
/// scene can reach. Combine with `--pos` at a low y for the depth-gated
/// fade; a run above `SKY_FADE_FULL_Y` is the geometry-identical
/// control, since the fade saturates there.
fn debug_tunnel_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// A one-block stone shell around a bore the player's capsule fits
	// (eye at `by`, feet two cells down), reaching far enough ahead in z
	// that the far wall photographs through every fallback regime at
	// once: past C0's coverage, past the gather ray, and short of the
	// window's edge.
	for x in (bx - 2)..=(bx + 2) {
		for y in (by - 3)..=(by + 1) {
			for z in (bz - 3)..=(bz + 53) {
				let interior = (bx - 1..=bx + 1).contains(&x)
					&& (by - 2..=by).contains(&y)
					&& (bz - 2..=bz + 52).contains(&z);
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
	use digblock_core::shapes::{Facing, stair_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// A stone shell reaching further ahead in z than behind, its interior
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
/// camera (yaw 0 faces +z), its surface a few blocks below the eye — the
/// "standing at a river's edge looking into it" frame-cost scene (R17).
/// Pair with `--pitch -35` so near water fills most of the screen. All
/// of it sits inside C0 beside bank geometry (the expensive gather
/// path), and the submerged stone floor keeps the resolve's rays
/// crossing the water column, which is the other half of that scene's
/// cost.
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
	use digblock_core::shapes::{Facing, door_state};
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
/// runs well past `LIGHT_RANGE`, so a capture crosses that edge with
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
	// Floor laid to DIG_VAULT's convention — one cell below the first air
	// cell — and cleared far enough overhead that nothing worldgen left
	// there shadows the run.
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
	// The cluster: the `SPOTS` roster, spaced a few blocks apart, laid down
	// in a fixed order so `n` = 1 keeps the same first torch in the same
	// cell as `n` = 8 and the A/B moves one variable.
	//
	// Parked well down the courtyard rather than at the camera's feet,
	// because the report is about distance: an eye-height camera at spawn
	// then holds all three regimes in one frame — near floor lit only by
	// what reaches it (the band the artifact is reported in, and the one a
	// half-res texel magnifies most), the pools themselves, and floor
	// running out past `LIGHT_RANGE` behind them where no NEE fires at all.
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
	// `far` torches on a pad well BEHIND the camera, outside the
	// courtyard wall. They cannot light one pixel of the capture — every
	// visible surface is past `LIGHT_RANGE` from them and the wall is in
	// the way — but they are well inside the reach `flush_lights` keeps,
	// so they land in the light LIST.
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

/// What a snapshot's held-item wire id looks like in a remote's hand.
///
/// Routes through `hud::item_icon`, the same function the hotbar and
/// your own viewmodel use, so a remote hand can never show a different
/// object from the one its owner is looking at. `None` = an empty hand,
/// an unknown id from a newer server, or an id no slot could legally
/// hold.
fn held_icon(held: u16) -> Option<crate::ui::hud::IconKind> {
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

/// Carrier key for the local player. Remote carriers key on their
/// `PlayerId`, which is a `u8`, so the two spaces cannot collide.
const CARRIER_SELF: u16 = u16::MAX;
/// Carrier key for `DIG_PLAYERBOX`'s fake remote. Out of the `PlayerId`
/// space on purpose: it borrows a real id for its COLOUR, and keying off
/// that made it share a `Carried::fired` slot with the live player wearing
/// the same id — two carriers, one last-fired position, so both re-placed
/// their light on almost every frame.
const CARRIER_DEBUG: u16 = u16::MAX - 1;

/// One remote as this frame sees them.
///
/// `key` and `id` are deliberately separate: the key identifies a
/// CARRIER across frames and the id only picks a colour, which is what
/// lets `DIG_PLAYERBOX` borrow a colour without borrowing a `PlayerId`.
struct Remote {
	key: u16,
	id: u8,
	pos: Vec3,
	yaw: f32,
	held: u16,
}

/// Everything one remote contributes to this frame: the body box, the
/// item in its hand, and the light that item carries.
///
/// ONE site, so the fake remote `DIG_PLAYERBOX` parks exercises exactly
/// what a real one does — which is the only reason that scene is worth
/// having.
fn push_remote(
	bodies: &mut Vec<[f32; 8]>,
	held_out: &mut Vec<(crate::ui::hud::IconKind, [f32; 8])>,
	carried: &mut Carried,
	r: &Remote,
) {
	let Remote { id, pos, yaw, .. } = *r;
	let c = player_color(id);
	bodies.push([pos.x, pos.y, pos.z, yaw, c[0], c[1], c[2], 0.0]);
	let Some(kind) = held_icon(r.held) else {
		return;
	};
	let block = crate::ui::hud::icon_block(kind);
	held_out.push((
		kind,
		[
			pos.x,
			pos.y,
			pos.z,
			yaw,
			f32::from(block.is_some_and(|b| b.props().emissive)),
			crate::item_geom::height(kind) as f32,
			0.0,
			0.0,
		],
	));
	// Level 0: a remote's skills are not in the snapshot, so their tool
	// casts at its row's own level. `carried_light` says why that is the
	// right answer rather than a gap — inventing a rank for someone
	// else's pick would light the room differently for the two of you.
	carried.push(r.key, pos, block, 0);
}

/// How near the camera a carried light has to be for its MOTION to be
/// worth re-placing in the probe field.
///
/// The light itself still reaches the list at any range — this is only
/// about the invalidation sphere, and past this distance the wake it
/// leaves is too dim and too far to read (a torch out there delivers less
/// than one step of 8-bit output before albedo). `GiDraw::invalidate`
/// drains a fixed few spheres a frame and shares that budget with world
/// edits, so a crowd of distant carriers must not be able to spend it.
///
/// This gate is the ONLY bound applied here, deliberately. Past it the
/// queue's own policy is already the right one: bounded, and dropping
/// the OLDEST, because the newest position is the one whose light is
/// most wrong. A second per-frame cap on top would either starve a
/// carrier (skip it, and its last-fire position is already recorded) or
/// hand the probes a position its carrier left seconds ago.
const CARRIED_INVAL_DIST: f32 = 32.0;

/// How far a carried light must travel before its motion is re-placed in
/// the probe field.
///
/// A DISTANCE since the last fire, deliberately **not** a cell crossing.
/// A crossing test is phase-dependent and pathological: a player resting
/// on a cell boundary with the sub-block wobble the tick lerp and the
/// decaying reconciliation offset both produce crosses back and forth
/// every frame, so somebody standing still would fire a sphere every
/// frame. Distance has no such failure mode, is frame-rate independent,
/// needs no hysteresis, and handles a teleport for free.
///
/// One C0 probe spacing, so a probe can never be more than its own cell
/// stale between fires — pinned by `a_carrier_never_outruns_a_probe_cell`.
const CARRY_INVAL_STEP: f32 = 2.0;

/// This frame's carried lights, and the bookkeeping that turns a light
/// which MOVES into a light that was re-PLACED.
///
/// The whole design is in that sentence: a torch in a hand is an
/// ordinary entry in the ordinary GI light list, and its motion is
/// expressed through the same hook a block edit uses. Travelling a probe
/// spacing fires `Renderer::gi_invalidate`, which resets probe ages
/// around the new position — so the bounce field re-traces at full
/// weight instead of decaying slowly from a stale value — and caps the
/// resolve's screen history nearby while it does. Standing still fires
/// nothing, so a lit room settles as deeply as one lit from the floor.
#[derive(Default)]
struct Carried {
	/// What goes to the GI light list, in `FrameScene::carried` order.
	lights: Vec<crate::gi::Emitter>,
	/// Carrier key per entry of `lights`, same index.
	keys: Vec<u16>,
	/// Where each carrier's light last fired an invalidation. A `Vec`
	/// rather than a map because a snapshot holds at most
	/// `MAX_SNAPSHOT_PLAYERS` of these and a linear scan over that many
	/// entries beats hashing one.
	fired: Vec<(u16, Vec3)>,
	/// Positions to invalidate this frame, each with the carrier it
	/// belongs to, drained into the renderer and only then banked by
	/// [`Carried::commit`] — see there for why the two are separate.
	pending: Vec<(u16, [f32; 3])>,
}

impl Carried {
	fn begin(&mut self) {
		self.lights.clear();
		self.keys.clear();
		self.pending.clear();
	}

	/// Add one carrier if what they hold emits at all.
	///
	/// Derived, never special-cased: any block whose `emits` level is
	/// nonzero is a light in a hand exactly as it is a light on the
	/// ground, and it enters the list under the SAME class byte a placed
	/// one does — so tint and cast level come from one LUT row and a
	/// carried torch cannot drift from the torch beside it.
	///
	/// What it does NOT share is `gi::light_origin`. That is applied
	/// CPU-side in `GiDraw::upload_chunk`, baked into a chunk emitter's
	/// world position before it ever reaches the list, and it answers
	/// "where inside its cell does this shape glow" — a question a
	/// carried light has no cell to ask. `carried_origin` is the whole
	/// answer here. Do not "unify" the two: removing the offset in
	/// upload_chunk silently undoes R26 and puts every placed torch's
	/// light back at its cell centre.
	fn push(&mut self, key: u16, eye: Vec3, held: Option<BlockType>, mining: u32) {
		let Some(b) = held.filter(|b| b.props().light > 0) else {
			return;
		};
		let o = digblock_core::gi::carried_origin(eye);
		// What a carried light does NOT take from the class byte: how
		// bright it burns and how blue it stays, when the wielder has
		// earned better. Both ride the entry rather than the LUT, because
		// the same tool in two hands may cast differently — see
		// `core::gi::carried_light`, which is where the rule lives. The
		// level goes out as 0 when it agrees with the row, so an ordinary
		// carrier packs the word R28 packed.
		let cast = digblock_core::gi::carried_light(b, mining);
		self.lights.push(crate::gi::Emitter {
			pos: o.to_array(),
			class: digblock_core::gi::gi_class(digblock_core::chunk::make_cell(b, 0)),
			level: if cast.level == b.props().light {
				0
			} else {
				cast.level
			},
			whitened: cast.whitened,
			// A light in a hand is a point. The one thing that could make
			// it otherwise is the item's own geometry, and an emissive
			// face a few inches across is a point at any distance that
			// matters.
			face: None,
		});
		self.keys.push(key);
	}

	/// Close the frame: note which carriers have travelled far enough to
	/// be re-placed, and which have STOPPED being lights.
	///
	/// A carrier that is NEW fires immediately — the probes around it
	/// have never seen this light — which is what makes selecting a torch
	/// light the room without waiting for a step.
	fn finish(&mut self, cam: Vec3) {
		for (i, &key) in self.keys.iter().enumerate() {
			let p = Vec3::from_array(self.lights[i].pos);
			let slot = self.fired.iter().position(|(k, _)| *k == key);
			if let Some(s) = slot
				&& self.fired[s].1.distance_squared(p) < CARRY_INVAL_STEP * CARRY_INVAL_STEP
			{
				continue;
			}
			// One sphere, not two. A step is far shorter than the sphere
			// `invalidate` queues in either cascade, so the space
			// just vacated is covered many times over; only a jump longer
			// than the radius would need the departure point, and nothing
			// in the game moves a player that far without a teleport,
			// which resets the whole screen history anyway.
			if cam.distance_squared(p) <= CARRIED_INVAL_DIST * CARRIED_INVAL_DIST {
				self.pending.push((key, p.to_array()));
			}
		}
		// A light going OUT is a lighting change exactly as a light
		// arriving is, and it is the half that was missing: `flush_lights`
		// drops the entry from the GPU list the same frame, so the direct
		// pool dies at once — but the probes keep its bounce and decay it
		// over seconds, so a room stayed lit by a torch that had been put
		// away. Digging a PLACED torch has always invalidated, through
		// `take_gi_edits`, which is precisely the asymmetry a carried
		// light is not supposed to have. Covers a remote disconnecting
		// or falling out of the snapshot too.
		let keys = &self.keys;
		let (cam, pending) = (cam, &mut self.pending);
		self.fired.retain(|&(k, p)| {
			if keys.contains(&k) {
				return true;
			}
			if cam.distance_squared(p) <= CARRIED_INVAL_DIST * CARRIED_INVAL_DIST {
				pending.push((k, [p.x, p.y, p.z]));
			}
			false
		});
	}

	/// Bank the positions that actually reached the renderer.
	///
	/// Separate from `finish` because `pending` is drained inside the
	/// `renderer.is_some()` arm: banking at queue time meant that a frame
	/// with no renderer — device loss, or the wait for the GPU at startup
	/// — recorded a fire that never happened, and the carrier then sat
	/// stale until it had walked another whole `CARRY_INVAL_STEP`.
	///
	/// A departed carrier is not re-banked: its key is gone from `fired`
	/// and its light no longer exists, so there is nothing left to
	/// compare a future position against.
	fn commit(&mut self) {
		for &(key, p) in &self.pending {
			let p = Vec3::from_array(p);
			match self.fired.iter().position(|(k, _)| *k == key) {
				Some(s) => self.fired[s].1 = p,
				None if self.keys.contains(&key) => self.fired.push((key, p)),
				None => {}
			}
		}
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
			"world seed: {seed} | {} chunk workers",
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
				touch_jump_button: config.touch_jump_button,
			},
			config.auto_join.clone(),
			config.account.clone(),
		);
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
			prospect: crate::audio::Prospect::default(),
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
			clipboard: None,
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
			tool_swap: crate::hotbar::ToolSwap::default(),
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
			held_instances: Vec::new(),
			carried: Carried::default(),
			confetti: Vec::new(),
			confetti_seq: 0,
			siphon: None,
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
			bench_reported: false,
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
		if engine.config.debug_ore > 0 {
			// The scene exists to photograph a tier, so it grants exactly
			// the level that reveals that tier — read out of the unlock
			// table rather than written down, so retuning a reveal moves
			// the capture with it. `sync_node_reveal` then hands the
			// level to the pipeline, which is what makes the stamp fire.
			let ore = digblock_core::worldgen::ore::ORES[engine.config.debug_ore.clamp(1, 4) - 1];
			let need = digblock_core::unlock::reveal(ore).unwrap_or(1);
			engine.skills.set_total(
				digblock_core::skills::Skill::Mining,
				digblock_core::skills::total_xp_for(need),
			);
			engine.sync_node_reveal(false);
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
			engine.sync_node_reveal(false);
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
			engine.sync_node_reveal(false);
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
				DebugScreen::Inventory => engine.inv_screen.open(),
				DebugScreen::Alchemy | DebugScreen::Transmute | DebugScreen::Transmuting => {
					// A mid-curve level so the header's bar, the pace,
					// and a mix of locked/unlocked rows all photograph.
					engine
						.skills
						.add_xp(digblock_core::skills::Skill::Alchemy, 20_000);
					engine.alch_screen.open();
					if screen == DebugScreen::Transmuting {
						// Stocked well past the batch, so the row reads as
						// a batch running rather than as one about to be
						// cut short by the holdings.
						for e in digblock_core::block::Essence::ALL {
							engine.inventory.add_sub(e, 5_000);
						}
						engine
							.alch_screen
							.debug_batch(BlockType::LumberSlab, 7, 0.45);
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
		// Hang up first if something is still attached. Not defensive
		// clutter: one account holds one live connection, so a replaced
		// connection the server has not noticed yet would make the JOIN
		// WE ARE ABOUT TO MAKE the duplicate, and refuse it.
		self.leave_server();
		self.net = Some(digblock_net::NetClient::new(
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
		self.prospect.reset();
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

	/// Swap in a fresh chunk pipeline, carrying any job pool across.
	/// Every world change (server join, return to title, loading a save)
	/// goes through here: workers cost real time to start and hold no
	/// world state, so respawning them per world would be pure loss —
	/// and `take_dispatch` is what stops the old world's in-flight
	/// columns landing as terrain in the new one.
	fn install_pipeline(&mut self, pipeline: Pipeline) {
		let dispatch = self.pipeline.take_dispatch();
		self.pipeline = pipeline;
		if let Some(dispatch) = dispatch {
			self.pipeline.set_dispatch(dispatch);
		}
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
			if let Some(net) = self.net.as_mut()
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
		use crate::audio::{AmbientZone, AudioCmd, LOOP_AMBIENT, ambient_sfx, step_sfx};
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
		// The prospector's sense (SKILLS-DESIGN §5.5): a faint shimmer near
		// a deposit whose tier you cannot yet see, reaching further at the
		// sense's second rank. Audio only — an overlay would make every
		// reveal level meaningless.
		//
		// It borrows the cave bed's voice as a POSITIONAL one-shot, which
		// is a stand-in: the design asks for its own sourced shimmer, and
		// swapping this to a new `Sfx` variant is the whole change when
		// one exists.
		let mining = self.skills.level(digblock_core::skills::Skill::Mining);
		let near = digblock_core::unlock::prospector(mining)
			.then(|| {
				digblock_core::worldgen::ore::unseen_nearby(
					(
						cam.x.floor() as i32,
						cam.y.floor() as i32,
						cam.z.floor() as i32,
					),
					self.seed,
					mining,
				)
			})
			.flatten();
		if self.prospect.advance(near.is_some(), dt)
			&& let Some(d) = near
		{
			self.emit(AudioCmd::Sfx {
				sound: crate::audio::Sfx::AmbientCave,
				pos: Some(cell_center((d.x, d.y, d.z))),
				gain: crate::audio::PROSPECT_GAIN,
			});
		}

		// Music pool follows the world clock. `tod` is the sun's angle
		// from straight down (0 = noon, ±90 = the horizons, 180 =
		// midnight), so hour = 12 - tod/15 and the night window is
		// exactly "sun below the horizon". No hysteresis needed: tod is
		// monotonic, and the filler only acts on this at the next track
		// boundary anyway.
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
		// Its `Skills` (always sent, right behind `Layout`) is the same
		// kind of base, and clearing these is not about the stale NUMBER
		// — it is about who reads it before the real one lands.
		// `reset_world_transients` below hands the Mining level straight
		// to the pipeline we just installed (`sync_node_reveal`), so a
		// previous world's level would stamp veins into THIS world's
		// terrain for the whole window between `Welcome` and `Skills`. In
		// that window `ore_dig_mode` answers `NeedsSiphon` on cells the
		// server calls plain stone, so `diggable` refuses holds the server
		// would have accepted — and the correction, when `Skills` arrives,
		// costs a full `remesh_all`. Level 1 is the honest guess: it
		// under-reveals rather than over-, and every reveal it misses
		// arrives moments later with the real totals. Toasts go with the
		// totals they animate, exactly as in the other two entry points.
		self.skills = Default::default();
		self.xp_toasts.clear();
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
		// them left hanging in the air. Same for the essence draw, which
		// names a cell in the world just left.
		self.confetti.clear();
		self.siphon = None;
		// And the bench job, which survives the SCREEN closing but not
		// the world: it points at this world's stacks and this server's
		// bucket, neither of which exists in whatever comes next.
		self.alch_screen.abandon();
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
		self.prospect.reset();
		self.swim_strokes.reset();
		self.view_bob.reset();
		self.view_swing.reset();
		self.view_swap.reset();
		// The tool slot's handover animates whenever the active tool
		// changes, which is right for a cycle and wrong for a world entry:
		// the bar we are about to draw is state ARRIVING, not an exchange
		// anybody performed. Advancing by a full window completes whatever
		// was in flight and adopts the tool the bar holds now, so the
		// first frame draws it at rest. A bare `Default::default()` will
		// not do — it shows the BUCKET, which is the wrong half of the
		// problem: entering a save whose layout makes any other tool
		// active would still grow that tool in from a bucket that was
		// never in the hand. Callers install the layout first
		// (`enter_local_world` reconciles before it calls here); a join
		// has none yet, and the server `Layout` rebase animating when it
		// lands is the behaviour `ToolSwap` documents.
		self.tool_swap
			.advance(crate::hotbar::ToolSwap::SECS, self.hotbar.active_tool());
		// A replaced pipeline starts with no Mining level at all, so a
		// world entry has to hand it the level again or a high-level miner
		// walks into plain stone until the next level-up. Silent: the
		// discovery was made long ago.
		self.sync_node_reveal(false);
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
		if let Some(store) = self.world_store.as_mut()
			&& self.in_local_save
		{
			for (x, y, z, cell) in recorded {
				store.append_edit(x, y, z, cell);
			}
			store.flush(self.pipeline.overlay());
			store.close_world();
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
	/// timeout escape hatch cannot fire on a hold that was never set.
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
	/// and camera — the reconciliation slide across half the map would
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

	/// Hang up deliberately: say goodbye, then drop the connection.
	/// `true` when there was one to drop.
	///
	/// EVERY path that ends a session on purpose goes through here —
	/// Leave Server, Leave World, closing the window. The server frees
	/// the account the moment it reads the `Bye`; skip it and the player
	/// waits out an idle timeout before they can rejoin, since one
	/// account may hold only one live connection.
	fn leave_server(&mut self) -> bool {
		let Some(mut net) = self.net.take() else {
			return false;
		};
		net.disconnect();
		true
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
				// predictor's buffer only reaches back `BUF_CAP` ticks:
				// an ack delayed past that falls out of the buffer, so
				// EVERY later snapshot takes the forced-replay path —
				// measured as a continuous stream of replays carrying
				// corrections big enough to see. Join sync on a heavily
				// edited world is enough to get there at
				// NET_EDIT_BUDGET per frame.
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
					&& edits_applied.is_multiple_of(NET_EDIT_CHECK)
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
					if let Some(author) = remote_author
						&& author != digblock_net::protocol::SERVER_ID
					{
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
					// `inv_delta` is the dig/place discriminator (a
					// positive count is a dig credit, a negative one a
					// place debit) — NOT `placed == Air`, which a slab
					// dig breaks by writing its shrunk value. A
					// multi-cell action carries the delta on its first
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
							R::NeedsTool => digblock_core::unlock::Kind::Tool.message(),
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
					// state arriving, not an award happening. The ore
					// reveal still has to follow it, or a Mining-60 player
					// joins a server and sees plain stone until the next
					// level-up.
					self.skills = digblock_core::skills::SkillSet::from_wire(&xp);
					self.sync_node_reveal(false);
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
	/// yaw sweep, advanced by a fixed sim tick so the path is a pure
	/// function of how many times this has been called — identical
	/// workload run-to-run, and identical pose whether the steps are
	/// spread over frames (`--fly`) or taken at once (`--warp`).
	fn fly_step(&mut self) {
		const BENCH_YAW: f32 = 12.0; // degrees per path-second
		// The walk: forward and back about a fixed origin, at eye
		// height with the ground just ahead — the repro as reported.
		// An oscillation rather than a line keeps the column under the feet
		// constant, so the eye cannot step a whole block mid-path and
		// invalidate the history for a reason that is not the one under
		// test.
		const WALK_AMP: f32 = 1.0; // blocks either side of the origin
		const WALK_PERIOD: f32 = 1.5; // seconds — its peak speed is a walk
		const WALK_PITCH: f32 = -25.0; // puts the ground just ahead of the eye
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
		// Sampled every frame from the executor rather than set once from
		// the config, because the config is a REQUEST. A web pool that
		// failed to start, hasn't finished its handshake, or has had every
		// worker written off reports 0 here and streams inline — and a run
		// that streamed inline must not be recorded in BENCH.md as a
		// pooled arm, which is what the `?workers=` table is selected by.
		self.metrics.workers = self.pipeline.worker_count() as u32;
		// ONE deadline for jobs, drain, evictions and uploads together.
		// Previously each stage got its own slice of the budget and they
		// summed to more than it; now they consume from the same clock, and the
		// per-stage minimum-progress floors keep any single stage from
		// being starved by an earlier one that overran.
		let deadline = t_maintain + Duration::from_secs_f32(budget_ms / 1000.0);
		// Offered unconditionally under an adaptive budget; `maintain`
		// consumes it only while jobs run on this thread, so a web client
		// whose worker pool is still starting (or was refused) keeps
		// exactly the pre-pool behaviour.
		let allowance = adaptive.then(|| Duration::from_secs_f32(budget_ms / 1000.0));
		let m = self
			.pipeline
			.maintain(center, usize::MAX, Some(deadline), allowance);
		self.pending.maintain_ms = ms_since(t_maintain);
		// RTGI: scroll the window to the camera (re-dirtying whatever
		// just entered coverage), pull freshly-dirty chunks into the
		// re-upload queue, then feed the voxel window under a byte budget
		// — a fraction of the mesh-upload ceiling — AND the shared
		// streaming deadline (GI-DESIGN.md).
		//
		// The clock matters as much as the bytes: `classify_gi` walks
		// every cell of a chunk, and the byte cap alone permits many
		// chunks' worth of that in one frame. This was the only streaming
		// stage that ignored `deadline`, and the only one charged to no
		// phase — a classify burst landed in `busy_ms` with nothing to
		// attribute it to. `GI_UPLOAD_MIN` is the same minimum-progress
		// floor the mesh loop uses, so a frame that is already over budget
		// still makes headway instead of starving the window forever.
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
			// A boundary crossing evicts a whole rim arc at once —
			// hundreds of chunks. Membership has to be a hash lookup, not
			// a linear scan of the evicted Vec per pending element, and the
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
			&& self.hotbar.active_tool() == Some(BlockType::Bucket)
	}

	/// The owned tool roster in deterministic block-id order: the
	/// always-owned bucket plus every tool id with a nonzero count.
	///
	/// The predicate lives in core so the rail this feeds and the
	/// ownership check `reconcile_layout` makes are literally the same
	/// question — a client offering a tool its own server would take back
	/// is a rejected swing, not a feature.
	fn owned_tools(&self) -> Vec<BlockType> {
		self.inventory.owned_tools().collect()
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

	/// What a dig on this cell IS for us right now (SKILLS-DESIGN §5.3)
	/// — the same `ore::dig_mode` the server asks of its own world, so
	/// prediction and authority cannot disagree about whether an action
	/// was a dig, a draw, or a refusal.
	///
	/// The chunk store still holds Stone here: the reveal is a stamp on
	/// the MESH input, never a world write, which is what keeps ore out of
	/// the journal and out of everyone else's world.
	fn node_dig_mode(&self, cell: (i32, i32, i32)) -> digblock_core::worldgen::nodes::DigMode {
		use digblock_core::worldgen::nodes::DigMode;
		let Some(v) = self.pipeline.cell_at(cell.0, cell.1, cell.2) else {
			return DigMode::Break;
		};
		digblock_core::worldgen::nodes::dig_mode(
			v,
			cell,
			self.seed,
			digblock_core::worldgen::nodes::Levels::of(&self.skills),
			self.pipeline.cell_edited(cell.0, cell.1, cell.2),
			self.hotbar.tool_in_hand(),
		)
	}

	/// Has this cell ever been written — the provenance half of
	/// `BlockType::dig_credit`, and the same bit `ore_dig_mode` above
	/// passes to the reveal. Read off the `EditOverlay`, which MIRRORS the
	/// server's journal in multiplayer, so both ends answer alike.
	///
	/// A place of one's own is recorded when the server echoes it rather
	/// than when it is predicted (recording predictions is unfixable — a
	/// rejection reverts through `set_cell` and `record` is insert-only),
	/// so for one round trip a stone you just laid still reads as the
	/// world's. That window cannot be reached by hand: a Rock-family hold
	/// is orders of magnitude longer than a round trip, and the pick only
	/// lengthens it.
	fn cell_edited(&self, cell: (i32, i32, i32)) -> bool {
		self.pipeline.cell_edited(cell.0, cell.1, cell.2)
	}

	/// The EXTRA cells a Resonant Pick hold breaks around `hit` — empty
	/// unless the pick is in hand at its own Mining level. The set is
	/// derived from the ray, never chosen: the server re-raycasts and
	/// runs the same `pick::cells` over its own world.
	fn pick_plane(&self, hit: &RayHit) -> Vec<(i32, i32, i32)> {
		if self.hotbar.active_tool() != Some(BlockType::ResonantPick) {
			return Vec::new();
		}
		let level = self.skills.level(digblock_core::skills::Skill::Mining);
		let alchemy = self.skills.level(digblock_core::skills::Skill::Alchemy);
		let id = digblock_core::chunk::cell_id(hit.cell_value);
		let credit = hit.block.dig_credit(self.cell_edited(hit.cell), alchemy);
		let mut out = digblock_core::pick::cells(
			hit.cell,
			hit.normal,
			self.camera.dir().to_array(),
			digblock_core::unlock::pick_cells(level),
			// The same rule the server runs over its own world — including
			// the world floor, which `dig_hardness` refuses at exactly the
			// y `protect::immutable` does. A server's spawn lock is its own
			// secret and stays unpredicted, as everywhere else.
			|c| {
				self.pipeline.cell_at(c.0, c.1, c.2).is_some_and(|v| {
					digblock_core::pick::breaks_as_extra(
						id,
						credit,
						v,
						c,
						self.node_dig_mode(c),
						self.cell_edited(c),
						alchemy,
					)
				})
			},
		);
		// The aimed cell leads the list and the caller already has it.
		out.retain(|&c| c != hit.cell);
		out
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
		// A revealed vein and the wrong tool: predicted here so the hold
		// never opens, exactly as the server refuses it at `DigStart`.
		if self.node_dig_mode(hit.cell) == digblock_core::worldgen::nodes::DigMode::NeedsSiphon {
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

	/// Drop every live touch: the driver's look/move/dig state, the HUD's
	/// claims, and the held state they were driving. The one call every
	/// "touches are gone" site makes (focus loss, a screen opening), so a
	/// held button can never survive its own finger.
	fn clear_touch_state(&mut self) {
		self.touch.clear();
		self.hud_touches.clear();
		self.hud_jump_held = false;
	}

	/// Physical-px display cutouts (notch, home indicator) the touch HUD
	/// keeps its buttons clear of. Shell-forwarded; zero when the shell
	/// cannot measure any.
	pub fn set_safe_area(&mut self, insets: crate::ui::hud::SafeArea) {
		self.safe_area = insets;
	}

	/// Mouse hold or touch hold — the one question the dig machinery asks.
	/// Touch is deliberately NOT folded into `dig_held`: that flag is
	/// force-cleared at many sites (screen opens, focus loss, slot swaps)
	/// whose touch half the driver's own `clear` already covers, and two
	/// writers to one flag would have to agree at every one of them.
	fn dig_active(&self) -> bool {
		self.dig_held || self.touch.dig_held()
	}

	/// One fixed-rate step of the hold-to-dig state machine. Purely
	/// predictive online — the server runs its own timer and aim checks.
	fn step_dig(&mut self) {
		// A charged bucket has no left-click action at all — not even the
		// "you can't dig that" complaint below, which belongs to a dig.
		if !self.dig_active() || (self.water_tool_selected() && !self.scoop_ready()) {
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
			} else if self.node_dig_mode(h.cell)
				== digblock_core::worldgen::nodes::DigMode::NeedsSiphon
			{
				Some((h.cell, digblock_core::unlock::Kind::Tool.message()))
			} else {
				digblock_core::unlock::dig_blocked(h.block, &self.skills)
					.map(|req| (h.cell, req.kind.message()))
			}
		}) && self.refused_cell != Some(cell)
		{
			self.refused_cell = Some(cell);
			self.push_feed(msg.into());
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
			// An EXTRACTION is timed as the ORE rather than as the stone
			// the cell still holds, so a deep seam takes longer than a
			// shallow one and the ore rows' hardnesses are not dead
			// numbers on blocks nobody digs.
			let mode = self.node_dig_mode(hit.cell);
			let timed = digblock_core::worldgen::nodes::dig_block(mode, hit.block);
			let required = timed
				.dig_hardness(hit.cell.1)
				.map(|h| {
					digblock_core::skills::dig_required_with(
						timed,
						h,
						self.skills.level(digblock_core::skills::Skill::Mining),
						self.hotbar.tool_in_hand(),
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
			if let Some(net) = self.net.as_mut()
				&& net.ready()
			{
				net.send_dig_start(hit.cell);
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
				// A draw rides the SAME voice as a dig, so every stop path
				// — release, cancel, completion, world swap — already
				// covers it, and the pitch ramp below reads as a resonance
				// winding up rather than as a pick speeding up. WHICH sound
				// it is belongs to `audio::dig_voice`, together with the
				// finish it has to agree with.
				let extracting =
					matches!(mode, digblock_core::worldgen::nodes::DigMode::Extract(_));
				self.emit(crate::audio::AudioCmd::LoopStart {
					id: crate::audio::LOOP_DIG,
					sound: crate::audio::dig_voice(extracting, hit.block).0,
					pos: Some(cell_center(hit.cell)),
					// Under the dig loop: a sustained hum at a hammer's
					// level would wear out over a hold that runs longer
					// than any dig.
					gain: if extracting { 0.85 } else { 1.0 },
				});
			}
			return;
		}
		let (done, pitch) = {
			let d = self.dig.as_mut().expect("live hold checked");
			d.ticks += 1;
			// Pitch ramp on a subdivision of the tick: the loop climbs
			// toward completion.
			let pitch = (d.ticks.is_multiple_of(6) && d.required > 0)
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
	/// the button no longer costs the whole block. Called once per sim
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
			if let Some(net) = self.net.as_mut()
				&& net.ready()
			{
				net.send_dig_cancel();
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
			if let Some(net) = self.net.as_mut()
				&& net.ready()
			{
				net.send_dig_cancel();
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
		// (cell, prev) list this dig clears — a door adds its partner, the
		// Resonant Pick adds the plane of the face (SKILLS-DESIGN §5.6).
		// Every extra cell holds the SAME id and is the plain breakable
		// case (`pick::breaks_as_extra`), which is what makes
		// `begin_multi`'s single `inv_delta` a correct description of the
		// whole group — and what keeps the aimed door's own partner out of
		// the plane, where it used to be counted AND written twice.
		let mut cleared = vec![(hit.cell, prev_cell)];
		for c in self.pick_plane(&hit) {
			if let Some(v) = self.pipeline.cell_at(c.0, c.1, c.2) {
				cleared.push((c, v));
			}
		}
		// How many cells this ONE hold broke — one normally, a whole plane
		// with the pick, and the count `begin_multi`'s single `inv_delta`
		// carries for the whole group.
		let broken = cleared.len() as u32;
		if prev.is_door() {
			let partner = digblock_core::shapes::door_partner(
				hit.cell,
				digblock_core::chunk::cell_state(prev_cell),
			);
			if let Some(pc) = self.pipeline.cell_at(partner.0, partner.1, partner.2)
				&& BlockType::from_id(digblock_core::chunk::cell_id(pc)).is_door()
			{
				cleared.push((partner, pc));
			}
		}
		// A field extraction (SKILLS-DESIGN §5.3): the vein is DRAWN out,
		// not dug out, so the cell becomes a tapped face and the mountain
		// keeps its shape. Resolved before the write, because it changes
		// both what is written and what is credited.
		let extracting = match self.node_dig_mode(hit.cell) {
			digblock_core::worldgen::nodes::DigMode::Extract(node) => Some(node),
			_ => None,
		};
		// What the dig pays, resolved before the write for the same reason
		// and one of its own: in a LOCAL world this engine is the authority,
		// so `set_cell` records the coordinate itself — asking afterwards
		// would find every dig standing in an edited cell and hand back
		// clean stone for a quarried one.
		let credit = prev.dig_credit(
			self.cell_edited(hit.cell),
			self.skills.level(digblock_core::skills::Skill::Alchemy),
		);
		// Slabs shrink one third per completed dig (2/3 → 1/3 → Air);
		// everything else clears. Door partners always clear.
		let new_val = if prev.is_slab() {
			digblock_core::shapes::slab_shrunk(prev_cell)
		} else if let Some(node) = extracting {
			digblock_core::worldgen::nodes::spent_block(node).expect("an Extract names a node kind")
				as u16
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
		// A finished draw flares and then dies on the ordinary fade-out
		// rather than blinking off with the seam it emptied. What it
		// flares FROM is already banked in `SiphonDraw`, which is the whole
		// reason that state outlives the hold: the cell is spent rock by
		// the write above, so there is no vein left to ask.
		if extracting.is_some()
			&& let Some(s) = self.siphon.as_mut()
		{
			s.progress = 1.0;
			s.envelope = crate::siphon::FLARE;
		}
		// A filled bucket splashes; everything else ends the way its hold
		// began, through the pair `audio::dig_voice` owns — a drawn seam
		// lets go, a broken block breaks. Water's material class is Earth
		// (the table's total fallback), which is right for nothing it does,
		// which is why it is the one case answered here.
		let tool = self.water_tool_selected();
		let extracted = extracting.is_some();
		let (end, end_gain) = if tool {
			(crate::audio::Sfx::Splash, 0.5)
		} else {
			(
				crate::audio::dig_voice(extracted, prev).1,
				if extracted { 0.9 } else { 1.0 },
			)
		};
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: end,
			pos: Some(cell_center(hit.cell)),
			gain: end_gain,
		});
		// No swing is started here: the arc that began with this hold is
		// running out on this very frame, and re-striking it is what used
		// to bolt a whole second stroke onto a finished scoop.
		// One credit even for the two-cell door.
		//
		// An extraction credits NOTHING here, and that is not an omission.
		// A drawn seam pays essence (`alchemy::field_yield`) rather than a
		// block — the cell is Stone and Stone hands out Rubble, which is
		// exactly what quarrying should give and exactly what drawing a
		// seam should not — and essence arrives as a SPLIT, which a
		// `PendingEdit`'s single `inv_delta` cannot carry. So online it is
		// deliberately un-predicted and the server's `InventoryFull` rebase
		// is the answer, the same path `ClientMsg::Alchemy` already takes;
		// offline the arm below pays it, where the engine is the authority.
		// The CELL is predicted on both, so the vein turns spent under the
		// player's hand with no latency either way.
		//
		// Everything else pays the `credit` resolved above the write: a
		// stone somebody MADE comes back whole where the mountain's
		// crumbles, so quarrying still hands out rubble and taking a
		// mason's wall apart returns the wall.
		let drop = if extracting.is_some() { None } else { credit };
		if let Some(d) = drop {
			self.inventory.add(d, broken);
			self.hotbar.auto_assign(d);
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		match self.net.as_mut() {
			Some(net) if net.ready() => {
				// `tool` is `water_tool_selected` — the SLOT is not enough.
				// It is the same predicate `diggable` and the scoop ray use,
				// and the same one the server's Collect gate asks
				// (`slot_item == WaterTool && active_tool == Bucket`).
				// Reading the slot alone called every hold with the siphon,
				// pick or sigil in hand a Collect, which the server answered
				// WrongSlot: cell and credit reverted, so with any tool but
				// the bucket active the player could dig NOTHING online —
				// veins included — while offline the identical input worked.
				let kind = if tool {
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
				let seq = net
					.edits
					.begin_multi(&entries, tick, drop.map(|d| (d, broken as i32)));
				let slot = self.hotbar.selected_index() as u8;
				net.send_edit(seq, tick, origin, dir, kind, hit.cell, (0, 0, 0), slot);
			}
			_ => {
				for &(c, _) in &cleared {
					self.fluid.schedule_around(c.0, c.1, c.2);
				}
				// Offline, the engine IS the authority — for the essence a
				// drawn seam pays as much as for the XP, and both go
				// through the same shared functions the server's
				// chokepoint calls, so §5.2's depth reward, §5.4's dual
				// award at the rock face and §5.3's field yield reach both
				// economies identically.
				if let Some(ore) = extracting {
					let mining = self.skills.level(digblock_core::skills::Skill::Mining);
					let alchemy = self.skills.level(digblock_core::skills::Skill::Alchemy);
					for (e, n) in digblock_core::alchemy::field_yield(
						ore, alchemy, mining, hit.cell, self.seed,
					) {
						self.inventory.add_sub(e, n);
					}
				}
				let (mining_xp, alch_xp) =
					digblock_core::skills::dig_award(prev, extracting, broken, hit.cell.1);
				self.award_xp_offline(digblock_core::skills::Skill::Mining, mining_xp);
				if alch_xp > 0 {
					self.award_xp_offline(digblock_core::skills::Skill::Alchemy, alch_xp);
				}
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
			//
			// The BUCKET, not the slot: the server's place gate requires
			// `active_tool == Bucket`, so pouring with the pick or siphon
			// active answered WrongSlot and the source appeared and
			// vanished. Offline there was no such correction and it simply
			// poured — a pick that puts down water.
			Some(HotbarItem::WaterTool)
				if self.hotbar.active_tool() == Some(BlockType::Bucket)
					&& self.inventory.bucket_full() =>
			{
				BlockType::Water
			}
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
			// Stairs and furniture with a front carry their facing in the
			// same state field, so one branch turns both — and a new
			// facing shape cannot forget to be turned at all.
			let placed = if block.takes_facing() {
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
		// Protected cells, scanned over the ASSEMBLED placements so a
		// door's derived upper half and a slab grow (which rewrites the
		// hit cell, not the one past its face) are both covered — the
		// mirror of the server's scan over `writes`. Until now the only
		// thing between a place and the world floor was
		// `cell_is_replaceable`, which asks about block IDENTITY, and the
		// floor rule is the one rule that must never depend on what
		// happens to be standing there. Only the floor is knowable
		// client-side; a spawn lock arrives as a rejection instead.
		if placements
			.iter()
			.any(|&(c, _)| digblock_core::protect::immutable(c.0, c.1, c.2, None))
		{
			self.push_feed(digblock_core::protect::REFUSE_PLACE_MSG.into());
			return;
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
		if let Some(pc) = self.pipeline.cell_at(partner.0, partner.1, partner.2)
			&& BlockType::from_id(cell_id(pc)).is_door()
		{
			entries.push((partner, pc, door_toggled(pc)));
		}
		// A door in a protected cell is furniture, not a mechanism — the
		// same refusal the server makes over its own write list, and
		// scanned over both halves because a leaf can straddle the floor
		// with only its upper cell reachable.
		if entries
			.iter()
			.any(|&(c, _, _)| digblock_core::protect::immutable(c.0, c.1, c.2, None))
		{
			return;
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
		if let Some(net) = self.net.as_mut()
			&& net.ready()
		{
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
				if let Some(Some(HotbarItem::Block(b))) = self.hotbar.slots().get(slot).copied()
					&& self.hotbar.clear_slot(slot)
				{
					self.inv_grid.file(b);
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
		use digblock_core::inventory::{AlchemyOp, apply_alchemy};
		let AlchAction::Craft { item, count } = act;
		let core_op = AlchemyOp::Craft { item, count };
		let wire_op = digblock_net::messages::AlchemyOp::Craft {
			item: item as u16,
			count,
		};
		match self.net.as_mut() {
			Some(net) if net.ready() => net.send_alchemy(wire_op),
			_ => {
				let level = self.skills.level(digblock_core::skills::Skill::Alchemy);
				if apply_alchemy(&mut self.inventory, core_op, level) {
					self.hotbar.auto_assign(item);
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
			self.sync_node_reveal(true);
		}
	}

	/// Hand the pipeline the skill levels its node stamp reveals at, and
	/// sweep every resident chunk when that crosses a threshold on either
	/// axis (§5.3 — Mining for the ore tiers, Alchemy for the surface
	/// nodes).
	///
	/// The sweep is `remesh_all`, which only ENQUEUES — the mesh heap
	/// drains under the ordinary `STREAM_BUDGET_MS`, so the veins fade in
	/// around you over a second or two instead of hitching the frame that
	/// levelled you up.
	fn sync_node_reveal(&mut self, announce: bool) {
		let levels = digblock_core::worldgen::nodes::Levels::of(&self.skills);
		if !self.pipeline.set_levels(levels) {
			return;
		}
		self.pipeline.remesh_all();
		// Only a LEVEL-UP says so. Entering a world, or a join sync
		// handing over a Mining 60 you already had, crosses the same
		// thresholds against a fresh pipeline and would announce a
		// discovery you made an hour ago.
		if announce {
			self.push_feed("New veins shimmer into view.".into());
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
		// A field extraction shows the outline and NO crack — the water
		// tool's precedent. Nothing is breaking: the seam is being drawn
		// out of rock that stays standing, and a spreading web would say
		// the opposite of what is happening.
		if matches!(
			self.node_dig_mode(hit.cell),
			digblock_core::worldgen::nodes::DigMode::Extract(_)
		) {
			return Some(boxed(None));
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

	/// The vein the siphon is drawing from this frame: the hit, the tier,
	/// and how far through the hold it is.
	///
	/// Deliberately narrower than `dig_progress`'s extraction branch,
	/// which answers on AIM: merely looking at a seam with the siphon out
	/// draws nothing, because the stream is the tool working rather than
	/// the tool being held. So it wants the button down and the hold live
	/// — a released hold is decaying, and the draw fades out with it,
	/// which is the same statement the retreating crack makes about a
	/// released dig.
	fn siphon_target(&self) -> Option<(RayHit, BlockType, f32)> {
		let hit = self.target?;
		if !self.diggable(&hit) {
			return None;
		}
		let digblock_core::worldgen::nodes::DigMode::Extract(ore) = self.node_dig_mode(hit.cell)
		else {
			return None;
		};
		// The capture pin answers on aim alone, exactly as `debug_crack`
		// does, and for the same reason: standing in a hold is the one
		// thing a headless run cannot do.
		let progress = match self.config.debug_siphon {
			Some(p) => p,
			None => {
				let d = self
					.dig
					.as_ref()
					.filter(|d| d.cell == hit.cell && d.idle == 0)?;
				f32::from(d.banked()) / f32::from(d.required.max(1))
			}
		};
		Some((hit, ore, progress.clamp(0.0, 1.0)))
	}

	/// Age the essence draw: in while the siphon is working a vein, out
	/// once it stops.
	///
	/// Runs after the dig stepping, because what it reads is that step's
	/// result. The geometry is rewritten every live frame rather than
	/// latched, so following a seam from one cell to the next is one
	/// continuous stream — the envelope carries over — instead of a
	/// stutter of fades.
	fn step_siphon(&mut self, dt: f32) {
		let Some((hit, ore, progress)) = self.siphon_target() else {
			// Nothing to draw from: run whatever was running out, and drop
			// it at the bottom — which is also how the flare a completed
			// draw ends on gets spent.
			if let Some(s) = self.siphon.as_mut() {
				s.envelope = crate::siphon::envelope_step(s.envelope, false, dt);
			}
			if self.siphon.is_some_and(|s| s.envelope <= 0.0) {
				self.siphon = None;
			}
			return;
		};
		let envelope =
			crate::siphon::envelope_step(self.siphon.map_or(0.0, |s| s.envelope), true, dt);
		let c = cell_center(hit.cell);
		let n = hit.normal;
		// `Extract` names one of the four tiers, so the palette is always
		// there; white is a visible wrong answer rather than a panic on a
		// path that only draws pixels.
		let (core, _) = ore.node_flecks().unwrap_or(([255; 3], [255; 3]));
		self.siphon = Some(SiphonDraw {
			source: Vec3::new(
				c[0] + n.0 as f32 * 0.5,
				c[1] + n.1 as f32 * 0.5,
				c[2] + n.2 as f32 * 0.5,
			),
			normal: n,
			tint: core.map(|v| f32::from(v) / 255.0),
			progress,
			envelope,
		});
	}

	/// Advance WHICH item is in the hand, and the swap dip between two.
	///
	/// Its own method because it has to run on three paths: once at the
	/// tail of the interactive update, where it must follow
	/// `reconcile_layout`; and once before each scripted-flight return,
	/// because the hand is part of the frame a capture photographs. The
	/// viewmodel draws `Swap::shown()` and a carried light hangs off it,
	/// so a harness that never advances this can neither draw a held item
	/// nor measure one — `--bench` could do neither, and the first attempt
	/// at R28's cost bracket read both `ginee` and `emitters` as empty and
	/// looked like a correct result. Costs the old baselines nothing: they
	/// fly with an empty bar, so `selected` is None and `shown` stays
	/// None. The bob and the swing stay interactive-only, since they are
	/// driven by motion the scripted camera does not have.
	fn advance_hand(&mut self, dt: f32) {
		let full = self.inventory.bucket_full();
		self.tool_swap.advance(dt, self.hotbar.active_tool());
		let selected = self.hotbar.selected_item().and_then(|i| match i {
			// `HotbarItem::WaterTool` names the SLOT, not what is in it,
			// so the hand has to ask which tool is active — otherwise
			// every tool in the game is held as a bucket. `shown` rather
			// than `active_tool`, so the hand and the slot's icon change
			// over together — and a BARE slot (no vessel crafted yet)
			// holds nothing, so the hand is empty on it.
			HotbarItem::WaterTool => self
				.tool_swap
				.shown()
				.map(|t| crate::ui::hud::tool_icon(t, full)),
			other => Some(crate::ui::hud::item_icon(other, full).0),
		});
		self.view_swap.advance(dt, selected);
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
	/// rendered surface of a resting source sits `WATER_SURFACE_DROP`
	/// below the cell top (water.wgsl sinks a corner by `d/9`) — without
	/// the offset the underwater atmosphere pops in while the camera is
	/// still visibly above water.
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
			// camera far plane, which is a larger multiple of draw_dist for
			// legacy reasons and therefore sits well beyond anywhere terrain
			// exists at all. Passing the far plane here (M2 did) put the rim
			// ramp outside the streamed cylinder entirely, so the
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
mod carried_tests {
	use super::*;

	fn torch_at(c: &mut Carried, key: u16, eye: Vec3) {
		c.push(key, eye, Some(BlockType::Torch), 1);
	}

	/// One frame of one carrier holding a torch, in exactly the order
	/// `Engine::render` runs it — `commit` included, since that is what
	/// banks the fire, and without it every frame looks like the first.
	/// Answers how many spheres the frame queued.
	fn frame(c: &mut Carried, key: u16, eye: Vec3, cam: Vec3) -> usize {
		frame_holding(c, key, eye, cam, Some(BlockType::Torch))
	}

	/// The same, for a hand holding anything (or nothing).
	fn frame_holding(
		c: &mut Carried,
		key: u16,
		eye: Vec3,
		cam: Vec3,
		held: Option<BlockType>,
	) -> usize {
		c.begin();
		c.push(key, eye, held, 1);
		c.finish(cam);
		let n = c.pending.len();
		c.commit();
		n
	}

	/// A carried light must be re-placed no less often than the bounce
	/// field can resolve, or a carrier outruns a whole probe cell between
	/// fires and the cache holds light from somewhere they have left.
	#[test]
	fn a_carrier_never_outruns_a_probe_cell() {
		assert!(
			CARRY_INVAL_STEP <= crate::gi::C0_SPACING as f32,
			"the step must stay inside C0's own spacing"
		);
	}

	/// The whole cost model rests on this: a player standing still with a
	/// torch is not a moving emitter, so nothing is invalidated and the
	/// room they are in converges as deeply as one lit from the floor.
	///
	/// It is also why the trigger is a DISTANCE and not a cell crossing.
	/// The second half of this test is the pathology a crossing rule
	/// has: a carrier wobbling either side of a cell boundary — which is
	/// what the tick lerp and the decaying reconciliation offset produce
	/// — would fire on every single frame.
	#[test]
	fn a_still_carrier_is_not_a_moving_emitter() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		for f in 0..600 {
			// The first frame places the light: the probes around it have
			// never seen it, which is what lights the room the moment you
			// select a torch.
			let want = usize::from(f == 0);
			assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), want, "frame {f}");
		}
		// Straddling a cell boundary, a hundredth of a block either way.
		let edge = Vec3::new(64.0, 70.5, 64.0);
		let mut c = Carried::default();
		frame(&mut c, CARRIER_SELF, edge, edge);
		for f in 0..600 {
			let p = edge + Vec3::Z * if f % 2 == 0 { 0.01 } else { -0.01 };
			assert_eq!(frame(&mut c, CARRIER_SELF, p, p), 0, "wobble fired on {f}");
		}
	}

	/// Walking fires once per step travelled, not once per frame — the
	/// probes around the carrier re-trace at full weight instead of
	/// decaying from a stale value.
	#[test]
	fn a_walking_carrier_fires_once_per_step() {
		let start = Vec3::new(64.0, 70.0, 64.0);
		let per_frame = digblock_core::player::WALK_SPEED / 60.0;
		let mut c = Carried::default();
		let mut fires = 0usize;
		let frames = 300; // five seconds
		for f in 0..frames {
			let p = start + Vec3::Z * (per_frame * f as f32);
			fires += frame(&mut c, CARRIER_SELF, p, p);
		}
		let walked = per_frame * (frames - 1) as f32;
		// One placement plus one per step of travel.
		let want = 1 + (walked / CARRY_INVAL_STEP) as usize;
		assert_eq!(fires, want, "walked {walked} blocks");
	}

	/// Only a block that EMITS is carried, and the answer is derived from
	/// the block table rather than named — so a second emitter needs no
	/// edit here, and the bucket (whose charge is Water, and whose empty
	/// state decodes to a tool sentinel) is never a light.
	#[test]
	fn only_an_emitting_block_lights_a_hand() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		for b in BlockType::ALL {
			let mut c = Carried::default();
			c.begin();
			c.push(CARRIER_SELF, eye, Some(b), 1);
			assert_eq!(
				c.lights.len(),
				usize::from(b.props().light > 0),
				"{b:?} light {}",
				b.props().light
			);
		}
		let mut c = Carried::default();
		c.begin();
		c.push(CARRIER_SELF, eye, None, 1);
		assert!(c.lights.is_empty(), "an empty hand is not a light");
	}

	/// A carried torch enters the list under the same class byte a placed
	/// one does, so tint and cast level come from one LUT row. NOT the
	/// flame offset — that is `gi::light_origin`, applied CPU-side to
	/// chunk emitters only, and a carried light has no cell to offset
	/// within; see `Carried::push`.
	#[test]
	fn a_carried_torch_borrows_the_placed_torchs_class() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		c.begin();
		torch_at(&mut c, CARRIER_SELF, eye);
		let want =
			digblock_core::gi::gi_class(digblock_core::chunk::make_cell(BlockType::Torch, 0));
		assert_eq!(c.lights[0].class, want);
		assert!(
			want >= digblock_core::gi::SHAPE_CLASS_BASE,
			"the shaped class"
		);
		let o = digblock_core::gi::carried_origin(eye);
		assert_eq!(c.lights[0].pos, o.to_array());
		// And no override: a torch is a torch in anyone's hand, whatever
		// they have earned.
		assert_eq!(c.lights[0].level, 0);
	}

	/// The one light in the game that answers to its wielder. Below the
	/// pick's plane rank the entry defers to the LUT exactly as a torch
	/// does; at it, the entry carries a level of its own — the rank the
	/// player just felt widen the swing is the rank that brightens the
	/// tool.
	#[test]
	fn the_pick_brightens_at_the_rank_that_widens_it() {
		use digblock_core::unlock::{PICK_PLANE, pick_cells};
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let base = BlockType::ResonantPick.props().light;
		let level_at = |mining| {
			let mut c = Carried::default();
			c.begin();
			c.push(CARRIER_SELF, eye, Some(BlockType::ResonantPick), mining);
			c.lights[0].level
		};
		// The rank boundary is asked of `pick_cells`, never restated: one
		// threshold, and this test moves with it.
		let plane = (1..=100u32)
			.find(|&l| pick_cells(l) == PICK_PLANE)
			.expect("the pick reaches its plane rank somewhere on the ladder");
		assert_eq!(
			level_at(plane - 1),
			0,
			"below the rank, the row's own level"
		);
		let raised = level_at(plane);
		assert!(
			raised > base,
			"at the rank it casts brighter than its row ({raised} vs {base})"
		);
		// It out-LEVELS a torch here, and still lights a room a fraction
		// as brightly, because what an emitter delivers is its level
		// times its tint's luminance and this one is a saturated blue —
		// `texture::layer_tests::a_tool_lights_less_than_a_torch` is
		// where that can be checked against the actual pixels.
		assert!(
			u32::from(raised) <= crate::gi::LIGHT_LEVEL_MASK,
			"a level has to fit the light entry's own field"
		);
	}

	/// A carrier far from the camera still LIGHTS — it is in the list —
	/// but its motion does not spend an invalidation sphere, because the
	/// four a frame belong to world edits and a wake that far out cannot
	/// be read.
	#[test]
	fn a_distant_carriers_motion_costs_nothing() {
		let cam = Vec3::new(64.0, 70.0, 64.0);
		let far = cam + Vec3::X * (CARRIED_INVAL_DIST + 4.0);
		let mut c = Carried::default();
		for f in 0..120 {
			let p = far + Vec3::Z * (f as f32 * 0.5);
			assert_eq!(frame(&mut c, 3, p, cam), 0, "spent a sphere on frame {f}");
			assert_eq!(c.lights.len(), 1, "still a light on frame {f}");
		}
	}

	/// A light going OUT is a lighting change exactly as one arriving is.
	///
	/// `flush_lights` drops the entry the same frame, so the direct pool
	/// dies at once — but the probes hold its bounce and decay it over
	/// seconds, so without this a room stayed lit by a torch that had
	/// been put away. Digging a PLACED torch has always invalidated,
	/// through `take_gi_edits`, and the whole R28 thesis is that a
	/// carried light is not a second kind of thing.
	#[test]
	fn putting_a_torch_away_invalidates_where_it_was() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let gone = digblock_core::gi::carried_origin(eye).to_array();
		let mut c = Carried::default();
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 1, "selecting lights");
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 0, "then settles");
		// Swap to something that does not emit: one sphere, at the place
		// the light was, and then silence.
		c.begin();
		c.push(CARRIER_SELF, eye, Some(BlockType::Dirt), 1);
		c.finish(eye);
		assert_eq!(c.pending.len(), 1, "putting it away must invalidate");
		assert_eq!(c.pending[0].1, gone, "at the position it lit from");
		c.commit();
		assert_eq!(
			frame_holding(&mut c, CARRIER_SELF, eye, eye, Some(BlockType::Dirt)),
			0,
			"and not again every frame afterwards"
		);
		// Re-selecting it is a new light, so it places again.
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 1, "re-selecting");
	}

	/// The same for a carrier who simply vanishes — a remote
	/// disconnecting, or dropping out of the snapshot.
	#[test]
	fn a_departed_carrier_invalidates_where_it_was() {
		let cam = Vec3::new(64.0, 70.0, 64.0);
		let eye = cam + Vec3::X * 6.0;
		let mut c = Carried::default();
		assert_eq!(frame(&mut c, 7, eye, cam), 1);
		// Nobody at all this frame.
		c.begin();
		c.finish(cam);
		assert_eq!(c.pending.len(), 1, "a carrier who left takes their light");
		c.commit();
		c.begin();
		c.finish(cam);
		assert!(c.pending.is_empty(), "and is then forgotten");
	}

	/// A frame that never reached the renderer must not be banked as a
	/// fire, or the carrier sits stale until it has walked another whole
	/// step. `commit` is what separates queued from applied.
	#[test]
	fn an_unapplied_fire_is_retried() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		for _ in 0..5 {
			// finish() without commit(): the renderer was absent (device
			// loss, or the wait for the GPU at startup).
			c.begin();
			torch_at(&mut c, CARRIER_SELF, eye);
			c.finish(eye);
			assert_eq!(c.pending.len(), 1, "still owed an invalidation");
		}
		c.commit();
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 0, "banked once");
	}

	/// The fake remote `DIG_PLAYERBOX` parks must not share a carrier key
	/// with a real player: two carriers on one `fired` slot overwrite each
	/// other's last-fired position, so both re-place on nearly every
	/// frame.
	#[test]
	fn the_debug_carrier_key_is_nobodys_player_id() {
		assert!(u16::from(u8::MAX) < CARRIER_DEBUG);
		assert_ne!(CARRIER_DEBUG, CARRIER_SELF);
	}
}

#[cfg(test)]
mod net_budget_tests {
	use super::*;

	/// The whole reason the edit drain is priced in time rather than
	/// counted: the same queue must take proportionally less of a shorter
	/// frame. The COUNT it replaced was sized against a native frame and
	/// spent that same absolute time on a web frame a fraction as long,
	/// which is far more of the interval than it was ever allowed.
	#[test]
	fn net_edit_budget_scales_with_the_display_interval() {
		let at60 = net_edit_budget_ms(1000.0 / 60.0);
		let at120 = net_edit_budget_ms(1000.0 / 120.0);
		assert!(
			(at120 * 2.0 - at60).abs() < 1e-4,
			"120 Hz must get exactly half: {at120} vs {at60}"
		);
		// `refresh_ms` reads zero until the estimate settles; falling back
		// to anything but the nominal interval would move every baseline.
		assert_eq!(net_edit_budget_ms(0.0), at60);
		assert_eq!(net_edit_budget_ms(-1.0), at60);
	}

	/// The budget is a slice of the frame, not the frame. Nothing here
	/// pins the exact share, but a drain allowed a large fraction of the
	/// interval is no longer a budget — the render still has to fit.
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
mod stream_budget_gate_tests {
	use super::*;

	/// The trap this gate's split exists to close. The adaptive budget is
	/// ONE deadline over jobs, drain, eviction and uploads, and only the
	/// jobs can move to a worker pool — so gating it on the executor sent
	/// the drain and the uploads back to the fixed floor the moment web
	/// workers were switched on. Jobs faster, absorption slower, on
	/// exactly the machines that had headroom to spend.
	#[test]
	fn a_worker_backed_web_client_keeps_its_adaptive_budget() {
		let mut web = EngineConfig::web();
		assert!(
			web.adaptive_effective(),
			"the inline web client is the baseline"
		);
		web.workers = 2;
		assert!(
			web.adaptive_effective(),
			"moving jobs to a pool must not drop drain and uploads to the floor"
		);
	}

	/// Captures stay hermetic: bench pins the fixed floor whatever else is
	/// configured, so no baseline drifts with the display's headroom.
	#[test]
	fn bench_still_pins_the_fixed_budget() {
		let mut web = EngineConfig::web();
		web.bench = true;
		assert!(!web.adaptive_effective());
		web.workers = 2;
		assert!(!web.adaptive_effective());
	}

	/// Desktop never opted in, and a worker count must not opt it in.
	#[test]
	fn desktop_stays_on_the_fixed_budget() {
		assert!(!EngineConfig::desktop().adaptive_effective());
	}
}

#[cfg(test)]
mod arrival_hold_tests {
	use super::*;
	use digblock_net::messages::ServerMsg;
	use digblock_net::protocol::{PROTOCOL_VERSION, encode_preamble};
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
	/// the player on "Joining world..." forever — the timeout escape hatch
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

		// Just past `AREA_SYNC_TIMEOUT_SECS` worth of the frames above.
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
				// A LEGACY save on purpose (rev 0): several tests here feed
				// layouts written when the bucket was owned by everyone, and
				// the migration grant is part of what they exercise — as is
				// §14's essence reset, which the flagged pairs below are for.
				rev: 0,
				edits: vec![((1, 100, 1), 0)],
				inventory: vec![
					(3, 5),
					(digblock_core::inventory::SUB_WIRE_FLAG, 500),
					(digblock_core::inventory::SUB_WIRE_FLAG | 4, 7),
				],
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

		// A local edit, then enough updates to cross the snapshot interval:
		// the edit reaches the store and a player snapshot fires; maintain
		// runs every frame.
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

	/// The two save-rev migrations, off one rev-0 fixture: the bucket
	/// grant (rev 1) and §14's essence reset (rev 2) — every balance but
	/// Aether clears, once, on load. The server's record reader runs the
	/// same reset at its own version, so the two careers cross the
	/// redesign identically.
	#[test]
	fn a_legacy_save_grants_the_bucket_and_resets_pre_redesign_essence() {
		use digblock_core::block::Essence;
		let calls = Rc::new(RefCell::new(Calls::default()));
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);
		engine.set_world_store(Box::new(MockStore(calls.clone())));
		engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
		assert_eq!(
			engine.inventory.count(BlockType::Bucket),
			1,
			"a rev-0 save keeps its implicit bucket"
		);
		assert_eq!(
			engine.inventory.sub_count(Essence::Flora),
			0,
			"pre-redesign balances clear"
		);
		assert_eq!(
			engine.inventory.sub_count(Essence::Aether),
			7,
			"Aether alone survives the reset"
		);
		assert_eq!(engine.inventory.count(BlockType::Grass), 5, "stacks stay");
	}

	#[test]
	fn saved_hotbar_arrangement_restores_and_round_trips() {
		use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem, WIRE_EMPTY, WIRE_TOOL};

		let calls = Rc::new(RefCell::new(Calls::default()));
		// Custom arrangement: the tool parked at the head of the bar, and
		// the inventory's block partway along it.
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
		// MockStore's save carries a different seed from the backdrop, so the
		// pipeline is rebuilt from scratch and nothing is generated at entry.
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
				rev: digblock_persist::SAVE_REV,
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
		assert!(
			engine
				.pipeline
				.cell_at(ground.0, ground.1, ground.2)
				.is_some()
		);

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
		// The rig owns its bucket like any player now — crafted rather
		// than assumed — and reconciling is what equips a first vessel.
		engine.inventory.add(BlockType::Bucket, 1);
		digblock_core::inv_grid::reconcile_layout(
			&engine.inventory,
			&mut engine.hotbar,
			&mut engine.inv_grid,
		);
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
	use digblock_core::skills::{Skill, total_xp_for};
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

		engine.skills.set_total(Skill::Alchemy, total_xp_for(9));
		engine.update_target();
		assert!(
			!engine.diggable(&engine.target.unwrap()),
			"one level short of the recipe is still short"
		);

		engine.skills.set_total(Skill::Alchemy, total_xp_for(10));
		engine.update_target();
		assert!(
			engine.diggable(&engine.target.unwrap()),
			"the Alchemy level that makes torches takes them too"
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
mod touch_hud_tests {
	use super::*;
	use crate::ui::hud;

	const SIZE: (u32, u32) = (1280, 720);

	/// An in-game engine with the touch HUD live, sized so normalized
	/// touch coordinates and the HUD's pixel rects agree.
	fn touch_engine() -> Engine {
		let mut config = EngineConfig::desktop();
		config.workers = 0;
		config.seed = Some(3.0);
		config.menu = true;
		config.touch_hud = true;
		let mut engine = Engine::new(config);
		engine.size = SIZE;
		engine.ui_scale = 1.0;
		// The title menu starts open; enter the world the way play does.
		engine.menu.set_connected(true);
		engine.input(InputEvent::Key {
			key: Key::Escape,
			pressed: true,
		});
		assert!(!engine.menu.is_open(), "in-game to start");
		engine
	}

	fn touch_at(engine: &mut Engine, id: u64, phase: TouchPhase, px: f32, py: f32) {
		engine.input(InputEvent::Touch {
			id,
			phase,
			x: px / SIZE.0 as f32,
			y: py / SIZE.1 as f32,
		});
	}

	fn center(r: crate::ui::Rect) -> (f32, f32) {
		(r.x + r.w / 2.0, r.y + r.h / 2.0)
	}

	#[test]
	fn corner_tap_opens_the_menu_and_closes_a_screen() {
		let mut engine = touch_engine();
		let (cx, cy) = center(hud::corner_button_rect(SIZE, 1.0, &engine.safe_area));
		touch_at(&mut engine, 1, TouchPhase::Started, cx, cy);
		assert!(engine.menu.is_open(), "corner = pause");
		// The claimed touch is consumed to its end even though the menu now
		// owns the pointer path.
		touch_at(&mut engine, 1, TouchPhase::Ended, cx, cy);
		assert!(engine.hud_touches.is_empty(), "claim released");

		// Back in-game, with a screen up: the same button is its close.
		engine.input(InputEvent::Key {
			key: Key::Escape,
			pressed: true,
		});
		assert!(!engine.menu.is_open());
		engine.input(InputEvent::Key {
			key: Key::I,
			pressed: true,
		});
		assert!(engine.inv_screen.is_open());
		touch_at(&mut engine, 2, TouchPhase::Started, cx, cy);
		assert!(!engine.inv_screen.is_open(), "corner = close");
		assert!(!engine.menu.is_open(), "close, not pause");
		touch_at(&mut engine, 2, TouchPhase::Ended, cx, cy);
	}

	#[test]
	fn place_tap_is_an_edge_and_never_becomes_look() {
		let mut engine = touch_engine();
		let (px, py) = center(hud::place_button_rect(SIZE, 1.0, &engine.safe_area));
		touch_at(&mut engine, 1, TouchPhase::Started, px, py);
		assert!(engine.click_place, "one place per tap");
		// A drag across the whole screen with the claimed finger is not a
		// camera move.
		touch_at(&mut engine, 1, TouchPhase::Moved, px + 300.0, py);
		assert_eq!(engine.touch.take_look(), glam::Vec2::ZERO);
		touch_at(&mut engine, 1, TouchPhase::Ended, px + 300.0, py);
		assert!(engine.hud_touches.is_empty());
	}

	#[test]
	fn the_jump_button_only_lives_in_button_mode() {
		let mut engine = touch_engine();
		let (jx, jy) = center(hud::jump_button_rect(SIZE, 1.0, &engine.safe_area));
		// Double-tap mode: no button there — the touch is an ordinary
		// look-side candidate, and no jump intent is held.
		touch_at(&mut engine, 1, TouchPhase::Started, jx, jy);
		assert!(!engine.hud_jump_held);
		assert!(engine.hud_touches.is_empty());
		touch_at(&mut engine, 1, TouchPhase::Ended, jx, jy);

		engine.config.touch_jump_button = true;
		touch_at(&mut engine, 2, TouchPhase::Started, jx, jy);
		assert!(engine.hud_jump_held, "held while the finger is down");
		// A second finger still looks while the button is held.
		touch_at(&mut engine, 3, TouchPhase::Started, 900.0, 200.0);
		touch_at(&mut engine, 3, TouchPhase::Moved, 1000.0, 200.0);
		touch_at(&mut engine, 3, TouchPhase::Moved, 1100.0, 200.0);
		assert!(engine.touch.take_look().x > 0.0);
		touch_at(&mut engine, 2, TouchPhase::Ended, jx, jy);
		assert!(!engine.hud_jump_held);
	}

	#[test]
	fn a_bar_slot_tap_selects_it() {
		let mut engine = touch_engine();
		let grid = hud::row_grid(SIZE, 1.0);
		let (sx, sy) = center(hud::bar_slot_rect(&grid, 2, 1.0));
		touch_at(&mut engine, 1, TouchPhase::Started, sx, sy);
		assert_eq!(engine.hotbar.selected_index(), 2);
		touch_at(&mut engine, 1, TouchPhase::Ended, sx, sy);

		// The tool slot mirrors tap-9: selecting it, then tapping it again,
		// cycles the owned roster (a no-op roster keeps the slot selected).
		let tool = digblock_core::hotbar::HOTBAR_SLOTS - 1;
		let (tx, ty) = center(hud::bar_slot_rect(&grid, tool, 1.0));
		touch_at(&mut engine, 2, TouchPhase::Started, tx, ty);
		touch_at(&mut engine, 2, TouchPhase::Ended, tx, ty);
		touch_at(&mut engine, 3, TouchPhase::Started, tx, ty);
		touch_at(&mut engine, 3, TouchPhase::Ended, tx, ty);
		assert_eq!(engine.hotbar.selected_index(), tool);
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
		// the web fall together. (Several ticks, not one — the decay
		// truncates down, so a short enough block loses nothing on the
		// very first.)
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

	/// The conditional rock rule, client half: a stone standing in a cell
	/// somebody WROTE comes back whole to a mason and crumbles for anyone
	/// who could not have cut it (SKILLS-DESIGN §7.1/§7.5). Prediction has
	/// to agree with the server here or a quarry silently drifts the two
	/// inventories apart — there is no rebase behind an ordinary dig.
	///
	/// `block_ahead` stands the block through `Pipeline::set_block`, and
	/// the engine always carries an `EditOverlay`, so the cell it leaves is
	/// an edited one — which is exactly the case under test. The pristine
	/// half is `only_the_worlds_own_rock_crumbles` in core and the server's
	/// own pair over the real edit path.
	#[test]
	fn the_client_predicts_a_placed_stone_coming_back_whole() {
		use digblock_core::skills::{Skill, total_xp_for};
		let mason = digblock_core::unlock::craft(BlockType::Stone).expect("clean stone is made");
		for (alchemy, want) in [(mason, BlockType::Stone), (mason - 1, BlockType::Rubble)] {
			let (mut engine, cell) = block_ahead(BlockType::Stone);
			engine.skills.set_total(Skill::Mining, total_xp_for(99));
			engine
				.skills
				.set_total(Skill::Alchemy, total_xp_for(alchemy));
			engine.update_target();
			assert_eq!(engine.target.map(|h| h.cell), Some(cell), "aimed at it");
			assert!(
				engine.pipeline.cell_edited(cell.0, cell.1, cell.2),
				"the harness stands the block through an edit"
			);
			hold(&mut engine, 1);
			let need = engine.dig.as_ref().expect("hold opened").required;
			hold(&mut engine, u32::from(need) + 1);
			assert_eq!(
				engine.pipeline.block_at(cell.0, cell.1, cell.2),
				Some(BlockType::Air),
				"the hold completed at Alchemy {alchemy}"
			);
			assert_eq!(engine.inventory.count(want), 1, "Alchemy {alchemy}");
			let other = if want == BlockType::Stone {
				BlockType::Rubble
			} else {
				BlockType::Stone
			};
			assert_eq!(engine.inventory.count(other), 0, "Alchemy {alchemy}");
		}
	}

	/// A capture ignores the keyboard AND leaves the mouse alone.
	///
	/// Both halves, because the second is the one that hurt: a capture
	/// opens with no menu, so `wants_cursor_grab` used to answer true on
	/// frame one and the desktop shell locked and hid the operator's
	/// cursor for the whole run — on a window nobody asked to play. The
	/// keystrokes they made in the meantime went into the world, and a
	/// stray `I` opened the inventory into the shot.
	///
	/// `bench` alone did not cover it. `--screenshot`/`--frames`/`--exit`
	/// runs are captures that never set `bench`, and those are precisely
	/// the runs somebody sits and watches, so the flag this asserts on is
	/// the launcher's own `interactive` predicate.
	#[test]
	fn a_capture_takes_neither_the_keyboard_nor_the_pointer() {
		use digblock_platform::{InputEvent, Key};

		let fresh = |interactive: bool| {
			let mut config = EngineConfig::desktop();
			config.interactive = interactive;
			config.workers = 0;
			config.seed = Some(3.0);
			Engine::new(config)
		};
		let any_screen_open = |e: &Engine| {
			e.menu.is_open()
				|| e.inv_screen.is_open()
				|| e.alch_screen.is_open()
				|| e.char_screen.is_open()
		};
		// One key per engine. Pressing all four in sequence cancels
		// itself — `I` opens the inventory, which then swallows `K` and
		// `C` as drag input and closes on `Escape` — so a combined
		// sweep would end with nothing open and "a capture opened
		// nothing" would pass for entirely the wrong reason.
		let opens = |interactive: bool, key: Key| {
			let mut e = fresh(interactive);
			e.input(InputEvent::Key { key, pressed: true });
			e.input(InputEvent::Key {
				key,
				pressed: false,
			});
			any_screen_open(&e)
		};
		// `look_accum`, not the camera: a mouse delta is banked here and
		// spent by `update`, so reading the camera straight after
		// `input` would answer "unmoved" on an interactive run too.
		let turns = |interactive: bool| {
			let mut e = fresh(interactive);
			e.input(InputEvent::MouseDelta {
				dx: 240.0,
				dy: 90.0,
			});
			e.look_accum != (0.0, 0.0)
		};

		// The three that were genuinely reachable. `Escape` is asserted
		// on the capture side too, but it is NOT a control: `Menu::open`
		// checks `enabled`, which a capture never sets, so Escape was
		// already inert there and a control on it would fail for a
		// reason that has nothing to do with this flag.
		const SCREEN_KEYS: [Key; 3] = [Key::I, Key::K, Key::C];

		// A capture is deaf and never asks for the pointer.
		assert!(
			!fresh(false).wants_cursor_grab(),
			"a capture must never take the operator's cursor"
		);
		for key in SCREEN_KEYS.into_iter().chain([Key::Escape]) {
			assert!(!opens(false, key), "a capture opened a screen on {key:?}");
		}
		assert!(!turns(false), "a capture banked a mouse delta to turn with");

		// The controls: every one of those events DOES land on a real
		// session, or the assertions above prove nothing at all.
		assert!(fresh(true).wants_cursor_grab(), "a session grabs to play");
		for key in SCREEN_KEYS {
			assert!(opens(true, key), "{key:?} does not open anything anyway");
		}
		assert!(turns(true), "MouseDelta does not turn the camera anyway");
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

#[cfg(test)]
mod ore_capture_tests {
	use super::*;
	use digblock_core::worldgen::ore;

	/// The chamber's own radius, mirrored from `debug_ore_plan`'s `R` so
	/// the plate's footprint can be reasoned about from outside it.
	const R: i32 = 5;

	/// DIG_ORE's one rule, and the only one that makes the scene work at
	/// all: the plan must not write over a single cell the deposit
	/// claims. A write reaches the anti-dupe for real — the plan is
	/// applied through `pipeline.set_cell`, which records into the
	/// `EditOverlay`, and `ore::revealed` answers None for an edited cell
	/// forever — so a stray write deletes the subject of the photograph
	/// permanently, and silently, because the chamber still looks right.
	///
	/// The carve loop always guarded this. The floor plate did not, and it
	/// is laid BELOW the deposit centre, straight through the lowest rim of
	/// a max-radius blob: the cell directly under the centre sits at
	/// `d2 == r2`, so `Deposit::contains` falls through to `rim_open` and
	/// keeps it more often than not.
	///
	/// `crossings` is what stops this going vacuous. It counts the vein
	/// cells lying IN the plate's plane, so a sample that never puts one
	/// there fails as loudly as a plan that paves over them — and the
	/// plate is asserted to be exactly the plane minus those cells, which
	/// pins that the guard skips the vein and nothing else.
	#[test]
	fn the_ore_capture_plan_never_writes_over_the_vein() {
		let mut crossings = 0usize;
		let mut plans = 0usize;
		for i in 0..20 {
			let seed = 1000.0 + f64::from(i) * 37.5;
			// Any pose does: the plan only reads spawn's x/z, to pick the
			// nearest deposit of the tier. Skipping the worldgen spawn
			// search keeps this a pure ore-geometry test.
			let spawn = Vec3::new(0.0, 90.0, 0.0);
			for &want in ore::ORES.iter().chain([BlockType::Leystone].iter()) {
				let (pos, plan) = debug_ore_plan(spawn, seed, want);
				if plan.is_empty() {
					continue;
				}
				plans += 1;
				for &(x, y, z, cell) in &plan {
					assert!(
						ore::ore_at(x, y, z, seed).is_none(),
						"seed {seed} tier {want:?}: plan writes {cell} over the vein \
						 at {x},{y},{z}"
					);
				}
				// The plate is the plan's only Stone, and the pose is
				// authored from the deposit centre — enough to rebuild the
				// footprint the unguarded loop used to pave.
				let plate: Vec<_> = plan
					.iter()
					.filter(|c| c.3 == BlockType::Stone as u16)
					.collect();
				let floor = plate[0].1;
				assert!(
					plate.iter().all(|c| c.1 == floor),
					"seed {seed} tier {want:?}: the plate is one plane"
				);
				let (cx, cz) = ((pos.x - 0.5) as i32, (pos.z + 4.5) as i32);
				let mut vein = 0usize;
				for z in (cz - R)..=(cz + R) {
					for x in (cx - R)..=(cx + R) {
						vein += usize::from(ore::ore_at(x, floor, z, seed).is_some());
					}
				}
				let square = (2 * R + 1).pow(2) as usize;
				assert_eq!(
					plate.len() + vein,
					square,
					"seed {seed} tier {want:?}: the plate is the plane minus the vein"
				);
				crossings += vein;
			}
		}
		assert!(plans > 50, "the sample found deposits to photograph");
		assert!(
			crossings > 0,
			"no vein crossed a floor plane in {plans} plans — this sample \
			 cannot tell a guarded plate from an unguarded one"
		);
	}
}
