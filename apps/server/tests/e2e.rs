//! End-to-end over real sockets: full server (wtransport endpoint + sim
//! thread) on an ephemeral UDP port, two real NetClients through the
//! native transport. Client A digs; client B must see the edit both in
//! its join sync (connecting later) and via the live broadcast path.

use crossbeam_channel::unbounded;
use digblock_core::block::BlockType;
use digblock_core::player::Player;
use digblock_core::raycast::{raycast, REACH};
use digblock_net::client::NetClientConfig;
use digblock_net::messages::EditKind;
use digblock_net::{NetClient, NetEvent};
use digblock_server::sim::Sim;
use digblock_server::{net, run_sim_loop};
use glam::Vec3;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct TestClient {
	client: NetClient,
	events: Vec<NetEvent>,
	seed: f64,
	spawn: Vec3,
	welcomed: bool,
	edits: FxHashMap<(i32, i32, i32), BlockType>,
	acks: u32,
}

impl TestClient {
	fn connect(url: &str, name: &str) -> Self {
		let transport = digblock_net_native::connect(url);
		Self {
			client: NetClient::new(
				Box::new(transport),
				NetClientConfig {
					name: name.into(),
					draw_dist: 4,
				},
			),
			events: Vec::new(),
			seed: 0.0,
			spawn: Vec3::ZERO,
			welcomed: false,
			edits: FxHashMap::default(),
			acks: 0,
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
				NetEvent::Edit { cell, block } => {
					self.edits.insert(cell, block);
				}
				NetEvent::Ack { .. } => self.acks += 1,
				NetEvent::Refused { reason } => panic!("refused: {reason}"),
				NetEvent::Disconnected { reason } => panic!("disconnected: {reason}"),
				NetEvent::EditRejected(e) => panic!("edit rejected: {e:?}"),
				NetEvent::PlayerJoined { .. } | NetEvent::PlayerLeft { .. } => {}
			}
		}
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

#[test]
fn edit_reaches_late_joiner_and_live_peer() {
	let _ = env_logger::builder().is_test(true).try_init();

	// Server on an ephemeral port. quinn needs a tokio reactor in scope
	// to bind, so create the runtime first.
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let _guard = rt.enter();
	let endpoint = net::make_endpoint(0).expect("bind ephemeral udp");
	let port = endpoint.local_addr().unwrap().port();
	let data_dir = std::env::temp_dir().join(format!("dbe2e-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&data_dir);
	let sim = Sim::new(42.0, &data_dir, 2, 2).unwrap();
	let (event_tx, event_rx) = unbounded();
	let running = Arc::new(AtomicBool::new(true));
	let sim_thread = {
		let running = running.clone();
		std::thread::spawn(move || run_sim_loop(sim, event_rx, running))
	};
	rt.spawn(net::serve(endpoint, event_tx));
	let url = format!("https://127.0.0.1:{port}");

	// Client A joins and waits until the server snapshots it (live).
	let mut a = TestClient::connect(&url, "a");
	a.wait("welcome", |c| c.welcomed);
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
	// this local twin settle onto the same ground fixed point.
	for _ in 0..60 {
		a.client
			.predictor
			.predict(&mut player, Default::default(), Vec3::Z, &world);
		a.client.send_inputs();
		a.pump();
		std::thread::sleep(Duration::from_millis(2));
	}
	// Dig at an angle so the ground under the player stays intact (a
	// hole underfoot would drop the server player away from our claims).
	let dig_dir = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(player.pos, dig_dir, REACH, &world).expect("ground beside spawn");
	let seq = a.client.edits.begin(hit.cell, hit.block, BlockType::Air, 0);
	a.client.send_edit(
		seq,
		a.client.predictor.current_tick(),
		player.pos,
		dig_dir,
		EditKind::Dig,
		hit.cell,
		(0, 0, 0),
	);
	// A receives its own confirm as an authoritative Edit.
	a.wait("dig confirm", |c| {
		c.edits.get(&hit.cell) == Some(&BlockType::Air)
	});

	// B joins late: the edit must arrive in its join sync.
	let mut b = TestClient::connect(&url, "b");
	b.wait("welcome", |c| c.welcomed);
	assert_eq!(b.seed, a.seed, "same world");
	b.wait("join-sync edit", |c| {
		c.edits.get(&hit.cell) == Some(&BlockType::Air)
	});

	// Live path: A digs a second block (mirrored angle, ground intact
	// underfoot); B (already connected) sees the broadcast.
	let dig_dir2 = Vec3::new(-0.45, -1.0, 0.0).normalize();
	let hit2 = raycast(player.pos, dig_dir2, REACH, &|x, y, z| {
		if (x, y, z) == hit.cell {
			Some(BlockType::Air)
		} else {
			world(x, y, z)
		}
	})
	.expect("second target");
	assert_ne!(hit2.cell, hit.cell);
	let seq = a
		.client
		.edits
		.begin(hit2.cell, hit2.block, BlockType::Air, 0);
	a.client.send_edit(
		seq,
		a.client.predictor.current_tick(),
		player.pos,
		dig_dir2,
		EditKind::Dig,
		hit2.cell,
		(0, 0, 0),
	);
	b.wait("live edit broadcast", |c| {
		c.edits.get(&hit2.cell) == Some(&BlockType::Air)
	});

	running.store(false, Ordering::Relaxed);
	let _ = sim_thread.join();
	let _ = std::fs::remove_dir_all(&data_dir);
}

/// A peer that opens the control stream but sends garbage instead of the
/// preamble must be dropped cleanly — never parsed, never hung.
#[test]
fn garbage_preamble_is_dropped_not_hung() {
	let _ = env_logger::builder().is_test(true).try_init();
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let _guard = rt.enter();
	let endpoint = net::make_endpoint(0).expect("bind ephemeral udp");
	let port = endpoint.local_addr().unwrap().port();
	let data_dir = std::env::temp_dir().join(format!("dbe2e-garb-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&data_dir);
	let sim = Sim::new(42.0, &data_dir, 2, 2).unwrap();
	let (event_tx, event_rx) = unbounded();
	let running = Arc::new(AtomicBool::new(true));
	let sim_thread = {
		let running = running.clone();
		std::thread::spawn(move || run_sim_loop(sim, event_rx, running))
	};
	rt.spawn(net::serve(endpoint, event_tx));

	// Raw wtransport client that speaks garbage on the control stream.
	let closed = rt.block_on(async {
		let config = wtransport::ClientConfig::builder()
			.with_bind_default()
			.with_no_cert_validation()
			.build();
		let conn = wtransport::Endpoint::client(config)
			.unwrap()
			.connect(format!("https://127.0.0.1:{port}"))
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

	running.store(false, Ordering::Relaxed);
	let _ = sim_thread.join();
	let _ = std::fs::remove_dir_all(&data_dir);
}
