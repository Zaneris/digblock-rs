use digblock_client::settings::{
	GI_SCALE_DIV_FULL, GI_SCALE_DIVISORS, RENDER_SCALE_MAX, RENDER_SCALE_MIN, UI_SCALE_MAX,
	UI_SCALE_MIN, lever_gi_div_ok,
};
use digblock_client::{Engine, EngineConfig};
use digblock_platform::RunOptions;

mod audio;
mod auth;
mod services;
mod settings;

/// Bad or missing flag values are fatal like unknown args — a silently
/// defaulted `--seed`/`--draw-dist` makes captures non-deterministic.
fn parse_or_die<T: std::str::FromStr>(flag: &str, val: Option<String>) -> T {
	let Some(val) = val else {
		eprintln!("missing value for {flag}");
		std::process::exit(2);
	};
	val.parse().unwrap_or_else(|_| {
		eprintln!("bad value for {flag}: {val}");
		std::process::exit(2);
	})
}

fn main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

	let mut config = EngineConfig::desktop();
	let mut options = RunOptions {
		title: "DigBlock".into(),
		screenshot_after_frames: 10,
		..Default::default()
	};

	// Deterministic-capture flags:
	//   --screenshot out.png [--exit] [--frames N]
	//   --offscreen (draw off the swapchain — the flag that lets a capture
	//            run with the screen locked or the window covered; not
	//            implied by --screenshot, since it also drops the present
	//            wait a frame-time run measures)
	//   --seed <f64 bits or decimal> --metrics-csv out.csv
	//   --yaw <deg> --pitch <deg> (initial camera view; yaw 90 faces +x,
	//            the sun azimuth plane)
	//   --bench (scripted flight, inputs off, vsync off; supplies its own
	//            seed, tod, frame count and CSV path)
	//   --fly N / --warp N (fly the bench path N frames then freeze, or
	//            arrive at that same pose with no motion behind it — the
	//            temporal-artifact A/B; pair with --frames)
	//   --walk (modifier on either: an eye-height oscillation, one block
	//            forward and one back, instead of the flyover — a
	//            screen-space artifact scales with screen motion, so the
	//            two paths are two regimes)
	//   --render-scale S (DEV/CAPTURE ONLY — no slider exposes it and it
	//            is locked at native for players. World resolution as a
	//            fraction of the surface; the UI stays native)
	//   --gi-scale D (GI resolve divisor against the render size: a slider
	//            rung, or the lever-only full-res divisor; the interactive
	//            high-DPI default is derived from display density. This is
	//            the knob that pays — see settings::density_gi_div)
	//   --bench-speed B (flyover blocks/s; the DEFAULT is every BENCH.md
	//            baseline's value, several times walking pace and a
	//            different STREAMING regime — drop it to walking pace to
	//            soak at how the game is played)
	//   --size WxH (logical window size; the GI resolve is a fraction of
	//            the surface, so this is what a cross-platform comparison
	//            has to hold equal)
	//   --pos x y z (stand here instead of at the seed's spawn)
	//   --fixed-dt S (step the world by exactly S per frame — what makes
	//            a capture comparable ACROSS BUILDS at all)
	//   DIG_STUB=<terms> (remove shading terms to attribute a frame)
	//   --no-vsync --workers N --draw-dist N (workload knobs / profiling)
	let mut frames: Option<u32> = None;
	// Multiplayer goes through the master server exclusively: it lists
	// servers, distributes cert pins, and mints the one-time join tokens
	// game servers require. There is no direct-connect path. The
	// production master is the default; --master-url / DIG_MASTER_URL
	// point dev stacks elsewhere, and an explicitly empty value opts out
	// of master-backed features (menu multiplayer, music) entirely.
	let mut master_url = std::env::var("DIG_MASTER_URL").ok();
	let mut join = false;
	let mut server_filter: Option<String> = None;
	// Scripted sign-in for runs that cannot open a browser (`just play`,
	// the drill, anything with --join). The master honours it only in
	// Development; see Dev/DevEndpoints.cs.
	let mut dev_signin: Option<String> = None;
	let mut mute = false;
	// Recorded, not applied: CLI overrides beat the saved settings, which
	// beat the platform defaults — resolved after the parse loop.
	let mut cli_draw: Option<i32> = None;
	let mut cli_ssr: Option<bool> = None;
	let mut cli_tod: Option<f32> = None;
	let mut cli_ui_scale: Option<f32> = None;
	let mut cli_render_scale: Option<f32> = None;
	let mut cli_gi_scale: Option<u32> = None;
	let mut cli_max_lights: Option<i32> = None;
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--master-url" => {
				master_url = Some(parse_or_die::<String>("--master-url", args.next()));
			}
			// Scripted flows (just play, soaks): skip the menu, join the
			// first listed server (or the one matching --server).
			"--join" => join = true,
			"--server" => server_filter = Some(parse_or_die::<String>("--server", args.next())),
			// Required for --join: a scripted run has no browser, so
			// its session comes from the master's development sign-in.
			"--dev-signin" => {
				dev_signin = Some(parse_or_die::<String>("--dev-signin", args.next()));
			}
			// Not a rename: a display name is no longer something a client
			// picks per run, so silently mapping this onto the account
			// would be a lie about what the run is doing.
			"--name" => {
				eprintln!(
					"--name is gone: your display name belongs to your DigBlock account now. \
					 For scripted runs use --dev-signin <subject> against a development master."
				);
				std::process::exit(2);
			}
			"--mute" => mute = true,
			"--screenshot" => {
				options.screenshot =
					Some(parse_or_die::<String>("--screenshot", args.next()).into());
			}
			"--exit" => options.exit_after_screenshot = true,
			"--frames" => frames = Some(parse_or_die("--frames", args.next())),
			"--drill" => options.auto_drill = true,
			"--mobile" => config = EngineConfig::mobile(),
			"--overlay" => config.overlay = true,
			"--bench" => config.bench = true,
			// Capture harness: fly the bench path N frames then freeze, vs
			// arrive at that same pose with no motion behind it. Pair them
			// with --frames to choose how long the camera then sits still —
			// the diff is what a temporal artifact actually costs, which a
			// plain still cannot show.
			"--fly" => config.fly = Some(parse_or_die("--fly", args.next())),
			"--warp" => {
				config.fly = Some(parse_or_die("--warp", args.next()));
				config.warp = true;
			}
			// Modifier on either: swap the flyover for an eye-height walk,
			// one block forward and one back. Screen-space artifacts scale
			// with screen motion, so the flyover and the walk are two
			// regimes and a fix has to be measured in both.
			"--walk" => config.walk = true,
			// Flyover speed in blocks/s. The default is several times
			// WALK_SPEED and is a different STREAMING regime, not just a
			// faster one: at that pace the GI upload budget cannot keep
			// up. Drop it to walking pace for a soak that matches how the
			// game is played. Leave the DEFAULT alone — every BENCH.md
			// baseline was taken at it.
			"--bench-speed" => {
				config.bench_speed = parse_or_die("--bench-speed", args.next());
			}
			// Fire the F7 GI drill at frame N. Pair with --fly/--frames so
			// the camera is frozen either side of it: same pose, same
			// thermals, only the GI's derived age differs.
			"--gi-drill-at" => {
				config.gi_drill_at = Some(parse_or_die("--gi-drill-at", args.next()));
			}
			// The WORLD's resolution as a fraction of the surface; the
			// upscale pass takes it to native and the UI is never touched.
			// An explicit value also suppresses the density-derived
			// default (see `render_scale_auto`).
			"--render-scale" => {
				let v: f32 = parse_or_die("--render-scale", args.next());
				cli_render_scale = Some(v.clamp(RENDER_SCALE_MIN, RENDER_SCALE_MAX));
			}
			// GI resolve divisor against the RENDER size: a slider rung, or
			// the lever-only full-res divisor the heavy-cost capture
			// brackets are taken at. Must divide exactly — chunk.wgsl's
			// bilateral upsample is a 4-tap gather over an integer ratio.
			"--gi-scale" => {
				let d: u32 = parse_or_die("--gi-scale", args.next());
				if !lever_gi_div_ok(d) {
					eprintln!(
						"--gi-scale wants one of {GI_SCALE_DIVISORS:?}, or {GI_SCALE_DIV_FULL} for captures"
					);
					std::process::exit(2);
				}
				cli_gi_scale = Some(d);
			}
			// A specific window size, LOGICAL. The GI resolve is a fraction of
			// the surface, so its cost scales with this directly — which makes
			// it the one thing a desktop-vs-web comparison must hold equal
			// before any of its numbers mean anything.
			"--size" => {
				let v: String = parse_or_die("--size", args.next());
				let (w, h) = v.split_once('x').unwrap_or_else(|| {
					eprintln!("--size wants WxH, e.g. --size 1728x1117");
					std::process::exit(2);
				});
				let parse = |s: &str| -> u32 {
					s.parse().unwrap_or_else(|_| {
						eprintln!("--size wants WxH, e.g. --size 1728x1117");
						std::process::exit(2);
					})
				};
				options.window_size = Some((parse(w), parse(h)));
			}
			"--no-vsync" => options.no_vsync = true,
			// Render into a texture of our own and never touch the
			// swapchain. What makes an unattended capture independent of
			// the desktop: a covered or screensaver'd window is handed no
			// surface texture on macOS, so a windowed run stops producing
			// frames the moment the screen locks. Never implied by
			// --screenshot — dropping the swapchain drops the present
			// wait with it, and BENCH.md's baselines were taken through
			// the surface.
			"--offscreen" => options.offscreen = true,
			// Pin the window to one monitor (index into winit's
			// enumeration). Frame-time captures are not comparable across
			// screens — backing scale sets the physical framebuffer and a
			// display-paced frame lets the GPU downclock — so any bracket
			// on a multi-monitor desk wants this.
			"--display" => options.display = Some(parse_or_die("--display", args.next())),
			// Fixed simulation step, in seconds — a capture flag. Without
			// it the water's ripple phase and the fluid/spawn settle ride
			// the wall clock, so two builds of different speed reach the
			// same `--frames N` in different world states and a parity
			// diff measures that instead of the change under test.
			"--fixed-dt" => {
				let s: f32 = parse_or_die("--fixed-dt", args.next());
				options.fixed_dt = Some(std::time::Duration::from_secs_f32(s));
			}
			"--workers" => config.workers = parse_or_die("--workers", args.next()),
			"--draw-dist" => cli_draw = Some(parse_or_die("--draw-dist", args.next())),
			// GI light-list cap (`Settings::max_lights`). Unsnapped on
			// purpose: the emitter-count bench sweeps caps the slider's
			// grid doesn't offer, and `flush_lights` clamps to the list's
			// capacity anyway.
			"--max-lights" => cli_max_lights = Some(parse_or_die("--max-lights", args.next())),
			"--seed" => config.seed = Some(parse_or_die("--seed", args.next())),
			// Dev builds, or a capture run that quits on its own (checked
			// after the loop, since --exit may come later). In multiplayer
			// the server dictates the world clock, so a shipped binary
			// must not let a SESSION carry a local override — but a run
			// that terminates itself after a screenshot is a capture, not
			// a session, and release is the only build whose frame times
			// mean anything. Measuring a stub A/B under a free-running sun
			// compares a lit frame against an unlit one: `gi_sun_vis`
			// returns immediately once the caster is below the horizon.
			"--tod" => cli_tod = Some(parse_or_die("--tod", args.next())),
			"--yaw" => config.yaw = Some(parse_or_die("--yaw", args.next())),
			"--pitch" => config.pitch = Some(parse_or_die("--pitch", args.next())),
			// Stand somewhere specific instead of at the seed's spawn. The
			// scenes this engine is slow in are natural water, which no
			// synthetic plan builds — see EngineConfig::start_pos.
			"--pos" => {
				config.start_pos = Some([
					parse_or_die("--pos x", args.next()),
					parse_or_die("--pos y", args.next()),
					parse_or_die("--pos z", args.next()),
				]);
			}
			// SSR override in either direction (--mobile --ssr is the
			// device-profiling escape hatch).
			"--ui-scale" => cli_ui_scale = Some(parse_or_die("--ui-scale", args.next())),
			"--ssr" => cli_ssr = Some(true),
			"--no-ssr" => cli_ssr = Some(false),
			"--metrics-csv" => {
				config.metrics_csv =
					Some(parse_or_die::<String>("--metrics-csv", args.next()).into());
			}
			other => {
				eprintln!("unknown arg: {other}");
				std::process::exit(2);
			}
		}
	}
	if let Some(n) = frames {
		options.screenshot_after_frames = n;
	}
	// See the --tod arm: dev builds always, release only for a run that
	// ends itself.
	if let Some(tod) = cli_tod {
		if !cfg!(debug_assertions) && !options.exit_after_screenshot {
			eprintln!("--tod needs a dev build, or --exit (a capture run that quits on its own)");
			std::process::exit(2);
		}
		config.tod = Some(tod);
	}
	if config.bench {
		config.seed.get_or_insert(42.0);
		config.tod.get_or_insert(45.0);
		config.metrics_csv.get_or_insert_with(|| "bench.csv".into());
		options.exit_after_frames = Some(u64::from(frames.unwrap_or(1800)));
		// Vsync-paced by default: the flight looks as played and the
		// over-budget frame count measures real drops. Add --no-vsync
		// for uncapped raw-cost measurement (percentiles = true frame
		// cost; presentation judders inherently at >display rate).
	} else if options.exit_after_screenshot && options.screenshot.is_none() {
		// `--exit` means what the --tod guard above already calls it: a
		// run that quits on its own. Without this it meant something much
		// narrower — the platform's only non-bench exit is the one behind
		// the SCREENSHOT (`exit_after_screenshot && screenshot.is_some()`),
		// and `--frames` on its own merely says WHEN to shoot rather than
		// when to stop.
		//
		// So `--metrics-csv out.csv --frames 400 --exit` armed neither
		// path and ran until it was killed, writing CSV rows the whole
		// time. It reads exactly like "run 400 frames and quit", which is
		// the trap: every flag in it is real, the combination is silently
		// inert, and the failure looks like a hang rather than a mistake.
		let Some(n) = frames else {
			eprintln!(
				"--exit needs something to end the run: --frames N, a --screenshot, or --bench"
			);
			std::process::exit(2);
		};
		options.exit_after_frames = Some(u64::from(n));
	}

	// Env-derived knobs live in the launcher; the engine stays env-free
	// (std::env is a stub on the wasm target).
	// DIG_PLAYERBOX=<item>: the fake remote's held item, in WIRE terms —
	// which is what makes the scene a real check of `held_icon` rather
	// than a second code path, and what lets it photograph a remote's lit
	// torch. Historically any value meant "on", and on meant a dirt cube,
	// so those spellings still do exactly that. An unknown one is fatal,
	// per DIG_CRACK: a typo must never silently capture the wrong scene.
	if let Some(v) = std::env::var_os("DIG_PLAYERBOX") {
		// var_os, not var: a value that is not UTF-8 is a typo like any
		// other and must be fatal, not silently drop the scene.
		match v.to_str().and_then(digblock_client::parse_held_item) {
			Some(id) => config.debug_playerbox = Some(id),
			None => {
				eprintln!(
					"DIG_PLAYERBOX: unknown item {v:?} — known: dirt, torch, \
					 stairs, slab, door, bucket, bucket-full, empty"
				);
				std::process::exit(2);
			}
		}
	}
	config.debug_confetti = std::env::var_os("DIG_CONFETTI").is_some();
	config.debug_shapes = std::env::var_os("DIG_SHAPES").is_some();
	config.debug_doors = std::env::var_os("DIG_DOORS").is_some();
	// DIG_VAULT_DARK=1: the vault with no torch on its floor, so the only
	// source in the room is the one being carried. It IMPLIES DIG_VAULT —
	// every other DIG_* scene switches itself on, and a modifier that
	// silently builds no room at all photographs open terrain and reads
	// as "a carried torch barely lights anything".
	config.debug_vault_dark = std::env::var_os("DIG_VAULT_DARK").is_some();
	config.debug_vault = config.debug_vault_dark || std::env::var_os("DIG_VAULT").is_some();
	config.debug_roof = std::env::var_os("DIG_ROOF").is_some();
	config.debug_tunnel = std::env::var_os("DIG_TUNNEL").is_some();
	config.debug_dive = std::env::var_os("DIG_DIVE").is_some();
	config.debug_river = std::env::var_os("DIG_RIVER").is_some();
	config.debug_torches_wall = std::env::var_os("DIG_TORCHES_WALL").is_some();
	// DIG_ORE=<tier 1..=4>: a chamber hollowed around the nearest deposit
	// of that tier, with the Mining level that reveals it. Ore is derived
	// and invisible below its level, so no pose alone can photograph it.
	config.debug_ore = std::env::var("DIG_ORE")
		.ok()
		.and_then(|v| v.parse().ok())
		.unwrap_or(0);
	// DIG_NODE=<heartwood|leystone|springstone>: the surface counterpart —
	// pose at the nearest node of that kind, its Alchemy reveal granted.
	// An unknown kind is FATAL rather than a silent open-terrain capture,
	// like every other capture flag here.
	config.debug_node = std::env::var("DIG_NODE").ok().map(|v| {
		digblock_client::parse_node_kind(&v)
			.unwrap_or_else(|| panic!("DIG_NODE: unknown node kind {v:?}"))
	});
	// DIG_MINING=<level>: start there. Everything a level gates is
	// otherwise unphotographable — the strength wall's refusals, dig
	// speed, and the Resonant Pick's ranks, the second of which brightens
	// the tool's own glow.
	config.debug_mining = std::env::var("DIG_MINING")
		.ok()
		.and_then(|v| v.parse().ok())
		.unwrap_or(0);
	// DIG_CRACK=<percent>: pin the crack decal at that progress, with a
	// wall ahead to hold it. A PERCENT so a sweep is `for p in 0 20 40 60
	// 80 100`, which is the only way to see the break spread — one still
	// says nothing about whether it grows outward or just fades in.
	if let Ok(v) = std::env::var("DIG_CRACK") {
		match v.parse::<f32>() {
			Ok(p) if (0.0..=100.0).contains(&p) => config.debug_crack = Some(p / 100.0),
			_ => {
				eprintln!("DIG_CRACK must be a percent in 0..=100, got {v:?}");
				std::process::exit(2);
			}
		}
	}
	// DIG_TORCHES=<n>: the courtyard, lit by n torches. A COUNT rather than
	// a flag, because the emitter count is the variable — the resolve's NEE
	// covers one light per pixel, so a single torch against a cluster, with
	// the geometry held fixed, is the whole experiment.
	for (name, slot) in [
		("DIG_TORCHES", &mut config.debug_torches),
		("DIG_TORCHES_FAR", &mut config.debug_torches_far),
	] {
		if let Ok(v) = std::env::var(name) {
			match v.parse::<i32>() {
				Ok(n) => *slot = n,
				Err(_) => {
					eprintln!("{name} must be a torch count, got {v:?}");
					std::process::exit(2);
				}
			}
		}
	}
	// DIG_GI_ITERS=<gather>,<sun>: the resolve's ray budgets, for the
	// tier A/B alone — the shipped values live in gi/resolve.rs and no
	// slider moves them. Fatal on a malformed value, same rule as
	// DIG_STUB: a sweep arm that silently ran the shipped budgets is the
	// worst possible outcome of a typo.
	if let Ok(v) = std::env::var("DIG_GI_ITERS") {
		let parsed = v.split_once(',').and_then(|(g, s)| {
			Some((g.trim().parse::<u32>().ok()?, s.trim().parse::<u32>().ok()?))
		});
		match parsed {
			Some((gather, sun)) => {
				config.gi_gather_iters = gather;
				config.gi_sun_iters = sun;
			}
			None => {
				eprintln!("DIG_GI_ITERS wants <gather>,<sun> iteration counts, got {v:?}");
				std::process::exit(2);
			}
		}
	}
	// DIG_STUB=wsun,wssr,…: remove shading terms so ONE build can
	// attribute a frame. A typo must be fatal — a run that silently
	// measures the un-stubbed build is precisely the mistake this exists
	// to stop.
	if let Ok(v) = std::env::var("DIG_STUB") {
		let (mask, bad) = digblock_client::stub::parse(&v);
		if !bad.is_empty() {
			eprintln!(
				"unknown DIG_STUB term(s): {} — known: {}",
				bad.join(", "),
				digblock_client::stub::names()
					.collect::<Vec<_>>()
					.join(", ")
			);
			std::process::exit(2);
		}
		config.stub = mask;
	}
	// DIG_GI=window|probes: paint the voxel-window raymarch over the
	// scene, shading with raw albedo (P1 gate) or the C0 probe field
	// (P2 gate).
	match std::env::var("DIG_GI").as_deref() {
		Ok("window") => config.debug_gi = true,
		Ok("probes") => {
			config.debug_gi = true;
			config.debug_gi_probes = true;
		}
		_ => {}
	}
	// DIG_SCREEN=inventory|alchemy: open that screen at startup with a
	// stocked inventory, so `--screenshot` can capture a UI screen (they
	// are otherwise reachable only through live input).
	config.debug_screen = std::env::var("DIG_SCREEN")
		.ok()
		.and_then(|v| digblock_client::DebugScreen::parse(&v));
	// DIG_SIPHON=<percent>: pin the essence draw at that progress on the vein
	// under the crosshair. A PERCENT, and meant as a SWEEP, for DIG_CRACK's
	// reason — one still cannot tell a stream that thickens from one that
	// merely brightens.
	//
	// Unlike the crack it cannot stand its own scene: a vein is derived worldgen
	// and there is no placing one, so it rides DIG_ORE, which already hollows a
	// chamber around a deposit and grants the level that reveals it. Without
	// that the crosshair lands on ordinary rock and the run photographs a bare
	// wall — the silent-wrong-scene failure every other parse here is fatal
	// about, so this one is too. And a draw needs the tool in hand: DIG_SCREEN's
	// own `siphon` spelling is what puts it there, defaulted rather than
	// re-implemented.
	if let Ok(v) = std::env::var("DIG_SIPHON") {
		match v.parse::<f32>() {
			Ok(p) if (0.0..=100.0).contains(&p) => config.debug_siphon = Some(p / 100.0),
			_ => {
				eprintln!("DIG_SIPHON must be a percent in 0..=100, got {v:?}");
				std::process::exit(2);
			}
		}
		if config.debug_ore == 0 {
			eprintln!("DIG_SIPHON needs a vein to draw from: set DIG_ORE=<tier 1..=4> too");
			std::process::exit(2);
		}
		config.debug_screen.get_or_insert_with(|| {
			digblock_client::DebugScreen::parse("siphon")
				.expect("DIG_SCREEN knows the siphon by name")
		});
		// And it has to be AIMED at the vein, which the default camera is
		// not: the fly camera starts pitched DOWN, so a DIG_ORE chamber
		// photographs the floor plate in front of the seam and the draw —
		// which exists only while the crosshair holds a vein — renders
		// nothing at all. Level puts the ray through the deposit's middle
		// row, since the plan already stands the eye there. An explicit
		// `--pitch` still wins, so a shot from under the seam is one flag
		// away.
		config.pitch.get_or_insert(0.0);
	}
	// Read at the launcher edge: the engine stays env-free so wasm, which
	// has no environment at all, can set the same flag from a query param.
	config.spike_log = std::env::var_os("DIG_SPIKELOG").is_some();
	config.chunk_slots_override = std::env::var("DIG_CHUNK_SLOTS").ok().map(|v| {
		v.parse::<u32>().ok().filter(|&n| n > 0).unwrap_or_else(|| {
			eprintln!("bad value for DIG_CHUNK_SLOTS: {v}");
			std::process::exit(2);
		})
	});

	// Deterministic capture paths (bench, screenshot, drill) get no menu,
	// no audio, and no master traffic — hermetic and bit-identical.
	let interactive = !config.bench && options.screenshot.is_none() && !options.auto_drill;
	// The engine needs the same answer: it decides from this whether to
	// listen to input at all, and whether the shell may take the mouse
	// pointer. Both used to key on `bench` alone, so a `--screenshot`
	// run locked the operator's cursor and swallowed their keystrokes.
	config.interactive = interactive;

	// Saved settings apply to interactive runs only (captures must not be
	// perturbed by user preferences), then explicit CLI flags win.
	if interactive {
		let saved = settings::load(digblock_client::settings::Settings {
			draw_dist: config.draw_dist,
			music_vol: config.music_vol,
			sfx_vol: config.sfx_vol,
			ssr: config.ssr,
			ui_scale: config.ui_scale,
			render_scale: config.render_scale,
			gi_scale_div: config.gi_scale_div,
			max_lights: config.max_lights,
			touch_jump_button: config.touch_jump_button,
		});
		config.draw_dist = saved.draw_dist;
		config.music_vol = saved.music_vol;
		config.sfx_vol = saved.sfx_vol;
		config.ssr = saved.ssr;
		config.ui_scale = saved.ui_scale;
		config.max_lights = saved.max_lights;
		config.touch_jump_button = saved.touch_jump_button;
		// A saved value (or a --gi-scale flag, applied after this block) is
		// an explicit preference; only its absence lets density pick.
		// `settings::load` leaves `base` untouched when the file has no
		// entry, so an unchanged value means nothing was saved.
		config.gfx_defaults_auto =
			cli_gi_scale.is_none() && saved.gi_scale_div == config.gi_scale_div;
		config.render_scale = saved.render_scale;
		config.gi_scale_div = saved.gi_scale_div;
	}
	if let Some(d) = cli_draw {
		config.draw_dist = d;
	}
	if let Some(n) = cli_max_lights {
		config.max_lights = n;
	}
	if let Some(ssr) = cli_ssr {
		config.ssr = ssr;
	}
	if let Some(u) = cli_ui_scale {
		config.ui_scale = u.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
	}
	if let Some(d) = cli_gi_scale {
		config.gi_scale_div = d;
		config.gfx_defaults_auto = false;
	}
	// Dev/capture only — no slider exposes render scale (settings.rs says
	// why), so this is the one way it moves.
	if let Some(r) = cli_render_scale {
		config.render_scale = r;
	}

	// The menu is for interactive runs only; --join must not sit on a
	// menu screen either.
	config.menu = !join && interactive;
	// ...except for a DIG_SCREEN=menu capture, whose whole point is to
	// photograph the menu a screenshot run would otherwise suppress.
	config.menu |= config.debug_screen == Some(digblock_client::DebugScreen::Menu);

	// Audio rides interactive runs only (the engine's sink stays None
	// elsewhere).
	let audio_on = !mute && interactive;

	let master_url = match master_url {
		Some(u) if u.is_empty() => None,
		Some(u) => Some(u),
		// The production default applies to interactive runs and joins
		// only — hermetic capture paths must not ping the master.
		None if interactive || join => Some(digblock_net::protocol::DEFAULT_MASTER_URL.into()),
		None => None,
	};

	// A scripted join has no browser and no menu, so its session comes from
	// the master's development sign-in rather than the real flow. There is
	// no unauthenticated fallback: joining is authenticated now, and
	// pretending otherwise would fail later and less clearly. Done BEFORE
	// the engine exists so the resolved name can go into its config.
	let join_session = if join {
		let master_url = master_url
			.as_deref()
			.map(|u| u.trim_end_matches('/'))
			.unwrap_or_else(|| {
				eprintln!(
					"--join needs a master server: don't combine it with an empty --master-url"
				);
				std::process::exit(2);
			});
		let Some(subject) = dev_signin.as_deref() else {
			eprintln!(
				"--join needs --dev-signin <subject>, and a master running with \
				 Master__Dev__AuthEnabled=true"
			);
			std::process::exit(2);
		};
		match auth::dev_sign_in(master_url, subject) {
			Ok(s) => {
				config.player_name = s.display_name.clone();
				Some(s)
			}
			Err(e) => {
				log::error!("--dev-signin via {master_url} failed: {e}");
				std::process::exit(1);
			}
		}
	} else {
		None
	};

	// The pre-account character's lookup key, for servers this machine
	// played on before accounts existed. Only on a path that can actually
	// join: a capture run has no business reading it, and the migration
	// costs a wire message per join for everyone who still has the file.
	if interactive || join {
		config.legacy_identity = legacy_identity();
	}

	let mut engine = Engine::new(config);
	if interactive {
		engine.set_settings_store(Box::new(settings::save));
	}
	// Named local saves ride the menu; scripted joins and capture paths
	// stay store-less (they keep the ephemeral world + hermetic runs).
	if !join && interactive {
		let worlds = dirs::data_dir()
			.map(|d| d.join("digblock").join("worlds"))
			.unwrap_or_else(|| std::path::PathBuf::from("./digblock-worlds"));
		engine.set_world_store(Box::new(digblock_persist::FsWorldStore::new(worlds)));
	}
	if audio_on {
		engine.set_audio(audio::start(
			master_url
				.as_deref()
				.map(|u| u.trim_end_matches('/').to_string()),
		));
	}
	if let Some(master_url) = master_url.as_deref().map(|u| u.trim_end_matches('/')) {
		if let Some(session) = &join_session {
			match services::auto_join(master_url, server_filter.as_deref(), &session.token) {
				Ok((transport, token)) => {
					engine.set_transport_with_token(Box::new(transport), token);
				}
				Err(e) => {
					log::error!("auto-join via {master_url} failed: {e}");
					std::process::exit(1);
				}
			}
		} else {
			// A stored session is rotated on the services thread before
			// anything can need it; the menu hears the result as an
			// Account response.
			engine.set_menu_services(services::start(
				master_url.to_string(),
				auth::load(&auth::session_path()),
			));
			engine.set_clipboard(Box::new(copy_to_clipboard));
		}
	} else if join {
		eprintln!("--join needs a master server: don't combine it with an empty --master-url");
		std::process::exit(2);
	}
	if let Err(e) = digblock_platform::run(engine, options) {
		log::error!("{e}");
		std::process::exit(1);
	}
}

/// The key this machine's pre-account character is filed under on the servers it
/// used to play on: the truncated SHA-256 of the old identity secret, which is
/// exactly what those servers stored.
///
/// **Read-only, and it never creates the file.** The secret it hashes was a
/// bearer credential the client used to mint out of thin air; nothing does that
/// any more, so a machine without one has no old character and must not be given
/// a key to somebody else's. Absent, unreadable and all-zeros all answer `None`.
///
/// The secret itself stops here — only the hash goes on the wire. Not much of a
/// defence, since the hash is what a claim needs, but it keeps the one file that
/// used to be replayed to every server from being replayed anywhere at all.
fn legacy_identity() -> Option<[u8; 16]> {
	let path = std::env::var("DIG_IDENTITY_FILE")
		.map(std::path::PathBuf::from)
		.unwrap_or_else(|_| {
			dirs::data_dir()
				.map(|d| d.join("digblock").join("identity"))
				.unwrap_or_else(|| std::path::PathBuf::from("./digblock-identity"))
		});
	let secret = <[u8; 16]>::try_from(std::fs::read(&path).ok()?.as_slice()).ok()?;
	if secret == [0; 16] {
		return None;
	}
	log::info!(
		"found a pre-account identity at {} — offering it to servers for migration",
		path.display()
	);
	Some(digblock_net::protocol::legacy_identity_hash(&secret))
}

/// The Account screen's Copy ID button.
///
/// The `Clipboard` is kept alive for the process rather than made per copy.
/// On X11 the clipboard is not a buffer the system owns — it is a promise
/// served by the application that put it there, so a context dropped right
/// after setting takes the contents with it and the paste comes back empty.
/// One long-lived instance keeps serving it. (The other obvious fix,
/// arboard's `set().wait()`, blocks until another app takes the selection,
/// which on this call path would freeze the frame.)
fn copy_to_clipboard(text: &str) {
	use std::sync::{Mutex, OnceLock};
	static CLIPBOARD: OnceLock<Option<Mutex<arboard::Clipboard>>> = OnceLock::new();
	let handle = CLIPBOARD.get_or_init(|| match arboard::Clipboard::new() {
		Ok(c) => Some(Mutex::new(c)),
		Err(e) => {
			log::warn!("no clipboard available ({e}) — ids are on screen to copy by hand");
			None
		}
	});
	let Some(handle) = handle else { return };
	let mut guard = handle.lock().expect("clipboard mutex");
	match guard.set_text(text.to_string()) {
		Ok(()) => log::info!("copied to clipboard"),
		Err(e) => log::warn!("clipboard write failed: {e}"),
	}
}
