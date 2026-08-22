//! DigBlock dedicated server binary.

use crossbeam_channel::unbounded;
use digblock_net::protocol::AccountId;
use digblock_server::master::{self, MasterConfig, MasterState};
use digblock_server::sim::Sim;
use digblock_server::{DEFAULT_PORT, DEFAULT_SEED, net, run_sim_loop};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Turn every `--admin` argument into an account id, in order.
///
/// An id passes straight through; a display name costs one lookup against
/// the master. Doing it here rather than at each chat command is what makes
/// admin a decision about an ACCOUNT: the name is resolved once, at boot,
/// against the only party that can answer, and after that the name is just
/// a label that its owner may change freely.
fn resolve_admins(specs: &[String], master_url: &str) -> Result<Vec<AccountId>, String> {
	specs
		.iter()
		.map(|spec| {
			if let Some(id) = AccountId::parse(spec).filter(|a| !a.is_none()) {
				return Ok(id);
			}
			if digblock_net::protocol::validate_name(spec).is_err() {
				return Err(format!(
					"--admin {spec:?}: not an account id, and not a possible display name either"
				));
			}
			master::lookup_account(master_url, spec).map_err(|e| {
				format!(
					"--admin {spec:?}: could not resolve that name with the master ({e}).\n\
					 The player must have signed in and picked their name at least once. \
					 Passing their account id instead avoids the lookup entirely."
				)
			})
		})
		.collect()
}

/// A silently-defaulted flag on a server with a persistent journal is a
/// world-corruption hazard (`--seed banana` → default seed over existing
/// data), so bad or missing values are fatal, like unknown args.
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

use digblock_net::protocol::DEFAULT_MASTER_URL;

const USAGE: &str = "usage: digblock-server [--advertise-host HOST] \
	 [--master-url URL] [--advertise-port N] [--server-name S] [--port N] \
	 [--seed F] [--data-dir P] [--sim-radius N] [--workers N] [--admin ID-OR-NAME] \
	 [--max-conns-per-ip N]\n\
	 (--advertise-host defaults to the address the master observes registering — your public \
	 IP; set it for a DNS name or multi-homed hosts. --master-url defaults to \
	 https://master.digblock.net. --admin grants a player chat commands (/help /ban \
	 /ban-list /unban /tp) — give an account id, which a player can copy from the game's \
	 Account screen, or a display name, which is resolved to one at boot; repeat for \
	 several admins. --max-conns-per-ip caps live connections from one address; \
	 raise it for a local soak, where every bot shares one. Env fallbacks: \
	 DIGBLOCK_MASTER_URL, DIGBLOCK_ADVERTISE_HOST, DIGBLOCK_ADVERTISE_PORT, \
	 DIGBLOCK_SERVER_NAME, DIGBLOCK_SEED, DIGBLOCK_MAX_CONNS_PER_IP, DIGBLOCK_ADMIN_IDS \
	 (comma-separated))";

fn main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
	// First line out, before anything that can fail: a bug report starts
	// with knowing which build produced it.
	log::info!("digblock-server {}", env!("CARGO_PKG_VERSION"));

	let mut port = DEFAULT_PORT;
	// Empty env values count as unset — compose files pass "${SEED:-}"
	// through, and an empty string must not be a parse error.
	let mut seed = std::env::var("DIGBLOCK_SEED")
		.ok()
		.filter(|s| !s.is_empty())
		.map(|v| parse_or_die("DIGBLOCK_SEED", Some(v)))
		.unwrap_or(DEFAULT_SEED);
	let mut data_dir = PathBuf::from("./data");
	let mut sim_radius = 6;
	let mut workers = std::thread::available_parallelism()
		.map(|n| n.get().saturating_sub(2))
		.unwrap_or(2)
		.clamp(2, 8);
	// A master is mandatory (there is no tokenless way onto a server, and
	// tokens come from the master, so an unregistered server could admit
	// no one anyway) — the URL just defaults to production. The advertise
	// host stays required: the server can never guess its public address.
	let mut master_url = std::env::var("DIGBLOCK_MASTER_URL").ok();
	let mut advertise_host = std::env::var("DIGBLOCK_ADVERTISE_HOST").ok();
	let mut advertise_port: Option<u16> = std::env::var("DIGBLOCK_ADVERTISE_PORT")
		.ok()
		.map(|v| parse_or_die("DIGBLOCK_ADVERTISE_PORT", Some(v)));
	let mut server_name =
		std::env::var("DIGBLOCK_SERVER_NAME").unwrap_or_else(|_| "DigBlock Server".into());
	let mut max_conns_per_ip: usize = std::env::var("DIGBLOCK_MAX_CONNS_PER_IP")
		.ok()
		.filter(|s| !s.is_empty())
		.map(|v| parse_or_die("DIGBLOCK_MAX_CONNS_PER_IP", Some(v)))
		.unwrap_or(net::DEFAULT_MAX_CONNECTIONS_PER_IP);
	// Account ids or display names; names are resolved once at boot.
	let mut admins: Vec<String> = std::env::var("DIGBLOCK_ADMIN_IDS")
		.unwrap_or_default()
		.split(',')
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
		.collect();

	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--port" => port = parse_or_die("--port", args.next()),
			"--seed" => seed = parse_or_die("--seed", args.next()),
			"--data-dir" => data_dir = parse_or_die("--data-dir", args.next()),
			"--sim-radius" => sim_radius = parse_or_die("--sim-radius", args.next()),
			"--workers" => workers = parse_or_die("--workers", args.next()),
			"--master-url" => master_url = Some(parse_or_die("--master-url", args.next())),
			"--advertise-host" => {
				advertise_host = Some(parse_or_die("--advertise-host", args.next()));
			}
			"--advertise-port" => {
				advertise_port = Some(parse_or_die("--advertise-port", args.next()));
			}
			"--server-name" => server_name = parse_or_die("--server-name", args.next()),
			"--max-conns-per-ip" => {
				max_conns_per_ip = parse_or_die("--max-conns-per-ip", args.next());
			}
			"--admin" => admins.push(parse_or_die("--admin", args.next())),
			// Both of these were security holes, not merely renames, so
			// they exit rather than mapping onto the replacement — an
			// operator who quietly kept using them would be relying on the
			// behaviour that was removed.
			"--admin-name" => {
				eprintln!(
					"--admin-name is gone: admin was a display-name compare, and display names \
					 are not an authorization input. Use --admin <account-id> (the player can \
					 copy theirs from the game's Account screen) or --admin <name> to resolve \
					 one at boot."
				);
				std::process::exit(2);
			}
			"--probe-host" => {
				eprintln!(
					"--probe-host is gone: the master no longer takes a probe address from the \
					 registrant, because that meant clients could be aimed at an address it had \
					 never verified. The dev NAT-hairpin case is Master:Dev:ProbeHostOverride on \
					 the master itself."
				);
				std::process::exit(2);
			}
			other => {
				eprintln!("unknown arg: {other}");
				eprintln!("{USAGE}");
				std::process::exit(2);
			}
		}
	}
	let master_url = master_url.unwrap_or_else(|| DEFAULT_MASTER_URL.into());
	// Empty counts as unset, like DIGBLOCK_SEED: compose files pass
	// blanks through.
	let advertise_host = advertise_host.filter(|h| !h.is_empty());
	let master_url = master_url.trim_end_matches('/').to_string();
	let key_path = data_dir.join(master::KEY_FILE);
	let master_cfg = MasterConfig {
		url: master_url.clone(),
		server_name,
		advertise_host,
		advertise_port: advertise_port.unwrap_or(port),
		server_key: master::load_key(&key_path),
	};

	let mut sim = match Sim::new(seed, &data_dir, sim_radius, workers) {
		Ok(sim) => sim,
		Err(e) => {
			eprintln!("failed to open world: {e}");
			std::process::exit(1);
		}
	};
	// Registered here rather than in Sim::new so the test harness's
	// hand-dressed worlds stay free of production rules; the rule itself
	// still gets its own end-to-end test against a real Sim.
	for job in digblock_core::worldjob::grass::jobs() {
		sim.register_world_job(job);
	}
	for job in digblock_core::worldjob::leaves::jobs() {
		sim.register_world_job(job);
	}
	for job in digblock_core::worldjob::wisplight::jobs() {
		sim.register_world_job(job);
	}
	// Resolve every admin to an account id before the first player can
	// connect. A name is looked up ONCE, against the master, and only the
	// resulting id is kept — so renaming does not drop admin, and a name
	// claimed later by somebody else never inherits it.
	//
	// An unresolvable admin is fatal, the same discipline the old
	// --admin-name shape check had: a server that boots with admin silently
	// granted to nobody is worse than one that refuses to boot.
	let admin_accounts = match resolve_admins(&admins, &master_url) {
		Ok(ids) => ids,
		Err(e) => {
			eprintln!("{e}");
			std::process::exit(2);
		}
	};
	for (spec, id) in admins.iter().zip(&admin_accounts) {
		log::info!("admin: {id} ({spec})");
	}
	sim.set_admin_accounts(admin_accounts);
	log::info!("world seed {seed}, data dir {}", data_dir.display());

	let (event_tx, event_rx) = unbounded();
	let running = Arc::new(AtomicBool::new(true));
	let players = Arc::new(AtomicUsize::new(0));
	let sim_thread = {
		let running = running.clone();
		let players = players.clone();
		std::thread::Builder::new()
			.name("sim".into())
			.spawn(move || run_sim_loop(sim, event_rx, running, players))
			.expect("spawn sim thread")
	};

	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.expect("tokio runtime");
	rt.block_on(async {
		let (endpoint, cert_hash) = match net::make_endpoint(port) {
			Ok(e) => e,
			Err(e) => {
				eprintln!("failed to bind udp {port}: {e}");
				std::process::exit(1);
			}
		};
		let endpoint = Arc::new(endpoint);
		log::info!("listening on udp {port}");
		let master_state = Arc::new(MasterState::new(master_cfg.url.clone()));
		let cert_hash = Arc::new(std::sync::RwLock::new(cert_hash));
		let reregister = Arc::new(tokio::sync::Notify::new());
		// Registration must not delay serving — the master's probe-back
		// needs the endpoint answering.
		tokio::spawn(master::run(
			master_state.clone(),
			master_cfg,
			cert_hash.clone(),
			reregister.clone(),
			players,
			Some(key_path),
		));
		// The pinned cert is short-lived; rotate it in-process so a
		// long-running server never becomes unjoinable.
		tokio::spawn(net::rotate_certs(endpoint.clone(), cert_hash, reregister));
		tokio::select! {
			_ = net::serve(endpoint, event_tx, master_state.clone(), max_conns_per_ip) => {}
			_ = shutdown_signal() => {}
		}
		if let Some(reg) = master_state.registration() {
			let url = master_state.url.clone();
			let _ = tokio::time::timeout(
				Duration::from_secs(2),
				tokio::task::spawn_blocking(move || master::notify_shutdown(&url, &reg)),
			)
			.await;
		}
	});

	running.store(false, Ordering::Relaxed);
	let _ = sim_thread.join();
}

/// Ctrl-c or, on unix, SIGTERM (what `docker stop` sends) — both exit
/// gracefully: master notified, journal synced.
async fn shutdown_signal() {
	#[cfg(unix)]
	{
		let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
			.expect("install SIGTERM handler");
		tokio::select! {
			_ = tokio::signal::ctrl_c() => log::info!("ctrl-c: shutting down"),
			_ = term.recv() => log::info!("SIGTERM: shutting down"),
		}
	}
	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
		log::info!("ctrl-c: shutting down");
	}
}
