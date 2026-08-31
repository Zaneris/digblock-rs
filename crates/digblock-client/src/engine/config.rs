//! Startup configuration: what a launcher hands the engine before the
//! first frame, and the debug screens a capture run can open.

use super::*;

/// A UI screen [`EngineConfig::debug_screen`] can open at startup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DebugScreen {
	/// The startup menu; needs `menu: true` too, which the launcher forces on captures.
	Menu,
	Inventory,
	Alchemy,
	/// The same two pages, cursor parked on a NAMED thing: a tooltip is otherwise uncapturable.
	InventoryHover,
	AlchemyHover,
	/// Not a screen: stock the inventory and open NOTHING, so the viewmodel is on screen.
	Hand,
	/// Also not a screen: the water tool, empty or charged — the game's one item state.
	Bucket {
		full: bool,
	},
	/// A CRAFTED tool, owned and selected: authored geometry showing in hand, slot and rail.
	Tool(BlockType),
	/// Stock and SELECT a torch — the one held item that is also a LIGHT; `Hand` gets Dirt.
	Torch,
	/// The character screen, with mid-curve XP stocked so the bars photograph as they play.
	Skills,
	/// The hiscores screen over a sample page, since no master answers a capture.
	Hiscores,
	/// The alchemy screen's second tab (craft rows, locked rows, costs).
	Transmute,
	/// The same tab mid-cycle: a paced craft is over in seconds and needs a press to start.
	Transmuting,
	/// The spellbook, with Arcana XP past the first rung so a threshold and a teaser both show.
	Spellbook,
	/// The same page with the cursor on its first FARE chip — the only named thing on it.
	SpellbookHover,
}

impl DebugScreen {
	/// Parse the launcher's DIG_SCREEN value. `None` is an unknown name,
	/// which [`knobs::apply`](super::knobs::apply) turns into the loud
	/// exit every other bad knob value gets: a capture that quietly ran
	/// with no screen open photographs the wrong thing.
	pub fn parse(s: &str) -> Option<Self> {
		match s.trim().to_ascii_lowercase().as_str() {
			"menu" => Some(Self::Menu),
			"inventory" | "inv" => Some(Self::Inventory),
			"alchemy" | "alch" => Some(Self::Alchemy),
			"inventory-hover" => Some(Self::InventoryHover),
			"alchemy-hover" => Some(Self::AlchemyHover),
			"hand" => Some(Self::Hand),
			"bucket" => Some(Self::Bucket { full: false }),
			"bucket-full" => Some(Self::Bucket { full: true }),
			"siphon" => Some(Self::Tool(BlockType::EssenceSiphon)),
			"pick" => Some(Self::Tool(BlockType::ResonantPick)),
			"sigil" => Some(Self::Tool(BlockType::MastersSigil)),
			"torch" => Some(Self::Torch),
			"skills" | "character" => Some(Self::Skills),
			"hiscores" => Some(Self::Hiscores),
			"transmute" => Some(Self::Transmute),
			"transmuting" => Some(Self::Transmuting),
			"spellbook" => Some(Self::Spellbook),
			"spellbook-hover" => Some(Self::SpellbookHover),
			_ => None,
		}
	}
}

/// Parse `DIG_PLAYERBOX`'s value: what the fake remote holds, in HOTBAR WIRE terms.
///
/// Lives here rather than in the launcher for the same reason [`DebugScreen::parse`] does — the
/// launcher owns the environment, the engine owns what the names mean. A wire id rather than a
/// block so the scene walks the real `held_icon` decode; an unknown name the launcher makes fatal.
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
	/// The lowest draw distance this platform's slider offers — `settings::DRAW_MIN` everywhere a
	/// GPU has proven it never needs less, `settings::DRAW_FLOOR` where one hasn't (mobile web).
	/// A platform bound on the UI, not on the value: `Settings::clamped` accepts down to the floor
	/// on every platform, so a save written under the lower bound survives the same browser
	/// flipping to a desktop profile.
	pub draw_min: i32,
	/// Runtime cap on the GI light list (`Settings::max_lights`), at most
	/// `gi::MAX_LIGHTS`. The per-pixel NEE scan is O(list length), so this
	/// is the knob that bounds what a torch field or a wall of revealed
	/// veins can cost a frame.
	pub max_lights: i32,
	/// The GI resolve's ray budgets — the frame's two largest, per
	/// the bench attribution. Every profile ships `gi::GI_GATHER_ITERS` /
	/// `GI_SUN_ITERS`; only the A/B levers (`DIG_GI_ITERS`, `?giiters=`)
	/// move them until a measured tier lands, because a shorter march
	/// silently un-shadows what lies past the cut (gi/resolve.rs).
	pub gi_gather_iters: u32,
	pub gi_sun_iters: u32,
	/// The resolve's adaptive ray budget: blocks of view distance per frame of trace period,
	/// indirect then sun (`DIG_GI_RATE`, `?girate=`); 0 disables a lane's gating. Every profile
	/// ships `gi::resolve`'s `GI_IND_RATE_BLOCKS` / `GI_SUN_RATE_BLOCKS`.
	pub gi_rate_blocks: [f32; 2],
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
	/// Start the player here instead of at the seed's spawn point (`--pos x y z`). The scenes this
	/// engine is slow in are NATURAL water — terraced beds, stepped descents, cascades — and no
	/// synthetic plan builds those: `debug_river_plan` digs a flat-bottomed box, so it has no
	/// impact cells and its gather early-outs almost everywhere. The debug plans key off the spawn
	/// too, so they follow this point.
	pub start_pos: Option<[f32; 3]>,
	pub metrics_csv: Option<std::path::PathBuf>,
	/// Start with the debug overlay visible (` toggles at runtime).
	pub overlay: bool,
	/// Benchmark mode: inputs ignored, deterministic scripted camera
	/// flight, run summary logged on exit.
	pub bench: bool,
	/// Is a PERSON at this keyboard? False for every capture path — bench, `--screenshot`,
	/// `--drill` — governing two things that must move together: whether [`Engine::input`] listens
	/// at all, and whether the shell may take the mouse pointer. `bench` alone does not cover it:
	/// a capture sets `menu: false`, so `wants_cursor_grab` answers true and the shell locks the
	/// pointer on a window nobody asked to play. The launcher's own predicate, not a second view.
	pub interactive: bool,
	/// Capture harness: run the bench flight for this many frames, then FREEZE the camera there.
	/// Every capture in this engine is a still, which is exactly the case where a temporal artifact
	/// has already converged away. Freezing at a pose the flight is a pure function of gives two
	/// runs the identical camera by construction, with only their history differing.
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
	/// No explicit lighting-detail preference (no flag, nothing saved), so derive `gi_scale_div`
	/// from display density at the first `set_scale_factor` — the first moment anything knows the
	/// backing scale, since the launcher resolves settings before a window exists. FALSE on every
	/// capture path, which is what keeps bench, screenshot and drill runs on the shipped values.
	pub gfx_defaults_auto: bool,
	/// Swap the path `fly`/`warp` follow for a ground walk. The bench flight is a fast flyover well
	/// above the ground — the fast, shallow-parallax end of screen motion; a player reports
	/// temporal artifacts from the other end, at eye height and walking pace, where one half-res
	/// texel covers far more world per pixel. Same harness, second regime.
	pub walk: bool,
	/// The third regime beside the flight and the walk: a still stare straight down from a glass
	/// block at the top of the world, a few stations of it (`--stare`, `engine/stare.rs`). The
	/// worst case for the GI resolve — every texel far, through glass, nothing reprojecting — and
	/// the number the GI work is judged by. Implies `markers: false`: the arrival spiral would
	/// otherwise sit in the middle of every station's frame.
	pub stare: bool,
	/// Flyover speed in blocks/s (`--bench-speed`). Every bench baseline was taken at the
	/// default — changing it silently invalidates all of them, so don't. The default is several
	/// times `WALK_SPEED`, a different STREAMING REGIME rather than merely a faster one: the GI
	/// upload budget cannot keep up and the classify drain drops entries, so a cost that appears
	/// only there may be starvation rather than a leak. (`--walk` brings no new terrain in at all.)
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
	/// Debug: park a fake remote player a few blocks ahead, holding this item (GPU-path check
	/// without a second client). A held-item WIRE id, not a block, so the scene walks `held_icon`
	/// end to end rather than a second code path — which is also how it photographs a remote's lit
	/// torch. Set by the desktop launcher from DIG_PLAYERBOX.
	pub debug_playerbox: Option<u16>,
	/// Debug: throw a level-up confetti burst every couple of seconds, a
	/// few blocks ahead. A burst is a rare, transient event, so it is
	/// otherwise invisible to headless capture — the same argument
	/// `debug_screen` makes for the input-only screens. Set by the
	/// desktop launcher from DIG_CONFETTI.
	pub debug_confetti: bool,
	/// DIG_BURSTS: the generic burst rack, one of each kind, on repeat.
	pub debug_bursts: bool,
	/// DIG_ORBIT: a slow cinematic orbit of the aim point,
	/// `(radius, deg/s)`.
	pub debug_orbit: Option<(f32, f32)>,
	/// DIG_DOLLY: a tracking dolly past the aim point, `(secs, blocks)`.
	pub debug_dolly: Option<(f32, f32)>,
	/// The persisted `Settings::show_tutorial` (launchers mirror it in).
	pub show_tutorial: bool,
	/// DIG_TUTORIAL: force the walkthrough on, off, or to one step.
	pub debug_tutorial: Option<crate::tutorial::TutorialOverride>,
	/// Draw the standing arrival markers (`waystone_fx`). A capture posed
	/// within `MARKER_RADIUS` of spawn otherwise has the spiral in frame —
	/// from above it is a white ball over the terrain under test. Cleared
	/// by DIG_NO_MARKERS.
	pub markers: bool,
	/// Debug: pin the crack decal at this progress (0..1) on whatever the crosshair is holding, and
	/// stand a wall of blocks two cells ahead for it to hold. Digging is INPUT-driven and a hold
	/// lasts a second or two, so the crack is otherwise unphotographable — `debug_confetti`'s
	/// argument. A sweep of runs over 0..1 is what shows the break spreading. Set by the desktop
	/// launcher from DIG_CRACK.
	pub debug_crack: Option<f32>,
	/// Debug: pin the essence draw at this progress (0..1) on whatever vein the crosshair is
	/// holding. `DIG_CRACK`'s argument exactly, with one difference that shapes the flag: it cannot
	/// stand its own scene, because a vein is derived worldgen and there is no placing one. It
	/// rides `DIG_ORE`, which hollows a chamber around a deposit and grants the level that reveals
	/// it, and the launcher refuses the pair unset rather than capturing an empty wall.
	pub debug_siphon: Option<f32>,
	/// Debug: hold the cast channel's aura at a fixed progress with no
	/// hold behind it — standing in a channel is the one thing a headless
	/// run cannot do (the DIG_SIPHON rule). Set by the desktop launcher
	/// from DIG_CAST.
	pub debug_cast: Option<f32>,
	/// Debug: open a UI screen at startup with a stocked inventory, so a
	/// headless `--screenshot` run can capture it. Set by the desktop
	/// launcher from DIG_SCREEN.
	pub debug_screen: Option<DebugScreen>,
	/// Debug: place a showcase of every shaped-block variant near spawn
	/// (both stair materials × 4 facings, doors × facings × open/closed)
	/// once chunks are Ready. Set by the desktop launcher from
	/// DIG_SHAPES — visual verification without gameplay wiring.
	pub debug_shapes: bool,
	/// Debug: a glass sphere of this radius closing about spawn from the
	/// moment every cell it would touch is resident — the cast's own plan
	/// and climb, with no channel or bill behind it. Set by the launcher
	/// from DIG_SPHERE.
	pub debug_sphere: Option<i32>,
	/// Debug: hold DIG_SPHERE's climb this many seconds in, DIG_CAST's rule
	/// — a sphere half-landed is what a headless run cannot otherwise
	/// photograph. Set by the launcher from DIG_SPHERE_AGE.
	pub debug_sphere_age: Option<f32>,
	/// Debug: stand a rank of doors three cells ahead of spawn, close
	/// enough that the crosshair lands on one. Set by the desktop
	/// launcher from DIG_DOORS. See [`debug_doors_plan`] for why
	/// `debug_shapes` does not cover this.
	pub debug_doors: bool,
	/// Debug: seal a stone vault around the spawn camera with one torch
	/// inside — the NEE capture, and any look call about a dim room (a
	/// roofed room goes DARK and torchlight is what makes it readable).
	/// Set by the desktop launcher from DIG_VAULT.
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
	/// DIG_MINING=<level>: start at that Mining level, 0 = leave it alone. A level is not a pose,
	/// nor a scene, and everything it gates is invisible without it: the strength wall's refusals,
	/// dig speed, and the Resonant Pick's two ranks — the second brightens the tool's own glow, so
	/// a capture at the lower rank photographs the wrong light. Applied AFTER `debug_ore`'s own
	/// grant, so asking for a level explicitly wins.
	pub debug_mining: u32,
	/// Debug: build DIG_VAULT's room with NO torch on its floor, so the only source in it is the
	/// one the player is CARRYING. Set by the desktop launcher from DIG_VAULT_DARK. A modifier
	/// rather than a re-spelling of DIG_VAULT, because `DIG_VAULT=0` already means "vault on" and
	/// silently inverting a value someone may have typed costs an afternoon.
	pub debug_vault_dark: bool,
	/// Debug: hang a wide stone roof with NO walls over the ground ahead
	/// of spawn — the beam-veto capture (a slanting morning sun must
	/// reach under it exactly as the shadow map says; only the ambient
	/// dome dims). Set by the desktop launcher from DIG_ROOF.
	pub debug_roof: bool,
	/// Debug: seal a long stone tunnel ahead of the spawn camera, no torch — the unproven-daylight
	/// capture (R32). Its far end sits past the probe cascades' reach and past the
	/// gather ray's length, which is exactly where every "answer sky on no data" fallback fires; a
	/// vault is too small to reach any of them. Pair with `--pos` at a low y — the fade is
	/// depth-gated and the plan keys off the spawn. Set by the desktop launcher from DIG_TUNNEL.
	pub debug_tunnel: bool,
	/// Debug: flood an open-topped stone tank around the spawn camera so
	/// the eye starts SUBMERGED — the underwater capture (P4: sun
	/// transmittance through the column dims submerged terrain,
	/// the surface reads as a plane from below). Set by the desktop
	/// launcher from DIG_DIVE.
	pub debug_dive: bool,
	/// Debug: excavate a wide, deep basin AHEAD of the spawn camera and flood it — the "standing at
	/// a river's edge looking into it" scene (R17). Near water fills most of the frame from above,
	/// inside C0 beside bank geometry, with a submerged floor behind it for the resolve's rays to
	/// cross; no other water capture puts a SHADED surface on screen at all. Set by the desktop
	/// launcher from DIG_RIVER.
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
	/// Debug: shading terms to REMOVE this run (`DIG_STUB`, names in `stub::parse`), so one build
	/// attributes a whole frame across a handful of runs instead of a rebuild per bisect. The mask
	/// rides `frame.misc.z` and the resolve's own uniform as a UNIFORM branch: zero cost at 0, and
	/// it can never diverge inside a wave, so a stubbed run measures the term it removed and
	/// nothing else.
	pub stub: u32,
	/// Spray droplets per waterfall-impact instance; the mobile config
	/// ships fewer (it used to be inferred in the renderer from the draw
	/// distance). Each is a sub-tenth-block alpha billboard, so the cost
	/// is overdraw on a handful of culled emitters, not geometry.
	pub spray_droplets: u32,
	/// Per-frame wall-clock budget for main-thread streaming (see the
	/// STREAM_BUDGET_MS doc below, which is the default).
	pub stream_budget_ms: f32,
	/// Grow the streaming budget from measured frame headroom, scaled by how near the closest
	/// pending job is (the web build; see `stream_budget`) — rim-only churn while walking stays at
	/// the fixed floor. Effective outside bench only: captures must stay deterministic. It is NOT
	/// conditioned on the executor — see `adaptive_effective`.
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
	/// The key this machine's pre-account character was filed under, if an old identity file is
	/// still on disk — see `NetClientConfig::legacy_identity`. The LAUNCHER reads the file and
	/// hashes it, so the secret itself never enters the engine and there is nothing here to leak.
	/// Read-only, and never created: a machine with no old identity has nothing to migrate, and
	/// minting one would invent a character out of thin air.
	pub legacy_identity: Option<[u8; 16]>,
	/// Log a per-phase breakdown of every over-budget frame. The LAUNCHER
	/// fills this (desktop: `DIG_SPIKELOG`; web: `?spikelog`) — reading the
	/// environment inside core answers None forever on wasm, which is
	/// exactly the platform whose spikes most need attributing.
	pub spike_log: bool,
	/// Ask the device for `TIMESTAMP_QUERY` (the overlay's gpu line, the CSV's gpu columns).
	/// `DIG_GPU_TIMER` / `?gputimer` sets it either way; the web profile ships it OFF — see
	/// [`Self::web`] for the measurement behind that.
	pub gpu_timer: bool,
	/// Present at most this many frames a second (`DIG_FPS_CAP` / `?fpscap`); `None` is the
	/// display's own pace. A measurement knob on desktop and a real one on a laptop or a
	/// 120 Hz browser tab, where halving the presents halves the GPU load — the thermal A/B.
	pub fps_cap: Option<f32>,
	/// Mesh bytes handed to the queue per frame. Wall-clock alone cannot bound this on the web:
	/// `write_buffer` merely RECORDS the copy and Chrome's GPU process performs the staging
	/// transfer at submit, so the cost lands after everything this engine can measure.
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
			draw_min: crate::settings::DRAW_MIN,
			max_lights: crate::settings::LIGHT_CAP_MAX,
			gi_gather_iters: crate::gi::GI_GATHER_ITERS,
			gi_sun_iters: crate::gi::GI_SUN_ITERS,
			gi_rate_blocks: [crate::gi::GI_IND_RATE_BLOCKS, crate::gi::GI_SUN_RATE_BLOCKS],
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
			stare: false,
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
			debug_bursts: false,
			debug_orbit: None,
			debug_dolly: None,
			show_tutorial: true,
			debug_tutorial: None,
			markers: true,
			debug_crack: None,
			debug_siphon: None,
			debug_cast: None,
			debug_shapes: false,
			debug_sphere: None,
			debug_sphere_age: None,
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
			gpu_timer: true,
			fps_cap: None,
			upload_bytes_max: UPLOAD_BYTES_MAX_NATIVE,
			debug_gi: false,
			debug_gi_probes: false,
		}
	}

	/// The quality knobs a small, battery-powered device wants, applied over
	/// any base profile. Both mobile profiles go through here so a retune is
	/// made ONCE — `mobile()` was a verbatim copy of `desktop()` differing in
	/// six fields, and `web_mobile()` re-listed four of them a third time.
	fn mobile_quality(base: Self) -> Self {
		Self {
			draw_dist: 10,
			ssr: false,
			spray_droplets: 48,
			touch_hud: true,
			..base
		}
	}

	/// The iOS profile.
	pub fn mobile() -> Self {
		Self {
			workers: 2,
			upload_bytes_max: UPLOAD_BYTES_MAX_CONSTRAINED,
			..Self::mobile_quality(Self::desktop())
		}
	}

	pub fn web() -> Self {
		Self {
			workers: 0,
			menu: true,
			adaptive_stream: true,
			// No timestamp queries. Chrome's Metal backend samples them at every pass boundary,
			// which hands the GPU the frame as a dozen small pieces with a drain between each —
			// and macOS picks the GPU's performance state from whether a command buffer looks
			// saturating, not from utilization. Measured on an M2 Max: with the queries on, a
			// 120 fps scene fell within a minute to ~50 fps at 533 MHz / 5 W, the governor
			// requesting P1 three-quarters of the time for a GPU 91% busy, thermal pressure
			// nominal; `?gputimer=0` alone held 120 fps at full clock. The native build, whose
			// wgpu-hal timestamps let passes overlap, is unaffected. `?gputimer=1` opts in.
			gpu_timer: false,
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
			gi_scale_div: 4, // the Settings screen's "Low" notch
			// The slider's own floor: phone GPUs start with the shortest
			// list and buy emitters back up, instead of paying the full
			// per-pixel scan until they find the slider.
			max_lights: crate::settings::LIGHT_CAP_MIN,
			// Here and not in `mobile_quality`: only mobile-web GPUs have
			// been named as needing less than the shared mobile default,
			// so iOS keeps the shared numbers and this profile alone
			// shortens the default and opens the slider's lower rungs.
			draw_dist: 8,
			draw_min: crate::settings::DRAW_FLOOR,
			touch_jump_button: false,
			fullscreen_button: false,
			fullscreen_hint: false,
			..Self::mobile_quality(Self::web())
		}
	}

	/// The adaptive budget's effective enable (see `adaptive_stream`).
	///
	/// Deliberately NOT gated on the executor: what this enables is ONE deadline covering jobs,
	/// drain, eviction and uploads together, and a `workers == 0` condition would snap the drain
	/// and the uploads back to the fixed floor. The JOB half is gated in `Pipeline::maintain`.
	pub(super) fn adaptive_effective(&self) -> bool {
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
			gi_gather_iters: self.gi_gather_iters,
			gi_sun_iters: self.gi_sun_iters,
			gi_rate_blocks: self.gi_rate_blocks,
		}
	}
}
