//! DigBlock dedicated server binary.

use crossbeam_channel::unbounded;
use digblock_server::sim::Sim;
use digblock_server::{net, run_sim_loop, DEFAULT_PORT, DEFAULT_SEED};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

	let mut port = DEFAULT_PORT;
	let mut seed = DEFAULT_SEED;
	let mut data_dir = PathBuf::from("./data");
	let mut sim_radius = 6;
	let mut workers = std::thread::available_parallelism()
		.map(|n| n.get().saturating_sub(2))
		.unwrap_or(2)
		.clamp(2, 8);

	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		let mut val = || args.next().unwrap_or_default();
		match arg.as_str() {
			"--port" => port = val().parse().unwrap_or(DEFAULT_PORT),
			"--seed" => seed = val().parse().unwrap_or(DEFAULT_SEED),
			"--data-dir" => data_dir = PathBuf::from(val()),
			"--sim-radius" => sim_radius = val().parse().unwrap_or(6),
			"--workers" => workers = val().parse().unwrap_or(workers),
			other => {
				eprintln!("unknown arg: {other}");
				eprintln!(
					"usage: digblock-server [--port N] [--seed F] [--data-dir P] \
					 [--sim-radius N] [--workers N]"
				);
				std::process::exit(2);
			}
		}
	}

	let sim = match Sim::new(seed, &data_dir, sim_radius, workers) {
		Ok(sim) => sim,
		Err(e) => {
			eprintln!("failed to open world: {e}");
			std::process::exit(1);
		}
	};
	log::info!("world seed {seed}, data dir {}", data_dir.display());

	let (event_tx, event_rx) = unbounded();
	let running = Arc::new(AtomicBool::new(true));
	let sim_thread = {
		let running = running.clone();
		std::thread::Builder::new()
			.name("sim".into())
			.spawn(move || run_sim_loop(sim, event_rx, running))
			.expect("spawn sim thread")
	};

	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.expect("tokio runtime");
	rt.block_on(async {
		let endpoint = match net::make_endpoint(port) {
			Ok(e) => e,
			Err(e) => {
				eprintln!("failed to bind udp {port}: {e}");
				std::process::exit(1);
			}
		};
		log::info!("listening on udp {port}");
		tokio::select! {
			_ = net::serve(endpoint, event_tx) => {}
			_ = tokio::signal::ctrl_c() => log::info!("ctrl-c: shutting down"),
		}
	});

	running.store(false, Ordering::Relaxed);
	let _ = sim_thread.join();
}
