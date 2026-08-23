//! The job scheduler: what to generate or mesh next, dispatched to
//! workers or run inline. Slicing column generation finer needs a
//! measurement first -- the obvious version was tried and lost.

use super::*;

impl Pipeline {
	/// Ensure a column's chunk slots exist; queues generation if any were
	/// missing. Idempotent, so re-entering a column that never left is free.
	pub(super) fn add_column(&mut self, x: i32, z: i32) {
		let mut fresh = false;
		for y in 0..WORLD_VCHUNK {
			let pos = ChunkPos::new(x, y, z);
			if let std::collections::hash_map::Entry::Vacant(e) = self.store.entry(pos) {
				e.insert(ChunkSlot {
					state: ChunkState::Generating,
					mesh_gen: 0,
					submitted_gen: 0,
					empty_mesh_gen: None,
					mesh_missing: false,
				});
				fresh = true;
			}
		}
		if fresh {
			let d2 = dist2(ChunkPos::new(x, 0, z), self.center);
			self.gen_heap.push((Reverse(d2), (x, z)));
		}
	}

	pub(super) fn drain_results(
		&mut self,
		out: &mut Maintenance,
		upload_budget: usize,
		deadline: Option<web_time::Instant>,
	) {
		let mut landed: Vec<(i32, i32)> = Vec::new();
		let mut pulled = 0usize;
		while out.meshed.len() < upload_budget {
			// Generated results are the expensive drains (stack insert plus
			// neighbor re-mesh cascade), so the deadline gates every pull,
			// not just meshed ones — but never the first: at least one
			// result must land per frame or an over-budget frame starves
			// the drain while jobs keep producing (the player would fall
			// through a world that exists but never reaches the store).
			if pulled > 0 && deadline.is_some_and(|d| web_time::Instant::now() >= d) {
				break;
			}
			let Some(result) = self.exec.try_recv() else {
				break;
			};
			pulled += 1;
			self.in_flight = self.in_flight.saturating_sub(1);
			match result {
				JobResult::Generated {
					x,
					z,
					mut stack,
					dur_ms,
				} => {
					out.gen_jobs.record(dur_ms);
					// Only a column still WAITING for terrain may take it.
					// A `Generated` result carries no generation counter,
					// so without this the SECOND one — a column evicted
					// with its job still in a pool, re-entered, and
					// dispatched again — overwrites live cells with
					// pristine worldgen. Single-player survives that (the
					// overlay is re-applied just below); a server's edits
					// do not, because `apply_authoritative` drops a cell
					// from `deferred_edits` the moment it lands. The
					// column's chunks enter and leave together, so its
					// bottom chunk is the probe `next_gen_job` uses too.
					if !matches!(
						self.store.get(&ChunkPos::new(x, 0, z)).map(|s| &s.state),
						Some(ChunkState::Generating)
					) {
						out.stale_dropped += 1;
						continue;
					}
					// Re-apply persistent edits BEFORE the per-chunk
					// air classification below: a block placed in a sky
					// chunk must defeat `is_air_chunk` or it would never
					// mesh. `Arc::make_mut` clones the shared AIR_CHUNK
					// sentinel rather than mutating it.
					//
					// Multiplayer included: the map there mirrors the
					// server's journal, and without this a hard-evicted
					// column comes back as pristine worldgen and STAYS
					// that way — `apply_authoritative` drops a cell the
					// moment it lands and the server never resends it.
					if let Some(cells) = self.edits.column(x, z) {
						for (&(wx, wy, wz), &cell) in cells {
							let Some(((_, cy, _), (lx, ly, lz))) =
								crate::chunk::split_world(wx, wy, wz)
							else {
								continue;
							};
							Arc::make_mut(&mut stack[cy as usize]).set_cell(lx, ly, lz, cell);
							if crate::block::BlockType::from_id(cell).is_water() {
								out.overlaid_water.push((wx, wy, wz));
							}
						}
					}
					landed.push((x, z));
					// Every arriving chunk dirties its GI-window region —
					// air ones included, since the toroidal window may hold
					// a stale previous tenant there (GI-DESIGN.md).
					for cy in 0..crate::chunk::WORLD_VCHUNK {
						self.gi_dirty.insert(ChunkPos::new(x, cy, z));
					}
					let c = self.center;
					let d = dist2(ChunkPos::new(x, 0, z), c);
					for (cy, data) in stack.into_iter().enumerate() {
						let pos = ChunkPos::new(x, cy as i32, z);
						out.generated += 1;
						// Air chunks are stored (uniform lookups) but never
						// meshed — they can't emit faces.
						let air = is_air_chunk(&data);
						let affects = if air {
							[false; 27]
						} else {
							shell_affects(&data)
						};
						if let Some(slot) = self.store.get_mut(&pos) {
							slot.state = ChunkState::Ready(data);
							if !air {
								slot.mesh_gen += 1;
								self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
							}
						}
						// Cross-border re-dirty: every Ready neighbour
						// re-meshes. Diagonals only shade existing faces
						// via the padded shell, so they are skipped for
						// air chunks — without that the buried bulk of the
						// world remeshes 26-ways on every generation.
						//
						// The empty-mesh and queued-remesh skips below
						// apply to FACES too, sound only because this is
						// the ARRIVAL path: coverage only ever INCREASES,
						// and more coverage can only cull faces. Edits
						// remove coverage, so `set_cell` bumps
						// unconditionally. Read `empty_mesh_gen`'s doc
						// before widening this skip.
						for oy in -1..=1 {
							for oz in -1..=1i32 {
								for ox in -1..=1i32 {
									if (ox, oy, oz) == (0, 0, 0) {
										continue;
									}
									let diagonal =
										(ox != 0) as u8 + (oy != 0) as u8 + (oz != 0) as u8 >= 2;
									// All-air shared cells (or an all-air
									// chunk) can't affect a diagonal
									// neighbor's mesh.
									if diagonal
										&& !affects
											[(((oy + 1) * 3 + (oz + 1)) * 3 + (ox + 1)) as usize]
									{
										continue;
									}
									let np = ChunkPos::new(pos.x + ox, pos.y + oy, pos.z + oz);
									if let Some(nslot) = self.store.get_mut(&np) {
										if !matches!(&nslot.state, ChunkState::Ready(d) if !is_air_chunk(d))
										{
											continue;
										}
										if nslot.empty_mesh_gen == Some(nslot.mesh_gen) {
											continue;
										}
										// A queued-but-undispatched remesh
										// snapshots at dispatch time, after
										// this chunk landed in the store — it
										// already sees the fresh shell, so
										// bumping again would only duplicate
										// the job.
										if nslot.submitted_gen < nslot.mesh_gen {
											continue;
										}
										nslot.mesh_gen += 1;
										self.mesh_heap.push((
											Reverse(dist2(
												ChunkPos::new(np.x, 0, np.z),
												self.center,
											)),
											(np.x, np.y, np.z),
										));
									}
								}
							}
						}
					}
				}
				JobResult::Meshed { out: m, dur_ms } => {
					out.mesh_jobs.record(dur_ms);
					match self.store.get_mut(&m.pos) {
						// Only the newest generation may reach the GPU.
						Some(slot) if slot.mesh_gen == m.generation => {
							slot.empty_mesh_gen =
								(m.mesh.is_empty() && !slot.mesh_missing).then_some(m.generation);
							out.meshed.push(m);
						}
						_ => out.stale_dropped += 1, // stale or evicted
					}
				}
			}
		}
		self.apply_deferred_for(&landed);
	}

	/// Nearest-first across both heaps; generation wins ties so terrain exists before meshes want
	/// it. Loops past superseded heap entries, spending `skips` as it goes; None means both heaps
	/// are exhausted OR the budget ran out. See [`STALE_SKIP_BUDGET`] for why there is one.
	pub(super) fn pop_next_job(&mut self, skips: &mut usize) -> Option<Job> {
		loop {
			let gen_d = self.gen_heap.peek().map(|(Reverse(d), _)| *d);
			let mesh_d = self.mesh_heap.peek().map(|(Reverse(d), _)| *d);
			let job = match (gen_d, mesh_d) {
				(None, None) => return None,
				(Some(_), None) => self.next_gen_job(),
				(None, Some(_)) => self.next_mesh_job(),
				(Some(g), Some(m)) => {
					if g <= m {
						self.next_gen_job()
					} else {
						self.next_mesh_job()
					}
				}
			};
			if let Some(job) = job {
				return Some(job);
			}
			// A refused entry: its slot was evicted, is still generating (its arrival re-pushes),
			// is air, or already has an equal-or-newer job in flight — so the entry itself had no
			// work behind it. Stopping abandons the SEARCH, never a job: whatever is deeper in
			// the heap is still there next call, so a spent budget delays streaming by a frame.
			if *skips == 0 {
				return None;
			}
			*skips -= 1;
		}
	}

	/// Inline executor: run the unconditional floor batch (one `Generate` or up to
	/// `INLINE_MESH_CAP` meshes), then — with an allowance — keep starting jobs while their
	/// predicted cost still fits. Jobs are uninterruptible, so prediction is the only overshoot
	/// control; a popped job that doesn't fit is requeued. Results queue for `drain_results`,
	/// which applies the caller's shared deadline.
	pub(super) fn run_inline_jobs(&mut self, allowance: Option<std::time::Duration>) {
		let start = web_time::Instant::now();
		let mut meshes = 0usize;
		let mut floor_done = false;
		let mut skips = STALE_SKIP_BUDGET;
		loop {
			if floor_done && allowance.is_none() {
				break;
			}
			let Some(job) = self.pop_next_job(&mut skips) else {
				break;
			};
			let is_gen = matches!(job, Job::Generate { .. });
			if floor_done {
				let fits = allowance
					.is_some_and(|a| start.elapsed() + self.job_cost.predicted(is_gen) <= a);
				if !fits {
					self.requeue(job);
					break;
				}
			}
			let d2 = self.job_dist2(&job);
			let result = run_job(job);
			let dur_ms = match &result {
				JobResult::Generated { dur_ms, .. } | JobResult::Meshed { dur_ms, .. } => *dur_ms,
			};
			self.job_cost.observe(is_gen, dur_ms);
			self.in_flight += 1;
			self.note_dispatch(d2);
			self.exec.push_result(result);
			if is_gen {
				floor_done = true;
			} else {
				meshes += 1;
				floor_done |= meshes >= INLINE_MESH_CAP;
			}
		}
	}

	/// Returns the number of jobs bounced back because workers were saturated (0 or 1 — a bounce
	/// ends the loop). No-op whenever jobs run on this thread. Shares the caller's frame deadline,
	/// with the usual one-job floor: handing a job to a pool is not free everywhere — the web's
	/// `submit` encodes it and copies it into a JS buffer, two passes over a whole `MeshInput`
	/// that native's channel send does not make, and a pass can offer `DEPTH × workers` of them.
	pub(super) fn top_up_workers(&mut self, deadline: Option<web_time::Instant>) -> u32 {
		if self.exec.runs_inline() {
			return 0;
		}
		let mut submitted = 0usize;
		let mut skips = STALE_SKIP_BUDGET;
		loop {
			if submitted > 0 && deadline.is_some_and(|d| web_time::Instant::now() >= d) {
				break;
			}
			let Some(job) = self.pop_next_job(&mut skips) else {
				break;
			};
			let d2 = self.job_dist2(&job);
			let sent = match &mut self.exec {
				Executor::Workers { job_tx, .. } => match job_tx.try_send(job) {
					Ok(()) => Ok(()),
					Err(TrySendError::Full(job)) => Err(Some(job)),
					// The pool is gone; stop feeding it. The job stays
					// popped — a disconnected pool is a dead client.
					Err(TrySendError::Disconnected(_)) => Err(None),
				},
				Executor::External { dispatch, .. } => dispatch.submit(job).map_err(Some),
				Executor::Inline { .. } => unreachable!("checked above"),
			};
			match sent {
				Ok(()) => {
					self.in_flight += 1;
					self.note_dispatch(d2);
					submitted += 1;
				}
				// Put it back; the pool is saturated.
				Err(Some(job)) => {
					self.requeue(job);
					return 1;
				}
				Err(None) => break,
			}
		}
		0
	}

	/// Flat d² from the streaming centre of whatever column a job covers —
	/// the same key both heaps are ordered by.
	pub(super) fn job_dist2(&self, job: &Job) -> i64 {
		let (x, z) = match job {
			Job::Generate { x, z, .. } => (*x, *z),
			Job::Mesh { pos, .. } => (pos.x, pos.z),
		};
		dist2(ChunkPos::new(x, 0, z), self.center)
	}

	/// Remember the nearest job dispatched since the executor was last
	/// idle. Jobs go out nearest-first, so the minimum over everything
	/// dispatched can only be nearer than the minimum over what is still
	/// outstanding — it overestimates urgency and never underestimates it,
	/// exactly like `stream_urgency`'s stale heap entries.
	pub(super) fn note_dispatch(&mut self, d2: i64) {
		self.in_flight_min_d2 = Some(self.in_flight_min_d2.map_or(d2, |m| m.min(d2)));
	}

	/// Put back a job the pool lost. The twin of `requeue` for a job whose
	/// payload is gone (see [`JobKey`]): a mesh job's `MeshInput` is
	/// rebuilt at redispatch, so only the marker rollback carries over.
	pub(super) fn reclaim(&mut self, key: JobKey) {
		match key {
			JobKey::Generate { x, z } => {
				// Still `Generating` means nothing else has taken the
				// column on; an evicted or already-landed one has no work
				// to redo. Same bottom-chunk probe `next_gen_job` uses.
				if !matches!(
					self.store.get(&ChunkPos::new(x, 0, z)).map(|s| &s.state),
					Some(ChunkState::Generating)
				) {
					return;
				}
				let d = dist2(ChunkPos::new(x, 0, z), self.center);
				self.gen_heap.push((Reverse(d), (x, z)));
			}
			JobKey::Mesh { pos, generation } => {
				let Some(slot) = self.store.get_mut(&pos) else {
					return; // evicted
				};
				if slot.submitted_gen == generation {
					slot.submitted_gen = generation.saturating_sub(1);
				}
				let d = dist2(ChunkPos::new(pos.x, 0, pos.z), self.center);
				self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
			}
		}
	}

	pub(super) fn next_gen_job(&mut self) -> Option<Job> {
		let (_, (x, z)) = self.gen_heap.pop()?;
		// The column's chunks enter and leave the store together; probe the
		// bottom one.
		match self.store.get(&ChunkPos::new(x, 0, z)) {
			Some(slot) if matches!(slot.state, ChunkState::Generating) => Some(Job::Generate {
				x,
				z,
				seed: self.seed,
			}),
			_ => None, // evicted or already generated — skip
		}
	}

	pub(super) fn next_mesh_job(&mut self) -> Option<Job> {
		let (_, p) = self.mesh_heap.pop()?;
		let pos = ChunkPos::new(p.0, p.1, p.2);
		let slot = self.store.get(&pos)?;
		let ChunkState::Ready(data) = &slot.state else {
			return None;
		};
		if is_air_chunk(data) {
			return None; // sky chunk: nothing to mesh
		}
		if slot.mesh_gen <= slot.submitted_gen {
			return None; // an equal-or-newer job is already in flight
		}
		let generation = slot.mesh_gen;
		let input = Box::new(self.build_mesh_input(pos, data));
		let missing = input.missing.iter().any(|&m| m);
		let slot = self.store.get_mut(&pos).expect("slot exists");
		slot.submitted_gen = generation;
		slot.mesh_missing = missing;
		Some(Job::Mesh {
			pos,
			generation,
			input,
		})
	}

	pub(super) fn requeue(&mut self, job: Job) {
		match job {
			Job::Generate { x, z, .. } => {
				let d = dist2(ChunkPos::new(x, 0, z), self.center);
				self.gen_heap.push((Reverse(d), (x, z)));
			}
			Job::Mesh {
				pos, generation, ..
			} => {
				// Roll back the submitted marker so the job reschedules.
				if let Some(slot) = self.store.get_mut(&pos)
					&& slot.submitted_gen == generation
				{
					slot.submitted_gen = generation - 1;
				}
				let d = dist2(ChunkPos::new(pos.x, 0, pos.z), self.center);
				self.mesh_heap.push((Reverse(d), (pos.x, pos.y, pos.z)));
			}
		}
	}
}
