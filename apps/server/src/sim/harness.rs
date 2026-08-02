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

pub(crate) fn connect(sim: &mut Sim, conn_id: u64) -> TestClient {
	let (tx, rx) = channel(OUT_CAP);
	sim.tick_once(vec![
		SimEvent::Connected { conn_id, out: tx },
		SimEvent::Msg {
			conn_id,
			msg: ClientMsg::Hello {
				protocol: PROTOCOL_VERSION,
				name: format!("t{conn_id}"),
				draw_dist: 4,
				token: [0; 16],
				sim_version: digblock_core::SIM_VERSION,
				build: "test".into(),
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
	assert!(
		matches!(c.msgs.first(), Some(ServerMsg::Welcome { .. })),
		"expected Welcome, got {:?}",
		c.msgs.first()
	);
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

pub(crate) fn input(tick: u32, forward: bool, claimed: PlayerState) -> InputTick {
	InputTick {
		tick,
		move_axis: [0.0, if forward { 1.0 } else { 0.0 }],
		jump: false,
		look: [0.0, 0.0, 1.0],
		claimed_pos: claimed.pos.to_array(),
		claimed_vel: claimed.vel.to_array(),
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
