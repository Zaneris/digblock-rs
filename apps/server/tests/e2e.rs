//! End-to-end over real sockets AND the real .NET master — run exactly as
//! it deploys: each test boots the apps/master Docker image (alpine +
//! libmsquic, QUIC probe ON), which probes the host-side game server for
//! real over `host.docker.internal` before listing it. Clients join the
//! only way that exists: mint a one-time token from the master, present it
//! in Hello, server verifies it back with the master. There is no
//! tokenless path. Docker must be running for these tests.
//!
//! One test does NOT use that harness. Running the game server on the host
//! forces `Master:Dev:ProbeHostOverride`, which aims BOTH of registration's
//! probes at one host and so collapses the source probe into the advertised
//! one — leaving everything that turns on the source probe specifically
//! untested. [`ComposeRig`] brings up `docker/compose.yml` instead, putting
//! both sides in containers on one network, which is the only topology here
//! where the master dials the registrant's real address.

use crossbeam_channel::unbounded;
use digblock_core::block::BlockType;
use digblock_core::player::Player;
use digblock_core::raycast::{REACH, raycast};
use digblock_net::client::NetClientConfig;
use digblock_net::messages::EditKind;
use digblock_net::{NetClient, NetEvent};
use digblock_server::master::{self, MasterConfig, MasterState};
use digblock_server::sim::Sim;
use digblock_server::{net, run_sim_loop};
use glam::Vec3;
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ---- containerized master harness ------------------------------------------

const MASTER_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../master");
const MASTER_IMAGE: &str = "digblock-master-e2e";
/// How the containerized master reaches game servers the tests run on the
/// host (Docker Desktop resolves it to the host; on a bare Linux engine
/// run docker with `--add-host host.docker.internal:host-gateway`).
const HOST_FROM_CONTAINER: &str = "host.docker.internal";

struct MasterProc {
	name: String,
	url: String,
}

impl MasterProc {
	/// Build the apps/master image once per test binary, then run one
	/// container per test with the real QUIC probe on (short timeout so
	/// probe-failure tests stay fast).
	fn spawn() -> Self {
		static BUILD: std::sync::Once = std::sync::Once::new();
		static NEXT: AtomicU32 = AtomicU32::new(0);
		BUILD.call_once(|| {
			let docker = std::process::Command::new("docker")
				.args(["version", "--format", "{{.Server.Version}}"])
				.output();
			assert!(
				docker.is_ok_and(|o| o.status.success()),
				"Docker is required for the server e2e tests: the master runs as its \
				 deployment container (alpine + libmsquic) with real QUIC probing"
			);
			let status = std::process::Command::new("docker")
				.args(["build", "-q", "-t", MASTER_IMAGE, MASTER_DIR])
				.stdout(std::process::Stdio::null())
				.status()
				.expect("run docker build");
			assert!(status.success(), "docker build apps/master failed");
		});
		let port = free_tcp_port();
		let name = format!(
			"dbe2e-master-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, Ordering::Relaxed)
		);
		let url = format!("http://127.0.0.1:{port}");
		let status = std::process::Command::new("docker")
			.args([
				"run",
				"-d",
				"--rm",
				"--name",
				&name,
				"-p",
				&format!("127.0.0.1:{port}:5200"),
				"-e",
				"Master__Probe__TimeoutSeconds=3",
				// Everything in this rig is on a private address, and the
				// SSRF filter refuses those by default. This is the same
				// switch a master running on its own LAN would use.
				"-e",
				"Master__Probe__AllowPrivateTargets=true",
				// The suite cannot open a browser, so accounts come from
				// the development sign-in — which requires Development.
				"-e",
				"ASPNETCORE_ENVIRONMENT=Development",
				"-e",
				"Master__Dev__AuthEnabled=true",
				// Replaces the registrant-supplied probeHost: the master
				// dials the docker host itself rather than believing an
				// address it was handed (NET-AUDIT-2.md §1.3).
				"-e",
				&format!("Master__Dev__ProbeHostOverride={HOST_FROM_CONTAINER}"),
				MASTER_IMAGE,
			])
			.stdout(std::process::Stdio::null())
			.status()
			.expect("docker run master");
		assert!(status.success(), "docker run {MASTER_IMAGE} failed");
		let ready_by = Instant::now() + Duration::from_secs(30);
		loop {
			if ureq::get(format!("{url}/health")).call().is_ok() {
				break;
			}
			assert!(
				Instant::now() < ready_by,
				"master container did not become ready"
			);
			std::thread::sleep(Duration::from_millis(50));
		}
		Self { name, url }
	}

	fn kill(&mut self) {
		let _ = std::process::Command::new("docker")
			.args(["rm", "-f", &self.name])
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status();
	}
}

impl Drop for MasterProc {
	fn drop(&mut self) {
		self.kill();
	}
}

fn free_tcp_port() -> u16 {
	std::net::TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port()
}

/// Sign in as `subject` through the master's development endpoint, which
/// is how anything without a browser gets an account. The same subject
/// always lands on the same account, so a test can rejoin as "itself".
///
/// Returns (session token, account id, display name). The display name is
/// the subject when that is a legal one — which is what the callers rely on
/// to assert the roster.
fn dev_sign_in(master_url: &str, subject: &str) -> (String, String, Option<String>) {
	let mut res = ureq::post(format!("{master_url}/api/dev/signin"))
		.send_json(serde_json::json!({ "subject": subject }))
		.unwrap();
	assert_eq!(res.status().as_u16(), 200, "dev sign-in refused");
	let body: serde_json::Value = res.body_mut().read_json().unwrap();
	(
		body["sessionToken"].as_str().unwrap().to_string(),
		body["accountId"].as_str().unwrap().to_string(),
		body["displayName"].as_str().map(str::to_string),
	)
}

/// Mint a one-time join token for the first listed server: exactly what a
/// real client does, minus the browser. Returns (token, game server url).
fn mint_token(master_url: &str, display_name: &str) -> ([u8; 16], String) {
	let (session, _, _) = dev_sign_in(master_url, display_name);
	mint_token_with(master_url, &session)
}

fn mint_token_with(master_url: &str, session: &str) -> ([u8; 16], String) {
	let mut res = ureq::get(format!("{master_url}/api/servers"))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	let server_id = list["servers"][0]["id"]
		.as_str()
		.expect("a registered server in the list");
	// No displayName: the master reads it off the authenticated account.
	let mut res = ureq::post(format!("{master_url}/api/join"))
		.header("authorization", &format!("Bearer {session}"))
		.send_json(serde_json::json!({ "serverId": server_id }))
		.unwrap();
	assert_eq!(res.status().as_u16(), 200, "join refused");
	let join: serde_json::Value = res.body_mut().read_json().unwrap();
	let token = parse_hex16(join["token"].as_str().unwrap());
	(token, join["url"].as_str().unwrap().to_string())
}

fn parse_hex16(hex: &str) -> [u8; 16] {
	let bytes: Vec<u8> = (0..hex.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
		.collect();
	bytes.try_into().expect("32 hex chars")
}

// ---- full game server harness ---------------------------------------------

struct TestServer {
	// Declaration order is drop order: runtime first (cancels serve), then
	// the sim thread is joined via Drop.
	rt: tokio::runtime::Runtime,
	running: Arc<AtomicBool>,
	sim_thread: Option<std::thread::JoinHandle<()>>,
	data_dir: PathBuf,
	url: String,
	port: u16,
	cert_hash: [u8; 32],
	/// Shared with the accept loop — lets tests rotate the cert live.
	endpoint: Arc<wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>>,
	/// Lets tests install a fresh registration (the rotation path).
	master_state: Arc<MasterState>,
}

impl TestServer {
	/// Boot endpoint + sim on an ephemeral UDP port. `registered` mirrors
	/// production: register with the master, whose REAL QUIC probe dials
	/// back through `host.docker.internal` — so serve() must already be
	/// accepting before registration. `false` leaves the server in its
	/// pre-registration state, where every join must be refused.
	fn start(tag: &str, master_url: &str, registered: bool) -> Self {
		let rt = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.unwrap();
		let _guard = rt.enter();
		let (endpoint, cert_hash) = net::make_endpoint(0).expect("bind ephemeral udp");
		let endpoint = Arc::new(endpoint);
		let port = endpoint.local_addr().unwrap().port();
		let data_dir = std::env::temp_dir().join(format!("dbe2e-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&data_dir);
		// Seed 7 spawns on GRASS, so the dig flows below run unskilled;
		// the default seed's spawn disc is bare Stone, which SIM v12's
		// rock gate refuses (a real design consequence — a stone-spawn
		// player levels on leaves/soil off the disc first).
		let sim = Sim::new(7.0, &data_dir, 2, 2).unwrap();
		let (event_tx, event_rx) = unbounded();
		let running = Arc::new(AtomicBool::new(true));
		let players = Arc::new(AtomicUsize::new(0));
		let sim_thread = {
			let running = running.clone();
			std::thread::spawn(move || run_sim_loop(sim, event_rx, running, players))
		};
		let state = Arc::new(MasterState::new(master_url.to_string()));
		// Every client in this suite connects from loopback, so the
		// internet-facing per-address default would cap the fixture rather
		// than anything under test.
		rt.spawn(net::serve(
			endpoint.clone(),
			event_tx,
			state.clone(),
			usize::MAX,
		));
		let server = Self {
			rt,
			running,
			sim_thread: Some(sim_thread),
			data_dir,
			url: format!("https://127.0.0.1:{port}"),
			port,
			cert_hash,
			endpoint,
			master_state: state.clone(),
		};
		if registered {
			let cfg = server.master_config(master_url, &format!("e2e-{tag}"));
			let reg = match master::register(&cfg, &server.cert_hash) {
				Ok(reg) => reg,
				Err(
					master::RegisterError::Unreachable(e)
					| master::RegisterError::Transport(e)
					| master::RegisterError::Rejected(e),
				) => panic!("register with test master (probe failed?): {e}"),
			};
			state.set_registration(Some(reg));
		}
		server
	}

	/// Advertise 127.0.0.1 (what host-side clients dial) while the
	/// containerized master probes via host.docker.internal — the same
	/// probeHost split `just play` uses.
	fn master_config(&self, master_url: &str, name: &str) -> MasterConfig {
		MasterConfig {
			url: master_url.to_string(),
			server_name: name.to_string(),
			advertise_host: Some("127.0.0.1".into()),
			server_key: None,
			advertise_port: self.port,
		}
	}
}

impl Drop for TestServer {
	fn drop(&mut self) {
		self.running.store(false, Ordering::Relaxed);
		if let Some(t) = self.sim_thread.take() {
			let _ = t.join();
		}
		let _ = std::fs::remove_dir_all(&self.data_dir);
	}
}

// ---- client harness --------------------------------------------------------

struct TestClient {
	client: NetClient,
	events: Vec<NetEvent>,
	seed: f64,
	spawn: Vec3,
	welcomed: bool,
	edits: FxHashMap<(i32, i32, i32), BlockType>,
	acks: u32,
	joined_names: Vec<String>,
	/// Authoritative inventory snapshots received (join sends one right
	/// after JournalDone — the reliable-ordering proof over real QUIC).
	inventories: Vec<Vec<(u16, u32)>>,
	/// (from, text) chat lines, sender echoes included.
	chats: Vec<(u8, String)>,
}

impl TestClient {
	fn connect(url: &str, name: &str, token: [u8; 16]) -> Self {
		let transport = digblock_net_native::connect(url);
		Self {
			client: NetClient::new(
				Box::new(transport),
				NetClientConfig {
					name: name.into(),
					draw_dist: 4,
					token,
				},
			),
			events: Vec::new(),
			seed: 0.0,
			spawn: Vec3::ZERO,
			welcomed: false,
			edits: FxHashMap::default(),
			acks: 0,
			joined_names: Vec::new(),
			inventories: Vec::new(),
			chats: Vec::new(),
		}
	}

	/// Connect and pump until the handshake resolves one way or the other.
	/// `Err` carries the server's refusal reason — for the tests that
	/// EXPECT one, where `pump`'s panic would be the wrong shape.
	fn try_connect(url: &str, name: &str, token: [u8; 16]) -> Result<Self, String> {
		let mut c = Self::connect(url, name, token);
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			c.events.clear();
			c.client.poll(&mut c.events, 0.005);
			for ev in c.events.drain(..) {
				match ev {
					NetEvent::Welcome { seed, spawn, .. } => {
						c.seed = seed;
						c.spawn = spawn;
						c.welcomed = true;
					}
					NetEvent::Refused { reason } | NetEvent::Disconnected { reason } => {
						return Err(reason);
					}
					_ => {}
				}
			}
			if c.welcomed {
				return Ok(c);
			}
			assert!(Instant::now() < deadline, "handshake never resolved");
			std::thread::sleep(Duration::from_millis(10));
		}
	}

	fn pump(&mut self) {
		self.events.clear();
		self.client.poll(&mut self.events, 0.005);
		for ev in self.events.drain(..) {
			match ev {
				NetEvent::Welcome { seed, spawn, .. } => {
					self.seed = seed;
					self.spawn = spawn;
					self.welcomed = true;
				}
				NetEvent::Edit {
					cell, cell_value, ..
				} => {
					self.edits.insert(
						cell,
						BlockType::from_id(digblock_core::chunk::cell_id(cell_value)),
					);
				}
				NetEvent::Ack { .. } => self.acks += 1,
				NetEvent::Refused { reason } => panic!("refused: {reason}"),
				NetEvent::Disconnected { reason } => panic!("disconnected: {reason}"),
				NetEvent::DigRefused { cell, reason } => {
					panic!("dig refused at {cell:?} ({reason:?})")
				}
				NetEvent::EditRejected { edit, reason } => {
					panic!("edit rejected: {edit:?} ({reason:?})")
				}
				NetEvent::PlayerJoined { name, .. } => self.joined_names.push(name),
				NetEvent::PlayerLeft { .. } => {}
				NetEvent::Inventory { counts, .. } => self.inventories.push(counts),
				NetEvent::Chat { from, text } => self.chats.push((from, text)),
				// No e2e case requests a teleport; unexpected = loud.
				NetEvent::Teleported { pos, .. } => panic!("unrequested teleport to {pos}"),
				NetEvent::TimeOfDay { .. }
				| NetEvent::AdminCommands { .. }
				| NetEvent::Layout { .. }
				| NetEvent::Skills { .. }
				| NetEvent::XpGain { .. }
				| NetEvent::LevelUp { .. }
				| NetEvent::AreaReady => {}
			}
		}
	}

	/// The real timed-dig flow: aim, DigStart, hold for the block's
	/// hardness (plus margin), then the completing Dig request. Paced at
	/// the real tick rate — the server's aim verification tracks wall clock.
	fn timed_dig(
		&mut self,
		player: &mut Player,
		world: &dyn Fn(i32, i32, i32) -> Option<u16>,
		dir: Vec3,
		hit: &digblock_core::raycast::RayHit,
	) {
		// Prime the aim so the server's look already points at the cell
		// when the hold starts (and any input backlog drains).
		for _ in 0..12 {
			self.client
				.predictor
				.predict(player, Default::default(), dir, world);
			self.client.send_inputs();
			self.pump();
			std::thread::sleep(Duration::from_millis(16));
		}
		self.client.send_dig_start(hit.cell);
		let hold = digblock_core::skills::dig_required(
			hit.block,
			hit.block.hardness_ticks().expect("diggable target"),
			1,
		) + 6;
		for _ in 0..hold {
			self.client
				.predictor
				.predict(player, Default::default(), dir, world);
			self.client.send_inputs();
			self.pump();
			std::thread::sleep(Duration::from_millis(16));
		}
		let seq = self.client.edits.begin(
			hit.cell,
			hit.cell_value,
			BlockType::Air as u16,
			0,
			hit.block.dig_drop().map(|d| (d, 1)),
		);
		self.client.send_edit(
			seq,
			self.client.predictor.current_tick(),
			player.pos,
			dir,
			EditKind::Dig,
			hit.cell,
			(0, 0, 0),
			0,
		);
	}

	fn wait(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
		let deadline = Instant::now() + Duration::from_secs(60);
		while !done(self) {
			assert!(Instant::now() < deadline, "timed out waiting for {what}");
			self.pump();
			std::thread::sleep(Duration::from_millis(5));
		}
	}
}

/// Connect expecting a `Refused` whose reason contains `substr`.
fn expect_refused(url: &str, token: [u8; 16], substr: &str) {
	let mut client = NetClient::new(
		Box::new(digblock_net_native::connect(url)),
		NetClientConfig {
			name: "x".into(),
			draw_dist: 4,
			token,
		},
	);
	let deadline = Instant::now() + Duration::from_secs(30);
	let mut events = Vec::new();
	loop {
		assert!(Instant::now() < deadline, "timed out waiting for refusal");
		client.poll(&mut events, 0.005);
		for ev in events.drain(..) {
			match ev {
				NetEvent::Refused { reason } => {
					assert!(
						reason.contains(substr),
						"refused with {reason:?}, wanted substring {substr:?}"
					);
					return;
				}
				NetEvent::Welcome { .. } => panic!("welcomed instead of refused"),
				_ => {}
			}
		}
		std::thread::sleep(Duration::from_millis(5));
	}
}

// ---- tests ------------------------------------------------------------------

#[test]
fn edit_reaches_late_joiner_and_live_peer() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("edits", &master.url, true);

	// Client A joins through the master and waits for its first snapshot.
	let (token_a, url_a) = mint_token(&master.url, "adam");
	assert_eq!(
		url_a, server.url,
		"master lists the advertise host, not the probe host"
	);
	let mut a = TestClient::connect(&server.url, "adam", token_a);
	a.wait("welcome", |c| c.welcomed);
	// InventoryFull follows JournalDone on the reliable stream — its
	// arrival proves the v4 join-sync ordering over real QUIC.
	a.wait("inventory base", |c| !c.inventories.is_empty());
	a.wait("first snapshot", |c| c.acks > 0);

	// A's local world: cached build_column — bit-identical to the
	// server's terrain, so predictions, claims, and raycasts agree.
	type ColumnCache = std::cell::RefCell<
		FxHashMap<(i32, i32), Vec<std::sync::Arc<digblock_core::chunk::ChunkData>>>,
	>;
	let seed = a.seed;
	let cache: ColumnCache = std::cell::RefCell::new(FxHashMap::default());
	let world = move |x: i32, y: i32, z: i32| -> Option<u16> {
		let ((cx, cy, cz), (lx, ly, lz)) = digblock_core::chunk::split_world(x, y, z)?;
		let mut cache = cache.borrow_mut();
		let stack = cache
			.entry((cx, cz))
			.or_insert_with(|| digblock_core::worldgen::build_column(cx, cz, seed));
		Some(stack[cy as usize].get_cell(lx, ly, lz))
	};
	let mut player = Player::new(a.spawn);
	// Predicted idle ticks: inputs flow, and both the server player and
	// this local twin settle onto the same ground fixed point. Real-time
	// paced — racing ahead of the live server builds an input backlog
	// that leaves its look direction stale through the dig's aim checks.
	for _ in 0..60 {
		a.client
			.predictor
			.predict(&mut player, Default::default(), Vec3::Z, &world);
		a.client.send_inputs();
		a.pump();
		std::thread::sleep(Duration::from_millis(16));
	}
	// Dig at an angle so the ground under the player stays intact (a
	// hole underfoot would drop the server player away from our claims).
	let dig_dir = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(player.pos, dig_dir, REACH, &world).expect("ground beside spawn");
	a.timed_dig(&mut player, &world, dig_dir, &hit);
	// A receives its own confirm as an authoritative Edit.
	a.wait("dig confirm", |c| {
		c.edits.get(&hit.cell) == Some(&BlockType::Air)
	});

	// B joins late (own token): the edit must arrive in its join sync.
	let (token_b, _) = mint_token(&master.url, "beth");
	let mut b = TestClient::connect(&server.url, "beth", token_b);
	b.wait("welcome", |c| c.welcomed);
	assert_eq!(b.seed, a.seed, "same world");
	b.wait("join-sync edit", |c| {
		c.edits.get(&hit.cell) == Some(&BlockType::Air)
	});

	// Live path: A digs a second block (mirrored angle, ground intact
	// underfoot); B (already connected) sees the broadcast.
	let dig_dir2 = Vec3::new(-0.45, -1.0, 0.0).normalize();
	let patched = |x: i32, y: i32, z: i32| {
		if (x, y, z) == hit.cell {
			Some(BlockType::Air as u16)
		} else {
			world(x, y, z)
		}
	};
	let hit2 = raycast(player.pos, dig_dir2, REACH, &patched).expect("second target");
	assert_ne!(hit2.cell, hit.cell);
	a.timed_dig(&mut player, &patched, dig_dir2, &hit2);
	b.wait("live edit broadcast", |c| {
		c.edits.get(&hit2.cell) == Some(&BlockType::Air)
	});
}

/// The master's display name is the identity — the name claimed in Hello
/// must lose to the one the token was minted with.
#[test]
fn verified_join_uses_master_display_name() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("names", &master.url, true);

	let (token_a, _) = mint_token(&master.url, "Real_Ada");
	let mut a = TestClient::connect(&server.url, "Impostor", token_a);
	a.wait("welcome", |c| c.welcomed);

	// B observes A through the join roster.
	let (token_b, _) = mint_token(&master.url, "beth");
	let mut b = TestClient::connect(&server.url, "beth", token_b);
	b.wait("welcome", |c| c.welcomed);
	b.wait("roster names", |c| !c.joined_names.is_empty());
	assert!(
		b.joined_names.iter().any(|n| n == "Real_Ada"),
		"roster {:?} should carry the master's name, not the claimed one",
		b.joined_names
	);
	assert!(
		!b.joined_names.iter().any(|n| n == "Impostor"),
		"claimed Hello name must not survive"
	);
}

/// Server names are display-only and sanitized, never rejected: the master
/// keeps letters/digits/space/-/_, collapses space runs, and caps the
/// length. Registration must not fail over cosmetics.
///
/// This used to ride on the end of the display-name test, re-registering
/// the same host:port anonymously. That is refused now — see
/// `a_keyless_registration_cannot_take_a_live_address` — so the test owns
/// the registration and presents the key it was given, which is what a real
/// server restart does.
#[test]
fn a_dirty_server_name_is_sanitized_not_rejected() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("dirtyname", &master.url, false);

	let mut cfg = server.master_config(&master.url, "e2e-clean");
	let reg = master::register(&cfg, &server.cert_hash).expect("first registration");

	cfg.server_name = "  ¡Léo's  <World>!  this-runs-way-past-the-cap  ".into();
	cfg.server_key = Some(reg.server_key.clone());
	let again = master::register(&cfg, &server.cert_hash).expect("dirty name still registers");
	assert_eq!(
		again.server_id, reg.server_id,
		"a keyed re-registration keeps the server's id"
	);

	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	let names: Vec<&str> = list["servers"]
		.as_array()
		.unwrap()
		.iter()
		.filter_map(|s| s["name"].as_str())
		.collect();
	assert_eq!(
		names,
		["Los World this-runs-way-"],
		"name sanitized, not rejected"
	);
}

/// The takeover NET-AUDIT-2.md §1.2 describes: read a live server's host,
/// port and certHash out of the public listing, POST them back as your own
/// registration, and inherit its entry. The probe passes — it dials the
/// victim — so the probe was never the thing standing in the way.
///
/// Registration is still anonymous. What changed is that an address
/// remembers the key that claimed it, so a registration without that key
/// is refused while anything is live there, and the source is temp-banned
/// so retrying in a loop is pointless.
#[test]
fn a_keyless_registration_cannot_take_a_live_address() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("takeover", &master.url, false);

	let cfg = server.master_config(&master.url, "e2e-incumbent");
	let reg = master::register(&cfg, &server.cert_hash).expect("first registration");

	// Everything the attacker needs is in the public listing.
	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	let entry = &list["servers"][0];
	let cert_hash = entry["certHash"].as_str().unwrap().to_string();

	let agent = ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.http_status_as_error(false)
			.build(),
	);
	let attempt = |name: &str| {
		agent
			.post(format!("{}/api/servers/register", master.url))
			.send_json(serde_json::json!({
				"name": name,
				"host": cfg.advertise_host,
				"port": cfg.advertise_port,
				"certHash": cert_hash,
				"players": 0,
				"maxPlayers": 8,
				"protocolVersion": digblock_net::protocol::PROTOCOL_VERSION,
				"simVersion": digblock_core::SIM_VERSION,
			}))
			.expect("register call")
			.status()
			.as_u16()
	};
	assert_eq!(
		attempt("e2e-attacker"),
		409,
		"keyless takeover must be refused"
	);
	// And the source is now banned, so the loop that made this worth doing
	// gets a different answer.
	assert_eq!(attempt("e2e-attacker"), 429, "the source is temp-banned");

	// The incumbent is untouched: same id, same name, still listed.
	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	assert_eq!(list["servers"].as_array().map(|s| s.len()), Some(1));
	assert_eq!(
		list["servers"][0]["id"].as_str(),
		Some(reg.server_id.as_str())
	);
	assert_eq!(list["servers"][0]["name"].as_str(), Some("e2e-incumbent"));
}

/// The listing carries the versions a server registered with, so a client
/// can tell an incompatible one apart before spending a QUIC connect on it.
/// They were accepted and read by nobody (NET-AUDIT-2.md §2.5).
#[test]
fn the_listing_carries_protocol_and_sim_versions() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("versions", &master.url, true);
	let _ = &server;

	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	let entry = &list["servers"][0];
	assert_eq!(
		entry["protocolVersion"].as_u64(),
		Some(digblock_net::protocol::PROTOCOL_VERSION as u64)
	);
	assert_eq!(
		entry["simVersion"].as_u64(),
		Some(digblock_core::SIM_VERSION as u64)
	);
}

/// Leaving properly frees the account IMMEDIATELY, so the same player can
/// come straight back.
///
/// This is what `ClientMsg::Bye` is for. Without it a closed client is only
/// reaped on QUIC's idle timeout, and since one account may hold one live
/// connection, the session being held hostage belongs to the very player
/// trying to rejoin — quit and relaunch, and the game refuses you with your
/// own connection.
///
/// The goodbye is only half of it: `write_all` merely buffers into the QUIC
/// stream, so `NetClient::disconnect` pairs the message with a bounded
/// flush. A test that sent `Bye` and dropped the transport would pass or
/// fail on timing.
#[test]
fn a_goodbye_frees_the_account_at_once() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("goodbye", &master.url, true);

	let (session, _, _) = dev_sign_in(&master.url, "Ada_L");
	let (token, _) = mint_token_with(&master.url, &session);
	let mut a = TestClient::connect(&server.url, "whatever", token);
	a.wait("welcome", |c| c.welcomed);

	// Hang up the way every deliberate exit does.
	a.client.disconnect();
	drop(a);

	// Straight back in. No sleep, no retry loop: if this needs either,
	// the goodbye did not arrive and the idle timeout is doing the work.
	let (token, _) = mint_token_with(&master.url, &session);
	let mut again = TestClient::try_connect(&server.url, "whatever", token)
		.expect("the account should be free the moment we said goodbye");
	again.wait("welcome on rejoin", |c| c.welcomed);
}

/// The development sign-in derives an account id from the subject string,
/// and that derivation is a golden constant.
///
/// `just play` hardcodes the id for "admin" as `DEV_ADMIN_ID` and passes it
/// to the game server as `--admin` before anybody has signed in — which is
/// only possible because the id is a pure function of the subject. Change
/// the derivation and `just play` silently grants admin to nobody, which is
/// exactly the failure mode the boot-time resolution exists to prevent
/// everywhere else.
#[test]
fn dev_account_ids_are_derived_from_the_subject() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();

	let (_, admin, name) = dev_sign_in(&master.url, "admin");
	assert_eq!(
		admin, "b2379408-3102-8b68-b56b-5774dec269b8",
		"DEV_ADMIN_ID in the justfile is this value — update both together"
	);
	assert_eq!(name.as_deref(), Some("admin"));

	// Same subject, same account, every time.
	let (_, again, _) = dev_sign_in(&master.url, "admin");
	assert_eq!(again, admin);
	let (_, other, _) = dev_sign_in(&master.url, "bot-0");
	assert_ne!(other, admin);
}

/// The server keys a player on the ACCOUNT the master vouches for, not on
/// the connection or on anything the client said about itself.
///
/// Observed two ways, both race-free. A second connection for the SAME
/// account is refused — which only makes sense if the server recognised it
/// as the same player. A connection for a DIFFERENT account is admitted
/// alongside it, and shows up in the roster under the name the master
/// holds, not the one the client claimed.
///
/// That a rejoin finds the same INVENTORY is pinned separately by the sim's
/// `account_inventory_persists_across_reconnect_and_reboot`, which drives
/// the disconnect deterministically.
#[test]
fn the_server_keys_a_player_on_their_account() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("acctchar", &master.url, true);

	let (session, account, name) = dev_sign_in(&master.url, "Ada_L");
	assert_eq!(
		name.as_deref(),
		Some("Ada_L"),
		"the subject became the name"
	);
	assert!(!account.is_empty());

	let (token, _) = mint_token_with(&master.url, &session);
	let mut a = TestClient::connect(&server.url, "whatever", token);
	a.wait("welcome", |c| c.welcomed);

	// The same account, a second connection, a fresh token: refused,
	// because the server knows it is already here.
	let (token, _) = mint_token_with(&master.url, &session);
	let twice = TestClient::try_connect(&server.url, "whatever", token);
	assert!(
		twice.is_err(),
		"one account must not hold two live connections"
	);

	// And a different account is a different player, joining fine
	// alongside.
	let (other_session, other_account, _) = dev_sign_in(&master.url, "Bob_R");
	assert_ne!(
		other_account, account,
		"different subjects, different accounts"
	);
	let (token, _) = mint_token_with(&master.url, &other_session);
	let mut other = TestClient::connect(&server.url, "whatever", token);
	other.wait("welcome", |c| c.welcomed);
	other.wait("roster", |c| !c.joined_names.is_empty());
	assert!(
		other.joined_names.iter().any(|n| n == "Ada_L"),
		"the roster shows the account's name, not the claimed one: {:?}",
		other.joined_names
	);
	assert!(
		!other.joined_names.iter().any(|n| n == "whatever"),
		"a claimed Hello name must never reach the roster: {:?}",
		other.joined_names
	);
}

// ---- the production registration path (docker/compose.yml) -----------------

/// The rest of this suite runs the game server on the HOST, which forces
/// `Master:Dev:ProbeHostOverride` on: a containerized master cannot reach
/// `127.0.0.1` any other way. That override points BOTH probes at one host,
/// which collapses the source probe into the advertised one — so everything
/// registration decides from the source probe specifically is untested there.
///
/// This rig puts both sides in containers on one network, which is the only
/// topology where the master dials the registrant's real address. It brings
/// up `docker/compose.yml` rather than a copy: that file already carries the
/// no-override, private-targets, dev-auth combination, and a second copy of
/// it would drift from the rig people actually run.
///
/// Cost: it builds the SERVER image, which the rest of the suite does not.
/// That is why it is one test doing two assertions rather than two tests.
struct ComposeRig {
	project: String,
	master_url: String,
	data_dir: PathBuf,
}

impl ComposeRig {
	const FILE: &'static str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/compose.yml");

	fn up(tag: &str) -> Self {
		let master_port = free_tcp_port();
		// Deliberately not the server's own default port. The takeover case
		// below dials the host address at the server's registered port and
		// must find nothing there; publishing the default would hairpin it
		// back into the server container and the probe would pass for the
		// wrong reason.
		let server_port = free_tcp_port();
		let project = format!("dbe2e-{tag}-{}", std::process::id());
		// Under target/, NOT $TMPDIR: this is a bind mount, and on macOS the
		// temp dir lives in /var/folders, which Docker Desktop does not
		// share by default. The repo is already shared — docker/data proves
		// it — so a path inside it is the portable choice.
		let data_dir =
			PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/e2e")).join(&project);
		let _ = std::fs::remove_dir_all(&data_dir);
		std::fs::create_dir_all(&data_dir).expect("compose data dir");

		let rig = Self {
			project: project.clone(),
			master_url: format!("http://127.0.0.1:{master_port}"),
			data_dir: data_dir.clone(),
		};
		let status = rig
			.compose()
			.args(["up", "-d", "--build", "master", "server"])
			.env("MASTER_PORT", master_port.to_string())
			.env("SERVER_PORT", server_port.to_string())
			.env("DATA_DIR", &data_dir)
			.env("NETEM", "")
			.stdout(std::process::Stdio::null())
			.status()
			.expect("docker compose up");
		assert!(status.success(), "docker compose up failed");
		rig
	}

	fn compose(&self) -> std::process::Command {
		let mut c = std::process::Command::new("docker");
		c.args(["compose", "-f", Self::FILE, "-p", &self.project]);
		c
	}

	/// The one listed server, once the master has probed and accepted it.
	/// Waits, because registration is a real QUIC round trip behind a
	/// container start.
	fn listed(&self) -> serde_json::Value {
		let deadline = Instant::now() + Duration::from_secs(90);
		loop {
			if let Ok(mut res) = ureq::get(format!("{}/api/servers", self.master_url)).call()
				&& let Ok(body) = res.body_mut().read_json::<serde_json::Value>()
				&& body["servers"][0]["id"].is_string()
			{
				return body["servers"][0].clone();
			}
			assert!(
				Instant::now() < deadline,
				"compose server never appeared in the master's listing"
			);
			std::thread::sleep(Duration::from_millis(500));
		}
	}

	fn server_exec(&self, args: &[&str]) {
		let status = self
			.compose()
			.args(["exec", "-T", "server"])
			.args(args)
			.stdout(std::process::Stdio::null())
			.status()
			.expect("docker compose exec");
		assert!(status.success(), "docker compose exec {args:?} failed");
	}

	fn server_signal(&self, cmd: &str) {
		let status = self
			.compose()
			.args([cmd, "server"])
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status()
			.expect("docker compose signal");
		assert!(status.success(), "docker compose {cmd} server failed");
	}
}

impl Drop for ComposeRig {
	fn drop(&mut self) {
		let _ = self
			.compose()
			.args(["down", "-v", "--remove-orphans"])
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status();
		// Best effort: the container wrote this as root, so a non-root
		// runner may not be able to. It is under target/ either way.
		let _ = std::fs::remove_dir_all(&self.data_dir);
	}
}

/// Registration's two hard cases, on the only topology that can tell them
/// apart — both turn on whether the SOURCE probe passes.
///
/// 1. An operator who lost `./data` and restarted must be able to re-key.
///    This regressed once already and silently: the refusal used to fire on
///    `live` alone, and `live` is the in-memory registry, which outlives the
///    process it describes by up to `Registry:IdleTimeoutSeconds`. So the
///    recovery path documented in AUTH-DESIGN.md §3 was unreachable in
///    exactly the case it was written for — the operator was 409'd and
///    temp-banned by their own stale entry. The kill here is deliberately
///    ungraceful, because a clean shutdown de-registers and the stale entry
///    is the whole point.
///
/// 2. The NET-AUDIT-2.md §1.2 takeover must still be refused. Passing the
///    source probe means serving the pinned certificate from the
///    registrant's OWN address, which needs the incumbent's private key —
///    so a caller elsewhere cannot reach the recovery branch.
#[test]
fn a_lost_key_is_recovered_only_by_the_machine_at_that_address() {
	let _ = env_logger::builder().is_test(true).try_init();
	let rig = ComposeRig::up("rekey");

	let before = rig.listed();
	let server_id = before["id"].as_str().expect("listed id").to_string();
	let cert_hash = before["certHash"].as_str().unwrap().to_string();

	// --- the takeover, from an address that is not the server ---------------
	// Everything needed is public, straight out of the listing.
	let agent = ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.http_status_as_error(false)
			.build(),
	);
	let steal = || {
		agent
			.post(format!("{}/api/servers/register", rig.master_url))
			.send_json(serde_json::json!({
				"name": "e2e-attacker",
				// The server's address INSIDE the compose network, which is
				// the one the master listed. The published host port is
				// deliberately something else — see `ComposeRig::up`.
				"host": "server",
				"port": digblock_server::DEFAULT_PORT,
				"certHash": cert_hash,
				"players": 0,
				"maxPlayers": 8,
				"protocolVersion": digblock_net::protocol::PROTOCOL_VERSION,
				"simVersion": digblock_core::SIM_VERSION,
			}))
			.expect("register call")
			.status()
			.as_u16()
	};
	assert_eq!(steal(), 409, "keyless takeover must be refused");
	assert_eq!(
		steal(),
		429,
		"and the source temp-banned, so a loop is futile"
	);

	let during = rig.listed();
	assert_eq!(during["id"].as_str(), Some(server_id.as_str()));
	assert_eq!(
		during["name"].as_str(),
		before["name"].as_str(),
		"the incumbent is untouched"
	);

	// --- the operator who lost their key ------------------------------------
	// Delete the key while the container is up (the running server holds it
	// in memory and will not rewrite it), then SIGKILL so nothing
	// de-registers and the master's entry goes stale rather than away.
	rig.server_exec(&["rm", "-f", "/data/master-key"]);
	rig.server_signal("kill");
	let stale = rig.listed();
	assert_eq!(
		stale["id"].as_str(),
		Some(server_id.as_str()),
		"the master still lists the dead server — this is the state that used to ban"
	);

	rig.server_signal("start");
	let deadline = Instant::now() + Duration::from_secs(90);
	loop {
		let now = rig.listed();
		// A restarted server mints a FRESH certificate, so a changed hash is
		// what distinguishes the new process from the stale entry it
		// replaced. The id must NOT change with it: that comes from the
		// persisted claim, so holding it is what proves the address was
		// re-keyed rather than claimed from scratch.
		if now["certHash"].as_str().is_some_and(|h| h != cert_hash) {
			assert_eq!(
				now["id"].as_str(),
				Some(server_id.as_str()),
				"re-registered, but as a new claim — the recovery path re-keys the existing one"
			);
			break;
		}
		assert!(
			Instant::now() < deadline,
			"server never re-registered after losing its key: {now}"
		);
		std::thread::sleep(Duration::from_millis(500));
	}
}

/// Omitting the advertise host lists the server at the address the
/// master OBSERVED making the registration — the port-forwarded-home
/// default. The probe still dials the dev-split probe host; the listing
/// must carry the observed address, echoed back as observedHost. (No
/// assumption about WHICH address Docker's NAT shows the container —
/// only that the listing and the echo agree.)
#[test]
fn omitted_advertise_host_lists_observed_address() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("observed", &master.url, false);
	let mut cfg = server.master_config(&master.url, "e2e-observed");
	cfg.advertise_host = None;
	let reg = master::register(&cfg, &server.cert_hash).expect("register without advertise host");
	assert!(!reg.observed_host.is_empty(), "master echoes observed host");
	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	assert_eq!(
		list["servers"][0]["url"].as_str().unwrap(),
		format!("https://{}:{}", reg.observed_host, server.port),
		"listed at the observed registration address"
	);
}

/// Chat crosses the real QUIC stack: A talks, both A (echo) and B hear
/// it attributed to A's player id.
#[test]
fn chat_round_trips_between_clients() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("chat", &master.url, true);

	let (token_a, _) = mint_token(&master.url, "adam");
	let mut a = TestClient::connect(&server.url, "adam", token_a);
	a.wait("welcome", |c| c.welcomed);
	let (token_b, _) = mint_token(&master.url, "beth");
	let mut b = TestClient::connect(&server.url, "beth", token_b);
	b.wait("welcome", |c| c.welcomed);

	a.client.send_chat("hello over QUIC");
	a.wait("chat echo", |c| !c.chats.is_empty());
	b.wait("chat broadcast", |c| !c.chats.is_empty());
	let a_id = a.chats[0].0;
	assert_eq!(a.chats[0].1, "hello over QUIC");
	assert_eq!(b.chats[0], (a_id, "hello over QUIC".to_string()));
}

#[test]
fn zero_token_is_refused() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("zerotok", &master.url, true);
	expect_refused(&server.url, [0; 16], "requires a token");
}

#[test]
fn unminted_token_is_refused() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("badtok", &master.url, true);
	expect_refused(&server.url, [9; 16], "token rejected");
}

/// A token is one-time: the first join consumes it at the master, a second
/// connection presenting it must be refused.
#[test]
fn replayed_token_is_refused() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("replay", &master.url, true);

	let (token, _) = mint_token(&master.url, "once");
	let mut a = TestClient::connect(&server.url, "once", token);
	a.wait("welcome", |c| c.welcomed);
	expect_refused(&server.url, token, "token already used");
}

/// Master gone: verification fails CLOSED — new joins refused (players who
/// are already in keep playing; A stays connected through it).
#[test]
fn master_down_fails_closed() {
	let _ = env_logger::builder().is_test(true).try_init();
	let mut master = MasterProc::spawn();
	let server = TestServer::start("mdown", &master.url, true);

	let (token_a, _) = mint_token(&master.url, "earlybird");
	let mut a = TestClient::connect(&server.url, "earlybird", token_a);
	a.wait("welcome", |c| c.welcomed);

	let (token_b, _) = mint_token(&master.url, "toolate");
	master.kill();
	expect_refused(&server.url, token_b, "authentication service unavailable");

	// The already-admitted player is unaffected.
	a.wait("still snapshotting", |c| c.acks > 0);
}

/// The master's QUIC probe pins the registered cert hash: a registration
/// claiming the wrong hash must be rejected as unreachable/certMismatch
/// even though the endpoint itself is up.
#[test]
fn probe_rejects_wrong_cert_hash() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("probecert", &master.url, false);
	let cfg = server.master_config(&master.url, "bad-hash");
	match master::register(&cfg, &[0u8; 32]) {
		Err(master::RegisterError::Unreachable(e)) => {
			assert!(e.contains("certMismatch"), "unexpected reason: {e}");
		}
		Err(master::RegisterError::Transport(e) | master::RegisterError::Rejected(e)) => {
			panic!("wrong error class: {e}")
		}
		Ok(_) => panic!("registration with a bogus cert hash must not succeed"),
	}
}

/// The LIVE connection publishes a wire round trip too, not just the
/// one-shot probe — that is what lets the overlay show the same quantity
/// the server browser does.
///
/// Pins the two failure modes loopback can see: that the background thread
/// publishes at all (the value starts as an "unknown" sentinel, so a
/// transport that never stores would read as `None` forever), and that it
/// is a measurement rather than quinn's 333 ms initial default. The value
/// is deliberately NOT compared against `rtt_ms`: they are independent by
/// construction, and the ack loop needs a joined, snapshotting session
/// while this needs only a connection.
#[test]
fn a_live_connection_publishes_its_wire_rtt() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("livertt", &master.url, true);
	let (token, _url) = mint_token(&master.url, "rttbot");
	let mut c = TestClient::connect(&server.url, "rttbot", token);
	c.wait("welcomed", |c| c.welcomed);
	// The background thread republishes on each wake, so give the
	// connection a few polls' worth of traffic to have woken at all.
	c.wait("wire rtt published", |c| c.client.stats().wire_rtt_ms > 0.0);

	let rtt = c.client.stats().wire_rtt_ms;
	assert!(
		rtt > 0.0 && rtt < 100.0,
		"loopback wire rtt read as {rtt} ms — that is quinn's 333 ms initial \
		 default, not a measurement"
	);
}

/// The CLIENT-side probe — the one behind the server browser's ping column
/// — reports the transport's own round-trip estimate, not the time the
/// handshake took.
///
/// Loopback cannot tell a good latency figure from a bad one, so this pins
/// the two failure modes it CAN see: that the probe still reaches a real
/// server after the API change, and that the number is a measurement rather
/// than a constant. The realistic regression is quinn's RFC 9002 initial
/// RTT — 333 ms, reported identically for every server in the list — which
/// is what `Connection::rtt` returns before it has a sample of its own.
///
/// No master here on purpose: an unregistered server still completes the
/// WebTransport handshake, which is all a probe does. That keeps this test
/// off Docker and fast.
#[test]
fn client_probe_reports_a_round_trip_not_a_handshake() {
	let _ = env_logger::builder().is_test(true).try_init();
	let server = TestServer::start("probertt", "http://127.0.0.1:1", false);
	let policy = digblock_net_native::CertPolicy::PinnedSha256(server.cert_hash);
	let (tx, rx) = std::sync::mpsc::channel();
	digblock_net_native::probe(
		&format!("https://127.0.0.1:{}", server.port),
		policy,
		Duration::from_secs(5),
		move |ms| {
			let _ = tx.send(ms);
		},
	);
	let ms = rx
		.recv_timeout(Duration::from_secs(10))
		.expect("probe called back")
		.expect("probe reached the server");
	assert!(
		ms < 100,
		"loopback round trip read as {ms} ms — that is quinn's 333 ms initial \
		 default or a handshake time, not a measurement"
	);
}

/// A server nobody can reach (here: the master container's own loopback,
/// where nothing listens) must be refused registration by the probe.
#[test]
fn probe_rejects_unreachable_server() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let cfg = MasterConfig {
		url: master.url.clone(),
		server_name: "nowhere".into(),
		// No probe-host split: the master probes the advertised address —
		// its own container loopback, where nothing listens.
		advertise_host: Some("127.0.0.1".into()),
		server_key: None,
		advertise_port: 4,
	};
	match master::register(&cfg, &[0xaa; 32]) {
		Err(master::RegisterError::Unreachable(_)) => {}
		Err(master::RegisterError::Transport(e) | master::RegisterError::Rejected(e)) => {
			panic!("wrong error class: {e}")
		}
		Ok(_) => panic!("registration of an unreachable server must not succeed"),
	}
}

/// Until registration lands, a server can admit no one.
#[test]
fn unregistered_server_refuses_joins() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("unreg", &master.url, false);
	// Any nonzero token: the refusal must come from the registration gate.
	expect_refused(&server.url, [7; 16], "not yet registered");
}

/// A peer that opens the control stream but sends garbage instead of the
/// preamble must be dropped cleanly — never parsed, never hung. (No master
/// involvement: the connection dies before the token gate.)
#[test]
fn garbage_preamble_is_dropped_not_hung() {
	let _ = env_logger::builder().is_test(true).try_init();
	let server = TestServer::start("garb", "http://127.0.0.1:9", false);

	// Raw wtransport client that speaks garbage on the control stream.
	let closed = server.rt.block_on(async {
		let config = wtransport::ClientConfig::builder()
			.with_bind_default()
			.with_no_cert_validation()
			.build();
		let conn = wtransport::Endpoint::client(config)
			.unwrap()
			.connect(server.url.clone())
			.await
			.expect("connect");
		let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
		send.write_all(b"NOPE-not-a-preamble").await.unwrap();
		// The server must drop us: the read side ends (None/err) instead
		// of hanging forever on a half-open session.
		let mut buf = [0u8; 64];
		tokio::time::timeout(Duration::from_secs(10), async {
			// Drain any bytes until the server drops the stream.
			while let Ok(Some(_)) = recv.read(&mut buf).await {}
		})
		.await
		.is_ok()
	});
	assert!(closed, "server hung instead of dropping a garbage preamble");
}

/// In-process cert rotation, end to end: after `rotate_cert` the live
/// endpoint must serve the new certificate — proven by the master's real
/// QUIC probe, which re-registration pins to the freshly minted hash — the
/// master must replace (not duplicate) the listing, a client connected
/// before the swap must ride through it undisturbed, and a join minted
/// after the swap must complete.
#[test]
fn cert_rotation_reregisters_and_serves_new_cert() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("rotate", &master.url, true);

	let (token, _) = mint_token(&master.url, "OldTimer");
	let mut before = TestClient::connect(&server.url, "OldTimer", token);
	before.wait("welcome before rotation", |c| c.welcomed);

	let new_hash = net::rotate_cert(&server.endpoint).expect("live cert swap");
	assert_ne!(new_hash, server.cert_hash, "rotation must mint a new cert");
	let mut cfg = server.master_config(&master.url, "e2e-rotate");
	// A live address may only be re-registered by whoever holds its key.
	// The real rotation path carries it in MasterConfig; this test stands
	// in for that path, so it carries it too.
	cfg.server_key = server.master_state.registration().map(|r| r.server_key);
	let reg = match master::register(&cfg, &new_hash) {
		Ok(reg) => reg,
		Err(
			master::RegisterError::Unreachable(e)
			| master::RegisterError::Transport(e)
			| master::RegisterError::Rejected(e),
		) => panic!("re-register after rotation (probe must see the new cert): {e}"),
	};
	server.master_state.set_registration(Some(reg));

	// Replaced at the same host:port, not listed twice.
	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	assert_eq!(
		list["servers"].as_array().map(|s| s.len()),
		Some(1),
		"re-registration must replace the old entry: {list}"
	);

	let (token, _) = mint_token(&master.url, "Newcomer");
	let mut after = TestClient::connect(&server.url, "Newcomer", token);
	after.wait("welcome after rotation", |c| c.welcomed);

	// The pre-rotation connection is still alive: it sees the newcomer
	// join, and pump() panics on Disconnected.
	before.wait("pre-rotation client observes post-rotation join", |c| {
		c.joined_names.iter().any(|n| n == "Newcomer")
	});
}

/// A hostile or buggy registrant cannot hide every server on the master.
///
/// The master types player counts as `int` and range-checked neither the
/// register nor the heartbeat, while both clients decode them as unsigned
/// — so one entry with `players: -1` failed the WHOLE `/api/servers`
/// decode client-side, and nobody could see any server at all. Fixed on
/// both sides: the master clamps so the bad value never leaves, and the
/// clients clamp so a stale master cannot do it either.
#[test]
fn a_negative_player_count_cannot_break_the_server_list() {
	let _ = env_logger::builder().is_test(true).try_init();
	let master = MasterProc::spawn();
	let server = TestServer::start("negcount", &master.url, false);
	let cfg = server.master_config(&master.url, "e2e-negcount");

	// Register by hand so the counts can be what a well-behaved client
	// could never send. The QUIC probe still runs against the real server.
	let res = ureq::post(format!("{}/api/servers/register", master.url))
		.send_json(serde_json::json!({
			"name": "e2e-negcount",
			"host": cfg.advertise_host,
			"port": cfg.advertise_port,
			"certHash": server
				.cert_hash
				.iter()
				.map(|b| format!("{b:02x}"))
				.collect::<String>(),
			"players": -1,
			"maxPlayers": -5,
			"protocolVersion": digblock_net::protocol::PROTOCOL_VERSION,
			"simVersion": digblock_core::SIM_VERSION,
		}))
		.expect("register with hostile counts");
	assert_eq!(res.status().as_u16(), 200, "registration itself succeeds");

	// The listing must be readable, and the counts sane.
	let mut res = ureq::get(format!("{}/api/servers", master.url))
		.call()
		.unwrap();
	let raw: String = res.body_mut().read_to_string().unwrap();
	let list: serde_json::Value = serde_json::from_str(&raw).unwrap();
	let entry = &list["servers"][0];
	assert_eq!(
		entry["players"].as_i64(),
		Some(0),
		"clamped at the master, not passed through: {raw}"
	);
	assert_eq!(entry["maxPlayers"].as_i64(), Some(0), "{raw}");
}
