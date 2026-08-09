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
	/// The harness's own prediction, run with the same physics the server
	/// runs, so the claim it sends is adoptable. Claims that miss short-
	/// circuit the `sane` chain on POS_TOLERANCE and skip `probe_solid`
	/// entirely — the shape-aware capsule test, and the most expensive call
	/// in the input path. Measuring the server without it understates every
	/// per-client number here.
	predicted: Option<digblock_core::player::Player>,
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
			predicted: None,
		});
	}
	clients
}

/// Walk everyone in a slow circle, each on their own phase, so players
/// spread out and cross column borders — the residency and interest paths
/// only cost anything when people move.
fn look_for(conn_id: u64, tick: u32, n: usize) -> glam::Vec3 {
	let phase = conn_id as f32 / n.max(1) as f32 * std::f32::consts::TAU;
	let t = tick as f32 / TICK_RATE as f32;
	let (sx, sz) = (phase + t * 0.4).sin_cos();
	glam::Vec3::new(sx, 0.0, sz)
}

const WALK_INTENT: digblock_core::player::PlayerIntent = digblock_core::player::PlayerIntent {
	move_axis: glam::Vec2::new(0.0, 1.0),
	jump: false,
};

fn input_for(c: &Client, tick: u32, n: usize) -> SimEvent {
	let look = look_for(c.conn_id, tick, n);
	// The claim is the harness's own prediction, run against the same world
	// with the same physics — so it lands inside the adoption envelope and
	// the server pays for `probe_solid` the way it does for a real player.
	let (pos, vel) = c
		.predicted
		.as_ref()
		.map_or(([0.0, -500.0, 0.0], [0.0; 3]), |p| {
			let s = p.state();
			(s.pos.to_array(), s.vel.to_array())
		});
	SimEvent::Inputs {
		conn_id: c.conn_id,
		batch: InputBatch {
			inputs: vec![InputTick {
				tick,
				move_axis: [0.0, 1.0],
				jump: false,
				look: look.to_array(),
				claimed_pos: pos,
				claimed_vel: vel,
				active_slot: 0,
			}],
		},
	}
}

/// Step every client's local prediction one tick, exactly as the server
/// will.
///
/// Re-seeded from the server's authoritative state every tick, which is
/// what a real client does on reconcile — without it the two diverge for
/// good the first time streaming freezes the server's physics on an
/// unloaded column (`step_physics` holds position and consumes nothing
/// while the prediction walks on), and every later claim falls outside the
/// envelope. The claim is then always "server state plus one tick", which
/// is exactly what an honest client sends.
fn predict_clients(clients: &mut [Client], sim: &Sim, tick: u32, n: usize) {
	for c in clients.iter_mut() {
		let Some(state) = sim.player_state(c.conn_id) else {
			continue;
		};
		let p = c
			.predicted
			.get_or_insert_with(|| digblock_core::player::Player::new(state.pos));
		p.set_state(state);
		let look = look_for(c.conn_id, tick, n);
		p.tick(
			digblock_core::player::TICK,
			look,
			WALK_INTENT,
			&|x, y, z| sim.cell_at(x, y, z),
		);
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
		let base = 100 + t as u32;
		predict_clients(&mut clients, &sim, base, n);
		let evs: Vec<SimEvent> = clients.iter().map(|c| input_for(c, base, n)).collect();
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
	let mut allocs = 0usize;
	let mut alloc_bytes = 0usize;
	let mut adopted = 0u64;
	for t in 0..TICKS {
		let base = 100 + (WARMUP + t) as u32;
		predict_clients(&mut clients, &sim, base, n);
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
		// Bracket allocations around `tick_once` ALONE, matching the timer.
		// Spanning the whole loop body counted the harness's own per-client
		// `vec![]` and the `Vec<SimEvent>` collect above — at n=64 that was
		// ~65 of the 88.6 allocs/tick being reported as the server's.
		let a0 = ALLOCS.load(Ordering::Relaxed);
		let b0 = ALLOC_BYTES.load(Ordering::Relaxed);
		let start = Instant::now();
		sim.tick_once(evs);
		let el = start.elapsed().as_secs_f64() * 1.0e6;
		allocs += ALLOCS.load(Ordering::Relaxed) - a0;
		alloc_bytes += ALLOC_BYTES.load(Ordering::Relaxed) - b0;
		tick_us.push(el);
		// Confirm the claims really are being adopted. Without this check the
		// sweep can silently revert to measuring the cheap path: the `sane`
		// chain short-circuits on POS_TOLERANCE before `probe_solid`, so a
		// claim that misses by too much skips the shape-aware capsule test
		// entirely — the most expensive call in the input path.
		for c in clients.iter() {
			if let (Some(p), Some(pos)) = (c.predicted.as_ref(), sim.player_pos(c.conn_id)) {
				if p.pos.distance(pos) < 1.0e-4 {
					adopted += 1;
				}
			}
		}
		drain(&mut clients);
	}
	let alloc_kb = alloc_bytes / 1024;
	assert!(
		adopted * 4 >= TICKS as u64 * n as u64,
		"claims are being adopted, so overlaps_solid is in the measurement \
		 ({adopted} of {} client-ticks)",
		TICKS as u64 * n as u64
	);

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
	println!(
		"alloc/t allocations inside `tick_once` alone — the harness's own per-tick vecs\n\
		 \x20       are outside the bracket. Chunk workers share the process-global counter,\n\
		 \x20       so streaming allocation lands here too; it is server cost either way.\n\
		 \x20       Every client claims an adoptable position, so `probe_solid` is in the\n\
		 \x20       measurement (asserted, not assumed)."
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

/// Wire/client sub-steps per 60 Hz server tick (2.08 ms each). The server
/// still ticks at 60 Hz; this is the resolution at which packets are
/// delivered and the client polls.
const SUBSTEPS: u32 = 8;

struct LinkResult {
	rtt_ms: f64,
	rtt_p95: f64,
	snaps_per_sec: f64,
	replays_per_sec: f64,
	mean_corr: f32,
	max_corr: f32,
	max_divergence: f32,
	input_delivery: f64,
	dgram_loss: f64,
	/// Server-side view of the same window: inputs lost for good because
	/// they arrived below the high-water mark having never been held, the
	/// depth that would have been needed to hold the worst of them, and
	/// what the drain's buffer depth actually was.
	late_per_sec: f64,
	late_want_max: u32,
	depth_mean: f64,
	/// Edit-path view of the same window, populated only when the row ran
	/// the click scenario. See [`EditDriver`].
	orig_mean: f64,
	orig_max: f32,
	tol_rej_per_sec: f64,
	dig_ok: u64,
	dig_attempts: u64,
	dig_short_max: u32,
}

/// Idle ticks between dig attempts (1 s), so a row gets several complete
/// holds rather than one long one.
const DIG_IDLE_TICKS: u32 = 60;
/// How often the walking client fires a residual probe (6/s, comfortably
/// inside `EDIT_RATE_PER_SEC` 10 so it never starves the real digs).
const PROBE_STRIDE: u32 = 10;
/// How far to the side of the heading the client aims, in blocks.
const TARGET_OFFSET: f32 = 3.0;

/// Drives the click half of a link row: a residual probe while walking, and
/// a stationary hold-to-dig on a cadence.
///
/// The two are deliberately different postures, because they measure
/// different things. The probe runs while WALKING — the residual it samples
/// is what `EDIT_POS_TOLERANCE` has to cover for a moving player, which is
/// the budget any change to input scheduling spends. The dig is STATIONARY
/// and looks at its target, because that is how a player actually mines,
/// and because `verify_digs` re-raycasts from the server's own `look`: a
/// client that kept walking would never have the server aiming at the cell
/// and every hold would fail for a reason that has nothing to do with the
/// link.
#[derive(Default)]
struct EditDriver {
	seq: u32,
	cell: (i32, i32, i32),
	aim: glam::Vec3,
	/// Ticks left in the current hold; non-zero means "standing still,
	/// looking at `cell`".
	holding: u32,
	idle: u32,
	attempts: u64,
}

/// Aim at the top face of a solid cell `TARGET_OFFSET` blocks to the right
/// of the heading. Right of a circular walk is a different circle, so the
/// holes never land on the path the player is walking.
///
/// Aiming at a point just INSIDE the top face is what makes the target
/// unambiguous: the ground is one flat plane, so a descending ray crosses
/// it exactly once, in the column of the point aimed at. A shallower ray at
/// the cell's centre would hit whichever column it entered the ground at
/// instead, which is not the cell being asked for.
fn pick_target(
	eye: glam::Vec3,
	heading: glam::Vec3,
	sim: &Sim,
) -> Option<((i32, i32, i32), glam::Vec3)> {
	let right = glam::Vec3::new(heading.z, 0.0, -heading.x);
	let t = eye + right * TARGET_OFFSET;
	let cell = (t.x.floor() as i32, FLAT_TOP as i32, t.z.floor() as i32);
	let v = sim.cell_at(cell.0, cell.1, cell.2)?;
	let block = digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v));
	if block != digblock_core::block::BlockType::Stone {
		return None;
	}
	let aim_pt = glam::Vec3::new(
		cell.0 as f32 + 0.5,
		FLAT_TOP as f32 + 0.999,
		cell.2 as f32 + 0.5,
	);
	Some((cell, (aim_pt - eye).normalize()))
}

/// Advance the click state machine one client tick. Runs BEFORE the
/// prediction for this tick, so `holding` already describes the posture the
/// input about to be sent will carry.
fn drive_edits(
	d: &mut EditDriver,
	client: &mut digblock_net::client::NetClient,
	sim: &Sim,
	eye: glam::Vec3,
	heading: glam::Vec3,
	step: u32,
	hardness: u32,
) {
	use digblock_net::messages::EditKind;
	let tick = client.predictor.current_tick();
	if d.holding > 0 {
		d.holding -= 1;
		if d.holding == 0 {
			// The hold ran its full client-side timer; claim it. DigStart
			// and this ride the same reliable stream in order, so the
			// server measures the same interval the client held — latency
			// cancels and only jitter is left, which is the point.
			d.seq += 1;
			client.send_edit(d.seq, tick, eye, d.aim, EditKind::Dig, d.cell, (0, 1, 0), 0);
			d.idle = DIG_IDLE_TICKS;
		}
		return;
	}
	if d.idle > 0 {
		d.idle -= 1;
	} else if let Some((cell, aim)) = pick_target(eye, heading, sim) {
		d.cell = cell;
		d.aim = aim;
		d.holding = hardness;
		d.attempts += 1;
		client.send_dig_start(cell);
		return;
	}
	// Residual probe, only while walking. `Interact` deliberately, not a
	// dig: it takes the OTHER tolerance site and is refused before any
	// timed-dig gate, so probing cannot pollute the dig-shortfall counter
	// with completions the client never intended to earn.
	if step % PROBE_STRIDE == 0 {
		if let Some((cell, aim)) = pick_target(eye, heading, sim) {
			d.seq += 1;
			client.send_edit(
				d.seq,
				tick,
				eye,
				aim,
				EditKind::Interact,
				cell,
				(0, 1, 0),
				0,
			);
		}
	}
}

/// One client, one server, `secs` of walking over `link`. With `edits`, the
/// client also probes and digs — see [`EditDriver`].
fn run_link(link: Link, secs: f64, edits: bool) -> LinkResult {
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
	// Measured harness-side over the whole window rather than read out of
	// the client's `StatsAcc`, which publishes a closed 1-second bucket and
	// an EWMA seeded during warmup — both of which report a phase of the run
	// rather than the run. (The wifi row used to print 20.7 snap/s against a
	// 20.0 nominal, which is the artifact admitting itself.)
	let mut rtts: Vec<f32> = Vec::new();
	let mut snaps = 0u64;
	let mut replays = 0u64;
	let mut stats_base = digblock_server::sim::InputStats::default();
	let mut edits_base = digblock_server::sim::EditStats::default();
	let mut driver = EditDriver::default();
	let stone_hardness = u32::from(
		digblock_core::block::BlockType::Stone
			.dig_hardness(FLAT_TOP as i32)
			.expect("stone is diggable"),
	);
	// Warm up before measuring: the client is not `live` server-side until
	// its spawn column has been generated, and until then it gets no
	// snapshots at all — which would read as 100% loss rather than as the
	// startup it is. Also long enough for a 500 ms link to finish its
	// handshake several times over.
	let warmup = (4.0 * TICK_RATE as f64) as u32;
	let steps = warmup + (secs * TICK_RATE as f64) as u32;
	let dt = 1.0 / TICK_RATE as f32;
	let sub_dt = dt / SUBSTEPS as f32;

	for step in 0..steps {
		let measuring = step >= warmup;
		if step == warmup {
			// Discard everything the startup transient produced.
			corrections.clear();
			max_div = 0.0;
			predicted_ticks = 0;
			consumed_base = sim.inputs_consumed(1);
			rtts.clear();
			snaps = 0;
			replays = 0;
			stats_base = sim.input_stats(1);
			edits_base = sim.edit_stats(1);
			driver.attempts = 0;
		}
		// Virtual time drives every measurement, but the chunk workers are
		// real threads: without yielding, terrain never finishes generating
		// and the player walks into a world that is not there yet.
		std::thread::sleep(std::time::Duration::from_millis(1));

		let mut sim_events: Vec<SimEvent> = Vec::new();
		// Sub-step the wire and the client within each server tick. The
		// server still ticks at exactly 60 Hz — a packet arriving mid-tick
		// really does wait for the next boundary, and that quantization is
		// the real thing being modelled. What was NOT real was the delivery
		// check running once per tick against the same `now` the send used
		// (`at = now + one_way` is never `<= now`), so every leg cost a
		// whole 16.67 ms tick no matter how short the configured wire.
		// Below one sub-step (2.08 ms) delays still round up.
		for sub in 0..SUBSTEPS {
			wire.lock().unwrap().now += sub_dt as f64;

			// ---- client half ---------------------------------------------
			events.clear();
			client.poll(&mut events, sub_dt);
			let mut acks: Vec<(u32, digblock_core::player::PlayerState)> = Vec::new();
			for ev in &events {
				match ev {
					NetEvent::Welcome { spawn, .. } => player = Some(Player::new(*spawn)),
					NetEvent::Ack {
						acked_input_tick,
						me,
					} => acks.push((*acked_input_tick, *me)),
					_ => {}
				}
			}
			for (t, me) in acks {
				let Some(p) = player.as_mut() else { continue };
				// One Ack per snapshot: `on_snapshot` emits it for every
				// snapshot a live client receives.
				if measuring {
					snaps += 1;
				}
				let world = |x: i32, y: i32, z: i32| sim.cell_at(x, y, z);
				let outcome = client.predictor.reconcile(p, t, me, &world);
				if let Reconciled::Replayed { error } = outcome {
					if measuring {
						corrections.push(error.length());
						replays += 1;
					}
				}
				// A stale ack returns early without touching `last_rtt`, so
				// sampling it there would re-count the previous round trip.
				if measuring && outcome != Reconciled::StaleAck {
					if let Some(rtt) = client.predictor.last_rtt() {
						rtts.push(rtt * 1000.0);
					}
				}
				client.note_reconciled(matches!(outcome, Reconciled::Replayed { .. }));
			}

			// The client predicts on a 60 Hz accumulator and sends right
			// after, exactly as `engine.rs` does — once per tick, not once
			// per poll.
			if sub == 0 {
				if let Some(p) = player.as_mut() {
					// Walk a slow circle so the world under the player changes.
					let a = step as f32 / TICK_RATE as f32 * 0.5;
					let heading = glam::Vec3::new(a.sin(), 0.0, a.cos());
					// Only click inside the measured window. The `_max`
					// counters are cumulative highs with no base to
					// difference against, so a click during the settle
					// would be indistinguishable from the run's own worst
					// case — and the settle is a fall, which is exactly
					// where the residual is largest.
					if edits && measuring {
						drive_edits(
							&mut driver,
							&mut client,
							&sim,
							p.pos,
							heading,
							step,
							stone_hardness,
						);
					}
					// Mining stands still and looks at the block; walking
					// looks where it is going.
					let (intent, dir) = if driver.holding > 0 {
						(
							PlayerIntent {
								move_axis: glam::Vec2::ZERO,
								jump: false,
							},
							driver.aim,
						)
					} else {
						(
							PlayerIntent {
								move_axis: glam::Vec2::new(0.0, 1.0),
								jump: false,
							},
							heading,
						)
					};
					let world = |x: i32, y: i32, z: i32| sim.cell_at(x, y, z);
					client.predictor.predict(p, intent, dir, &world);
					predicted_ticks += 1;
					client.send_inputs();
				}
			}

			// ---- wire → server --------------------------------------------
			// Delivered at sub-step resolution, but held for the server's own
			// tick boundary below.
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
		if measuring {
			if let (Some(p), Some(sp)) = (player.as_ref(), sim.player_pos(1)) {
				max_div = max_div.max((p.pos - sp).length());
			}
		}
	}

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
	let is = sim.input_stats(1);
	let es = sim.edit_stats(1);
	sim.shutdown();
	let _ = std::fs::remove_dir_all(&dir);

	rtts.sort_by(f32::total_cmp);
	let rtt_mean = if rtts.is_empty() {
		0.0
	} else {
		rtts.iter().map(|&v| v as f64).sum::<f64>() / rtts.len() as f64
	};
	// Depth over the measured window only: the counters are cumulative, so
	// the warmup's samples have to be differenced out.
	let d_samples = is.depth_samples - stats_base.depth_samples;
	let depth_mean = if d_samples == 0 {
		0.0
	} else {
		(is.depth_sum - stats_base.depth_sum) as f64 / d_samples as f64
	};
	LinkResult {
		rtt_ms: rtt_mean,
		rtt_p95: pct(&rtts.iter().map(|&v| v as f64).collect::<Vec<_>>(), 0.95),
		snaps_per_sec: snaps as f64 / secs,
		replays_per_sec: replays as f64 / secs,
		mean_corr: if corrections.is_empty() {
			0.0
		} else {
			corrections.iter().sum::<f32>() / corrections.len() as f32
		},
		max_corr: corrections.iter().copied().fold(0.0, f32::max),
		max_divergence: max_div,
		input_delivery: delivery,
		dgram_loss: loss,
		late_per_sec: (is.late_lost - stats_base.late_lost) as f64 / secs,
		late_want_max: is.late_want_max,
		depth_mean,
		// Differenced out of the warmup the same way depth is: the counters
		// are cumulative and the handshake's first clicks are not the run.
		orig_mean: {
			let n = es.origin_samples - edits_base.origin_samples;
			if n == 0 {
				0.0
			} else {
				(es.origin_sum - edits_base.origin_sum) / n as f64
			}
		},
		orig_max: es.origin_max,
		tol_rej_per_sec: (es.tol_rejects - edits_base.tol_rejects) as f64 / secs,
		dig_ok: es.dig_ok - edits_base.dig_ok,
		dig_attempts: driver.attempts,
		dig_short_max: es.dig_short_max,
	}
}

fn run_links(secs: f64) {
	println!("\nlink quality — one client, one server, {secs:.0}s of walking each");
	println!("real NetClient against the real Sim over a seeded impairing wire\n");
	println!(
		"link     | cfg rtt  loss | drop | rtt mean  p95 | snap/s | repl/s | corr mean  max | max div | in-deliv | late/s want | depth"
	);
	println!(
		"---------+---------------+------+---------------+--------+--------+----------------+---------+----------+-------------+------"
	);
	for &l in LINKS {
		let r = run_link(l, secs, false);
		println!(
			"{:<8} | {:>5.0}ms {:>4.0}% | {:>3.0}% | {:>7.1} {:>6.1} | {:>6.1} | {:>6.2} | {:>9.3} {:>4.2} | {:>7.2} | {:>7.1}% | {:>5.1} {:>4} | {:>4.1}",
			l.name,
			l.rtt_ms,
			l.loss * 100.0,
			r.dgram_loss * 100.0,
			r.rtt_ms,
			r.rtt_p95,
			r.snaps_per_sec,
			r.replays_per_sec,
			r.mean_corr,
			r.max_corr,
			r.max_divergence,
			r.input_delivery * 100.0,
			r.late_per_sec,
			r.late_want_max,
			r.depth_mean,
		);
	}
	println!(
		"\nEvery column is measured over the window, not sampled from a rate bucket, and the\n\
		 wire runs at {SUBSTEPS} sub-steps per server tick so a short link is not charged a whole\n\
		 tick per leg. The server still ticks at 60 Hz: a packet arriving mid-tick waits for\n\
		 the next boundary, which is real. Delays under one sub-step (2.08 ms) round up."
	);
	println!(
		"\ndrop      datagrams the wire actually dropped, against the configured loss. It is\n\
		 \x20         here to prove the impairment model is doing what the row claims."
	);
	println!(
		"rtt       what the CLIENT measures on its own input->ack loop — the overlay's number.\n\
		 \x20         It sits ABOVE the configured wire rtt on purpose: it includes the server's\n\
		 \x20         jitter buffer, the 60 Hz tick boundary and the snapshot cadence, which is\n\
		 \x20         the latency a player feels rather than the one a ping shows. It does NOT\n\
		 \x20         include the send stride: `sent_at` is stamped at transmission, so a tick\n\
		 \x20         waiting for its turn on the wire is felt but not counted."
	);
	println!(
		"corr      reconciliation corrections in blocks. These are smoothed into the camera\n\
		 \x20         over ~200ms, so small numbers are invisible; a growing max is a rubber-band."
	);
	println!(
		"max div   client-predicted position against the server's. Includes the intended\n\
		 \x20         CLIENT_TICK_LEAD, so it is a headroom reading, not pure error."
	);
	println!(
		"in-deliv  inputs the server applied against the ticks the client produced. Near 100%\n\
		 \x20         under loss is INPUT_REDUNDANCY doing its job; a shortfall is lost motion."
	);
	println!(
		"late/s    the server's own view of that shortfall: inputs that arrived below the\n\
		 \x20         high-water mark having NEVER been held, so they were lost for good rather\n\
		 \x20         than being ordinary redundancy copies. `want` is the buffer depth that\n\
		 \x20         would have been needed to still be holding the worst of them."
	);
	println!(
		"depth     mean jitter-buffer depth at drain time, as a tick SPAN rather than a count\n\
		 \x20         (loss punches holes in the map, so a count under-reads exactly when the\n\
		 \x20         link is worst). This is the quantity a fix would control."
	);
	println!(
		"\nWHY in-deliv falls off on the worst rows: JITTER, not loss. Heavy jitter reorders\n\
		 datagrams, and `handle_inputs` skips anything at or below `last_input_tick` — a\n\
		 high-water mark — so a tick arriving after a newer one has been consumed used to be\n\
		 discarded rather than applied out of order. Redundancy cannot help, because every\n\
		 copy of a tick is equally old.\n\
		 \n\
		 `INPUT_HOLD_MAX` is what recovers them: the drain refuses to step past a HOLE in the\n\
		 tick sequence for a few ticks, so the late input is still admitted when it lands.\n\
		 It fires only when a hole actually exists, which is why the lan and wifi rows are\n\
		 unchanged to the digit. Note that this is not the same knob as JITTER_BUFFER_TICKS,\n\
		 which only ever set the CATCH-UP threshold — the drain has no way to consume zero\n\
		 without the hold, so depth was a one-way ratchet that raising a constant could not\n\
		 have fixed."
	);
	run_edit_links(secs);
}

/// The click half, as a SEPARATE table rather than extra columns on the one
/// above. Two reasons, both load-bearing: the scenarios differ (probes walk,
/// digs stand still), and the edits put reliable frames on the wire, which
/// consumes draws from the shared `Rng` — folding them in would shift every
/// number in the walk table and destroy its comparability with the recorded
/// baseline.
fn run_edit_links(secs: f64) {
	use digblock_net::protocol::EDIT_POS_TOLERANCE;
	println!("\nedit path — same links, same wire, but the client also clicks\n");
	println!("link     | cfg rtt  loss | orig mean   max | tol-rej/s | dig ok/att | dig short");
	println!("---------+---------------+-----------------+-----------+------------+----------");
	for &l in LINKS {
		let r = run_link(l, secs, true);
		println!(
			"{:<8} | {:>5.0}ms {:>4.0}% | {:>9.3} {:>5.3} | {:>9.2} | {:>5}/{:<5} | {:>9}",
			l.name,
			l.rtt_ms,
			l.loss * 100.0,
			r.orig_mean,
			r.orig_max,
			r.tol_rej_per_sec,
			r.dig_ok,
			r.dig_attempts,
			r.dig_short_max,
		);
	}
	println!(
		"\norig      how far the claimed edit origin sat from the server's own simulated\n\
		 \x20         position, in blocks — the quantity EDIT_POS_TOLERANCE ({EDIT_POS_TOLERANCE})\n\
		 \x20         bounds, sampled at both tolerance sites BEFORE either rejects. Sampled\n\
		 \x20         while WALKING, which is the expensive posture. Latency itself cancels\n\
		 \x20         here: the request and the server's position are delayed by the same one\n\
		 \x20         way, so what is left is buffer depth plus arrival jitter. That is why\n\
		 \x20         this column, not `max div`, is what a change to input scheduling spends."
	);
	println!(
		"tol-rej/s requests refused by that check specifically, kept apart from the other\n\
		 \x20         OutOfReach paths (a ray that hit nothing, a degenerate direction)."
	);
	println!(
		"dig       completing digs the timed-dig gate accepted, over attempts started. Digs\n\
		 \x20         are STATIONARY and look at the target, because that is how mining works\n\
		 \x20         and because `verify_digs` re-raycasts from the server's own look — a\n\
		 \x20         client that kept walking would fail every hold for a reason that is not\n\
		 \x20         the link. Stone is 90 ticks, so a row gets a handful of attempts: enough\n\
		 \x20         to separate `all pass` from `all fail`, which is the question."
	);
	println!(
		"dig short the worst shortfall in banked aim-verified ticks after dig_grace. Zero\n\
		 \x20         means the grace still had room to give on that link."
	);
}
