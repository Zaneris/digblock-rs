//! DigBlock dedicated server binary.

use crossbeam_channel::unbounded;
use digblock_net::protocol::AccountId;
use digblock_server::master::{self, MasterConfig, MasterState};
use digblock_server::sim::Sim;
use digblock_server::{DEFAULT_PORT, DEFAULT_SEED, PORT_RANGE, net, run_sim_loop};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// A yes/no setting, spelled the way a compose file or a shell writes one;
/// anything else is a misconfiguration and exits like a bad number does.
fn parse_bool_or_die(what: &str, v: &str) -> bool {
	match v.trim().to_ascii_lowercase().as_str() {
		"true" | "1" | "yes" | "on" => true,
		"false" | "0" | "no" | "off" => false,
		other => {
			eprintln!("{what}: expected true or false, got {other:?}");
			std::process::exit(2);
		}
	}
}

/// Turn every `--admin` argument into an account id, in order.
///
/// An id passes straight through; a display name costs one lookup against the master (`None`:
/// none, so ids only). Resolving here rather than at each chat command makes admin a decision
/// about an ACCOUNT: the name is asked once of the only party that can answer, then just a label.
fn resolve_admins(specs: &[String], master_url: Option<&str>) -> Result<Vec<AccountId>, String> {
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
			let Some(master_url) = master_url else {
				return Err(format!(
					"--admin {spec:?}: a name needs a master to resolve it, and this server runs \
					 with --no-master. Pass the player's account id (Account -> Copy ID)."
				));
			};
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
	 [--max-conns-per-ip N] [--use-master-levels BOOL] [--archive-world] [--adopt-terrain] \
	 [--no-master] [--move-save FROM-ID TO-ID]\n\
	 (--advertise-host defaults to the address the master observes registering — your public \
	 IP; set it for a DNS name or multi-homed hosts. --advertise-port defaults to --port; set \
	 it when players reach you at a different one (a port forward, a container mapping) — \
	 the master lists only a fixed block of ports, printed if yours is outside it. \
	 --master-url defaults to \
	 https://master.digblock.net. --admin grants a player chat commands (/help /ban \
	 /ban-list /unban /tp) — give an account id, which a player can copy from the game's \
	 Account screen, or a display name, which is resolved to one at boot; repeat for \
	 several admins. --max-conns-per-ip caps live connections from one address; \
	 raise it for a local soak, where every bot shares one. --use-master-levels (default \
	 true) is whether a joiner arrives at the levels the master's board holds for their \
	 account; false keeps this server's skill ladder its own. Replacing a world takes one of \
	 two flags, never one for both — each moves the world aside (.vN.bak, byte-intact, with \
	 its spawn and waystone pads) and starts fresh. --archive-world is the RECOVERY one: a \
	 journal this build cannot read, which boot refuses rather than replacing on its own. \
	 --adopt-terrain is the TRADE: a world stays on the generator that made it, and this \
	 gives up that world for the current generator. Both are inert until their own reason \
	 applies, so neither is a standing hazard — but they are separate because a flag left \
	 set for one must not fire for the other. --no-master takes the master out of the loop: \
	 nothing registers, and a player's Hello token is their own device key for this server, \
	 the account id derived from it; --admin then takes ids only. A certificate in \
	 <data-dir>/tls (fullchain.pem + privkey.pem, e.g. copied in by certbot) is served instead \
	 of a self-signed one and reloaded when it changes, which only --no-master allows: \
	 browsers can pin nothing but a self-signed cert. --move-save moves one player's save \
	 onto another account id (the old character wins, anything the new one holds is added) \
	 and exits; run it with the server STOPPED, or its next save puts the old file back. \
	 Env fallbacks: \
	 DIGBLOCK_NO_MASTER, DIGBLOCK_MASTER_URL, DIGBLOCK_ADVERTISE_HOST, DIGBLOCK_ADVERTISE_PORT, \
	 DIGBLOCK_SERVER_NAME, DIGBLOCK_SEED, DIGBLOCK_MAX_CONNS_PER_IP, DIGBLOCK_ADMIN_IDS \
	 (comma-separated), DIGBLOCK_USE_MASTER_LEVELS, DIGBLOCK_ARCHIVE_WORLD, \
	 DIGBLOCK_ADOPT_TERRAIN)";

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
	let mut no_master = std::env::var("DIGBLOCK_NO_MASTER")
		.ok()
		.filter(|s| !s.trim().is_empty())
		.map(|v| parse_bool_or_die("DIGBLOCK_NO_MASTER", &v))
		.unwrap_or(false);
	let mut use_master_levels = std::env::var("DIGBLOCK_USE_MASTER_LEVELS")
		.ok()
		.filter(|s| !s.trim().is_empty())
		.map(|v| parse_bool_or_die("DIGBLOCK_USE_MASTER_LEVELS", &v))
		.unwrap_or(true);
	let mut archive = digblock_server::journal::Archive {
		stale_format: std::env::var("DIGBLOCK_ARCHIVE_WORLD")
			.ok()
			.filter(|s| !s.trim().is_empty())
			.map(|v| parse_bool_or_die("DIGBLOCK_ARCHIVE_WORLD", &v))
			.unwrap_or(false),
		foreign_terrain: std::env::var("DIGBLOCK_ADOPT_TERRAIN")
			.ok()
			.filter(|s| !s.trim().is_empty())
			.map(|v| parse_bool_or_die("DIGBLOCK_ADOPT_TERRAIN", &v))
			.unwrap_or(false),
	};
	// Account ids or display names; names are resolved once at boot.
	let mut admins: Vec<String> = std::env::var("DIGBLOCK_ADMIN_IDS")
		.unwrap_or_default()
		.split(',')
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
		.collect();

	let mut move_save: Option<(String, String)> = None;
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
			"--use-master-levels" => {
				let v = args.next().unwrap_or_default();
				use_master_levels = parse_bool_or_die("--use-master-levels", &v);
			}
			"--max-conns-per-ip" => {
				max_conns_per_ip = parse_or_die("--max-conns-per-ip", args.next());
			}
			"--admin" => admins.push(parse_or_die("--admin", args.next())),
			"--archive-world" => archive.stale_format = true,
			"--no-master" => no_master = true,
			"--move-save" => {
				let from: String = parse_or_die("--move-save", args.next());
				move_save = Some((from, parse_or_die("--move-save", args.next())));
			}
			"--adopt-terrain" => archive.foreign_terrain = true,
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
	// Maintenance, not a boot: done before anything opens the world.
	if let Some((from, to)) = move_save {
		let id = |s: &str| {
			AccountId::parse(s)
				.filter(|a| !a.is_none())
				.unwrap_or_else(|| {
					eprintln!("--move-save: {s:?} is not an account id");
					std::process::exit(2);
				})
		};
		let store = digblock_server::player_store::PlayerStore::move_save;
		match store(&data_dir, id(&from), id(&to)) {
			Ok(true) => log::info!("moved the save of {from} onto {to}"),
			Ok(false) => {
				eprintln!("--move-save: nothing is saved under {from}");
				std::process::exit(1);
			}
			Err(e) => {
				eprintln!("--move-save failed: {e}");
				std::process::exit(1);
			}
		}
		return;
	}
	let master_url = master_url.unwrap_or_else(|| DEFAULT_MASTER_URL.into());
	// Empty counts as unset, like DIGBLOCK_SEED: compose files pass
	// blanks through.
	let advertise_host = advertise_host.filter(|h| !h.is_empty());
	// The port the master is asked to list, checked against the block it
	// lists BEFORE anything binds or registers: the refusal the master
	// would send is a 400 after a round trip, and this one names the fix.
	let advertise_port = advertise_port.unwrap_or(port);
	if !no_master && !PORT_RANGE.contains(&advertise_port) {
		eprintln!(
			"advertised port {advertise_port} is outside {}-{}, the block the master lists \
			 servers at. Pick a port in that block for --port, or for --advertise-port \
			 (DIGBLOCK_ADVERTISE_PORT) when players reach a different port than the one \
			 this process binds.",
			PORT_RANGE.start(),
			PORT_RANGE.end()
		);
		std::process::exit(2);
	}
	let master_url = master_url.trim_end_matches('/').to_string();
	let key_path = data_dir.join(master::KEY_FILE);
	let master_cfg = MasterConfig {
		url: master_url.clone(),
		server_name,
		advertise_host,
		advertise_port,
		server_key: master::load_key(&key_path),
	};

	// Losing a world is the operator's decision, never a side effect: boot
	// refuses a stale record format, and a worldgen release keeps a world
	// on the generator that made it. These flags ARE the decision — one
	// each, because a lever set to recover an unreadable world must not
	// still be armed at the next terrain release — and each is inert until
	// its own reason applies, so leaving one set costs nothing per restart.
	if archive != digblock_server::journal::Archive::default() {
		match digblock_server::journal::archive_world(&data_dir, seed, archive) {
			// The pads go with the world or the fresh one wears the old
			// one's spawn: `archive_arrival_pads` says why.
			Ok(true) => match digblock_server::archive_arrival_pads(&data_dir) {
				Ok(()) => log::warn!("world archived; starting fresh under the current generator"),
				Err(e) => {
					eprintln!("archiving the arrival pads failed: {e}");
					std::process::exit(1);
				}
			},
			Ok(false) => log::info!("nothing to archive: the world is already current"),
			Err(e) => {
				eprintln!("archiving the world failed: {e}");
				std::process::exit(1);
			}
		}
	}
	// Force every .shape asset through parse and bake NOW: a malformed
	// definition dies at boot, never mid-session under a player's feet.
	digblock_core::shapes::prewarm();
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
	for job in digblock_core::worldjob::production() {
		sim.register_world_job(job);
	}
	// Resolve every admin to an account id before the first player can connect. A name is looked
	// up ONCE, against the master, and only the resulting id is kept — so renaming does not drop
	// admin, and a name claimed later by somebody else never inherits it. An unresolvable admin is
	// fatal: a server that boots with admin silently granted to nobody is worse than one that
	// refuses to boot.
	let admin_accounts = match resolve_admins(&admins, (!no_master).then_some(&*master_url)) {
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
	// Master-era saves, reclaimable by the session a returning client still holds. Malformed is
	// fatal: a line skipped is a player whose save never comes back.
	match digblock_server::master_claims::load(&data_dir) {
		Ok(claims) if claims.is_empty() => {}
		Ok(claims) => {
			log::info!(
				"{} master-era session(s) may reclaim their saves",
				claims.len()
			);
			sim.set_master_claims(claims);
		}
		Err(e) => {
			eprintln!(
				"failed to read {}: {e}",
				digblock_server::master_claims::FILE
			);
			std::process::exit(1);
		}
	}
	sim.set_master_levels(use_master_levels && !no_master);
	if !use_master_levels && !no_master {
		log::info!("skills are local to this server; the master's copy is not pulled at join");
	}
	// The board's outbox: every stored record now, every change of
	// skills from here on. Whether any of it is sent is the master's call
	// at registration, so without a master there is no outbox at all.
	let outbox = Arc::new(digblock_server::hiscores::SkillOutbox::new());
	if !no_master {
		sim.attach_skill_outbox(outbox.clone());
	}
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
		let tls_dir = data_dir.join(net::TLS_DIR);
		let tls = match net::load_tls(&tls_dir).await {
			Ok(tls) => tls,
			Err(e) => {
				eprintln!(
					"failed to load the certificate in {}: {e}",
					tls_dir.display()
				);
				std::process::exit(1);
			}
		};
		// The master can only hand out a certificate HASH, and browsers pin
		// a hash only for a cert that lives two weeks at most, so a
		// CA-issued one under a master is a server no browser can join.
		if tls.is_some() && !no_master {
			eprintln!(
				"{} holds a certificate, which only a --no-master server can serve: browsers \
				 refuse the master's pin for a cert valid longer than 14 days. Remove it, or run \
				 with --no-master.",
				tls_dir.display()
			);
			std::process::exit(2);
		}
		let using_tls = tls.is_some();
		let bound = match tls {
			Some((identity, hash)) => net::make_endpoint_with(port, identity).map(|e| (e, hash)),
			None => net::make_endpoint(port),
		};
		let (endpoint, cert_hash) = match bound {
			Ok(e) => e,
			Err(e) => {
				eprintln!("failed to bind udp {port}: {e}");
				std::process::exit(1);
			}
		};
		let endpoint = Arc::new(endpoint);
		log::info!("listening on udp {port}");
		if no_master {
			if using_tls {
				log::info!("serving the certificate in {}", tls_dir.display());
				tokio::spawn(net::watch_tls(endpoint.clone(), tls_dir));
			} else {
				// Still rotated, so a long-running server stays joinable by
				// a client pinning the logged hash; nobody re-registers it.
				let cert_hash = Arc::new(std::sync::RwLock::new(cert_hash));
				let nobody = Arc::new(tokio::sync::Notify::new());
				tokio::spawn(net::rotate_certs(endpoint.clone(), cert_hash, nobody));
			}
			// The boot watch (`just fleet apply`) reads this line as live.
			log::info!("accepting players without a master");
			tokio::select! {
				_ = net::serve(endpoint, event_tx, net::Admission::Direct, max_conns_per_ip) => {}
				_ = shutdown_signal() => {}
			}
			return;
		}
		let master_state = Arc::new(MasterState::new(master_cfg.url.clone(), event_tx.clone()));
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
			outbox,
		));
		// The pinned cert is short-lived; rotate it in-process so a
		// long-running server never becomes unjoinable.
		tokio::spawn(net::rotate_certs(endpoint.clone(), cert_hash, reregister));
		tokio::select! {
			_ = net::serve(
				endpoint,
				event_tx,
				net::Admission::Master(master_state.clone()),
				max_conns_per_ip,
			) => {}
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
