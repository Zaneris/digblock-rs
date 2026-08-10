//! Shared sim-test harness: an in-memory client over the `OutMsg`
//! channel, and the flat synthetic world that makes tests instant.

use super::{OutMsg, Sim, SimEvent, OUT_CAP};
use crate::store::ColumnSource;
use digblock_core::block::BlockType;
use digblock_core::chunk::{ChunkData, AIR_CHUNK, WORLD_VCHUNK};
use digblock_core::player::PlayerState;
use digblock_net::codec::{self, FrameReader};
use digblock_net::messages::{ClientMsg, Datagram, InputBatch, InputTick, ServerMsg, Snapshot};
use digblock_net::protocol::PROTOCOL_VERSION;
use glam::Vec3;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Receiver};

pub(crate) const SEED: f64 = 42.0;

pub(crate) fn temp_dir(tag: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("dbsim-{tag}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	dir
}

/// Flat synthetic world: stone through y = 126, air above. Column
/// generation is microseconds, so tests that don't care about terrain
/// shape never pay real worldgen.
pub(crate) const FLAT_GROUND_TOP: usize = 126;
/// Standing eye height over the flat ground: `top + 2.5`, exactly what
/// `player::spawn` returns for a surface block — grounded from tick 1.
pub(crate) const FLAT_SPAWN: Vec3 = Vec3::new(8.5, FLAT_GROUND_TOP as f32 + 2.5, 8.5);

pub(crate) fn flat_source() -> ColumnSource {
	Arc::new(|_cx, _cz| {
		let mut full = ChunkData::default();
		let mut top = ChunkData::default();
		for y in 0..16 {
			for z in 0..16 {
				for x in 0..16 {
					full.set(x, y, z, BlockType::Stone);
					if y <= FLAT_GROUND_TOP % 16 {
						top.set(x, y, z, BlockType::Stone);
					}
				}
			}
		}
		let (full, top) = (Arc::new(full), Arc::new(top));
		(0..WORLD_VCHUNK as usize)
			.map(|cy| match cy.cmp(&(FLAT_GROUND_TOP / 16)) {
				std::cmp::Ordering::Less => full.clone(),
				std::cmp::Ordering::Equal => top.clone(),
				std::cmp::Ordering::Greater => AIR_CHUNK.clone(),
			})
			.collect()
	})
}

pub(crate) fn flat_sim(tag: &str) -> Sim {
	let dir = temp_dir(tag);
	Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap()
}

pub(crate) struct TestClient {
	pub(crate) rx: Receiver<OutMsg>,
	pub(crate) frames: FrameReader,
	pub(crate) msgs: Vec<ServerMsg>,
	pub(crate) snaps: Vec<Snapshot>,
}

impl TestClient {
	pub(crate) fn drain(&mut self) {
		while let Ok(out) = self.rx.try_recv() {
			match out {
				OutMsg::Frame(bytes) => {
					self.frames.extend(&bytes);
					while let Some(frame) = self.frames.next_frame().unwrap() {
						self.msgs.push(codec::decode(&frame).unwrap());
					}
				}
				OutMsg::Datagram(bytes) => {
					if let Datagram::Snapshot(s) = codec::decode_datagram(&bytes).unwrap() {
						self.snaps.push(s);
					}
				}
				OutMsg::Close => {}
			}
		}
	}
}

/// Per-conn dummy peer address: distinct IPs (10.0.x.y) so ban tests
/// never cross-ban unrelated test clients.
pub(crate) fn test_addr(conn_id: u64) -> std::net::SocketAddr {
	let (hi, lo) = ((conn_id >> 8) as u8, conn_id as u8);
	std::net::SocketAddr::from(([10, 0, hi, lo], 40000))
}

pub(crate) fn connect(sim: &mut Sim, conn_id: u64) -> TestClient {
	connect_with_identity(sim, conn_id, [0; 16])
}

pub(crate) fn connect_with_identity(sim: &mut Sim, conn_id: u64, identity: [u8; 16]) -> TestClient {
	let mut c = try_connect(sim, conn_id, identity);
	assert!(
		matches!(c.msgs.first(), Some(ServerMsg::Welcome { .. })),
		"expected Welcome, got {:?}",
		c.msgs.first()
	);
	c.drain();
	c
}

/// Like `connect` but joining under an explicit display name (admin
/// tests key on names).
pub(crate) fn connect_named(sim: &mut Sim, conn_id: u64, name: &str) -> TestClient {
	let mut c = try_connect_as(sim, conn_id, [0; 16], name);
	assert!(
		matches!(c.msgs.first(), Some(ServerMsg::Welcome { .. })),
		"expected Welcome, got {:?}",
		c.msgs.first()
	);
	c.drain();
	c
}

/// Like `connect` but without asserting the handshake succeeded — for
/// tests exercising refusals.
pub(crate) fn try_connect(sim: &mut Sim, conn_id: u64, identity: [u8; 16]) -> TestClient {
	let name = format!("t{conn_id}");
	try_connect_as(sim, conn_id, identity, &name)
}

pub(crate) fn try_connect_as(
	sim: &mut Sim,
	conn_id: u64,
	identity: [u8; 16],
	name: &str,
) -> TestClient {
	let (tx, rx) = channel(OUT_CAP);
	sim.tick_once(vec![
		SimEvent::Connected {
			conn_id,
			out: tx,
			addr: test_addr(conn_id),
		},
		SimEvent::Msg {
			conn_id,
			msg: ClientMsg::Hello {
				protocol: PROTOCOL_VERSION,
				name: name.into(),
				draw_dist: 4,
				token: [0; 16],
				sim_version: digblock_core::SIM_VERSION,
				build: "test".into(),
				identity,
			},
		},
	]);
	let mut c = TestClient {
		rx,
		frames: FrameReader::default(),
		msgs: Vec::new(),
		snaps: Vec::new(),
	};
	c.drain();
	c
}

pub(crate) fn wait_live(sim: &mut Sim, conn_id: u64) {
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	while !sim.test_is_live(conn_id) {
		assert!(
			std::time::Instant::now() < deadline,
			"client never went live"
		);
		sim.tick_once(vec![]);
		std::thread::sleep(std::time::Duration::from_millis(1));
	}
}

/// One aim-holding input: stand still, look along `dir`, claim nothing.
pub(crate) fn aim(tick: u32, dir: Vec3) -> InputTick {
	InputTick {
		tick,
		move_axis: [0.0, 0.0],
		jump: false,
		look: dir.to_array(),
		claimed_pos: [0.0, -500.0, 0.0], // never adopted: server physics
		claimed_vel: [0.0; 3],
		active_slot: 0,
	}
}

/// Drive the full timed-dig flow: DigStart on the cell `dir` hits from
/// the player's position, `hold_ticks` sim ticks of aim-holding inputs,
/// then the completing EditRequest. `t` is the caller's monotonically
/// increasing input-tick counter. Returns the dug cell.
pub(crate) fn dig_hold(
	sim: &mut Sim,
	conn_id: u64,
	dir: Vec3,
	hold_ticks: u32,
	seq: u32,
	kind: digblock_net::messages::EditKind,
	t: &mut u32,
) -> (i32, i32, i32) {
	use digblock_core::raycast::{raycast, raycast_fluid, REACH};
	let pos = sim.test_player(conn_id).unwrap().pos;
	let cast = if kind == digblock_net::messages::EditKind::Collect {
		raycast_fluid
	} else {
		raycast
	};
	let hit = cast(pos, dir, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("dig target");
	// These tests exercise the EDIT path, not the rock gate: grant the
	// target's gate level if the client is under it (the dedicated gate
	// test drives DigStart raw). Callers pass base-hardness-flavored
	// hold lengths; stretch them by the effective level's FAMILY
	// multiplier (SIM v12) so a "full hold" stays a full hold and a
	// deliberately-short one stays short.
	let gate = digblock_core::skills::dig_level_gate(hit.block);
	let level = sim.test_mining_level(conn_id).max(gate);
	sim.bench_set_mining_level(conn_id, level);
	let permille = digblock_core::skills::dig_speed_permille(hit.block.dig_family(), level);
	let hold_ticks = (hold_ticks * permille).div_ceil(1000);
	*t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			},
		},
		batch(vec![aim(*t, dir)], conn_id),
	]);
	for _ in 0..hold_ticks {
		*t += 1;
		sim.tick_once(vec![batch(vec![aim(*t, dir)], conn_id)]);
	}
	sim.tick_once(vec![SimEvent::Msg {
		conn_id,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: dir.to_array(),
			kind,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			// Collect needs the WaterTool's slot (fresh bars pin it last).
			slot: if kind == digblock_net::messages::EditKind::Collect {
				digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1
			} else {
				0
			},
		},
	}]);
	hit.cell
}

pub(crate) fn input(tick: u32, forward: bool, claimed: PlayerState) -> InputTick {
	InputTick {
		tick,
		move_axis: [0.0, if forward { 1.0 } else { 0.0 }],
		jump: false,
		look: [0.0, 0.0, 1.0],
		claimed_pos: claimed.pos.to_array(),
		claimed_vel: claimed.vel.to_array(),
		active_slot: 0,
	}
}

/// A claim that is never adopted (far outside the envelope), for
/// tests that want pure server physics.
pub(crate) fn no_claim() -> PlayerState {
	PlayerState {
		pos: Vec3::new(0.0, -500.0, 0.0),
		vel: Vec3::ZERO,
	}
}

pub(crate) fn batch(inputs: Vec<InputTick>, conn_id: u64) -> SimEvent {
	SimEvent::Inputs {
		conn_id,
		batch: InputBatch { inputs },
	}
}
