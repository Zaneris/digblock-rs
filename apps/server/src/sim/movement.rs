//! Movement: input intake, per-tick server physics, and the adoption
//! envelope + sustained-drift gate (model described in the `sim` docs).

use super::{Client, Sim};
use digblock_core::player::{PlayerIntent, PlayerState, TICK};
use digblock_net::messages::{InputBatch, InputTick};
use digblock_net::protocol::{
	DRIFT_ALPHA, DRIFT_TOLERANCE, INPUT_REDUNDANCY, JITTER_BUFFER_TICKS, MAX_SPEED, POS_TOLERANCE,
};
use glam::Vec3;

/// Jitter-buffer hard cap (~half a second of inputs).
const INPUT_BUF_CAP: usize = 30;
/// Extra slack on the per-tick displacement clamp for float noise.
const DISPLACEMENT_MARGIN: f32 = 0.05;

impl Sim {
	pub(super) fn handle_inputs(&mut self, conn_id: u64, batch: InputBatch) {
		// An honest client never sends more than the redundancy window;
		// oversized batches are garbage, not backlog.
		if batch.inputs.len() > INPUT_REDUNDANCY * 4 {
			return;
		}
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		for input in batch.inputs {
			if input.tick <= client.last_input_tick && client.last_input_tick != 0 {
				continue;
			}
			let Some(clean) = sanitize(input) else {
				continue;
			};
			// First-wins is load-bearing: the client freezes each tick's
			// claim at first send and reconciles acks against that frozen
			// value. Last-wins here would adopt replay-rewritten re-sends
			// the client no longer judges by, reintroducing the
			// standing-still reconcile ping-pong at high ping.
			client.inputs.entry(clean.tick).or_insert(clean);
		}
		while client.inputs.len() > INPUT_BUF_CAP {
			client.inputs.pop_first();
		}
	}

	pub(super) fn step_physics(&mut self) {
		let store = &self.store;
		for client in self.clients.values_mut() {
			if !client.live {
				continue;
			}
			// Hold on unloaded ground: right after an admin /tp the
			// target column may still be generating, and unloaded cells
			// read as air — stepping physics would free-fall the player
			// through the world. Freeze (position kept, velocity shed)
			// until the store answers for the column under their feet.
			let (cx, cz) = super::column_of(client.player.pos);
			if !store.column_ready(cx, cz) {
				client.player.set_state(PlayerState {
					pos: client.player.pos,
					vel: Vec3::ZERO,
				});
				continue;
			}
			// Drain toward the target jitter-buffer depth: normally one
			// input per tick; two when the buffer has backed up.
			let consume = if client.inputs.len() > (JITTER_BUFFER_TICKS + 2) as usize {
				2
			} else {
				1
			};
			for _ in 0..consume {
				let input = client.inputs.pop_first().map(|(_, i)| i);
				step_client(client, input, &|x, y, z| store.cell_at(x, y, z));
			}
		}
	}
}

/// One client sim tick: consume `input` (or coast on the last intent),
/// then run the adoption envelope against the claimed post-state.
fn step_client(
	client: &mut Client,
	input: Option<InputTick>,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) {
	let before = client.player.pos;
	match input {
		Some(input) => {
			let intent = PlayerIntent {
				move_axis: glam::Vec2::from_array(input.move_axis),
				jump: input.jump,
			};
			client.look = Vec3::from_array(input.look).normalize_or_zero();
			client.last_intent = PlayerIntent {
				jump: false,
				..intent
			};
			client.player.tick(TICK, client.look, intent, cell_at);
			client.last_input_tick = input.tick;
			client.active_slot = input.active_slot;

			// Adoption envelope.
			let claimed = PlayerState {
				pos: Vec3::from_array(input.claimed_pos),
				vel: Vec3::from_array(input.claimed_vel),
			};
			let max_step = MAX_SPEED * TICK + DISPLACEMENT_MARGIN;
			let sane = claimed.pos.is_finite()
				&& claimed.vel.is_finite()
				&& claimed.pos.distance(client.player.pos) <= POS_TOLERANCE
				&& claimed.pos.distance(before) <= max_step
				&& !probe_solid(claimed.pos, cell_at);
			if sane {
				// Second gate: sustained drift. The per-tick envelope
				// bounds instantaneous divergence; the delta-vector EWMA
				// bounds it over time. Honest deltas are zero-mean float
				// noise (identical physics from the same adopted state)
				// and cancel; riding the envelope edge needs a persistent
				// directional bias that pumps the average instead. Insane
				// claims never feed the average — one wild claim at the
				// streaming edge must not lock an honest client out.
				let delta = claimed.pos - client.player.pos;
				client.drift = client.drift * (1.0 - DRIFT_ALPHA) + delta * DRIFT_ALPHA;
				if client.drift.length() <= DRIFT_TOLERANCE {
					client.player.set_state(claimed);
				}
			} else {
				client.drift *= 1.0 - DRIFT_ALPHA;
			}
		}
		None => {
			// Underrun: coast on the last intent, never adopt.
			let intent = client.last_intent;
			client.player.tick(TICK, client.look, intent, cell_at);
			client.drift *= 1.0 - DRIFT_ALPHA;
		}
	}
}

/// True if the claimed position's capsule overlaps solid GEOMETRY (a
/// claim we never adopt). The shape-aware core probe, shared with the
/// client's placement/spawn checks: standing on a stair tread (feet
/// inside the stair's unit cell) or mid-mount onto a landing is never
/// "inside solid", so honest walks over shaped blocks stay adopted.
fn probe_solid(pos: Vec3, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>) -> bool {
	digblock_core::player::overlaps_solid(pos, cell_at)
}

fn sanitize(mut input: InputTick) -> Option<InputTick> {
	let axis = glam::Vec2::from_array(input.move_axis);
	let look = Vec3::from_array(input.look);
	if !axis.is_finite() || !look.is_finite() {
		return None;
	}
	if look.normalize_or_zero() == Vec3::ZERO {
		return None;
	}
	if axis.length_squared() > 1.0 {
		input.move_axis = axis.normalize().to_array();
	}
	input.active_slot = input
		.active_slot
		.min(digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1);
	Some(input)
}

#[cfg(test)]
mod tests {
	use super::super::harness::*;
	use super::super::Sim;
	use digblock_core::player::{Player, PlayerIntent, PlayerState, TICK};
	use glam::Vec3;

	#[test]
	fn honest_claims_adopted_and_echoed() {
		// Deliberately the one sim test on REAL worldgen: full movement +
		// adoption over real terrain, so the injected-source shortcut can
		// never hide a worldgen-facing regression.
		let dir = temp_dir("honest");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Honest local twin: same physics over the same (server) world,
		// one tick ahead of the server consuming it. Delivered one input
		// per tick, the buffer never backs up, so consumed inputs and
		// server ticks stay 1:1.
		let mut local = Player::new(sim.spawn);
		let mut history = Vec::new(); // local post-state per input tick
		let mut t = 100;
		for _ in 0..60 {
			let intent = PlayerIntent {
				move_axis: glam::Vec2::new(0.0, 1.0),
				jump: false,
			};
			local.tick(TICK, Vec3::Z, intent, &|x, y, z| sim.test_cell_at(x, y, z));
			let claim = local.state();
			history.push(claim.pos);
			sim.tick_once(vec![batch(vec![input(t, true, claim)], 1)]);
			t += 1;
		}

		let server = sim.test_player(1).unwrap();
		assert_eq!(server.pos, local.pos, "honest claim adopted verbatim");
		c.drain();
		// The last snapshot echoes our own claim at its acked tick.
		let last = c.snaps.last().expect("snapshots flowed");
		assert!(last.your_last_input_tick >= 100);
		let echoed = Vec3::from_array(last.players[0].pos);
		assert_eq!(echoed, history[(last.your_last_input_tick - 100) as usize]);
	}

	#[test]
	fn off_envelope_claim_keeps_server_state() {
		let mut sim = flat_sim("offclaim");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let before = sim.test_player(1).unwrap();

		let mut t = 100;
		for _ in 0..30 {
			// Claim teleports 3 blocks sideways: outside POS_TOLERANCE.
			let claim = PlayerState {
				pos: before.pos + Vec3::new(3.0, 0.0, 0.0),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let server = sim.test_player(1).unwrap();
		assert!(
			(server.pos.x - before.pos.x).abs() < 0.01,
			"server kept its own x: {} vs claim +3",
			server.pos.x
		);
	}

	#[test]
	fn speed_hack_claims_rejected() {
		let mut sim = flat_sim("speed");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let start = sim.test_player(1).unwrap();

		// Each claim is 2 blocks further: near the *previous claim* but
		// far beyond MAX_SPEED * TICK per tick.
		let mut t = 100;
		for i in 1..=30 {
			let claim = PlayerState {
				pos: start.pos + Vec3::new(2.0 * i as f32, 0.0, 0.0),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let server = sim.test_player(1).unwrap();
		assert!(
			(server.pos.x - start.pos.x).abs() < 0.5,
			"teleport chain not adopted: {} vs start {}",
			server.pos.x,
			start.pos.x
		);
	}

	#[test]
	fn inside_solid_claim_rejected() {
		let mut sim = flat_sim("solid");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let start = sim.test_player(1).unwrap();

		// Claim the feet dipped 0.2 into the surface block: well within
		// POS_TOLERANCE and the per-tick displacement clamp, but the
		// capsule's bottom sphere overlaps the floor cell — only a real
		// body collider catches this (the old point probes sat above it).
		let claim = PlayerState {
			pos: start.pos - Vec3::new(0.0, 0.2, 0.0),
			vel: Vec3::ZERO,
		};
		let mut t = 100;
		for _ in 0..10 {
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let server = sim.test_player(1).unwrap();
		assert!(
			(server.pos.y - start.pos.y).abs() < 0.2,
			"in-ground claim not adopted: {} vs {}",
			server.pos.y,
			start.pos.y
		);
	}

	/// The anti-rubber-band regression: a player standing ON a stair
	/// tread has feet inside the stair's unit CELL — the old
	/// cell-granular probe called that "inside solid" and rejected every
	/// claim, rubber-banding forever. The shape-aware probe adopts the
	/// whole honest walk up the flight verbatim.
	#[test]
	fn standing_on_stair_claim_adopted() {
		use digblock_core::block::BlockType;
		use digblock_core::chunk::{make_cell, ChunkData, AIR_CHUNK, WORLD_VCHUNK};
		use digblock_core::player::{Player, PlayerIntent, TICK};
		use digblock_core::shapes::{stair_state, Facing};
		use std::sync::Arc;

		// Flat ground (top at 126) with a 3-stair flight ascending +z at
		// z = 12..14 and a platform beyond, in chunk (0, 0).
		let source: crate::store::ColumnSource = Arc::new(|cx, cz| {
			let mut full = ChunkData::default();
			let mut top = ChunkData::default();
			for y in 0..16 {
				for z in 0..16 {
					for x in 0..16 {
						full.set(x, y, z, BlockType::Stone);
						if y <= 126 % 16 {
							top.set(x, y, z, BlockType::Stone);
						}
					}
				}
			}
			if (cx, cz) == (0, 0) {
				// Surface is y=127 (local 15 in chunk 7); the flight
				// rises 127→129, crossing into the next vchunk.
				top.set_cell(
					8,
					15,
					12,
					make_cell(BlockType::WoodStairs, stair_state(Facing::PosZ)),
				);
			}
			let (full, top) = (Arc::new(full), Arc::new(top));
			(0..WORLD_VCHUNK as usize)
				.map(|cy| {
					use std::cmp::Ordering::*;
					match cy.cmp(&(126 / 16)) {
						Less => full.clone(),
						Equal => top.clone(),
						Greater if cy == 126 / 16 + 1 && (cx, cz) == (0, 0) => {
							let mut d = ChunkData::default();
							d.set_cell(
								8,
								0,
								13,
								make_cell(BlockType::WoodStairs, stair_state(Facing::PosZ)),
							);
							for z in 14..16 {
								for x in 0..16 {
									d.set(x, 0, z, BlockType::Stone);
								}
							}
							Arc::new(d)
						}
						Greater => AIR_CHUNK.clone(),
					}
				})
				.collect()
		});
		let spawn = Vec3::new(8.5, 129.5, 8.5);
		let dir = temp_dir("stairadopt");
		let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, source, spawn).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Honest local twin over the same world, one tick ahead. Claims
		// carry a float-noise x offset so adoption is DETECTABLE: the
		// server's own sim never produces the offset, so equality below
		// holds only if every claim was adopted verbatim (the old
		// cell-granular probe passed the un-offset version of this test
		// vacuously — server sim and twin were bit-identical anyway).
		const EPS: f32 = 1e-4;
		let mut local = Player::new(spawn);
		let mut t = 100;
		let mut climbed = false;
		for _ in 0..120 {
			let intent = PlayerIntent {
				move_axis: glam::Vec2::new(0.0, 1.0),
				jump: false,
			};
			local.tick(TICK, Vec3::Z, intent, &|x, y, z| sim.test_cell_at(x, y, z));
			let mut claim = local.state();
			claim.pos.x += EPS;
			// The walk spends real time standing ON the stair cells.
			climbed |= local.pos.y > 130.0 && local.pos.z > 12.0 && local.pos.z < 15.0;
			sim.tick_once(vec![batch(vec![input(t, true, claim)], 1)]);
			t += 1;
			let server = sim.test_player(1).unwrap();
			assert_eq!(
				server.pos, claim.pos,
				"claim adopted verbatim at t={t} (no rubber-band), twin at {:?}",
				local.pos
			);
		}
		assert!(climbed, "the twin actually walked the flight");
	}

	/// Honest auto-step claims adopt verbatim (SIM v9): a twin walks a
	/// slab staircase (1/3 → 2/3 → full stone, each riser exactly one
	/// AUTO_STEP), and every post-step claim must pass the probe and the
	/// per-tick displacement clamp — no rubber-band on the way up.
	#[test]
	fn slab_stack_claim_adopted() {
		use digblock_core::block::BlockType;
		use digblock_core::chunk::{make_cell, ChunkData, AIR_CHUNK, WORLD_VCHUNK};
		use digblock_core::player::{Player, PlayerIntent, TICK};
		use digblock_core::shapes::slab_state;
		use std::sync::Arc;

		// Flat ground (top plane 127) with slab risers ascending +z at
		// y = 127: 1/3 at z=12, 2/3 at z=13, full stone at z=14..16.
		let source: crate::store::ColumnSource = Arc::new(|cx, cz| {
			let mut full = ChunkData::default();
			let mut top = ChunkData::default();
			for y in 0..16 {
				for z in 0..16 {
					for x in 0..16 {
						full.set(x, y, z, BlockType::Stone);
						if y <= 126 % 16 {
							top.set(x, y, z, BlockType::Stone);
						}
					}
				}
			}
			if (cx, cz) == (0, 0) {
				top.set_cell(8, 15, 12, make_cell(BlockType::WoodSlab, slab_state(false)));
				top.set_cell(8, 15, 13, make_cell(BlockType::WoodSlab, slab_state(true)));
				top.set(8, 15, 14, BlockType::Stone);
				top.set(8, 15, 15, BlockType::Stone);
			}
			let (full, top) = (Arc::new(full), Arc::new(top));
			(0..WORLD_VCHUNK as usize)
				.map(|cy| {
					use std::cmp::Ordering::*;
					match cy.cmp(&(126 / 16)) {
						Less => full.clone(),
						Equal => top.clone(),
						Greater => AIR_CHUNK.clone(),
					}
				})
				.collect()
		});
		let spawn = Vec3::new(8.5, 128.5, 8.5);
		let dir = temp_dir("slabadopt");
		let mut sim = Sim::new_with_source(SEED, &dir, 1, 2, source, spawn).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Honest local twin, claims offset by float noise so adoption is
		// detectable (same construction as the stair adoption test).
		const EPS: f32 = 1e-4;
		let mut local = Player::new(spawn);
		let mut t = 100;
		let mut climbed = false;
		for _ in 0..120 {
			let intent = PlayerIntent {
				move_axis: glam::Vec2::new(0.0, 1.0),
				jump: false,
			};
			local.tick(TICK, Vec3::Z, intent, &|x, y, z| sim.test_cell_at(x, y, z));
			let mut claim = local.state();
			claim.pos.x += EPS;
			// The walk spends real time on top of the full stack.
			climbed |= local.pos.y > 129.4 && local.pos.z > 14.0 && local.pos.z < 16.0;
			sim.tick_once(vec![batch(vec![input(t, true, claim)], 1)]);
			t += 1;
			let server = sim.test_player(1).unwrap();
			assert_eq!(
				server.pos, claim.pos,
				"claim adopted verbatim at t={t} (no rubber-band), twin at {:?}",
				local.pos
			);
		}
		assert!(climbed, "the twin actually stepped the whole stack");
	}

	#[test]
	fn sustained_edge_riding_locked_out_then_recovers() {
		let mut sim = flat_sim("drift");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let start = sim.test_player(1).unwrap();

		// Fly cheat riding the per-tick envelope: every claim +0.4 blocks
		// up from the previous claim — each one individually within
		// POS_TOLERANCE and the displacement clamp while adopted, but the
		// delta EWMA is pure +y bias and trips DRIFT_TOLERANCE within a
		// handful of ticks; after that the sim stops following and the
		// marching claims fall out of the hard envelope too.
		let mut t = 100;
		for i in 1..=60 {
			let claim = PlayerState {
				pos: start.pos + Vec3::new(0.0, 0.4 * i as f32, 0.0),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let cheated = sim.test_player(1).unwrap();
		let gained = cheated.pos.y - start.pos.y;
		assert!(
			gained < 5.0,
			"attempted 24 blocks of fly, kept {gained} (drift gate)"
		);

		// Honest again: out-of-envelope garbage claims decay the EWMA
		// (they never feed it), the player falls back and re-grounds.
		for _ in 0..150 {
			sim.tick_once(vec![batch(vec![input(t, false, no_claim())], 1)]);
			t += 1;
		}
		// Adoption works again: a tiny honest offset is taken verbatim.
		let s = sim.test_player(1).unwrap();
		let claim = PlayerState {
			pos: s.pos + Vec3::new(0.01, 0.0, 0.0),
			vel: s.vel,
		};
		sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
		let after = sim.test_player(1).unwrap();
		assert_eq!(after.pos, claim.pos, "drift decayed, adoption resumed");
	}

	#[test]
	fn zero_mean_jitter_stays_adopted() {
		let mut sim = flat_sim("jitter");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Settle onto the ground first (garbage claims, pure server sim).
		let mut t = 100;
		for _ in 0..90 {
			sim.tick_once(vec![batch(vec![input(t, false, no_claim())], 1)]);
			t += 1;
		}
		let settled = sim.test_player(1).unwrap();

		// Honest client whose claims wobble ±5 mm around the sim —
		// alternating sign, so the delta EWMA cancels vectorially and
		// every single claim keeps being adopted, even over many ticks.
		let mut last_claim = settled;
		for i in 0..240 {
			let jitter = if i % 2 == 0 { 0.005 } else { -0.005 };
			let claim = PlayerState {
				pos: Vec3::new(settled.pos.x + jitter, settled.pos.y, settled.pos.z),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
			last_claim = claim;
		}
		let server = sim.test_player(1).unwrap();
		assert_eq!(server.pos, last_claim.pos, "noise never trips the gate");
	}

	#[test]
	fn determinism_under_datagram_loss() {
		let run = |tag: &str, lossy: bool| -> Vec3 {
			let mut sim = flat_sim(tag);
			let _c = connect(&mut sim, 1);
			wait_live(&mut sim, 1);

			// 60 forward inputs. Clean: one batch per tick. Lossy: every
			// other delivery dropped, survivors carry the last 4 ticks
			// (the wire redundancy window).
			let inputs: Vec<_> = (0..60).map(|i| input(100 + i, true, no_claim())).collect();
			for (i, _) in inputs.iter().enumerate() {
				let events = if lossy {
					if i % 2 == 1 {
						let lo = i.saturating_sub(3);
						vec![batch(inputs[lo..=i].to_vec(), 1)]
					} else {
						vec![]
					}
				} else {
					vec![batch(vec![inputs[i]], 1)]
				};
				sim.tick_once(events);
			}
			// Settle: let the jitter buffer drain completely.
			for _ in 0..40 {
				sim.tick_once(vec![]);
			}
			// Every input tick reached the server: redundancy defeated
			// the datagram loss (this is the real assertion; position
			// equality alone would also pass with dropped inputs since
			// coasting reuses the last intent).
			assert_eq!(sim.test_last_input_tick(1), 159, "all inputs consumed");
			sim.test_player(1).unwrap().pos
		};

		// The hard guarantee (asserted inside `run`): every input tick is
		// consumed exactly once despite half the deliveries vanishing.
		// Positions are close but not bit-equal: when the buffer backs
		// up the server deliberately double-ticks to catch up, so the
		// server-tick↔player-tick mapping is elastic under loss.
		let clean = run("det-clean", false);
		let lossy = run("det-lossy", true);
		assert!(
			clean.distance(lossy) < 1.0,
			"paths stayed close: {clean:?} vs {lossy:?}"
		);
	}
}
