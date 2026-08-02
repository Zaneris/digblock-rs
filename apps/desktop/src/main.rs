use digblock_platform::RunOptions;
use digblock_render::{Engine, EngineConfig};

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
	let mut connect: Option<String> = None;
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--connect" => connect = Some(parse_or_die::<String>("--connect", args.next())),
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

	let mut engine = Engine::new(config);
	if let Some(url) = connect {
		log::info!("connecting to {url}");
		engine.set_transport(Box::new(digblock_net_native::connect(&url)));
	}
	digblock_platform::run(engine, options);
}
