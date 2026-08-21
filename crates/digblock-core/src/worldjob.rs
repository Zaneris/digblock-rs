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

pub mod grass;
pub mod wisplight;

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

/// Where a sweep's candidate cells come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Candidates {
	/// Every cell of the y band in every column near players — the full
	/// scan. Writes are handed to the host slice by slice, as they are
	/// judged.
	Band,
	/// Cells a player has touched and their 27-neighborhoods, read from
	/// the host's edit record ([`CellSource::edited_cells`]) — the set
	/// that can have changed since worldgen. Two-phase: every candidate
	/// is judged against the PRE-sweep world, and the writes emit
	/// afterwards in bounded slices. Per-slice application would let an
	/// early write flip a later candidate's neighbor checks in the same
	/// run — how far that chains depends on edit-map iteration order,
	/// which the server and the offline engine do not share.
	Frontier {
		/// Also offer each block column's [`crate::worldgen::subsoil_span`]
		/// — the only place pristine terrain can hold what a frontier
		/// cannot see: soil bared by the cave carve rather than by an
		/// edit.
		surface_shell: bool,
	},
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
	/// Inclusive vertical band. A [`Candidates::Band`] sweep visits every
	/// cell in it, so a job that only cares about the surface has to say
	/// so — the runner has no way to guess. A frontier sweep clamps its
	/// candidates to it.
	pub y: (i32, i32),
	/// How candidates are found within the radius and band.
	pub candidates: Candidates,
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

	/// Push every player-touched cell of chunk column `(cx, cz)` to `f`.
	/// The default offers nothing — truthful for a source with no edit
	/// record, and exactly what starves a [`Candidates::Frontier`] job on
	/// a host that forgot to override; the host-arm sweep tests are what
	/// stand behind that.
	fn edited_cells(&self, _cx: i32, _cz: i32, _f: &mut dyn FnMut(i32, i32, i32)) {}
}

/// A rule run on a clock over the cells near active players.
pub trait WorldJob: Send {
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
	/// Block columns for a Band sweep, chunk columns for a Frontier one —
	/// each mode's unit of coverage.
	columns: Vec<(i32, i32)>,
	/// Judgment cursor: (column, y) cells done for Band, whole columns
	/// done for Frontier.
	done: usize,
	/// Frontier only: writes judged but not yet handed to the host. They
	/// wait so that every candidate is judged against the pre-sweep world,
	/// then leave in bounded slices rather than one burst of journal
	/// appends.
	pending: Vec<Write>,
	/// Emission cursor into `pending`.
	emitted: usize,
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

	/// Drop any sweep in flight — judged-but-unemitted writes die with it.
	/// Leaving the world abandons it rather than resuming into terrain
	/// that is no longer there.
	pub fn abandon(&mut self) {
		self.active = None;
	}

	/// Forget everything session-scoped: the sweep in flight and every
	/// job's `last_run`. For a host whose seconds accumulator restarts at
	/// zero — a retained `last_run` against a rewound clock reads
	/// `last >= slot` for a day per slot, and nothing ever comes due.
	pub fn reset(&mut self) {
		self.active = None;
		for r in &mut self.jobs {
			r.last_run = None;
		}
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
			let scope = r.job.scope();
			let columns = match scope.candidates {
				Candidates::Band => columns_near(eyes, scope.radius_chunks),
				Candidates::Frontier { .. } => chunk_columns_near(eyes, scope.radius_chunks),
			};
			self.active = Some(Sweep {
				job: i,
				run: slot,
				columns,
				done: 0,
				pending: Vec::new(),
				emitted: 0,
			});
			return;
		}
	}

	fn slice(&mut self, seed: f64, world: &dyn CellSource) -> Vec<Write> {
		let Some(sweep) = self.active.as_mut() else {
			return Vec::new();
		};
		let job = self.jobs[sweep.job].job.as_ref();
		let scope = job.scope();
		match scope.candidates {
			Candidates::Band => {
				let span = (scope.y.1 - scope.y.0 + 1) as usize;
				let total = sweep.columns.len() * span;
				let end = (sweep.done + CELLS_PER_TICK).min(total);
				let mut out = Vec::new();
				for n in sweep.done..end {
					let (x, z) = sweep.columns[n / span];
					let y = scope.y.0 + (n % span) as i32;
					offer(job, &scope, world, seed, sweep.run, (x, y, z), &mut out);
				}
				sweep.done = end;
				if end >= total {
					self.active = None;
				}
				out
			}
			Candidates::Frontier { surface_shell } => {
				if sweep.done < sweep.columns.len() {
					// Judgment: whole chunk columns until the offer count
					// reaches the budget (always at least one, so a dense
					// column cannot stall the cursor). Writes land in
					// `pending`, not the return — see Candidates::Frontier.
					let mut offered = 0;
					while sweep.done < sweep.columns.len() && offered < CELLS_PER_TICK {
						let (cx, cz) = sweep.columns[sweep.done];
						offered += judge_frontier_column(
							job,
							&scope,
							world,
							seed,
							sweep.run,
							(cx, cz),
							surface_shell,
							&mut sweep.pending,
						);
						sweep.done += 1;
					}
					return Vec::new();
				}
				// Emission. Re-read each cell and drop the write unless it
				// still holds a scope block: judgment ran against an older
				// world, and LWW would let a stale write stomp whatever a
				// player has done there since.
				let end = (sweep.emitted + CELLS_PER_TICK).min(sweep.pending.len());
				let mut out = Vec::new();
				for &(at, val) in &sweep.pending[sweep.emitted..end] {
					let Some(cell) = world.cell_at(at.0, at.1, at.2) else {
						continue;
					};
					if scope.blocks.iter().any(|&b| cell_id(cell) == b as u16) {
						out.push((at, val));
					}
				}
				sweep.emitted = end;
				if end >= sweep.pending.len() {
					self.active = None;
				}
				out
			}
		}
	}
}

/// Read, filter, apply: the one path a candidate takes in either mode, so
/// the two cannot drift on what "offered" means. Unloaded and
/// wrong-block cells are skipped, exactly as the band scan always has.
fn offer(
	job: &dyn WorldJob,
	scope: &Scope,
	world: &dyn CellSource,
	seed: f64,
	run: u64,
	at: (i32, i32, i32),
	out: &mut Vec<Write>,
) {
	let Some(cell) = world.cell_at(at.0, at.1, at.2) else {
		return;
	};
	if !scope.blocks.iter().any(|&b| cell_id(cell) == b as u16) {
		return;
	}
	if let Some(next) = job.apply(at, world, seed, run) {
		out.push((at, next));
	}
}

/// Judge one chunk column of a frontier sweep: the edits it holds and
/// their 27-neighborhoods, plus the subsoil shell when asked. Returns how
/// many candidates were offered, for the slice budget. Deduplication is
/// per column only — a cell adjacent to edits in two columns is offered
/// twice and produces the same write twice, which the second apply of a
/// LWW edit turns into a no-op.
#[allow(clippy::too_many_arguments)]
fn judge_frontier_column(
	job: &dyn WorldJob,
	scope: &Scope,
	world: &dyn CellSource,
	seed: f64,
	run: u64,
	(cx, cz): (i32, i32),
	surface_shell: bool,
	pending: &mut Vec<Write>,
) -> usize {
	let mut seen = FxHashSet::default();
	let mut cand: Vec<(i32, i32, i32)> = Vec::new();
	let (y0, y1) = scope.y;
	world.edited_cells(cx, cz, &mut |x, y, z| {
		for dy in -1..=1 {
			for dz in -1..=1 {
				for dx in -1..=1 {
					let c = (x + dx, y + dy, z + dz);
					if (y0..=y1).contains(&c.1) && seen.insert(c) {
						cand.push(c);
					}
				}
			}
		}
	});
	if surface_shell {
		let n = CHUNK_SIZE as i32;
		for lz in 0..n {
			for lx in 0..n {
				let (x, z) = (cx * n + lx, cz * n + lz);
				let (lo, hi) = crate::worldgen::subsoil_span(x, z, seed);
				for y in lo.max(y0)..=hi.min(y1) {
					if seen.insert((x, y, z)) {
						cand.push((x, y, z));
					}
				}
			}
		}
	}
	let offered = cand.len();
	for at in cand {
		offer(job, scope, world, seed, run, at, pending);
	}
	offered
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

/// [`columns_near`]'s chunk-granular sibling: every CHUNK column within
/// `radius_chunks` of any eye, deduplicated — the frontier's unit of
/// coverage, because the hosts' edit records are keyed by chunk column.
fn chunk_columns_near(eyes: &[(i32, i32, i32)], radius_chunks: i32) -> Vec<(i32, i32)> {
	let s = CHUNK_SIZE as i32;
	let mut seen = FxHashSet::default();
	let mut out = Vec::new();
	for &(ex, _, ez) in eyes {
		let (cx, cz) = (ex.div_euclid(s), ez.div_euclid(s));
		for z in (cz - radius_chunks)..=(cz + radius_chunks) {
			for x in (cx - radius_chunks)..=(cx + radius_chunks) {
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
