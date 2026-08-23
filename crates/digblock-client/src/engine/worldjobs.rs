//! The offline half of the scheduled-sweep seam. Hosted, the server owns
//! these and their results arrive as ordinary edits; in a local world the
//! pipeline IS the authority, so the same jobs run here through the same
//! core scheduler — a rule must not fork between the two.

use super::*;
use digblock_core::worldjob::WorldJob;

impl Engine {
	/// Register a rule to run on the clock in local worlds. Panics on a
	/// scope that could destroy node reveals.
	pub fn register_world_job(&mut self, job: Box<dyn WorldJob>) {
		self.world_jobs.register(job);
	}

	pub(super) fn step_world_jobs(&mut self) {
		let p = self.player.pos;
		let eye = (p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
		let (secs, seed) = (self.world_secs, self.seed);
		// The one player, on the same tracker the server feeds its
		// roster into — so a rule reading the day's head-count is on one
		// scale in both arms.
		self.presence.observe(secs, 1);
		let players = self.presence.peak(secs);
		let writes = {
			let world = PipelineWorld(&mut self.pipeline, &self.protected);
			self.world_jobs.tick(secs, seed, &[eye], players, &world)
		};
		for (cell, val) in writes {
			// The floor and the offline arrival regions, refused exactly
			// where `PipelineWorld::set` refuses them.
			if digblock_core::protect::immutable(cell.0, cell.1, cell.2, &self.protected) {
				continue;
			}
			self.pipeline.set_cell(cell.0, cell.1, cell.2, val);
		}
	}
}
