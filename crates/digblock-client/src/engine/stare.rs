//! `--bench --stare`: the worst case as a bench regime. The flight and the walk both move, so
//! most of the screen reprojects and the near ground is what the resolve spends on. Standing on a
//! glass block at the top of the world, staring straight down and NOT moving, is the other end:
//! every half-res texel is far terrain seen through glass, so all three of the resolve's marches
//! run at their longest, the glass pass gathers per fragment on top of them (R17), and nothing
//! reprojects away. A few stations of it, each a real arrival apart, is the number the GI plan is
//! judged by; the flight and the walk are kept for the streaming and the near-field regimes.

use super::*;

/// Poses held, one after another. Three is enough terrain variety to notice a station-specific
/// number without tripling the run.
pub(super) const STARE_STATIONS: u32 = 3;
/// Seconds each station is held, `STARE_SETTLE_SECS` of which is the arrival.
pub(super) const STARE_SECS: f32 = 30.0;
/// The head of every station dropped from its summary line: the fill after the hop, the GI
/// re-upload it queues, and a full probe sweep (48 frames for C0) all land inside it, and the
/// station line is meant to read the pose AT REST.
pub(super) const STARE_SETTLE_SECS: f32 = 5.0;
/// Blocks between stations. Past twice the largest draw distance, so a hop evicts everything
/// resident and goes through the arrival path (`rim_backlog`) exactly as a teleport does — a
/// station never inherits the previous one's lens of retained chunks or its probe field.
pub(super) const STARE_HOP: i32 = 1088;
/// The block stood on: the top layer of the world, so the whole column below is in frame.
pub(super) const STARE_TOP: i32 = digblock_core::chunk::WORLD_VBLOCK - 1;
/// The pose when the launcher gives none: straight down, short of the −90° the camera's own
/// `look` clamps at, so the view basis never degenerates and the frame holds nothing but the
/// ground. `--pitch`/`--yaw` override it — a 45° stare trades the deepest column for a horizon,
/// the far-field regime where the resolve's distance gating and the sky show up.
const STARE_PITCH: f32 = -89.0;
const STARE_YAW: f32 = 0.0;
/// Frames in a whole stare run, for the launchers' `exit_after_frames` default: the regime ends
/// itself at the last station's last tick rather than at the flight's 1800.
pub const STARE_FRAMES: u64 = (STARE_STATIONS as f32 * STARE_SECS / TICK + 0.5) as u64;

// A hop must evict everything at the largest draw distance the slider allows: the residency
// disc is `draw_dist` chunks in radius, so two discs stop touching past twice that. And the
// settle has to leave rest inside the station for the summary line to read.
const _: () = assert!(STARE_HOP > 2 * crate::settings::DRAW_MAX * 16);
const _: () = assert!(STARE_SETTLE_SECS < STARE_SECS);

/// Station `k`'s column offset from the origin, in blocks: the origin itself, then one hop
/// along each lateral axis.
pub(super) fn stare_offset(k: u32) -> (i32, i32) {
	match k {
		0 => (0, 0),
		1 => (STARE_HOP, 0),
		_ => (0, STARE_HOP),
	}
}

impl Engine {
	/// One tick of the stare (the `--stare` arm of `fly_step`): pick the station the clock is
	/// in, place its glass block, and hold the pose. The block is placed through the ordinary
	/// edit funnel — the chunk re-classifies for GI and the probes around it invalidate — so the
	/// bench also exercises one edit per station. The top chunk of a freshly arrived column may
	/// not be generated yet, so the placement is retried every tick until it lands; the settle
	/// window absorbs the wait. A bench run has no world store, so the block never persists.
	pub(super) fn stare_step(&mut self) {
		let o = *self.bench.stare_origin.get_or_insert(self.camera.pos);
		let k = ((self.bench.bench_t / STARE_SECS) as u32).min(STARE_STATIONS - 1);
		let (dx, dz) = stare_offset(k);
		let (bx, bz) = (
			(o.x + dx as f32).floor() as i32,
			(o.z + dz as f32).floor() as i32,
		);
		if self.bench.stare_station != Some(k) {
			self.bench.stare_station = Some(k);
			self.bench.stare_placed = false;
			let settle = (STARE_SETTLE_SECS / TICK + 0.5) as usize;
			self.bench.metrics.mark_station(k, settle);
			log::info!("bench: station {k} at ({bx}, {STARE_TOP}, {bz})");
		}
		if !self.bench.stare_placed {
			let glass = BlockType::Glass as u16;
			self.bench.stare_placed = self.pipeline.cell_at(bx, STARE_TOP, bz) == Some(glass)
				|| self.pipeline.set_cell(bx, STARE_TOP, bz, glass).is_some();
		}
		self.camera.pos = Vec3::new(
			bx as f32 + 0.5,
			STARE_TOP as f32 + 1.0 + digblock_core::player::EYE_TO_FEET,
			bz as f32 + 0.5,
		);
		self.camera.yaw_deg = self.config.yaw.unwrap_or(STARE_YAW);
		self.camera.pitch_deg = self.config.pitch.unwrap_or(STARE_PITCH).clamp(-89.0, 89.0);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every station past the first is a whole hop from the origin.
	#[test]
	fn the_stare_stations_are_a_real_arrival_apart() {
		for k in 1..STARE_STATIONS {
			let (dx, dz) = stare_offset(k);
			assert!(dx.abs().max(dz.abs()) >= STARE_HOP, "station {k}");
		}
	}

	/// The run's frame count is whole ticks of whole stations.
	#[test]
	fn the_stare_run_is_whole_stations() {
		assert_eq!(STARE_FRAMES, 3 * 30 * 60);
	}
}
