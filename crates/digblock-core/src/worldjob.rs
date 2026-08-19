//! Clock-scheduled sweeps over the world near active players.
//!
//! Not the fluid sim, and deliberately not built on it: fluid is a
//! continuous per-tick simulation over cells already in flight, while a
//! world job is a rare batch sweep triggered by the clock — a
//! `BackgroundService` with a timer rather than a tick system. They share
//! only their write path.
//!
//! The world is infinite, so a sweep is only ever defined relative to
//! players. The scope resolver snapshots the column set when the sweep
//! STARTS; players move, and a set that shifts mid-sweep gives uneven
//! coverage.

use crate::block::BlockType;
use crate::chunk::{CHUNK_SIZE, cell_id};
use crate::sun::DAY_SECS;
use rustc_hash::FxHashSet;

/// One cell a sweep decided to change, and what it becomes.
pub type Write = ((i32, i32, i32), u16);

/// Cells inspected per call to [`Scheduler::tick`]. A sweep is far too
/// large for one tick, so it is sliced: seconds of wall time at a few
/// hundred cells a tick, never a stall.
pub const CELLS_PER_TICK: usize = 512;

/// When a job comes due, in the GAME's day — the same clock the sky runs
/// on, [`DAY_SECS`] long. Never wall time: a server's local 09:00 means
/// nothing to a player in another timezone, and the game clock is already
/// dictated to every client.
///
/// The clock is fixed by this type rather than chosen per job, so two
/// jobs can never disagree about what "twice a day" means.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
	/// Slots per day, spread evenly.
	pub per_day: u32,
	/// Where the first slot falls, as a fraction of the day.
	pub offset: f32,
}

impl Schedule {
	/// The slot standing at `secs`, as an index that strictly increases
	/// with time. Comparing two of these is the whole of "is it due yet".
	pub fn slot(&self, secs: f64) -> u64 {
		let days = secs / f64::from(DAY_SECS) - f64::from(self.offset);
		(days * f64::from(self.per_day)).floor().max(0.0) as u64
	}
}

/// Which cells a job is allowed to see.
#[derive(Clone, Copy, Debug)]
pub struct Scope {
	/// Only cells currently holding one of these are offered to the job.
	///
	/// **None of them may host a node** ([`node_host`]): a journal write
	/// sets the cell's monotone edited bit, and `nodes::revealed` reads
	/// that bit as spent — so a sweep over a host type would silently and
	/// irreversibly destroy every reveal in a wide band around every
	/// player. [`Scheduler::register`] refuses such a scope outright.
	pub blocks: &'static [BlockType],
	/// Chunk radius around each active player.
	pub radius_chunks: i32,
	/// Inclusive vertical band. A sweep visits every cell in it, so a job
	/// that only cares about the surface has to say so — the runner has no
	/// way to guess.
	pub y: (i32, i32),
}

/// True where a pristine cell of this block can BE a node. Mirrors what
/// `nodes::revealed` asks; `a_node_host_is_never_sweepable` pins the two
/// together by walking every block.
pub fn node_host(b: BlockType) -> bool {
	matches!(b, BlockType::Stone | BlockType::Wood | BlockType::Sand)
}

/// What a sweep reads while deciding. Cells outside loaded terrain answer
/// `None`, exactly as they do everywhere else.
pub trait CellSource {
	fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16>;
}

/// A rule run on a clock over the cells near active players.
pub trait WorldJob {
	fn name(&self) -> &'static str;
	fn schedule(&self) -> Schedule;
	fn scope(&self) -> Scope;

	/// The cell this one becomes, or `None` to leave it alone. `run` is
	/// the slot index driving this sweep: seed any randomness from
	/// `(world seed, cell, run)` rather than a live RNG, so a sweep is
	/// reproducible from the journal.
	fn apply(
		&self,
		at: (i32, i32, i32),
		world: &dyn CellSource,
		seed: f64,
		run: u64,
	) -> Option<u16>;
}

struct Registered {
	job: Box<dyn WorldJob>,
	/// The last slot a sweep was STARTED for. Advancing it at the start is
	/// what caps catch-up at one run: a world reopened after a month runs
	/// one sweep, not thirty.
	last_run: Option<u64>,
}

/// A sweep in flight. Holds the column set it started with, so players
/// walking during it cannot change what gets covered.
struct Sweep {
	job: usize,
	run: u64,
	columns: Vec<(i32, i32)>,
	/// Cursor: how many (column, y) cells of this sweep are done.
	done: usize,
}

pub struct Scheduler {
	jobs: Vec<Registered>,
	active: Option<Sweep>,
}

impl Default for Scheduler {
	fn default() -> Self {
		Self::new()
	}
}

impl Scheduler {
	pub fn new() -> Self {
		Self {
			jobs: Vec::new(),
			active: None,
		}
	}

	/// Register a job. Panics on a scope that could destroy node reveals —
	/// a loud failure at boot, because the quiet one is unrecoverable.
	pub fn register(&mut self, job: Box<dyn WorldJob>) {
		let scope = job.scope();
		assert!(
			!scope.blocks.iter().copied().any(node_host),
			"{}: a scheduled sweep may not target a node host — the write \
			 would set the edited bit and destroy every reveal it touched",
			job.name()
		);
		assert!(scope.y.0 <= scope.y.1, "{}: empty y band", job.name());
		assert!(scope.radius_chunks >= 0, "{}: negative radius", job.name());
		assert!(job.schedule().per_day > 0, "{}: never due", job.name());
		self.jobs.push(Registered {
			job,
			last_run: None,
		});
	}

	pub fn sweeping(&self) -> Option<&'static str> {
		self.active.as_ref().map(|s| self.jobs[s.job].job.name())
	}

	/// Advance by one tick: start a sweep if one is due and none is in
	/// flight, then run a slice of it. Returns the writes this slice
	/// earned, for the caller to apply through its own authoritative path
	/// — journal, protection and broadcast all stay where they are.
	///
	/// `eyes` are the active players' positions. In a local world that is
	/// the one player, which is what keeps offline and multiplayer on the
	/// same rule.
	pub fn tick(
		&mut self,
		secs: f64,
		seed: f64,
		eyes: &[(i32, i32, i32)],
		world: &dyn CellSource,
	) -> Vec<Write> {
		if self.active.is_none() {
			self.start_due(secs, eyes);
		}
		self.slice(seed, world)
	}

	/// Drop any sweep in flight. Leaving the world abandons it rather than
	/// resuming into terrain that is no longer there.
	pub fn abandon(&mut self) {
		self.active = None;
	}

	fn start_due(&mut self, secs: f64, eyes: &[(i32, i32, i32)]) {
		if eyes.is_empty() {
			return;
		}
		for (i, r) in self.jobs.iter_mut().enumerate() {
			let slot = r.job.schedule().slot(secs);
			if r.last_run.is_some_and(|last| last >= slot) {
				continue;
			}
			// Set on START, never on completion: a job whose sweep is
			// still slicing is skipped rather than queued.
			r.last_run = Some(slot);
			self.active = Some(Sweep {
				job: i,
				run: slot,
				columns: columns_near(eyes, r.job.scope().radius_chunks),
				done: 0,
			});
			return;
		}
	}

	fn slice(&mut self, seed: f64, world: &dyn CellSource) -> Vec<Write> {
		let Some(sweep) = self.active.as_mut() else {
			return Vec::new();
		};
		let scope = self.jobs[sweep.job].job.scope();
		let job = &self.jobs[sweep.job].job;
		let span = (scope.y.1 - scope.y.0 + 1) as usize;
		let total = sweep.columns.len() * span;
		let end = (sweep.done + CELLS_PER_TICK).min(total);
		let mut out = Vec::new();
		for n in sweep.done..end {
			let (x, z) = sweep.columns[n / span];
			let y = scope.y.0 + (n % span) as i32;
			let Some(cell) = world.cell_at(x, y, z) else {
				continue;
			};
			if !scope.blocks.iter().any(|&b| cell_id(cell) == b as u16) {
				continue;
			}
			if let Some(next) = job.apply((x, y, z), world, seed, sweep.run) {
				out.push(((x, y, z), next));
			}
		}
		sweep.done = end;
		if end >= total {
			self.active = None;
		}
		out
	}
}

/// Every world column within `radius_chunks` of any eye, deduplicated.
fn columns_near(eyes: &[(i32, i32, i32)], radius_chunks: i32) -> Vec<(i32, i32)> {
	let s = CHUNK_SIZE as i32;
	let r = radius_chunks * s;
	let mut seen = FxHashSet::default();
	let mut out = Vec::new();
	for &(ex, _, ez) in eyes {
		// Chunk-aligned so two players in the same chunk resolve to
		// exactly the same set, whatever their sub-chunk position.
		let (cx, cz) = (ex.div_euclid(s) * s, ez.div_euclid(s) * s);
		for z in (cz - r)..(cz + r + s) {
			for x in (cx - r)..(cx + r + s) {
				if seen.insert((x, z)) {
					out.push((x, z));
				}
			}
		}
	}
	out
}

#[cfg(test)]
mod tests;
