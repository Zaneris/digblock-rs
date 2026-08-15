//! Movement: input intake, per-tick server physics, and the adoption
//! envelope + sustained-drift gate (model described in the `sim` docs).

use super::{Client, Sim};
use digblock_core::player::{PlayerIntent, PlayerState, TICK};
use digblock_net::messages::{InputBatch, InputTick};
use digblock_net::protocol::{
	DRIFT_ALPHA, DRIFT_TOLERANCE, INPUT_HOLD_MAX, INPUT_REDUNDANCY, JITTER_BUFFER_TICKS, MAX_SPEED,
	POS_TOLERANCE,
};
use glam::Vec3;

/// Jitter-buffer hard cap: a fraction of a second's worth of inputs.
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
				// Classify before dropping. A redundancy copy of a tick we
				// already held is the common case and costs nothing; a tick
				// the buffer had drained past before it arrived is an input
				// lost for good, and the only evidence the buffer was too
				// shallow. `want` records exactly how much deeper it needed
				// to be. Deliberately NOT derived from `input.tick` against
				// `self.tick`: the client's counter free-runs and
				// permanently loses ticks to its own frame hitches
				// (`tick_accum` is capped and zeroes on an unloaded
				// column), so that difference measures client frame pacing
				// as much as the wire. This measures the wire alone — a
				// stalled client still delivers in order, and an in-order
				// stream produces no late-lost events at all.
				let age = client.admit_hi.wrapping_sub(input.tick);
				if age >= 64 || client.admit_seen & (1u64 << age) == 0 {
					client.stats.late_lost += 1;
					let want = client.last_input_tick.wrapping_sub(input.tick) + 1;
					client.stats.late_want_max = client.stats.late_want_max.max(want);
				} else {
					client.stats.dup += 1;
				}
				continue;
			}
			let Some(clean) = sanitize(input) else {
				continue;
			};
			// Record the admission before the map insert, so a tick evicted
			// by the cap and later re-delivered still reads as a duplicate
			// rather than as late-lost: this bitmap means "ever admitted",
			// not "currently resident".
			let hi = client.admit_hi;
			if clean.tick > hi || hi == 0 {
				let shift = clean.tick.wrapping_sub(hi);
				client.admit_seen = if hi == 0 || shift >= 64 {
					0
				} else {
					client.admit_seen << shift
				};
				client.admit_seen |= 1;
				client.admit_hi = clean.tick;
			} else {
				let age = hi.wrapping_sub(clean.tick);
				if age < 64 {
					client.admit_seen |= 1u64 << age;
				}
			}
			// First-wins is load-bearing: the client freezes each tick's
			// claim at first send and reconciles acks against that frozen
			// value. Last-wins here would adopt replay-rewritten re-sends
			// the client no longer judges by, reintroducing the
			// standing-still reconcile ping-pong at high ping.
			client.inputs.entry(clean.tick).or_insert(clean);
		}
		while client.inputs.len() > INPUT_BUF_CAP {
			client.inputs.pop_first();
			client.stats.evicted += 1;
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
				// Discard what arrived while frozen instead of banking it.
				// Skipping the drain entirely let the buffer fill to
				// INPUT_BUF_CAP and then double-drain every stale tick in it
				// on unfreeze — a visible sprint after every /tp into
				// ungenerated terrain, replaying intent from before the
				// teleport. The newest input's aim and slot are still worth
				// keeping: they describe the client NOW, and nothing about
				// them depends on the physics that did not run.
				if let Some((_, last)) = client.inputs.pop_last() {
					client.stats.frozen_dropped += client.inputs.len() as u64 + 1;
					client.inputs.clear();
					client.look = Vec3::from_array(last.look).normalize_or_zero();
					client.last_intent = PlayerIntent {
						jump: false,
						move_axis: glam::Vec2::from_array(last.move_axis),
					};
					client.active_slot = last.active_slot;
					client.last_input_tick = client.admit_hi;
				}
				client.hold_ticks = 0;
				continue;
			}
			// Drain toward the target jitter-buffer depth: normally one
			// input per tick; two when the buffer has backed up.
			//
			// The threshold is the target ITSELF, not target + slack. It
			// used to be `JITTER_BUFFER_TICKS` plus a slack, which only
			// trimmed above that sum and so could never converge on the
			// depth it names: with `CLIENT_TICK_LEAD` starting the client
			// ahead of the server, equilibrium sat above the nominal depth
			// and every input paid the difference on its way to adoption.
			// `DIG_GRACE_BASE` was sized around that lag — it was paying
			// for this.
			let consume = if client.inputs.len() > JITTER_BUFFER_TICKS as usize {
				2
			} else {
				1
			};
			// Sample the depth the drain is actually working against, as the
			// tick SPAN rather than `inputs.len()`: loss punches holes in the
			// map, so the count under-reads the real depth exactly when the
			// link is worst. Diagnostic only — `consume` above is unchanged.
			let depth = if client.last_input_tick == 0 {
				0
			} else {
				client.admit_hi.saturating_sub(client.last_input_tick)
			};
			client.stats.depth_sum += depth as u64;
			client.stats.depth_samples += 1;
			client.stats.depth_max = client.stats.depth_max.max(depth);

			// Hold a HOLE open. `pop_first` takes the lowest tick present,
			// so a buffer missing the next tick but holding later ones steps
			// straight past the gap and kills the missing ticks the moment
			// they land — `handle_inputs` refuses anything at or below the
			// mark. Waiting a few ticks for the hole to fill is the entire
			// difference between a reordered input arriving late and one
			// lost for good.
			//
			// This is a hole, not an underrun: the buffer is non-empty and
			// the tick we want is simply not in it yet. An empty buffer
			// falls through to the drain below and coasts, as it always has.
			//
			// `last_input_tick != 0` is load-bearing. A fresh client's first
			// input is tick ~CLIENT_TICK_LEAD, so `next` would be 1 and
			// every join would stall the full cap against a "hole" the size
			// of the client's whole lead, which was never a hole.
			let next = client.last_input_tick.wrapping_add(1);
			if client.last_input_tick != 0
				&& !client.inputs.is_empty()
				&& !client.inputs.contains_key(&next)
				&& client.hold_ticks < INPUT_HOLD_MAX
			{
				client.hold_ticks += 1;
				client.stats.held += 1;
				// Coast through the underrun path verbatim: a hold and an
				// underrun are physically the same tick. Only the buffer
				// state differs, and that difference is the whole point.
				step_client(client, None, &|x, y, z| store.cell_at(x, y, z));
				continue;
			}
			client.hold_ticks = 0;
			for _ in 0..consume {
				let input = client.inputs.pop_first().map(|(_, i)| i);
				if input.is_none() {
					client.stats.underruns += 1;
				}
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
			client.stats.consumed += 1;
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
		let mut local = Player::new(sim.spawn_anchor);
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
		let echoed = Vec3::from_array(last.me.expect("own state in snapshot").pos);
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
			// Claim teleports sideways, well outside POS_TOLERANCE.
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

		// Each claim steps further along: near the *previous claim* but
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

		// Claim the feet dipped slightly into the surface block: well within
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

		// Flat ground with a short stair flight ascending +z and a platform
		// beyond, in chunk (0, 0).
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
				// The surface sits in the top row of its vchunk, so the
				// flight rises across the boundary into the next one.
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
	/// slab staircase (thin slab → thick slab → full stone, each riser
	/// exactly one AUTO_STEP), and every post-step claim must pass the probe
	/// and the per-tick displacement clamp — no rubber-band on the way up.
	#[test]
	fn slab_stack_claim_adopted() {
		use digblock_core::block::BlockType;
		use digblock_core::chunk::{make_cell, ChunkData, AIR_CHUNK, WORLD_VCHUNK};
		use digblock_core::player::{Player, PlayerIntent, TICK};
		use digblock_core::shapes::slab_state;
		use std::sync::Arc;

		// Flat ground with slab risers ascending +z on the surface plane:
		// thin slab, then thick slab, then full stone.
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
				top.set_cell(
					8,
					15,
					12,
					make_cell(BlockType::WoodSlab, slab_state(false, false)),
				);
				top.set_cell(
					8,
					15,
					13,
					make_cell(BlockType::WoodSlab, slab_state(true, false)),
				);
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

		// Fly cheat riding the per-tick envelope: every claim a small step
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

		// Honest client whose claims wobble a few millimetres around the
		// sim — alternating sign, so the delta EWMA cancels vectorially and
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

			// A run of forward inputs. Clean: one batch per tick. Lossy:
			// every other delivery dropped, survivors carrying the wire's
			// redundancy window.
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

	/// The classifier must not cry wolf on ordinary traffic. Every tick
	/// rides `INPUT_REDUNDANCY` datagrams, so re-sends of already-held
	/// ticks are the common case and cost nothing — only a tick the buffer
	/// had already drained past is a real loss.
	#[test]
	fn redundancy_duplicates_are_not_counted_as_late() {
		let mut sim = flat_sim("dup-not-late");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// In-order delivery, every datagram carrying a redundancy window's
		// worth of ticks — exactly what the wire does at INPUT_REDUNDANCY.
		let inputs: Vec<_> = (0..60).map(|i| input(100 + i, true, no_claim())).collect();
		for i in 0..inputs.len() {
			let lo = i.saturating_sub(3);
			sim.tick_once(vec![batch(inputs[lo..=i].to_vec(), 1)]);
		}
		for _ in 0..40 {
			sim.tick_once(vec![]);
		}

		let s = sim.input_stats(1);
		assert_eq!(s.late_lost, 0, "in-order redundancy is never late: {s:?}");
		assert_eq!(s.consumed, 60, "every tick applied exactly once: {s:?}");
		// One re-send per redundancy copy, for every tick past the first
		// window.
		assert_eq!(s.dup, 174, "re-sends were seen and classified: {s:?}");
		assert_eq!(sim.test_last_input_tick(1), 159, "all inputs consumed");
	}

	/// Reordering, which nothing pinned before `INPUT_HOLD_MAX` existed:
	/// `determinism_under_datagram_loss` delivers in order, so it cannot see
	/// this. Blocks arriving newest-first used to cost every tick but the
	/// newest in each block — that one set the high-water mark and the rest
	/// landed under it. The hold now waits for the hole and recovers them.
	#[test]
	fn reordered_inputs_are_recovered_by_the_hold() {
		let mut sim = flat_sim("reorder-late");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Fixed-size blocks of ticks, each delivered newest-first. One tick
		// per datagram, so the reorder is isolated from redundancy.
		for block in 0..15u32 {
			let base = 100 + block * 4;
			for k in (0..4).rev() {
				sim.tick_once(vec![batch(vec![input(base + k, true, no_claim())], 1)]);
			}
		}
		for _ in 0..40 {
			sim.tick_once(vec![]);
		}

		let s = sim.input_stats(1);
		// Nearly all of them, not one per block. Every block after the first
		// is fully recovered: its newest tick opens a hole the drain refuses
		// to step past, and the older ones fill it before the cap expires.
		assert_eq!(s.consumed, 57, "the hold recovered the reorder: {s:?}");
		assert!(s.held > 0, "it recovered them by holding: {s:?}");
		assert_eq!(s.dup, 0, "nothing was a re-send: {s:?}");
		// The first block is the exception and must stay one: with nothing
		// consumed yet there is no high-water mark to hold against, so its
		// older ticks land under the first and are lost. That is the
		// `last_input_tick != 0` guard being visible, not a leak — without
		// it a fresh join would stall against a phantom hole the size of the
		// client's lead.
		assert_eq!(s.late_lost, 3, "only the first block was lost: {s:?}");
		assert_eq!(s.late_want_max, 4, "depth that would have held it: {s:?}");
	}

	/// The mechanism in isolation: one hole, filled one tick later. Without
	/// the hold the drain steps to the later tick immediately and the late
	/// one is dead on arrival.
	#[test]
	fn a_hole_is_held_open_for_the_late_input() {
		let mut sim = flat_sim("hole-filled");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		sim.tick_once(vec![batch(vec![input(101, true, no_claim())], 1)]);
		sim.tick_once(vec![batch(vec![input(103, true, no_claim())], 1)]);
		sim.tick_once(vec![batch(vec![input(102, true, no_claim())], 1)]);
		for _ in 0..10 {
			sim.tick_once(vec![]);
		}

		let s = sim.input_stats(1);
		assert_eq!(s.late_lost, 0, "the late tick was still admitted: {s:?}");
		assert_eq!(s.consumed, 3, "all three applied, in order: {s:?}");
		assert_eq!(sim.test_last_input_tick(1), 103, "and up to date: {s:?}");
	}

	/// The cap is what stops a hole that will never fill (both redundancy
	/// copies dropped) from stalling the player indefinitely. The drain
	/// gives up after `INPUT_HOLD_MAX` and steps over it.
	#[test]
	fn an_unfilled_hole_is_skipped_after_the_bound() {
		use digblock_net::protocol::INPUT_HOLD_MAX;
		let mut sim = flat_sim("hole-abandoned");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		sim.tick_once(vec![batch(vec![input(101, true, no_claim())], 1)]);
		// The tick between them never arrives.
		sim.tick_once(vec![batch(vec![input(103, true, no_claim())], 1)]);
		for _ in 0..(INPUT_HOLD_MAX + 4) {
			sim.tick_once(vec![]);
		}

		let s = sim.input_stats(1);
		assert_eq!(
			s.held, INPUT_HOLD_MAX as u64,
			"held exactly the bound, then gave up: {s:?}"
		);
		assert_eq!(s.consumed, 2, "101 and 103 both applied: {s:?}");
		assert_eq!(sim.test_last_input_tick(1), 103, "drain resumed: {s:?}");
	}

	/// The `last_input_tick != 0` guard. A fresh client's first input is
	/// tick ~CLIENT_TICK_LEAD, so a drain that treated "tick 1 is missing"
	/// as a hole would stall the full cap on every single join.
	#[test]
	fn the_first_input_is_never_held() {
		let mut sim = flat_sim("first-input");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		sim.tick_once(vec![batch(vec![input(105, true, no_claim())], 1)]);

		let s = sim.input_stats(1);
		assert_eq!(s.held, 0, "no phantom hole below the first input: {s:?}");
		assert_eq!(s.consumed, 1, "applied on the very next drain: {s:?}");
	}

	/// A hold must be a DELAY, not a step: the catch-up drain (the extra
	/// consume above `JITTER_BUFFER_TICKS`) has to give back the
	/// ticks the hold borrowed, or every hole would permanently deepen the
	/// buffer and every later input would pay for it.
	#[test]
	fn a_hold_is_paid_back_by_the_catch_up_drain() {
		use digblock_net::protocol::JITTER_BUFFER_TICKS;
		let mut sim = flat_sim("hold-repaid");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// A steady one-per-tick stream with a single hole in the middle,
		// delivered two ticks late.
		let mut t = 100u32;
		for _ in 0..10 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}
		let hole = t;
		t += 1;
		for _ in 0..3 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}
		sim.tick_once(vec![batch(vec![input(hole, true, no_claim())], 1)]);
		// Resume the steady stream; the catch-up has to work it off while
		// inputs keep arriving, which is the case that matters.
		for _ in 0..10 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}

		let s = sim.input_stats(1);
		assert_eq!(s.late_lost, 0, "the hole was recovered: {s:?}");
		assert!(s.held > 0, "and recovered by holding: {s:?}");
		// The borrowed ticks were given back, not banked. Without the
		// catch-up the backlog would still be sitting at the hold's full
		// depth — one in, one out forever — and every later input would pay
		// that as standing latency. `JITTER_BUFFER_TICKS` is the level the
		// catch-up trims to, so being at or under it is "level with the
		// wire"; it is not zero, because a sender delivering in strides
		// always has a tick or so in hand.
		let backlog = sim.test_input_backlog(1);
		assert!(
			backlog <= JITTER_BUFFER_TICKS as usize,
			"drained back to the steady level: {backlog} queued, {s:?}"
		);
		assert!(
			t - 1 - sim.test_last_input_tick(1) <= JITTER_BUFFER_TICKS,
			"and level with the wire: at {}, wire at {}, {s:?}",
			sim.test_last_input_tick(1),
			t - 1
		);
	}

	/// Freezing on an unloaded column must not BANK a buffer. Skipping the
	/// drain entirely let inputs pile to `INPUT_BUF_CAP` and then
	/// double-drain every stale tick in it on unfreeze — a sprint,
	/// replaying intent from before the teleport that caused the freeze.
	#[test]
	fn a_frozen_client_does_not_bank_a_buffer() {
		let mut sim = flat_sim("frozen-nobank");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Somewhere the flat source has not been asked for yet.
		let far = Vec3::new(6000.5, 129.5, 6000.5);
		sim.test_teleport(1, far);
		let mut t = 200u32;
		for _ in 0..60 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}

		let s = sim.input_stats(1);
		assert!(
			s.frozen_dropped > 0,
			"the freeze was real and the inputs were dropped, not banked: {s:?}"
		);
		assert!(
			sim.test_input_backlog(1) <= 1,
			"nothing banked: {} queued, {s:?}",
			sim.test_input_backlog(1)
		);
	}

	/// Depth is a tick SPAN, not `inputs.len()`. Loss punches holes in the
	/// map, so a count under-reads the real depth exactly when the link is
	/// worst — which is when a depth reading matters.
	#[test]
	fn buffer_depth_is_a_span_not_a_count() {
		let mut sim = flat_sim("depth-span");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Deliver a sparse run in one batch — two ticks with a gap between
		// them — then let the drain sample it. Two inputs are resident, but
		// the buffer spans the whole gap.
		sim.tick_once(vec![batch(
			vec![input(100, true, no_claim()), input(108, true, no_claim())],
			1,
		)]);
		sim.tick_once(vec![]);

		let s = sim.input_stats(1);
		assert!(
			s.depth_max >= 8,
			"span from admit_hi down to the consumed mark: {s:?}"
		);
	}
}
