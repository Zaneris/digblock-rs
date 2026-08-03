//! End-to-end over real sockets AND the real .NET master — run exactly as
//! it deploys: each test boots the apps/master Docker image (alpine +
//! libmsquic, QUIC probe ON), which probes the host-side game server for
//! real over `host.docker.internal` before listing it. Clients join the
//! only way that exists: mint a one-time token from the master, present it
//! in Hello, server verifies it back with the master. There is no
//! tokenless path. Docker must be running for these tests.

use crossbeam_channel::unbounded;
use digblock_core::block::BlockType;
use digblock_core::player::Player;
use digblock_core::raycast::{raycast, REACH};
use digblock_net::client::NetClientConfig;
use digblock_net::messages::EditKind;
use digblock_net::{NetClient, NetEvent};
use digblock_server::master::{self, MasterConfig, MasterState};
use digblock_server::sim::Sim;
use digblock_server::{net, run_sim_loop};
use glam::Vec3;
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
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
				MASTER_IMAGE,
			])
			.stdout(std::process::Stdio::null())
			.status()
			.expect("docker run master");
		assert!(status.success(), "docker run {MASTER_IMAGE} failed");
		let ready_by = Instant::now() + Duration::from_secs(30);
		loop {
			if ureq::get(format!("{url}/api/servers")).call().is_ok() {
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

/// Mint a one-time join token for the first listed server: exactly what a
/// real client does. Returns (token, game server url).
fn mint_token(master_url: &str, display_name: &str) -> ([u8; 16], String) {
	let mut res = ureq::get(format!("{master_url}/api/servers"))
		.call()
		.unwrap();
	let list: serde_json::Value = res.body_mut().read_json().unwrap();
	let server_id = list["servers"][0]["id"]
		.as_str()
		.expect("a registered server in the list");
	let mut res = ureq::post(format!("{master_url}/api/join"))
		.send_json(serde_json::json!({ "serverId": server_id, "displayName": display_name }))
		.unwrap();
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
		let port = endpoint.local_addr().unwrap().port();
		let data_dir = std::env::temp_dir().join(format!("dbe2e-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&data_dir);
		let sim = Sim::new(42.0, &data_dir, 2, 2).unwrap();
		let (event_tx, event_rx) = unbounded();
		let running = Arc::new(AtomicBool::new(true));
		let players = Arc::new(AtomicUsize::new(0));
		let sim_thread = {
			let running = running.clone();
			std::thread::spawn(move || run_sim_loop(sim, event_rx, running, players))
		};
		let state = Arc::new(MasterState::new(master_url.to_string()));
		rt.spawn(net::serve(endpoint, event_tx, state.clone()));
		let server = Self {
			rt,
			running,
			sim_thread: Some(sim_thread),
			data_dir,
			url: format!("https://127.0.0.1:{port}"),
			port,
			cert_hash,
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
			advertise_host: "127.0.0.1".into(),
			probe_host: Some(HOST_FROM_CONTAINER.into()),
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
	inventories: Vec<Vec<(BlockType, u32)>>,
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
					identity: [0; 16],
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
				NetEvent::Edit { cell, block, .. } => {
					self.edits.insert(cell, block);
				}
				NetEvent::Ack { .. } => self.acks += 1,
				NetEvent::Refused { reason } => panic!("refused: {reason}"),
				NetEvent::Disconnected { reason } => panic!("disconnected: {reason}"),
				NetEvent::EditRejected(e) => panic!("edit rejected: {e:?}"),
				NetEvent::PlayerJoined { name, .. } => self.joined_names.push(name),
				NetEvent::PlayerLeft { .. } => {}
				NetEvent::Inventory { counts } => self.inventories.push(counts),
				NetEvent::Chat { from, text } => self.chats.push((from, text)),
			}
		}
	}

	/// The real timed-dig flow: aim, DigStart, hold for the block's
	/// hardness (plus margin), then the completing Dig request. Paced at
	/// real 60 Hz — the server's aim verification tracks wall clock.
	fn timed_dig(
		&mut self,
		player: &mut Player,
		world: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
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
		let hold = hit.block.hardness_ticks().expect("diggable target") as u32 + 6;
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
			hit.block,
			BlockType::Air,
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
			identity: [0; 16],
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
	let world = move |x: i32, y: i32, z: i32| -> Option<BlockType> {
		let ((cx, cy, cz), (lx, ly, lz)) = digblock_core::chunk::split_world(x, y, z)?;
		let mut cache = cache.borrow_mut();
		let stack = cache
			.entry((cx, cz))
			.or_insert_with(|| digblock_core::worldgen::build_column(cx, cz, seed));
		Some(stack[cy as usize].get(lx, ly, lz))
	};
	let mut player = Player::new(a.spawn);
	// Predicted idle ticks: inputs flow, and both the server player and
	// this local twin settle onto the same ground fixed point. Real-time
	// paced — racing ahead of the 60 Hz server builds an input backlog
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
			Some(BlockType::Air)
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
		advertise_host: "127.0.0.1".into(),
		probe_host: None,
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
