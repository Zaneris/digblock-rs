//! Waterfall audio emitters: turns the mesher's per-chunk impact cells
//! (the same data spray.rs renders) into a small set of stable looping
//! voices. Pure and GPU-free — reconciliation is unit-tested with
//! synthetic impact maps.

use crate::audio::{AudioCmd, LoopId, Sfx, LOOP_WATER_BASE};
use digblock_core::chunk::ChunkPos;
use glam::Vec3;
use rustc_hash::FxHashMap;

/// Hearing radius. Deliberately much tighter than spray's visual 64: you
/// can see a cascade across a valley without standing in its noise.
const RADIUS: f32 = 32.0;
/// Max concurrent waterfall voices. Kept low because voices sum — four
/// mid-gain loops around the camera read as "water everywhere" even when
/// each one is individually reasonable.
const MAX_VOICES: usize = 3;
/// Greedy clustering: a new emitter target must be at least this far
/// from every already-accepted target.
const MIN_SEP: f32 = 8.0;
/// A target within this distance of a live voice is that voice moving,
/// not a new one — kills remesh jitter.
const MATCH_DIST: f32 = 6.0;
/// A voice with no matching target survives this long before stopping
/// (cap-boundary flicker, transient remesh gaps).
const GRACE_SECS: f32 = 1.0;
/// Reconcile at 10 Hz — voices don't need frame-rate updates.
const STEP_SECS: f32 = 0.1;

/// Authored gain for a cluster of `n` impact cells at `dist` blocks.
///
/// Bigger clusters are louder, capped well under the mix ceiling. The
/// distance taper is the load-bearing half: without it every voice
/// inside [`RADIUS`] was authored at 0.3-0.6 regardless of range, and
/// the *only* falloff came from the platform panner — which on desktop
/// clamps to unity for everything close by, so a trickle across the
/// valley and the cascade at your feet arrived at the same loudness.
/// Squared so distant water sits well back in the mix, and reaching
/// exactly 0 at `RADIUS` also removes the pop when a voice ages in or
/// out at the range edge.
fn gain_of(n: u32, dist: f32) -> f32 {
	// Cluster gains are ~1.4x their original authoring (0.3/0.1/cap 0.6):
	// a waterfall you are standing next to was too polite. Only this term
	// moved — the taper's SHAPE is what the test below pins, and it is
	// scale-invariant, so loudness and falloff stay independent knobs.
	// Headroom check: MIN_SEP keeps the 2nd and 3rd voices at least 8
	// blocks out, so the worst realistic sum is ~0.8 + 0.45 + 0.45.
	let cluster = (0.45 + 0.13 * (1.0 + n as f32).ln()).min(0.8);
	let taper = (1.0 - dist / RADIUS).clamp(0.0, 1.0);
	cluster * taper * taper
}

struct Voice {
	id: LoopId,
	pos: Vec3,
	gain: f32,
	missing_secs: f32,
}

#[derive(Default)]
pub struct WaterAudio {
	impacts: FxHashMap<ChunkPos, Vec<Vec3>>,
	voices: Vec<Voice>,
	accum: f32,
	next_id: u32,
	scratch: Vec<(Vec3, f32)>,
}

impl WaterAudio {
	/// Record a chunk's impact cells (same packed cells and decode as
	/// `Spray::set_chunk`; empty clears).
	pub fn set_chunk(&mut self, pos: ChunkPos, origin: (f32, f32, f32), impacts: &[u16]) {
		if impacts.is_empty() {
			self.impacts.remove(&pos);
			return;
		}
		let pts = impacts
			.iter()
			.map(|&p| {
				Vec3::new(
					origin.0 + (p & 0xf) as f32 + 0.5,
					origin.1 + ((p >> 4) & 0xf) as f32 + 0.1,
					origin.2 + ((p >> 8) & 0xf) as f32 + 0.5,
				)
			})
			.collect();
		self.impacts.insert(pos, pts);
	}

	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		self.impacts.remove(&pos);
	}

	/// Stop every voice (world swap / audio teardown).
	pub fn clear(&mut self, out: &mut Vec<AudioCmd>) {
		for v in self.voices.drain(..) {
			out.push(AudioCmd::LoopStop { id: v.id });
		}
		self.impacts.clear();
		self.accum = 0.0;
	}

	/// Advance and reconcile. Emits LoopStart/LoopMove/LoopStop into
	/// `out`; internally throttled to [`STEP_SECS`].
	pub fn update(&mut self, dt: f32, cam: Vec3, out: &mut Vec<AudioCmd>) {
		self.accum += dt;
		if self.accum < STEP_SECS {
			return;
		}
		let step = self.accum;
		self.accum = 0.0;

		// Nearest-first candidate points within range.
		let mut near: Vec<(Vec3, f32)> = std::mem::take(&mut self.scratch);
		near.clear();
		for pts in self.impacts.values() {
			for p in pts {
				let d2 = cam.distance_squared(*p);
				if d2 <= RADIUS * RADIUS {
					near.push((*p, d2));
				}
			}
		}
		near.sort_by(|a, b| a.1.total_cmp(&b.1));

		// Greedy clustering: nearest point wins an emitter, later points
		// within MIN_SEP fold into its loudness instead.
		let mut targets: Vec<(Vec3, u32)> = Vec::new();
		for (p, _) in &near {
			if let Some(t) = targets
				.iter_mut()
				.find(|(c, _)| c.distance_squared(*p) < MIN_SEP * MIN_SEP)
			{
				t.1 += 1;
			} else if targets.len() < MAX_VOICES {
				targets.push((*p, 1));
			}
		}
		self.scratch = near;
		self.scratch.clear();

		// Match targets to live voices (move), start the unmatched,
		// stop voices whose targets stayed gone past the grace.
		let mut claimed = vec![false; self.voices.len()];
		for (pos, n) in targets {
			let gain = gain_of(n, cam.distance(pos));
			let best = self
				.voices
				.iter()
				.enumerate()
				.filter(|(i, v)| !claimed[*i] && v.pos.distance(pos) <= MATCH_DIST)
				.min_by(|(_, a), (_, b)| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)))
				.map(|(i, _)| i);
			match best {
				Some(i) => {
					claimed[i] = true;
					let v = &mut self.voices[i];
					v.missing_secs = 0.0;
					if v.pos != pos || (v.gain - gain).abs() > 0.01 {
						v.pos = pos;
						v.gain = gain;
						out.push(AudioCmd::LoopMove {
							id: v.id,
							pos: pos.to_array(),
							gain,
						});
					}
				}
				None => {
					let id = LoopId(LOOP_WATER_BASE + self.next_id);
					self.next_id = self.next_id.wrapping_add(1);
					self.voices.push(Voice {
						id,
						pos,
						gain,
						missing_secs: 0.0,
					});
					claimed.push(true);
					out.push(AudioCmd::LoopStart {
						id,
						sound: Sfx::Waterfall,
						pos: Some(pos.to_array()),
						gain,
					});
				}
			}
		}
		let mut i = 0;
		while i < self.voices.len() {
			if claimed[i] {
				i += 1;
				continue;
			}
			self.voices[i].missing_secs += step;
			if self.voices[i].missing_secs >= GRACE_SECS {
				out.push(AudioCmd::LoopStop {
					id: self.voices[i].id,
				});
				self.voices.swap_remove(i);
				claimed.swap_remove(i);
			} else {
				i += 1;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const ORIGIN: (f32, f32, f32) = (0.0, 0.0, 0.0);

	fn pack(x: u16, y: u16, z: u16) -> u16 {
		x | (y << 4) | (z << 8)
	}

	fn starts(cmds: &[AudioCmd]) -> usize {
		cmds.iter()
			.filter(|c| matches!(c, AudioCmd::LoopStart { .. }))
			.count()
	}

	fn stops(cmds: &[AudioCmd]) -> usize {
		cmds.iter()
			.filter(|c| matches!(c, AudioCmd::LoopStop { .. }))
			.count()
	}

	#[test]
	fn one_cluster_one_voice() {
		let mut w = WaterAudio::default();
		let mut out = Vec::new();
		w.set_chunk(
			ChunkPos::new(0, 0, 0),
			ORIGIN,
			&[pack(1, 1, 1), pack(2, 1, 1), pack(3, 1, 2)],
		);
		w.update(1.0, Vec3::ZERO, &mut out);
		assert_eq!(starts(&out), 1, "{out:?}");
	}

	#[test]
	fn distant_clusters_get_own_voices_capped() {
		let mut w = WaterAudio::default();
		let mut out = Vec::new();
		// Two clusters 20 blocks apart in different chunks.
		w.set_chunk(ChunkPos::new(0, 0, 0), ORIGIN, &[pack(1, 1, 1)]);
		w.set_chunk(ChunkPos::new(1, 0, 0), (16.0, 0.0, 0.0), &[pack(5, 1, 1)]);
		w.update(1.0, Vec3::ZERO, &mut out);
		assert_eq!(starts(&out), 2);
	}

	#[test]
	fn jitter_moves_never_restarts() {
		let mut w = WaterAudio::default();
		let mut out = Vec::new();
		w.set_chunk(ChunkPos::new(0, 0, 0), ORIGIN, &[pack(4, 4, 4)]);
		w.update(1.0, Vec3::ZERO, &mut out);
		assert_eq!(starts(&out), 1);
		out.clear();
		// The impact shifts one cell (remesh jitter).
		w.set_chunk(ChunkPos::new(0, 0, 0), ORIGIN, &[pack(5, 4, 4)]);
		w.update(1.0, Vec3::ZERO, &mut out);
		assert_eq!(starts(&out), 0, "{out:?}");
		assert_eq!(stops(&out), 0);
		assert!(out.iter().any(|c| matches!(c, AudioCmd::LoopMove { .. })));
	}

	#[test]
	fn removal_stops_only_after_grace() {
		let mut w = WaterAudio::default();
		let mut out = Vec::new();
		w.set_chunk(ChunkPos::new(0, 0, 0), ORIGIN, &[pack(1, 1, 1)]);
		w.update(1.0, Vec3::ZERO, &mut out);
		out.clear();
		w.remove_chunk(ChunkPos::new(0, 0, 0));
		// Shortly after: still inside the grace window.
		w.update(0.2, Vec3::ZERO, &mut out);
		assert_eq!(stops(&out), 0);
		// Well past it: the voice stops.
		w.update(1.5, Vec3::ZERO, &mut out);
		assert_eq!(stops(&out), 1, "{out:?}");
	}

	/// The bug this pins: `gain_of` used to ignore distance entirely, so
	/// every audible waterfall was authored at 0.3-0.6 whatever its range
	/// and the whole falloff was left to the platform panner (which on
	/// desktop is flat inside 5.6 blocks). Water was audible everywhere.
	#[test]
	fn gain_falls_off_with_distance_and_reaches_zero_at_the_radius() {
		let near = gain_of(1, 0.0);
		assert!(near > 0.0);
		let mut prev = f32::INFINITY;
		for step in 0..=32 {
			let g = gain_of(1, step as f32);
			assert!(g <= prev + 1e-6, "gain rose at {step} blocks: {g} > {prev}");
			prev = g;
		}
		assert!(gain_of(1, RADIUS) <= 1e-6, "audible at the range edge");
		// Half way out, a lone cell is well back in the mix.
		assert!(gain_of(1, RADIUS / 2.0) < near * 0.3);
		// A bigger cluster is still louder than a lone cell at the same range.
		assert!(gain_of(8, 10.0) > gain_of(1, 10.0));
	}

	#[test]
	fn out_of_range_impacts_are_silent_and_clear_stops_all() {
		let mut w = WaterAudio::default();
		let mut out = Vec::new();
		w.set_chunk(ChunkPos::new(0, 0, 0), (500.0, 0.0, 0.0), &[pack(0, 0, 0)]);
		w.update(1.0, Vec3::ZERO, &mut out);
		assert_eq!(starts(&out), 0);
		w.set_chunk(ChunkPos::new(1, 0, 0), ORIGIN, &[pack(1, 1, 1)]);
		w.update(1.0, Vec3::ZERO, &mut out);
		assert_eq!(starts(&out), 1);
		out.clear();
		w.clear(&mut out);
		assert_eq!(stops(&out), 1);
	}
}
