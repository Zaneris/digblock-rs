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
				// already held is the common case and costs nothing; a tick the
				// buffer had drained past before it arrived is an input lost for
				// good, and the only evidence the buffer was too shallow.
				//
				// Deliberately NOT derived from `input.tick` against
				// `self.tick`: the client's counter free-runs and permanently
				// loses ticks to its own frame hitches, so that difference
				// measures client frame pacing as much as the wire. This
				// measures the wire alone.
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
					client.hotbar.set_selected(last.active_slot as usize);
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

			// Hold a HOLE open. `pop_first` takes the lowest tick present, so a
			// buffer missing the next tick but holding later ones steps
			// straight past the gap and kills the missing ticks the moment they
			// land — `handle_inputs` refuses anything at or below the mark.
			// Waiting a few ticks for the hole to fill is the entire difference
			// between a reordered input arriving late and one lost for good.
			//
			// This is a hole, not an underrun: the buffer is non-empty and the
			// tick we want is simply not in it yet. An empty buffer falls
			// through to the drain below and coasts.
			//
			// `last_input_tick != 0` is load-bearing. A fresh client's first
			// input is tick ~CLIENT_TICK_LEAD, so every join would otherwise
			// stall the full cap against a "hole" the size of the client's lead.
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
			client.hotbar.set_selected(input.active_slot as usize);

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
mod tests;
