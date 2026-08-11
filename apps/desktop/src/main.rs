use digblock_platform::RunOptions;
use digblock_render::settings::{UI_SCALE_MAX, UI_SCALE_MIN};
use digblock_render::{Engine, EngineConfig};

mod audio;
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
	//   --seed <f64 bits or decimal> --metrics-csv out.csv
	//   --yaw <deg> --pitch <deg> (initial camera view; yaw 90 faces +x,
	//            the sun azimuth plane)
	//   --bench (scripted flight, inputs off, vsync off; defaults seed 42,
	//            tod 45, 1800 frames, bench.csv)
	//   --fly N / --warp N (fly the bench path N frames then freeze, or
	//            arrive at that same pose with no motion behind it — the
	//            temporal-artifact A/B; pair with --frames)
	//   --walk (modifier on either: an eye-height oscillation, one block
	//            forward and one back, instead of the 30 blocks/s flyover —
	//            a screen-space artifact scales with screen motion, so the
	//            two paths are two regimes)
	//   --size WxH (logical window size; the GI resolve is half-res off
	//            the surface, so this is what a cross-platform comparison
	//            has to hold equal)
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
	let mut name: Option<String> = None;
	let mut mute = false;
	// Recorded, not applied: CLI overrides beat the saved settings, which
	// beat the platform defaults — resolved after the parse loop.
	let mut cli_draw: Option<i32> = None;
	let mut cli_ssr: Option<bool> = None;
	let mut cli_ui_scale: Option<f32> = None;
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
			// Display name (3-10 chars: a-z A-Z 0-9 - _). Pre-fills the
			// menu field; required for --join. Never defaulted — entering
			// a name is mandatory.
			"--name" => name = Some(parse_or_die::<String>("--name", args.next())),
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
			// A specific window size, LOGICAL. The GI resolve is half-res off
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
			"--workers" => config.workers = parse_or_die("--workers", args.next()),
			"--draw-dist" => cli_draw = Some(parse_or_die("--draw-dist", args.next())),
			"--seed" => config.seed = Some(parse_or_die("--seed", args.next())),
			// Dev builds only: in multiplayer the server dictates the
			// world clock, so a shipped binary must not carry a local
			// override (--bench still freezes tod internally — that's a
			// programmatic single-player capture, not this flag).
			"--tod" if cfg!(debug_assertions) => {
				config.tod = Some(parse_or_die("--tod", args.next()));
			}
			"--tod" => {
				eprintln!("--tod is available in dev builds only");
				std::process::exit(2);
			}
			"--yaw" => config.yaw = Some(parse_or_die("--yaw", args.next())),
			"--pitch" => config.pitch = Some(parse_or_die("--pitch", args.next())),
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
	if config.bench {
		config.seed.get_or_insert(42.0);
		config.tod.get_or_insert(45.0);
		config.metrics_csv.get_or_insert_with(|| "bench.csv".into());
		options.exit_after_frames = Some(u64::from(frames.unwrap_or(1800)));
		// Vsync-paced by default: the flight looks as played and the
		// over-12.5ms count measures real dropped frames. Add --no-vsync
		// for uncapped raw-cost measurement (percentiles = true frame
		// cost; presentation judders inherently at >display rate).
	}

	// An explicit --name must be valid; a bad one is fatal like any other
	// bad flag value (silently dropping it would surprise scripted joins).
	if let Some(n) = &name {
		if let Err(e) = digblock_net::protocol::validate_name(n) {
			eprintln!("bad value for --name: {e}");
			std::process::exit(2);
		}
	}
	// Env-derived knobs live in the launcher; the engine stays env-free
	// (std::env is a stub on the wasm target).
	config.player_name = name.clone();
	config.identity = load_identity();
	config.debug_playerbox = std::env::var_os("DIG_PLAYERBOX").is_some();
	config.debug_shapes = std::env::var_os("DIG_SHAPES").is_some();
	config.debug_vault = std::env::var_os("DIG_VAULT").is_some();
	config.debug_roof = std::env::var_os("DIG_ROOF").is_some();
	config.debug_dive = std::env::var_os("DIG_DIVE").is_some();
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
		.and_then(|v| digblock_render::DebugScreen::parse(&v));
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

	// Saved settings apply to interactive runs only (captures must not be
	// perturbed by user preferences), then explicit CLI flags win.
	if interactive {
		let saved = settings::load(digblock_render::settings::Settings {
			draw_dist: config.draw_dist,
			music_vol: config.music_vol,
			sfx_vol: config.sfx_vol,
			ssr: config.ssr,
			ui_scale: config.ui_scale,
			name: String::new(),
		});
		config.draw_dist = saved.draw_dist;
		config.music_vol = saved.music_vol;
		config.sfx_vol = saved.sfx_vol;
		config.ssr = saved.ssr;
		config.ui_scale = saved.ui_scale;
		// --name beats the remembered name (validated above vs at load).
		if name.is_none() && !saved.name.is_empty() {
			name = Some(saved.name);
			config.player_name = name.clone();
		}
	}
	if let Some(d) = cli_draw {
		config.draw_dist = d;
	}
	if let Some(ssr) = cli_ssr {
		config.ssr = ssr;
	}
	if let Some(u) = cli_ui_scale {
		config.ui_scale = u.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
	}

	// The menu is for interactive runs only; --join must not sit on a
	// menu screen either.
	config.menu = !join && interactive;
	// ...except for a DIG_SCREEN=menu capture, whose whole point is to
	// photograph the menu a screenshot run would otherwise suppress.
	config.menu |= config.debug_screen == Some(digblock_render::DebugScreen::Menu);

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
		if join {
			let Some(name) = name.as_deref() else {
				eprintln!("--join needs --name <name> (3-10 chars: a-z A-Z 0-9 - _)");
				std::process::exit(2);
			};
			match services::auto_join(master_url, server_filter.as_deref(), name) {
				Ok((transport, token)) => {
					engine.set_transport_with_token(Box::new(transport), token);
				}
				Err(e) => {
					log::error!("auto-join via {master_url} failed: {e}");
					std::process::exit(1);
				}
			}
		} else {
			engine.set_menu_services(services::start(master_url.to_string()));
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

/// Load-or-create the persistent 16-byte identity secret keying the
/// server-side persisted inventory. A bearer secret by design (there are
/// no accounts): whoever holds the bytes owns the inventory, so the file
/// lives in the user's data dir. DIG_IDENTITY_FILE overrides the path —
/// how a second local client gets its own identity for testing.
fn load_identity() -> [u8; 16] {
	let path = std::env::var("DIG_IDENTITY_FILE")
		.map(std::path::PathBuf::from)
		.unwrap_or_else(|_| {
			dirs::data_dir()
				.map(|d| d.join("digblock").join("identity"))
				.unwrap_or_else(|| std::path::PathBuf::from("./digblock-identity"))
		});
	if let Ok(bytes) = std::fs::read(&path) {
		if let Ok(id) = <[u8; 16]>::try_from(bytes.as_slice()) {
			if id != [0; 16] {
				return id;
			}
		}
		log::warn!("identity file {} malformed — regenerating", path.display());
	}
	let id: [u8; 16] = rand::random();
	if let Some(parent) = path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}
	if let Err(e) = write_secret(&path, &id) {
		log::warn!(
			"could not persist identity to {} ({e}) — inventory is per-run",
			path.display()
		);
	}
	id
}

/// Write the identity secret owner-readable only. It is a bearer
/// credential — whoever holds it owns that player's inventory, layout and
/// last position on every server — so the default 0644 of `fs::write` handed
/// it to every other local user. (Scoping the secret per server, so a
/// hostile server operator cannot replay it, needs the master's login flow;
/// see NET-AUDIT-2.md §1.4.)
fn write_secret(path: &std::path::Path, id: &[u8; 16]) -> std::io::Result<()> {
	use std::io::Write;
	let mut opts = std::fs::OpenOptions::new();
	opts.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		opts.mode(0o600);
	}
	// `mode` only applies when the file is CREATED, so an existing 0644 file
	// from an older build keeps its permissions — fix those up explicitly.
	let mut f = opts.open(path)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
	}
	f.write_all(id)
}
