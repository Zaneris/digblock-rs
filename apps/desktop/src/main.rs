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

	// Deterministic-capture flags.
	// The couplings that bite: --fixed-dt is what makes a capture comparable ACROSS BUILDS,
	// --offscreen is deliberately not implied by --screenshot (it also drops the present wait a
	// frame-time run measures), --render-scale is dev/capture only, and --bench-speed's default is
	// every bench baseline's value — a different STREAMING regime, not merely a faster one.
	let mut frames: Option<u32> = None;
	// Multiplayer goes through the master exclusively: it lists servers, distributes cert pins and
	// mints the one-time join tokens game servers require, so there is no direct-connect path. The
	// production master is the default; --master-url / DIG_MASTER_URL point dev stacks elsewhere,
	// and an explicitly empty value opts out of master-backed features (menu multiplayer, music).
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
			// The scene roster, printed from the table that defines it.
			// The web harness shows THIS listing in its own `--help`
			// (tools/web-e2e/dig.mjs) instead of keeping a copy, which is
			// what stops the two drifting; `Kind` is what spells a value
			// knob with its placeholder and a flag without one.
			"--knobs" => {
				for k in digblock_client::knobs::ALL {
					let spelling = match k.kind {
						digblock_client::knobs::Kind::Flag => format!("--{}", k.name),
						digblock_client::knobs::Kind::Value => format!("--{} <value>", k.name),
					};
					println!("{spelling:<22} {:<18} {}", k.env, k.help);
				}
				return;
			}
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
			// The third regime: a still stare straight down from the top of
			// the world, a few stations of it. The GI worst case — see
			// `engine/stare.rs`. Ends itself after the last station.
			"--stare" => config.stare = true,
			// Flyover speed in blocks/s. The default is several times
			// WALK_SPEED and is a different STREAMING regime, not just a
			// faster one: at that pace the GI upload budget cannot keep
			// up. Drop it to walking pace for a soak that matches how the
			// game is played. Leave the DEFAULT alone — every bench
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
			// wait with it, and the bench baselines were taken through
			// the surface.
			"--offscreen" => options.offscreen = true,
			// Borderless fullscreen on the pinned display. The bench's resolve cost is per pixel,
			// so this is what makes two runs measure the same number of them.
			"--fullscreen" => options.fullscreen = true,
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
	if config.walk && config.stare {
		eprintln!("--walk and --stare are two regimes; pick one");
		std::process::exit(2);
	}
	if config.bench {
		config.seed.get_or_insert(42.0);
		config.tod.get_or_insert(45.0);
		config.metrics_csv.get_or_insert_with(|| "bench.csv".into());
		if config.stare {
			config.markers = false;
			// A late-game pick, unless DIG_MINING says otherwise (knobs
			// apply below and override): every vein in the window reveals
			// and joins the light list, which is the emitter load the
			// resolve's NEE and the list's selection scale with.
			if config.debug_mining == 0 {
				config.debug_mining = digblock_core::skills::MAX_LEVEL;
			}
		}
		let default_frames = if config.stare {
			digblock_client::STARE_FRAMES
		} else {
			1800
		};
		options.exit_after_frames = Some(frames.map_or(default_frames, u64::from));
		// Vsync-paced by default: the flight looks as played and the
		// over-budget frame count measures real drops. Add --no-vsync
		// for uncapped raw-cost measurement (percentiles = true frame
		// cost; presentation judders inherently at >display rate).
	} else if options.exit_after_screenshot && options.screenshot.is_none() {
		// `--exit` means what the --tod guard above already calls it: a run that quits on its own.
		// Without this it meant something much narrower — the platform's only non-bench exit sits
		// behind the SCREENSHOT, and `--frames` alone says WHEN to shoot rather than when to stop,
		// so `--metrics-csv out.csv --frames 400 --exit` armed neither path and ran until it was
		// killed. Every flag in it is real, the combination inert, and it looked like a hang.
		let Some(n) = frames else {
			eprintln!(
				"--exit needs something to end the run: --frames N, a --screenshot, or --bench"
			);
			std::process::exit(2);
		};
		options.exit_after_frames = Some(u64::from(n));
	}

	// Every DIG_* scene and lever is read through ONE table the web page
	// reads too (`knobs::ALL`), so neither launcher can grow a knob the
	// other lacks; the engine itself stays env-free (std::env is a stub on
	// the wasm target). A malformed value is exit 2, never a silent default.
	if let Err(e) = digblock_client::knobs::apply(&mut config, &|k| {
		// var_os, not var: a value that is not UTF-8 is a typo like any
		// other and must be fatal, not silently drop the scene.
		std::env::var_os(k.env).map(|v| v.to_string_lossy().into_owned())
	}) {
		eprintln!("{e}");
		std::process::exit(2);
	}
	// The two knobs the SHELL acts on, before a device exists.
	options.no_gpu_timer = !config.gpu_timer;
	options.fps_cap = config.fps_cap;
	// The other half of that rule, and the one the environment needs: the page refuses a `?key` it
	// does not know, but reading the table out of the environment can only MISS a misspelled
	// variable, so `DIG_VUALT=1` photographs open terrain and exits 0 — the silent wrong scene this
	// sweep exists to prevent. Tolerated beside the table: this launcher's own three, and
	// DIG_CHROME, which the shader gate and the web harness read and a developer keeps exported.
	const OWN_ENV: &[&str] = &[
		"DIG_MASTER_URL",
		"DIG_IDENTITY_FILE",
		"DIG_SESSION_FILE",
		"DIG_CHROME",
	];
	let unknown: Vec<String> = std::env::vars_os()
		.map(|(k, _)| k.to_string_lossy().into_owned())
		.filter(|k| {
			k.starts_with("DIG_")
				&& !OWN_ENV.contains(&k.as_str())
				&& !digblock_client::knobs::ALL.iter().any(|n| n.env == k)
		})
		.collect();
	if !unknown.is_empty() {
		eprintln!(
			"unknown DIG_* variable(s): {} — known: {}, and every scene `--knobs` lists",
			unknown.join(", "),
			OWN_ENV.join(", ")
		);
		std::process::exit(2);
	}

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
			invert_mouse_y: config.invert_mouse_y,
			touch_jump_button: config.touch_jump_button,
			show_tutorial: config.show_tutorial,
			bindings: config.bindings.clone(),
		});
		config.draw_dist = saved.draw_dist;
		config.music_vol = saved.music_vol;
		config.sfx_vol = saved.sfx_vol;
		config.ssr = saved.ssr;
		config.ui_scale = saved.ui_scale;
		config.invert_mouse_y = saved.invert_mouse_y;
		config.touch_jump_button = saved.touch_jump_button;
		config.show_tutorial = saved.show_tutorial;
		config.bindings = saved.bindings.clone();
		// A saved value (or a flag, applied after this block) is an
		// explicit preference; only its absence lets the engine pick.
		// `settings::load` leaves `base` untouched when the file has no
		// entry, so an unchanged value means nothing was saved. Every
		// graphics term the block picks is tested, light cap included —
		// one that is not would be overwritten after the player chose it.
		config.gfx_defaults_auto = cli_gi_scale.is_none()
			&& cli_max_lights.is_none()
			&& saved.gi_scale_div == config.gi_scale_div
			&& saved.max_lights == config.max_lights;
		// After the test, like its siblings: each is compared against the
		// base it would overwrite.
		config.render_scale = saved.render_scale;
		config.gi_scale_div = saved.gi_scale_div;
		config.max_lights = saved.max_lights;
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

/// The key this machine's pre-account character is filed under on servers it used to play on: the
/// truncated SHA-256 of the old identity secret, which is what those servers stored.
/// **Read-only, and it never creates the file** — the secret it hashes was a bearer credential
/// nothing mints any more, so a machine without one must not be handed a key to somebody else's.
/// Absent, unreadable and all-zeros all answer `None`; only the hash ever goes on the wire.
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

/// The Account screen's Copy ID button. The `Clipboard` is kept alive for the process rather than
/// made per copy: on X11 the clipboard is not a buffer the system owns but a promise served by the
/// application that put it there, so a context dropped right after setting takes the contents with
/// it and the paste comes back empty. arboard's `set().wait()` is the other obvious fix and blocks
/// until another app takes the selection, which on this call path would freeze the frame.
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
