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
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--screenshot" => options.screenshot = args.next().map(Into::into),
			"--exit" => options.exit_after_screenshot = true,
			"--frames" => {
				options.screenshot_after_frames =
					args.next().and_then(|v| v.parse().ok()).unwrap_or(10)
			}
			"--drill" => options.auto_drill = true,
			"--seed" => config.seed = args.next().and_then(|v| v.parse().ok()),
			"--metrics-csv" => config.metrics_csv = args.next().map(Into::into),
			other => {
				eprintln!("unknown arg: {other}");
				std::process::exit(2);
			}
		}
	}

	digblock_platform::run(Engine::new(config), options);
}
