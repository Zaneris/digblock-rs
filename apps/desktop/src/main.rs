use digblock_platform::RunOptions;
use digblock_render::{Engine, EngineConfig};

mod services;

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
	//   --no-vsync --workers N --draw-dist N (workload knobs / profiling)
	let mut frames: Option<u32> = None;
	// Multiplayer goes through the master server exclusively: it lists
	// servers, distributes cert pins, and mints the one-time join tokens
	// game servers require. There is no direct-connect path.
	let mut master_url = std::env::var("DIG_MASTER_URL").ok();
	let mut join = false;
	let mut server_filter: Option<String> = None;
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
			"--no-vsync" => options.no_vsync = true,
			"--workers" => config.workers = parse_or_die("--workers", args.next()),
			"--draw-dist" => config.draw_dist = parse_or_die("--draw-dist", args.next()),
			"--seed" => config.seed = Some(parse_or_die("--seed", args.next())),
			"--tod" => config.tod = Some(parse_or_die("--tod", args.next())),
			"--yaw" => config.yaw = Some(parse_or_die("--yaw", args.next())),
			"--pitch" => config.pitch = Some(parse_or_die("--pitch", args.next())),
			// SSR override in either direction (--mobile --ssr is the
			// device-profiling escape hatch).
			"--ssr" => config.ssr = true,
			"--no-ssr" => config.ssr = false,
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

	// Env-derived knobs live in the launcher; the engine stays env-free
	// (std::env is a stub on the wasm target).
	config.player_name = std::env::var("USER").ok();
	config.debug_playerbox = std::env::var_os("DIG_PLAYERBOX").is_some();
	config.chunk_slots_override = std::env::var("DIG_CHUNK_SLOTS").ok().map(|v| {
		v.parse::<u32>().ok().filter(|&n| n > 0).unwrap_or_else(|| {
			eprintln!("bad value for DIG_CHUNK_SLOTS: {v}");
			std::process::exit(2);
		})
	});

	// The menu is for interactive runs only; every automated capture path
	// (bench, screenshot, drill) and --join must not sit on a menu screen.
	config.menu = !join && !config.bench && options.screenshot.is_none() && !options.auto_drill;

	let mut engine = Engine::new(config);
	if let Some(master_url) = master_url.as_deref().map(|u| u.trim_end_matches('/')) {
		if join {
			let name = std::env::var("USER").unwrap_or_else(|_| "player".into());
			match services::auto_join(master_url, server_filter.as_deref(), &name) {
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
		eprintln!("--join needs a master server: pass --master-url or set DIG_MASTER_URL");
		std::process::exit(2);
	}
	if let Err(e) = digblock_platform::run(engine, options) {
		log::error!("{e}");
		std::process::exit(1);
	}
}
