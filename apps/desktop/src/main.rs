use digblock_platform::RunOptions;
use digblock_render::{Engine, EngineConfig};

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
	//   --bench (scripted flight, inputs off, vsync off; defaults seed 42,
	//            tod 45, 1800 frames, bench.csv)
	//   --no-vsync --workers N --draw-dist N (workload knobs / profiling)
	let mut frames: Option<u32> = None;
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--screenshot" => options.screenshot = args.next().map(Into::into),
			"--exit" => options.exit_after_screenshot = true,
			"--frames" => frames = args.next().and_then(|v| v.parse().ok()),
			"--drill" => options.auto_drill = true,
			"--mobile" => config = EngineConfig::mobile(),
			"--overlay" => config.overlay = true,
			"--bench" => config.bench = true,
			"--no-vsync" => options.no_vsync = true,
			"--workers" => {
				if let Some(n) = args.next().and_then(|v| v.parse().ok()) {
					config.workers = n;
				}
			}
			"--draw-dist" => {
				if let Some(n) = args.next().and_then(|v| v.parse().ok()) {
					config.draw_dist = n;
				}
			}
			"--seed" => config.seed = args.next().and_then(|v| v.parse().ok()),
			"--tod" => config.tod = args.next().and_then(|v| v.parse().ok()),
			"--metrics-csv" => config.metrics_csv = args.next().map(Into::into),
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

	digblock_platform::run(Engine::new(config), options);
}
