//! Deterministic netcode benchmark: what one sim tick costs, and what it
//! puts on the wire, as a function of player count.
//!
//! Socket-free by design. It drives the REAL `Sim` through the same
//! `SimEvent` seam the wtransport layer uses, so it measures the code that
//! ships — scheduling, interest management, snapshot encoding, edit
//! broadcast — without a network in the way. That makes it repeatable
//! enough to compare two commits, which a socket benchmark on a laptop is
//! not.
//!
//! What it does NOT measure: latency. There is no wire here, so RTT and
//! jitter are meaningless; those are live numbers, and they are on the
//! debug overlay's `net` line instead.
//!
//! Run with `just bench-net`. Output is one table plus a machine-diffable
//! summary line per player count, for pasting into BENCH.md.

use digblock_net::messages::{ClientMsg, InputBatch, InputTick};
use digblock_net::protocol::{PROTOCOL_VERSION, SNAPSHOT_DIVISOR, TICK_RATE};
use digblock_server::sim::{OutMsg, Sim, SimEvent, OUT_CAP};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::mpsc::channel;

/// Counting allocator: the round-2 audit was largely about per-tick
/// allocation, so a netcode benchmark that could not see allocations would
/// miss the thing most worth regressing on.
struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
	unsafe fn alloc(&self, l: Layout) -> *mut u8 {
		ALLOCS.fetch_add(1, Ordering::Relaxed);
		ALLOC_BYTES.fetch_add(l.size(), Ordering::Relaxed);
		unsafe { System.alloc(l) }
	}
	unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
		unsafe { System.dealloc(p, l) }
	}
	unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
		ALLOCS.fetch_add(1, Ordering::Relaxed);
		ALLOC_BYTES.fetch_add(new.saturating_sub(l.size()), Ordering::Relaxed);
		unsafe { System.realloc(p, l, new) }
	}
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Player counts to sweep. 40 is `MAX_SNAPSHOT_PLAYERS`, where interest
/// management starts having to choose.
const COUNTS: &[usize] = &[1, 8, 24, 40, 64];
/// Ticks measured per count, after warmup. 600 = 10 s of sim.
const TICKS: usize = 600;
const WARMUP: usize = 120;

struct Client {
	conn_id: u64,
	rx: tokio::sync::mpsc::Receiver<OutMsg>,
	/// Bytes the server sent this client, split by transport.
	dgram_bytes: u64,
	frame_bytes: u64,
	dgrams: u64,
}

fn spawn_clients(sim: &mut Sim, n: usize) -> Vec<Client> {
	let mut clients = Vec::with_capacity(n);
	for i in 0..n {
		let conn_id = i as u64 + 1;
		let (tx, rx) = channel(OUT_CAP);
		let mut identity = [0u8; 16];
		identity[..8].copy_from_slice(&(conn_id).to_le_bytes());
		sim.tick_once(vec![
			SimEvent::Connected {
				conn_id,
				out: tx,
				addr: format!("127.0.0.1:{}", 40000 + i).parse().unwrap(),
			},
			SimEvent::Msg {
				conn_id,
				msg: ClientMsg::Hello {
					protocol: PROTOCOL_VERSION,
					name: format!("bench{i:03}"),
					// A real client's default, so interest management is
					// exercised at the radius players actually use.
					draw_dist: 20,
					token: [0; 16],
					sim_version: digblock_core::SIM_VERSION,
					build: "netbench".into(),
					identity,
				},
			},
		]);
		clients.push(Client {
			conn_id,
			rx,
			dgram_bytes: 0,
			frame_bytes: 0,
			dgrams: 0,
		});
	}
	clients
}

/// Walk everyone in a slow circle, each on their own phase, so players
/// spread out and cross column borders — the residency and interest paths
/// only cost anything when people move.
fn input_for(c: &Client, tick: u32, n: usize) -> SimEvent {
	let phase = c.conn_id as f32 / n.max(1) as f32 * std::f32::consts::TAU;
	let t = tick as f32 / TICK_RATE as f32;
	let (sx, sz) = (phase + t * 0.4).sin_cos();
	SimEvent::Inputs {
		conn_id: c.conn_id,
		batch: InputBatch {
			inputs: vec![InputTick {
				tick,
				move_axis: [0.0, 1.0],
				jump: false,
				look: [sx, 0.0, sz],
				// Never adoptable, so the server's own physics carries
				// everyone — the point is the server's cost, not adoption.
				claimed_pos: [0.0, -500.0, 0.0],
				claimed_vel: [0.0; 3],
				active_slot: 0,
			}],
		},
	}
}

fn drain(clients: &mut [Client]) {
	for c in clients.iter_mut() {
		while let Ok(msg) = c.rx.try_recv() {
			match msg {
				OutMsg::Datagram(b) => {
					c.dgram_bytes += b.len() as u64;
					c.dgrams += 1;
				}
				OutMsg::Frame(b) => c.frame_bytes += b.len() as u64,
				OutMsg::Close => {}
			}
		}
	}
}

fn pct(sorted: &[f64], p: f64) -> f64 {
	if sorted.is_empty() {
		return 0.0;
	}
	let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
	sorted[i]
}

fn run(n: usize, edits: bool) -> String {
	let dir = std::env::temp_dir().join(format!("digblock-netbench-{n}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	// sim_radius 6 is the shipped default; workers 4 keeps generation off
	// the measured thread.
	let mut sim = Sim::new(42.0, &dir, 6, 4).expect("open bench world");
	let mut clients = spawn_clients(&mut sim, n);

	// Let everyone go live and their spawn columns generate before timing.
	for t in 0..WARMUP {
		let evs: Vec<SimEvent> = clients
			.iter()
			.map(|c| input_for(c, 100 + t as u32, n))
			.collect();
		sim.tick_once(evs);
		drain(&mut clients);
		std::thread::sleep(std::time::Duration::from_millis(1));
	}
	for c in clients.iter_mut() {
		c.dgram_bytes = 0;
		c.frame_bytes = 0;
		c.dgrams = 0;
	}

	let mut tick_us: Vec<f64> = Vec::with_capacity(TICKS);
	let allocs0 = ALLOCS.load(Ordering::Relaxed);
	let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
	for t in 0..TICKS {
		let base = 100 + (WARMUP + t) as u32;
		let mut evs: Vec<SimEvent> = clients.iter().map(|c| input_for(c, base, n)).collect();
		// A steady trickle of edits, so broadcast fan-out is in the
		// measurement rather than an idle world.
		if edits && t % 10 == 0 {
			let c = &clients[t % clients.len()];
			evs.push(SimEvent::Msg {
				conn_id: c.conn_id,
				msg: ClientMsg::Chat {
					text: "bench".into(),
				},
			});
		}
		let start = Instant::now();
		sim.tick_once(evs);
		tick_us.push(start.elapsed().as_secs_f64() * 1.0e6);
		drain(&mut clients);
	}
	let allocs = ALLOCS.load(Ordering::Relaxed) - allocs0;
	let alloc_kb = (ALLOC_BYTES.load(Ordering::Relaxed) - bytes0) / 1024;

	tick_us.sort_by(f64::total_cmp);
	let secs = TICKS as f64 / TICK_RATE as f64;
	let dgram: u64 = clients.iter().map(|c| c.dgram_bytes).sum();
	let frame: u64 = clients.iter().map(|c| c.frame_bytes).sum();
	let dgrams: u64 = clients.iter().map(|c| c.dgrams).sum();
	// Per client: what one player's connection actually costs downstream.
	let per_client_kbps = (dgram + frame) as f64 / n as f64 / secs / 1024.0;
	let snap_bytes = if dgrams > 0 {
		dgram as f64 / dgrams as f64
	} else {
		0.0
	};
	let expected_snaps = secs * (TICK_RATE / SNAPSHOT_DIVISOR) as f64 * n as f64;

	sim.shutdown();
	let _ = std::fs::remove_dir_all(&dir);
	format!(
		"{n:>3} | {:>6.1} {:>6.1} {:>6.1} {:>6.1} | {:>6.1} | {:>6.1} | {:>7.1} {:>7}K | {:>4.0}%",
		pct(&tick_us, 0.5),
		pct(&tick_us, 0.95),
		pct(&tick_us, 0.99),
		tick_us[tick_us.len() - 1],
		snap_bytes,
		per_client_kbps,
		allocs as f64 / TICKS as f64,
		alloc_kb,
		dgrams as f64 / expected_snaps * 100.0,
	)
}

fn main() {
	let edits = !std::env::args().any(|a| a == "--no-edits");
	// Long enough for the rate windows to settle and for a 500 ms link to
	// complete many round trips.
	let link_secs = 20.0;
	println!("digblock netbench — deterministic, socket-free, {TICKS} ticks per row");
	println!(
		"sim_radius 6, draw_dist 20, everyone walking; {} \n",
		if edits {
			"with chat/edit fan-out"
		} else {
			"movement only"
		}
	);
	println!(
		"  n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap"
	);
	println!(
		"----+--------------------------------------+--------+--------+-----------------+------"
	);
	for &n in COUNTS {
		println!("{}", run(n, edits));
	}
	println!(
		"\nsnap  snapshots delivered against the {} Hz nominal. Under 100% means the\n\
		 \x20     outbound queue refused some — a real problem, not measurement noise.",
		TICK_RATE / SNAPSHOT_DIVISOR
	);
	println!(
		"snap B  bytes per snapshot datagram. Flat past MAX_SNAPSHOT_PLAYERS ({}) is\n\
		 \x20       interest management working, not a plateau in the measurement.",
		digblock_net::protocol::MAX_SNAPSHOT_PLAYERS
	);
	println!(
		"tick us the sim thread's own budget is {:.0} us at {} Hz; p99 is the number\n\
		 \x20       that matters, and max is where a stall would show.",
		1.0e6 / TICK_RATE as f64,
		TICK_RATE
	);
	if !std::env::args().any(|a| a == "--no-links") {
		run_links(link_secs);
	}
}
// ---- link-quality sweep ----------------------------------------------
//
// The capacity table above is socket-free because server COST is what it
// measures. This half measures the opposite thing — what a session FEELS
// like — and for that the wire is the whole point. So it runs the real
// `NetClient` (predictor, reconciliation, RTT estimator and all) against
// the real `Sim`, joined by a deterministic impairing link.
//
// Everything is driven off a virtual 60 Hz clock with a seeded PRNG: no
// wall-clock, no threads, no sockets. Two runs of the same profile produce
// the same numbers, which is what makes "did this change make loss worse"
// a question you can actually answer.

/// xorshift64*. Deterministic and seeded — `rand` would be fine too, but
/// pinning the exact sequence here is the point.
struct Rng(u64);

impl Rng {
	fn next_u64(&mut self) -> u64 {
		let mut x = self.0;
		x ^= x >> 12;
		x ^= x << 25;
		x ^= x >> 27;
		self.0 = x;
		x.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}
	/// Uniform in [0, 1).
	fn unit(&mut self) -> f64 {
		(self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
	}
}

#[derive(Clone, Copy)]
struct Link {
	name: &'static str,
	/// Round trip, split evenly across the two directions.
	rtt_ms: f64,
	/// Peak +- variation on each one-way delay.
	jitter_ms: f64,
	/// Datagram loss, each direction, per packet.
	loss: f64,
}

/// Shipped-adjacent conditions, plus two deliberately past the point of
/// comfort. `just compose wifi|cellular` shapes traffic with tc netem at
/// roughly the middle two.
const LINKS: &[Link] = &[
	Link {
		name: "lan",
		rtt_ms: 4.0,
		jitter_ms: 1.0,
		loss: 0.000,
	},
	Link {
		name: "wifi",
		rtt_ms: 30.0,
		jitter_ms: 10.0,
		loss: 0.010,
	},
	Link {
		name: "cellular",
		rtt_ms: 120.0,
		jitter_ms: 40.0,
		loss: 0.030,
	},
	Link {
		name: "bad-4g",
		rtt_ms: 300.0,
		jitter_ms: 80.0,
		loss: 0.050,
	},
	Link {
		name: "awful",
		rtt_ms: 500.0,
		jitter_ms: 150.0,
		loss: 0.100,
	},
];

struct InFlight {
	at: f64,
	bytes: Vec<u8>,
}

/// The impaired wire. Datagrams may be dropped, delayed and reordered;
/// the reliable stream is never dropped and never reordered (QUIC
/// retransmits and delivers in order), so a delayed reliable frame holds
/// up the ones behind it — head-of-line blocking, modelled rather than
/// wished away.
#[derive(Default)]
struct Wire {
	now: f64,
	to_server_dgram: Vec<InFlight>,
	to_server_rel: Vec<InFlight>,
	to_client_dgram: Vec<InFlight>,
	to_client_rel: Vec<InFlight>,
	rel_ready_server: f64,
	rel_ready_client: f64,
	sent_dgrams: u64,
	dropped_dgrams: u64,
}

impl Wire {
	fn one_way(&mut self, rng: &mut Rng, link: &Link) -> f64 {
		let j = (rng.unit() * 2.0 - 1.0) * link.jitter_ms;
		((link.rtt_ms / 2.0) + j).max(0.0) / 1000.0
	}
}

/// `Arc`/`Mutex`, not `Rc`/`RefCell`: `NetTransport` is `Send`, because
/// the native transport really does live on another thread. Single-
/// threaded here, so the lock is never contended.
type WireRef = std::sync::Arc<std::sync::Mutex<Wire>>;
type RngRef = std::sync::Arc<std::sync::Mutex<Rng>>;

/// The client's end of the impaired link.
struct LinkTransport {
	wire: WireRef,
	rng: RngRef,
	link: Link,
	connected: bool,
}

impl digblock_net::transport::NetTransport for LinkTransport {
	fn send_datagram(&mut self, bytes: &[u8]) {
		let mut w = self.wire.lock().unwrap();
		let mut rng = self.rng.lock().unwrap();
		w.sent_dgrams += 1;
		if rng.unit() < self.link.loss {
			w.dropped_dgrams += 1;
			return;
		}
		let at = w.now + w.one_way(&mut rng, &self.link);
		w.to_server_dgram.push(InFlight {
			at,
			bytes: bytes.to_vec(),
		});
	}

	fn send_reliable(&mut self, bytes: &[u8]) {
		let mut w = self.wire.lock().unwrap();
		let mut rng = self.rng.lock().unwrap();
		let at = (w.now + w.one_way(&mut rng, &self.link)).max(w.rel_ready_server);
		w.rel_ready_server = at;
		w.to_server_rel.push(InFlight {
			at,
			bytes: bytes.to_vec(),
		});
	}

	fn poll(&mut self, out: &mut Vec<digblock_net::transport::TransportEvent>) {
		use digblock_net::transport::TransportEvent;
		if !self.connected {
			self.connected = true;
			out.push(TransportEvent::Connected);
		}
		let mut w = self.wire.lock().unwrap();
		let now = w.now;
		// Reliable first and in order, then datagrams (which may reorder).
		let mut rel: Vec<Vec<u8>> = Vec::new();
		w.to_client_rel.retain(|p| {
			if p.at <= now {
				rel.push(p.bytes.clone());
				false
			} else {
				true
			}
		});
		for b in rel {
			out.push(TransportEvent::Reliable(b));
		}
		let mut dg: Vec<Vec<u8>> = Vec::new();
		w.to_client_dgram.retain(|p| {
			if p.at <= now {
				dg.push(p.bytes.clone());
				false
			} else {
				true
			}
		});
		for b in dg {
			out.push(TransportEvent::Datagram(b));
		}
	}

	fn status(&self) -> digblock_net::transport::ConnStatus {
		digblock_net::transport::ConnStatus::Connected
	}
}

/// Ground plane for the link sweep's flat world.
const FLAT_TOP: usize = 126;

/// Stone up to `FLAT_TOP`, air above — the harness's flat world, rebuilt
/// here because that one is `#[cfg(test)]`.
fn flat_source() -> digblock_server::store::ColumnSource {
	use digblock_core::chunk::{ChunkData, AIR_CHUNK, WORLD_VCHUNK};
	use std::sync::Arc;
	Arc::new(|_cx, _cz| {
		let mut full = ChunkData::default();
		let mut top = ChunkData::default();
		for y in 0..16 {
			for z in 0..16 {
				for x in 0..16 {
					full.set(x, y, z, digblock_core::block::BlockType::Stone);
					if y <= FLAT_TOP % 16 {
						top.set(x, y, z, digblock_core::block::BlockType::Stone);
					}
				}
			}
		}
		let (full, top) = (Arc::new(full), Arc::new(top));
		(0..WORLD_VCHUNK as usize)
			.map(|cy| match cy.cmp(&(FLAT_TOP / 16)) {
				std::cmp::Ordering::Less => full.clone(),
				std::cmp::Ordering::Equal => top.clone(),
				std::cmp::Ordering::Greater => AIR_CHUNK.clone(),
			})
			.collect()
	})
}

struct LinkResult {
	rtt_ms: f32,
	jitter_ms: f32,
	snaps_per_sec: f32,
	replays_per_sec: f32,
	mean_corr: f32,
	max_corr: f32,
	max_divergence: f32,
	input_delivery: f64,
	dgram_loss: f64,
}

/// One client, one server, `secs` of walking over `link`.
fn run_link(link: Link, secs: f64) -> LinkResult {
	use digblock_core::player::{Player, PlayerIntent};
	use digblock_net::client::{NetClient, NetClientConfig};
	use digblock_net::codec::{self, FrameReader};
	use digblock_net::messages::Datagram;
	use digblock_net::predict::Reconciled;
	use digblock_net::protocol::PREAMBLE_LEN;
	use digblock_net::NetEvent;

	let dir = std::env::temp_dir().join(format!(
		"digblock-netbench-link-{}-{}",
		link.name,
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	// A FLAT world, not real worldgen. This half measures the protocol,
	// and real generation would put worker-thread completion times into
	// the loop: which columns are resident on which tick changes what the
	// physics does, and the rows stop being reproducible. Flat ground
	// generates fast and identically every run, so a difference between
	// two runs is a difference in the netcode.
	let mut sim = Sim::new_with_source(
		42.0,
		&dir,
		6,
		2,
		flat_source(),
		glam::Vec3::new(8.5, FLAT_TOP as f32 + 2.5, 8.5),
	)
	.expect("open bench world");

	let wire: WireRef = Default::default();
	let rng: RngRef = std::sync::Arc::new(std::sync::Mutex::new(Rng(0x5EED_1234_ABCD_0001)));
	let transport = LinkTransport {
		wire: wire.clone(),
		rng: rng.clone(),
		link,
		connected: false,
	};
	let mut client = NetClient::new(
		Box::new(transport),
		NetClientConfig {
			name: "link".into(),
			draw_dist: 20,
			token: [0; 16],
			identity: [7; 16],
		},
	);

	let (out_tx, mut out_rx) = channel(OUT_CAP);
	sim.tick_once(vec![SimEvent::Connected {
		conn_id: 1,
		out: out_tx,
		addr: "127.0.0.1:40000".parse().unwrap(),
	}]);

	// Server→client needs the preamble the real net layer writes; the sim
	// only produces frames.
	let mut server_preamble_sent = false;
	let mut server_frames = FrameReader::default();
	let mut client_preamble_seen = 0usize;

	let mut player: Option<Player> = None;
	let mut corrections: Vec<f32> = Vec::new();
	let mut max_div = 0.0f32;
	let mut predicted_ticks = 0u64;
	let mut consumed_base = 0u64;
	let mut events = Vec::new();
	// Warm up before measuring: the client is not `live` server-side until
	// its spawn column has been generated, and until then it gets no
	// snapshots at all — which would read as 100% loss rather than as the
	// startup it is. Also long enough for a 500 ms link to finish its
	// handshake several times over.
	let warmup = (4.0 * TICK_RATE as f64) as u32;
	let steps = warmup + (secs * TICK_RATE as f64) as u32;
	let dt = 1.0 / TICK_RATE as f32;

	for step in 0..steps {
		if step == warmup {
			// Discard everything the startup transient produced.
			corrections.clear();
			max_div = 0.0;
			predicted_ticks = 0;
			consumed_base = sim.inputs_consumed(1);
		}
		// Virtual time drives every measurement, but the chunk workers are
		// real threads: without yielding, terrain never finishes generating
		// and the player walks into a world that is not there yet.
		std::thread::sleep(std::time::Duration::from_millis(1));
		wire.lock().unwrap().now += dt as f64;

		// ---- client half -------------------------------------------------
		events.clear();
		client.poll(&mut events, dt);
		let mut ack: Option<(u32, digblock_core::player::PlayerState)> = None;
		for ev in &events {
			match ev {
				NetEvent::Welcome { spawn, .. } => player = Some(Player::new(*spawn)),
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => ack = Some((*acked_input_tick, *me)),
				_ => {}
			}
		}
		if let (Some(p), Some((t, me))) = (player.as_mut(), ack) {
			let world = |x: i32, y: i32, z: i32| sim.cell_at(x, y, z);
			let outcome = client.predictor.reconcile(p, t, me, &world);
			if let Reconciled::Replayed { error } = outcome {
				corrections.push(error.length());
			}
			client.note_reconciled(matches!(outcome, Reconciled::Replayed { .. }));
		}
		if let Some(p) = player.as_mut() {
			// Walk a slow circle so the world under the player changes.
			let a = step as f32 / TICK_RATE as f32 * 0.5;
			let dir = glam::Vec3::new(a.sin(), 0.0, a.cos());
			let world = |x: i32, y: i32, z: i32| sim.cell_at(x, y, z);
			client.predictor.predict(
				p,
				PlayerIntent {
					move_axis: glam::Vec2::new(0.0, 1.0),
					jump: false,
				},
				dir,
				&world,
			);
			predicted_ticks += 1;
			client.send_inputs();
		}

		// ---- wire → server ------------------------------------------------
		let now = wire.lock().unwrap().now;
		let mut rel: Vec<Vec<u8>> = Vec::new();
		let mut dgs: Vec<Vec<u8>> = Vec::new();
		{
			let mut w = wire.lock().unwrap();
			w.to_server_rel.retain(|p| {
				if p.at <= now {
					rel.push(p.bytes.clone());
					false
				} else {
					true
				}
			});
			w.to_server_dgram.retain(|p| {
				if p.at <= now {
					dgs.push(p.bytes.clone());
					false
				} else {
					true
				}
			});
		}
		let mut sim_events: Vec<SimEvent> = Vec::new();
		for mut b in rel {
			// Strip the client's one-time preamble, as net.rs does.
			if client_preamble_seen < PREAMBLE_LEN {
				let take = (PREAMBLE_LEN - client_preamble_seen).min(b.len());
				client_preamble_seen += take;
				b.drain(..take);
			}
			server_frames.extend(&b);
			while let Ok(Some(f)) = server_frames.next_frame() {
				if let Ok(msg) = codec::decode(&f) {
					sim_events.push(SimEvent::Msg { conn_id: 1, msg });
				}
			}
		}
		for b in dgs {
			if let Ok(Datagram::Input(batch)) = codec::decode_datagram(&b) {
				sim_events.push(SimEvent::Inputs { conn_id: 1, batch });
			}
		}

		sim.tick_once(sim_events);

		// ---- server → wire ------------------------------------------------
		while let Ok(msg) = out_rx.try_recv() {
			let mut w = wire.lock().unwrap();
			let mut r = rng.lock().unwrap();
			match msg {
				OutMsg::Datagram(b) => {
					w.sent_dgrams += 1;
					if r.unit() < link.loss {
						w.dropped_dgrams += 1;
						continue;
					}
					let at = w.now + w.one_way(&mut r, &link);
					w.to_client_dgram.push(InFlight { at, bytes: b });
				}
				OutMsg::Frame(b) => {
					let mut bytes = Vec::new();
					if !server_preamble_sent {
						server_preamble_sent = true;
						bytes.extend_from_slice(&digblock_net::protocol::encode_preamble(
							PROTOCOL_VERSION,
						));
					}
					bytes.extend_from_slice(&b);
					let at = (w.now + w.one_way(&mut r, &link)).max(w.rel_ready_client);
					w.rel_ready_client = at;
					w.to_client_rel.push(InFlight { at, bytes });
				}
				OutMsg::Close => {}
			}
		}

		// Divergence: what the player sees vs what the server holds. This
		// is the number that becomes a rubber-band if it grows.
		if let (Some(p), Some(sp)) = (player.as_ref(), sim.player_pos(1)) {
			max_div = max_div.max((p.pos - sp).length());
		}
	}

	let s = client.stats();
	// Inputs the server actually applied against the ticks the client
	// produced: the direct measure of whether INPUT_REDUNDANCY is covering
	// the loss. Below 1.0 means ticks vanished for good.
	let consumed = sim.inputs_consumed(1) - consumed_base;
	let delivery = if predicted_ticks > 0 {
		consumed as f64 / predicted_ticks as f64
	} else {
		0.0
	};
	let (sent, dropped) = {
		let w = wire.lock().unwrap();
		(w.sent_dgrams, w.dropped_dgrams)
	};
	let loss = if sent > 0 {
		dropped as f64 / sent as f64
	} else {
		0.0
	};
	sim.shutdown();
	let _ = std::fs::remove_dir_all(&dir);
	LinkResult {
		rtt_ms: s.rtt_ms,
		jitter_ms: s.jitter_ms,
		snaps_per_sec: s.snapshots_per_sec,
		replays_per_sec: s.replays_per_sec,
		mean_corr: if corrections.is_empty() {
			0.0
		} else {
			corrections.iter().sum::<f32>() / corrections.len() as f32
		},
		max_corr: corrections.iter().copied().fold(0.0, f32::max),
		max_divergence: max_div,
		input_delivery: delivery,
		dgram_loss: loss,
	}
}

fn run_links(secs: f64) {
	println!("\nlink quality — one client, one server, {secs:.0}s of walking each");
	println!("real NetClient against the real Sim over a seeded impairing wire\n");
	println!(
		"link     | cfg rtt  loss | seen rtt  +-jit | snap/s | replay/s | corr mean  max | max div | in-deliv"
	);
	println!(
		"---------+---------------+-----------------+--------+----------+----------------+---------+---------"
	);
	for &l in LINKS {
		let r = run_link(l, secs);
		println!(
			"{:<8} | {:>5.0}ms {:>4.0}% | {:>7.1} {:>7.1} | {:>6.1} | {:>8.2} | {:>9.3} {:>4.2} | {:>7.2} | {:>7.1}%",
			l.name,
			l.rtt_ms,
			l.loss * 100.0,
			r.rtt_ms,
			r.jitter_ms,
			r.snaps_per_sec,
			r.replays_per_sec,
			r.mean_corr,
			r.max_corr,
			r.max_divergence,
			r.input_delivery * 100.0,
		);
		let _ = r.dgram_loss;
	}
	println!(
		"\nseen rtt  what the CLIENT measures on its own input->ack loop — the overlay's\n\
		 \x20         number. It sits ABOVE the configured wire rtt on purpose: it includes\n\
		 \x20         the send stride, the server's jitter buffer and the snapshot cadence,\n\
		 \x20         which is the latency a player feels rather than the one a ping shows."
	);
	println!(
		"corr      reconciliation corrections in blocks. These are smoothed into the camera\n\
		 \x20         over ~200ms, so small numbers are invisible; a growing max is a rubber-band."
	);
	println!(
		"in-deliv  inputs the server applied against the ticks the client produced. Near 100%\n\
		 \x20         under loss is INPUT_REDUNDANCY doing its job; a shortfall is lost motion."
	);
	println!(
		"\nKNOWN: in-deliv falls off on the last two rows, and it is JITTER, not loss, that\n\
		 does it. Heavy jitter reorders datagrams, and `handle_inputs` skips anything at or\n\
		 below `last_input_tick` — a high-water mark — so a tick that arrives after a newer\n\
		 one has already been consumed is discarded rather than applied out of order. With\n\
		 JITTER_BUFFER_TICKS at 2 (33 ms) the buffer cannot absorb +-150 ms of reordering.\n\
		 Deepening it would fix these rows and cost every other player latency, so it is a\n\
		 tradeoff to make deliberately (adaptively, on measured jitter) rather than by\n\
		 raising a constant."
	);
}
