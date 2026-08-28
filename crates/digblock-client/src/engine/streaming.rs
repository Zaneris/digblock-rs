//! Feeding the renderer: chunk residency, mesh upload and GI upload,
//! all under one per-frame deadline so streaming can never hitch the
//! render loop.

use super::*;

impl Engine {
	/// The ONLY way Engine builds a Renderer — anything uploaded to the renderer outside
	/// `upload_chunk` must be re-sent here. Shared by first init and device-loss rebuild so the two
	/// paths can't drift: meshes stream back in via the pipeline re-mesh, and spray/player state
	/// rides the same uploads.
	pub(super) fn build_renderer(&mut self, gpu: &Gpu, surface_format: wgpu::TextureFormat) {
		self.renderer = Some(Renderer::new(
			gpu,
			surface_format,
			&self.config.render_config(),
		));
		self.pipeline.remesh_all();
		// The fresh renderer's GI window is empty; every resident chunk
		// re-uploads through the ordinary budgeted drain (the GI mirror
		// of the re-mesh above — device loss must never need a relight).
		self.pipeline.gi_redirty_all();
	}

	/// The GI's ratcheting state, or zeros before the renderer exists.
	pub(super) fn renderer_gi_state(&self) -> (f32, u32) {
		self.renderer
			.as_ref()
			.map_or((0.0, 0), Renderer::gi_debug_state)
	}

	pub(super) fn camera_chunk(&self) -> ChunkPos {
		ChunkPos::new(
			(self.camera.pos.x / 16.0).floor() as i32,
			0,
			(self.camera.pos.z / 16.0).floor() as i32,
		)
	}

	/// One tick of the scripted flight: a constant-speed line with a slow
	/// yaw sweep, advanced by a fixed sim tick so the path is a pure
	/// function of how many times this has been called — identical
	/// workload run-to-run, and identical pose whether the steps are
	/// spread over frames (`--fly`) or taken at once (`--warp`).
	pub(super) fn fly_step(&mut self) {
		const BENCH_YAW: f32 = 12.0; // degrees per path-second
		// The walk: forward and back about a fixed origin, at eye height with the ground just
		// ahead — the repro as reported. An oscillation rather than a line keeps the column
		// under the feet constant, so the eye cannot step a whole block mid-path and
		// invalidate the history for a reason that is not the one under test.
		const WALK_AMP: f32 = 1.0; // blocks either side of the origin
		const WALK_PERIOD: f32 = 1.5; // seconds — its peak speed is a walk
		const WALK_PITCH: f32 = -25.0; // puts the ground just ahead of the eye
		self.bench.bench_t += TICK;
		if self.config.stare {
			// The stare: no motion at all, a few poses held in turn.
			self.stare_step();
			return;
		}
		if self.config.walk {
			// Anchored on wherever the camera started, so the walk happens
			// at spawn — which is where DIG_SHAPES/DIG_VAULT put geometry
			// worth casting a shadow.
			let o = *self.bench.walk_origin.get_or_insert(self.camera.pos);
			let phase = std::f32::consts::TAU * self.bench.bench_t / WALK_PERIOD;
			self.camera.pos = Vec3::new(o.x, o.y, o.z + WALK_AMP * phase.sin());
			self.camera.yaw_deg = 0.0;
			self.camera.pitch_deg = WALK_PITCH;
			return;
		}
		// Altitude follows the terrain so worldgen retunes can't put the
		// flight underground; still a pure function of the step count.
		let x = self.config.bench_speed * self.bench.bench_t;
		let h = digblock_core::worldgen::terrain_height(x as i32, 8, self.seed);
		self.camera.pos = Vec3::new(x, h as f32 + 12.0, 8.0);
		self.camera.look(BENCH_YAW * TICK, 0.0);
	}

	pub(super) fn pump_pipeline(&mut self, gpu: &Gpu) {
		let center = self.camera_chunk();
		let Some(renderer) = self.renderer.as_mut() else {
			return;
		};
		let t_maintain = web_time::Instant::now();
		// The adaptive controller feeds on the PREVIOUS frame's wall delta
		// and MEASURED occupancy — both complete by now; this frame's own
		// timings aren't. At the floor every value below matches the fixed
		// path.
		let adaptive = self.config.adaptive_effective();
		let budget_ms = if adaptive {
			// Urgency reads last frame's heaps (this frame's scan hasn't
			// run) — one frame of lag, and the floor batch still runs
			// regardless.
			self.stream.stream_budget.on_frame(
				self.bench.metrics.last.total_ms,
				self.bench.metrics.last.busy_ms,
				self.pipeline.stream_urgency(),
			)
		} else {
			self.config.stream_budget_ms
		};
		self.bench.metrics.stream_budget_ms = budget_ms;
		// Sampled every frame from the executor rather than set once from
		// the config, because the config is a REQUEST. A web pool that
		// failed to start, hasn't finished its handshake, or has had every
		// worker written off reports 0 here and streams inline — and a run
		// that streamed inline must not be recorded in BENCH.md as a
		// pooled arm, which is what the `?workers=` table is selected by.
		self.bench.metrics.workers = self.pipeline.worker_count() as u32;
		// ONE deadline for jobs, drain, evictions and uploads together.
		// Previously each stage got its own slice of the budget and they
		// summed to more than it; now they consume from the same clock, and the
		// per-stage minimum-progress floors keep any single stage from
		// being starved by an earlier one that overran.
		let deadline = t_maintain + Duration::from_secs_f32(budget_ms / 1000.0);
		// Offered unconditionally under an adaptive budget; `maintain`
		// consumes it only while jobs run on this thread, so a web client
		// whose worker pool is still starting (or was refused) keeps
		// exactly the pre-pool behaviour.
		let allowance = adaptive.then(|| Duration::from_secs_f32(budget_ms / 1000.0));
		let m = self
			.pipeline
			.maintain(center, usize::MAX, Some(deadline), allowance);
		self.bench.pending.maintain_ms = ms_since(t_maintain);
		// RTGI: scroll the window to the camera (re-dirtying whatever entered coverage), pull
		// freshly-dirty chunks into the re-upload queue, then feed the voxel window under a
		// byte budget AND the shared streaming deadline (GI-DESIGN.md). The clock matters as
		// much as the bytes: `classify_gi` walks every cell of a chunk, and the byte cap alone
		// permits many chunks in one frame; `GI_UPLOAD_MIN` floors progress.
		let t_gi = web_time::Instant::now();
		// The window follows the draw distance the way the render targets follow the scale: keyed
		// on what the renderer last built, rebuilt here where the device is in hand, and the
		// fresh window is empty so every resident chunk goes back through the drain below.
		let want = crate::gi::window_chunks_for(self.config.draw_dist);
		if renderer.gi_window_chunks() != want as i32 {
			renderer.gi_resize_window(gpu, want);
			self.pipeline.gi_redirty_all();
		}
		// Still the window AS BUILT: approving a chunk beyond it makes `upload_chunk`'s
		// `rem_euclid` wrap it onto a different cell than the gather reads — one part of the
		// world written into another's slot.
		let gi_window = renderer.gi_window_chunks().min(want as i32);
		self.pipeline.gi_recenter(center, gi_window);
		for pos in self.pipeline.take_gi_dirty() {
			if self.stream.gi_queued.insert(pos) {
				self.stream.gi_queue.push_back(pos);
			}
		}
		for (x, y, z) in self.pipeline.take_gi_edits() {
			renderer.gi_invalidate([x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5]);
		}
		let mut gi_budget = self.config.upload_bytes_max / 4;
		let mut gi_uploads = 0u32;
		while gi_budget >= 4096 {
			if gi_uploads >= GI_UPLOAD_MIN && web_time::Instant::now() >= deadline {
				break;
			}
			let Some(pos) = self.stream.gi_queue.pop_front() else {
				break;
			};
			self.stream.gi_queued.remove(&pos);
			// Outside the window's half-extent: uploading would ALIAS onto
			// an in-window column — drop it; `gi_recenter` re-dirties it
			// the moment it enters coverage. Not resident anymore =
			// evicted while queued: same answer.
			if !self.pipeline.gi_uploadable(pos, gi_window) {
				continue;
			}
			if let Some(chunk) = self.pipeline.classify_gi(pos) {
				renderer.gi_upload(gpu, pos, &chunk);
				gi_budget -= 4096;
				gi_uploads += 1;
			}
		}
		self.bench.pending.gi_ms = ms_since(t_gi);
		// Overlay-restored water resumes flowing (the client twin of the
		// sim's ReadyColumn handling). A server world restores the same
		// cells — the overlay is re-applied there too — but its fluid is
		// the server's, and relaxing it here would predict a flow nobody
		// asked us to.
		if self.session.client.is_none() {
			for &(x, y, z) in &m.overlaid_water {
				self.fluid.schedule_around(x, y, z);
			}
		}
		let t_upload = web_time::Instant::now();
		self.bench.metrics.evicted += m.evicted.len() as u32;
		if !m.evicted.is_empty() {
			// A boundary crossing evicts a whole rim arc at once —
			// hundreds of chunks. Membership has to be a hash lookup, not
			// a linear scan of the evicted Vec per pending element, and the
			// GPU-side release is spread over frames below: buffers merely
			// idle until dropped, so nothing is gained by freeing them all
			// inside one frame.
			for pos in m.evicted {
				if self.stream.pending_evict_set.insert(pos) {
					self.stream.pending_evicts.push_back(pos);
				}
			}
			let gone = &self.stream.pending_evict_set;
			self.stream
				.pending_uploads
				.retain(|out| !gone.contains(&out.pos));
		}
		let appended = !m.meshed.is_empty();
		self.stream.pending_uploads.extend(m.meshed);
		// Nearest-first: on fast travel the queue backs up, and the chunks
		// that matter are the ones at and around the player. The queue is
		// already ordered, so re-sort only when new work arrived or the
		// player changed column — not every frame it happens to be
		// non-empty, which is exactly when frames are tightest.
		if self.stream.pending_uploads.len() > 1
			&& (appended || center != self.stream.upload_sort_center)
		{
			self.stream.upload_sort_center = center;
			self.stream
				.pending_uploads
				.make_contiguous()
				.sort_by_key(|out| {
					let dx = out.pos.x - center.x;
					let dz = out.pos.z - center.z;
					dx * dx + dz * dz
				});
		}
		// Release evicted GPU buffers under the same clock as everything else, always making some
		// progress so the queue can't grow without bound while the player keeps moving.
		//
		// An ARRIVAL raises the floor instead of riding it: past `rim_backlog` this is a world
		// leaving, not a crescent, and the two worlds' meshes are live together until it drains.
		// Everything derived goes with it, once, and is reloaded whole — the device-loss rebuild,
		// deliberately, so an arrival has no state of its own to get wrong: the coarse mirror is
		// keyed by a TOROIDAL wrap and holds another region's occupancy in every slot the new
		// world has yet to write, and the light maps ride the same two uploads. The re-queues are
		// the half that bites if dropped: a jump within draw distance keeps a lens of RETAINED
		// chunks that `gi_recenter` never re-dirties (they never left coverage), so a wipe alone
		// left them dark and reading as air. The re-mesh is what puts `surfaces` back.
		let arrival = self.stream.pending_evicts.len() > rim_backlog(self.config.draw_dist);
		if arrival && !self.stream.evict_burst {
			renderer.gi_clear_derived_state();
			self.pipeline.remesh_all();
			self.pipeline.gi_redirty_all();
		}
		self.stream.evict_burst = arrival;
		let floor = if arrival { EVICT_BURST } else { EVICT_MIN };
		let mut released = 0usize;
		while let Some(pos) = self.stream.pending_evicts.pop_front() {
			// Cancelled: the chunk came back (walked around the rim, or a
			// teleport) and re-uploaded before its removal was reached.
			// Releasing it now would delete the FRESH mesh and leave a hole
			// until something happened to re-dirty the chunk.
			if !self.stream.pending_evict_set.remove(&pos) {
				continue;
			}
			renderer.remove_chunk(pos);
			self.audio.water_audio.remove_chunk(pos);
			released += 1;
			if released >= floor && web_time::Instant::now() >= deadline {
				break;
			}
		}
		// Uploads are bounded by BYTES as well as by the clock: on the web
		// `write_buffer` only records the copy and Chrome's GPU process
		// performs the staging transfer at submit, so a frame that hands
		// over several megabytes looks free to `upload_ms` and hitches
		// anyway.
		let mut uploads = 0u32;
		let mut bytes = 0usize;
		while let Some(out) = self.stream.pending_uploads.pop_front() {
			// Cancels any queued release for this position (see above).
			self.stream.pending_evict_set.remove(&out.pos);
			// Both vertex streams are u32 words (see `mesher::pack`).
			bytes += (out.mesh.solid.len() + out.mesh.water.len()) * 4;
			renderer.upload_chunk(gpu, out.pos, &out.mesh);
			// Waterfall audio follows the same impacts spray renders.
			self.audio
				.water_audio
				.set_chunk(out.pos, out.pos.origin(), &out.mesh.impacts);
			uploads += 1;
			if uploads as usize >= UPLOAD_MIN
				&& (bytes >= self.config.upload_bytes_max || web_time::Instant::now() >= deadline)
			{
				break;
			}
		}
		self.bench.pending.upload_ms = ms_since(t_upload);
		self.bench.metrics.chunks_generated += m.generated;
		self.bench.metrics.meshes_uploaded += uploads;
		self.bench.metrics.upload_kb += (bytes / 1024) as u32;
		self.bench.metrics.gen_jobs.merge(m.gen_jobs);
		self.bench.metrics.mesh_jobs.merge(m.mesh_jobs);
		self.bench.metrics.stale_dropped += m.stale_dropped;
		self.bench.metrics.requeued += m.requeued;
		self.bench.metrics.upload_saturated += m.saturated as u32;
		self.bench.metrics.gauges = PipelineGauges {
			gen_queue: m.gen_queue,
			mesh_queue: m.mesh_queue,
			in_flight: m.in_flight,
			resident: self.pipeline.resident() as u32,
			upload_backlog: self.stream.pending_uploads.len() as u32,
			gi_backlog: self.stream.gi_queue.len() as u32,
			evict_backlog: self.stream.pending_evicts.len() as u32,
			// Written by pump_net earlier this frame; preserved here
			// because the gauges are replaced wholesale.
			net_backlog: self.session.net_backlog.len() as u32,
			// The bytes the loop above just handed over — the number the
			// byte cap actually governs, and the only per-frame signal for
			// a staging transfer `upload_ms` structurally cannot time.
			uploaded: uploads,
			upload_bytes: bytes as u32,
		};
		// First full quiesce = initial world fill complete (the bench's
		// primary streaming-throughput metric).
		if m.gen_queue == 0
			&& m.mesh_queue == 0
			&& m.in_flight == 0
			&& self.stream.pending_uploads.is_empty()
		{
			self.bench.metrics.mark_streamed();
		}
	}
}
