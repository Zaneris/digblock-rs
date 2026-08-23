//! The server's half of the scheduled-sweep seam: which cells a job can
//! see, and how the writes it earns become ordinary journaled edits.
//!
//! Everything a sweep produces goes through `apply_write`, so
//! journalling, protection, LWW replay and client broadcast keep working
//! unchanged and the client needs no new wire message.

use super::*;
use crate::store::ChunkStore;
use digblock_core::worldjob::{CellSource, WorldJob};

/// The sweep's read seam. The journal is consulted behind the store for
/// the same reason `world_cell` does it: an edited cell in a column that
/// has been evicted is still the world's answer.
struct SweepWorld<'a> {
	store: &'a ChunkStore,
	journal: &'a Journal,
}

impl CellSource for SweepWorld<'_> {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.store.cell_at(x, y, z).or_else(|| {
			self.journal
				.index
				.column(x.div_euclid(16), z.div_euclid(16))
				.and_then(|cells| cells.get(&(x, y, z)).copied())
		})
	}

	fn edited_cells(&self, cx: i32, cz: i32, f: &mut dyn FnMut(i32, i32, i32)) {
		// The index is the complete edit record — a frontier sweep's own
		// writes land back in it through `apply_write`, which is what
		// keeps a spread self-sustaining across runs.
		if let Some(cells) = self.journal.index.column(cx, cz) {
			for &(x, y, z) in cells.keys() {
				f(x, y, z);
			}
		}
	}
}

/// Seconds this world had already lived when the process opened it.
///
/// The sweep clock has to be the WORLD's age: a rolled job salts its draw
/// with the slot index, so a clock that restarts at zero on every boot
/// re-offers one slot's draw forever — the same finds come back and the
/// sites that lost that roll keep losing it. The data directory's own
/// creation is the world's birth and costs no format change to read.
/// Where the filesystem will not say when that was, the wall clock
/// stands in: not an age, but it has the one property this needs, which
/// is that it does not restart.
pub(super) fn world_epoch_secs(data_dir: &Path) -> f64 {
	let now = std::time::SystemTime::now();
	let elapsed = match std::fs::metadata(data_dir).and_then(|m| m.created()) {
		Ok(born) => now.duration_since(born),
		Err(_) => now.duration_since(std::time::UNIX_EPOCH),
	};
	elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl Sim {
	/// Register a rule to run on the clock. Panics on a scope that could
	/// destroy node reveals — loudly at boot, because the quiet failure is
	/// unrecoverable.
	pub fn register_world_job(&mut self, job: Box<dyn WorldJob>) {
		self.world_jobs.register(job);
	}

	/// The clock the scheduled sweeps run on: this world's age, which the
	/// session's own elapsed time only adds to. `/time` moves it with the
	/// sky, since the sweeps are the game day's.
	pub(super) fn sweep_secs(&self) -> f64 {
		self.world_epoch_secs + f64::from(self.tick) / f64::from(TICK_RATE) + self.tod_offset_secs
	}

	pub(super) fn step_world_jobs(&mut self) {
		let eyes: Vec<(i32, i32, i32)> = self
			.clients
			.values()
			.filter(|c| c.live)
			.map(|c| {
				let p = c.player.pos;
				(p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32)
			})
			.collect();
		let secs = self.sweep_secs();
		// Fed ahead of the journal gate below: the head-count is a fact
		// about the day whether or not the world can be written to.
		self.presence.observe(secs, eyes.len() as u32);
		// Unhealthy journal: stop mutating the world durably, exactly as
		// fluid does — a sweep's writes are journaled edits like any other.
		if !self.journal.healthy() {
			return;
		}
		let players = self.presence.peak(secs);
		let writes = {
			let world = SweepWorld {
				store: &self.store,
				journal: &self.journal,
			};
			self.world_jobs
				.tick(secs, self.store.seed(), &eyes, players, &world)
		};
		if writes.is_empty() {
			return;
		}
		let regions = self.protected_regions();
		for (cell, val) in writes {
			if digblock_core::protect::immutable(cell.0, cell.1, cell.2, &regions) {
				continue;
			}
			// The sweep read through the journal, so a cell it names may
			// not be resident. `set_cell` answering None is that case, and
			// the write is dropped rather than journaled against terrain
			// nobody is holding.
			let Some(prev) = self.store.set_cell(cell.0, cell.1, cell.2, val) else {
				continue;
			};
			self.apply_write(cell, val, prev, SERVER_ID, NO_CLIENT_SEQ, true);
		}
	}
}

#[cfg(test)]
mod tests;
