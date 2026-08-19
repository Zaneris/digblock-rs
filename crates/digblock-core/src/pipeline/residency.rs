//! Which columns are resident. The visibility diff walks only the rows
//! that entered or left, so a one-chunk step costs a shell rather than
//! a full re-scan.

use super::*;

impl Pipeline {
	/// Call every frame with the player's chunk position. Performs the
	/// visibility scan on chunk-boundary crossings, tops up workers, and
	/// drains results (mesh outputs capped at `upload_budget`; the whole
	/// drain stops at `deadline` — undrained results wait in the channel,
	/// so worker throughput is unaffected, only main-thread pacing).
	/// `inline_allowance` lets the inline executor run extra jobs beyond
	/// its fixed floor while their predicted cost fits (the adaptive web
	/// budget); `None` keeps the floor-only batch. Ignored while jobs run
	/// off-thread — the allowance prices JOBS, and a pool doesn't spend
	/// this thread's time on them. `deadline` still governs everything
	/// else, pool or not: pulling the pool's replies, the drain, and
	/// feeding the pool are all main-thread work either way.
	pub fn maintain(
		&mut self,
		center: ChunkPos,
		upload_budget: usize,
		deadline: Option<web_time::Instant>,
		inline_allowance: Option<std::time::Duration>,
	) -> Maintenance {
		let mut out = Maintenance {
			evicted: Vec::new(),
			meshed: Vec::new(),
			generated: 0,
			gen_jobs: JobTiming::default(),
			mesh_jobs: JobTiming::default(),
			stale_dropped: 0,
			requeued: 0,
			saturated: false,
			gen_queue: 0,
			mesh_queue: 0,
			in_flight: 0,
			overlaid_water: Vec::new(),
		};
		let center = ChunkPos::new(center.x, 0, center.z);
		if self.scanned != Some((center, self.draw_dist)) {
			self.center = center;
			self.visibility_scan(&mut out.evicted);
		}
		// Inline jobs run before the drain so their results land this frame;
		// workers get topped up after.
		//
		// The deadline is deliberately NOT extended by what the jobs spent —
		// that makes the stages ADD UP, which is the miss the budget exists
		// to avoid. Slicing bounds the overrun at the source and every stage
		// keeps a minimum-progress floor, so a blown deadline delays work
		// without ever starving it.
		//
		// An external pool's results arrived as their own event-loop tasks,
		// so pull them into the queue the drain reads first — under the
		// deadline, because every `poll` decodes, but always at least one, or
		// the pool's inbox only grows.
		let mut lost: Vec<JobKey> = Vec::new();
		// The pull can stop with replies still in the pool, which
		// `has_pending` — which sees only the decoded queue — cannot know.
		let mut pull_cut = false;
		if let Executor::External { dispatch, results } = &mut self.exec {
			let mut pulled = 0usize;
			loop {
				if pulled > 0 && deadline.is_some_and(|d| web_time::Instant::now() >= d) {
					pull_cut = true;
					break;
				}
				let Some(r) = dispatch.poll() else {
					break;
				};
				results.push_back(r);
				pulled += 1;
			}
			// Jobs the pool accepted and cannot answer for. Unbudgeted on
			// purpose: this is a handful of small keys in a failure that
			// is rare by construction, and deferring it is deferring the
			// only record that those chunks were ever dispatched.
			while let Some(key) = dispatch.lost() {
				lost.push(key);
			}
		}
		if !lost.is_empty() {
			log::warn!("{} chunk job(s) lost by the pool; requeued", lost.len());
			for key in lost {
				self.in_flight = self.in_flight.saturating_sub(1);
				self.reclaim(key);
			}
		}
		if self.exec.runs_inline() {
			self.run_inline_jobs(inline_allowance);
		}
		self.drain_results(&mut out, upload_budget, deadline);
		out.saturated = self.exec.has_pending() || pull_cut;
		out.requeued = self.top_up_workers(deadline);
		out.gen_queue = self.gen_heap.len() as u32;
		out.mesh_queue = self.mesh_heap.len() as u32;
		out.in_flight = self.in_flight;
		// Everything dispatched has been absorbed, so the in-flight half of
		// `stream_urgency` has nothing left to speak for.
		if self.in_flight == 0 {
			self.in_flight_min_d2 = None;
		}
		out
	}

	/// Desired set: y ∈ 0..WORLD_VCHUNK, Euclidean distance < draw_dist in
	/// chunk units — the Java updateVisible cull. Chunks leaving the set
	/// are dropped (results in flight are rejected by the slot check).
	///
	/// Ordinary movement takes the incremental path: crossing one chunk
	/// boundary shifts the disc by one, which changes O(r) columns out of
	/// O(r²), and this runs OUTSIDE the streaming deadline — a full rescan
	/// here was an unbudgeted multi-millisecond burst arriving at walking
	/// cadence, once per chunk boundary crossed.
	pub(super) fn visibility_scan(&mut self, evicted: &mut Vec<ChunkPos>) {
		let r = self.draw_dist;
		let c = self.center;
		match self.scanned {
			Some((pc, pr)) if pr == r => self.visibility_diff(pc, c, r, evicted),
			// First scan, or the draw distance itself changed. Rebuild
			// exactly — and with no hysteresis band, because draw distance
			// is a memory/perf setting the user just chose and must be
			// honoured to the chunk.
			_ => self.visibility_rebuild(c, r, evicted),
		}
		self.scanned = Some((c, r));
		// Pending meshes are deliberately NOT re-prioritized for the new
		// centre: heap keys are priorities, not correctness, they drift by
		// at most one chunk per crossing, and `next_mesh_job` already drops
		// entries whose chunk left the store. Draining and rebuilding the
		// whole heap to reorder it cost more than the misordering ever did.
	}

	/// Full rebuild: exact membership, no hysteresis.
	pub(super) fn visibility_rebuild(&mut self, c: ChunkPos, r: i32, evicted: &mut Vec<ChunkPos>) {
		let r2 = i64::from(r) * i64::from(r);
		self.store.retain(|pos, _| {
			let keep = dist2(ChunkPos::new(pos.x, 0, pos.z), c) < r2;
			if !keep {
				evicted.push(*pos);
			}
			keep
		});
		for z in (c.z - r)..=(c.z + r) {
			let Some(w) = row_half_width(z - c.z, r) else {
				continue;
			};
			for x in (c.x - w)..=(c.x + w) {
				self.add_column(x, z);
			}
		}
	}

	/// Incremental update between two centres at the same radius: only the
	/// entering and leaving crescents are touched.
	pub(super) fn visibility_diff(
		&mut self,
		pc: ChunkPos,
		c: ChunkPos,
		r: i32,
		evicted: &mut Vec<ChunkPos>,
	) {
		if pc == c {
			return;
		}
		// Leaving: in the old KEEP disc, out of the new one.
		let keep = r + KEEP_MARGIN;
		for z in (pc.z - keep)..=(pc.z + keep) {
			let Some(w) = row_half_width(z - pc.z, keep) else {
				continue;
			};
			let now = row_span(c.x, row_half_width(z - c.z, keep));
			for (a, b) in interval_diff(pc.x - w, pc.x + w, now) {
				for x in a..=b {
					for y in 0..WORLD_VCHUNK {
						let pos = ChunkPos::new(x, y, z);
						if self.store.remove(&pos).is_some() {
							evicted.push(pos);
						}
					}
				}
			}
		}
		// Entering: in the new desired disc, out of the old one. Columns
		// still resident from the hysteresis band land here too and are
		// no-ops — which is the entire point of keeping them.
		for z in (c.z - r)..=(c.z + r) {
			let Some(w) = row_half_width(z - c.z, r) else {
				continue;
			};
			let before = row_span(pc.x, row_half_width(z - pc.z, r));
			for (a, b) in interval_diff(c.x - w, c.x + w, before) {
				for x in a..=b {
					self.add_column(x, z);
				}
			}
		}
	}
}
