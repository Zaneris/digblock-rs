//! Frame-time, phase, and pipeline metrics. Logged as one-line summaries,
//! optionally dumped to CSV, and snapshotted per-frame for the debug
//! overlay. GPU-free: GPU-derived numbers are fed in as plain data.

use std::collections::VecDeque;
use std::time::Duration;
use web_time::Instant;

const WINDOW: usize = 600; // frames of rolling history
/// A frame counts as a hitch above a plainly doubled standard-refresh
/// frame.
const HITCH_MS: f32 = 33.4;
/// EMA weight for the smoothed per-phase timings shown on the overlay.
const SMOOTH_ALPHA: f32 = 0.05;
/// A frame past this multiple of the estimated display interval missed its
/// slot outright. Below 2.0 so a SINGLE dropped frame counts, but not as
/// tight as 1.5: `refresh_ms` is a low percentile of noise that's
/// symmetric about the true period, so it reads UNDER the true interval,
/// and a factor that tight then lands below the honest bar — where
/// ordinary jitter gets counted as a drop. This margin covers that bias
/// and still sits well below a genuinely doubled frame.
///
/// Correcting the percentile instead would be worse: p10 is deliberate, so
/// that an app dropping every OTHER frame can't have its own half-rate
/// cadence adopted as the display period and report zero drops.
const DROP_FACTOR: f32 = 1.75;
/// Percentile of the frame window taken as the display interval: fast
/// frames are the ones that hit their slot, so the low end of the
/// distribution tracks the refresh period when vsync-bound (and merely
/// "a typical fast frame" when uncapped, which is still the right
/// reference for calling a frame anomalously slow).
const REFRESH_PCTL: f32 = 0.10;
/// Sane bounds for the estimate — guards startup, background tabs, and
/// resume gaps from poisoning it.
const REFRESH_MIN_MS: f32 = 2.0;
const REFRESH_MAX_MS: f32 = 40.0;

/// CPU cost of one frame, split by phase. `total_ms` is filled by
/// [`Metrics::frame`] from the frame-to-frame delta; the rest are timed at
/// their call sites and handed in.
///
/// Phases partition the frame, summing to roughly `busy_ms`: acquire,
/// update, maintain, gi, upload, prep, encode. Note that `sim`, `net` and
/// `persist` are a BREAKDOWN of `update_ms` rather than phases of their
/// own — don't sum them alongside it.
///
/// `busy_ms + acquire_ms + outside_ms` then partitions `total_ms`, so
/// every millisecond of the frame lands in exactly one bucket.
#[derive(Clone, Copy, Default)]
pub struct FrameTimings {
	/// Swapchain acquire wait. On native FIFO this is where vsync blocks;
	/// on the web it is ~0 because `getCurrentTexture` cannot block (see
	/// `busy_ms`).
	pub acquire_ms: f32,
	pub update_ms: f32,
	/// `pump_net`: decode + apply inbound server events (⊂ `update_ms`).
	pub net_ms: f32,
	/// World/player/fluid stepping (⊂ `update_ms`).
	pub sim_ms: f32,
	/// World-save maintenance — fs or localStorage writes (⊂ `update_ms`).
	pub persist_ms: f32,
	/// `Pipeline::maintain`.
	pub maintain_ms: f32,
	/// RTGI window feed: scroll, classify, voxel-window upload. Sits
	/// between `maintain` and the mesh upload loop and used to be timed by
	/// nothing at all — a classify burst over hundreds of chunks (a full
	/// cell walk each) landed only in `busy_ms`, with no phase to attribute
	/// it to.
	pub gi_ms: f32,
	/// Evict + mesh-upload loop.
	pub upload_ms: f32,
	/// Post-pipeline, pre-encode frame prep: remote-player sampling, name
	/// tags, UI rebuild, frame uniforms. Was previously untimed — a spike
	/// here showed in `total_ms` and in no phase at all.
	pub prep_ms: f32,
	/// Renderer encode + submit.
	pub encode_ms: f32,
	/// Main-thread occupancy: redraw-handler entry through submit, minus
	/// the acquire wait. THE signal the adaptive stream budget feeds on.
	///
	/// It must be measured, not derived from `total_ms`: the frame-to-frame
	/// delta includes whatever the platform spends idle between frames, and
	/// on the web (rAF-paced, non-blocking acquire) that idle is the whole
	/// margin the controller is trying to detect.
	pub busy_ms: f32,
	/// The complement of `busy_ms` + `acquire_ms`: everything the frame
	/// spent with the main thread NOT running. Present, the compositor,
	/// the browser's rAF dispatch — and, on the web, Chrome's GPU-process
	/// staging transfer, which `write_buffer` defers to submit (see the
	/// upload loop's byte cap). That transfer is charged to no phase at
	/// all: `upload_ms` times the record, never the copy.
	///
	/// Derived, not measured, so it absorbs every accounting error too.
	/// On a healthy vsync-paced frame it IS the idle margin and should be
	/// most of the period. A frame that grows HERE while `busy_ms` holds
	/// flat spent its time somewhere the main thread never ran, which is
	/// the one conclusion no other phase can support.
	pub outside_ms: f32,
	pub total_ms: f32,
}

impl FrameTimings {
	/// Apply `f` to every field pairwise — one place to add a phase.
	fn zip_mut(&mut self, x: &FrameTimings, mut f: impl FnMut(&mut f32, f32)) {
		f(&mut self.acquire_ms, x.acquire_ms);
		f(&mut self.update_ms, x.update_ms);
		f(&mut self.net_ms, x.net_ms);
		f(&mut self.sim_ms, x.sim_ms);
		f(&mut self.persist_ms, x.persist_ms);
		f(&mut self.maintain_ms, x.maintain_ms);
		f(&mut self.gi_ms, x.gi_ms);
		f(&mut self.upload_ms, x.upload_ms);
		f(&mut self.prep_ms, x.prep_ms);
		f(&mut self.encode_ms, x.encode_ms);
		f(&mut self.busy_ms, x.busy_ms);
		f(&mut self.outside_ms, x.outside_ms);
		f(&mut self.total_ms, x.total_ms);
	}

	fn ema(&mut self, x: &FrameTimings) {
		self.zip_mut(x, |s, v| *s = *s * (1.0 - SMOOTH_ALPHA) + v * SMOOTH_ALPHA);
	}

	fn add(&mut self, x: &FrameTimings) {
		self.zip_mut(x, |s, v| *s += v);
	}

	fn scaled(&self, k: f32) -> FrameTimings {
		let mut out = *self;
		out.zip_mut(self, |s, v| *s = v * k);
		out
	}
}

/// Aggregate of worker-job durations (chunk gen or mesh).
#[derive(Clone, Copy, Default)]
pub struct JobTiming {
	pub count: u32,
	pub total_ms: f32,
	pub max_ms: f32,
}

impl JobTiming {
	pub fn record(&mut self, ms: f32) {
		self.count += 1;
		self.total_ms += ms;
		self.max_ms = self.max_ms.max(ms);
	}

	pub fn merge(&mut self, other: JobTiming) {
		self.count += other.count;
		self.total_ms += other.total_ms;
		self.max_ms = self.max_ms.max(other.max_ms);
	}

	pub fn avg_ms(&self) -> f32 {
		if self.count == 0 {
			0.0
		} else {
			self.total_ms / self.count as f32
		}
	}
}

/// Live connection health, filled by the net client each frame.
///
/// Lives here rather than in `digblock-net` so the overlay and the CSV can
/// read it without core depending on net (net already depends on core, so
/// the arrow only points one way).
///
/// `rtt_*` is measured on the INPUT→ACK loop, not a separate ping: the
/// client stamps a tick when it first goes on the wire and stops the clock
/// when a snapshot acknowledges that tick. That is the latency a player
/// actually feels — it includes the server's jitter buffer and the
/// snapshot cadence, which a ping/pong would quietly exclude — and it
/// needs no protocol round trip of its own.
#[derive(Clone, Copy, Default)]
pub struct NetStats {
	/// Smoothed input→ack round trip.
	pub rtt_ms: f32,
	/// The TRANSPORT's own round trip — QUIC's estimate, what a ping
	/// reports, and the same quantity the server browser's probe shows.
	/// Zero when the transport has none (loopback, the benchmark wires).
	///
	/// Kept beside `rtt_ms` rather than replacing it because they answer
	/// different questions: this one is "how far away is the server", and
	/// `rtt_ms` is "how long until my action is authoritative", which
	/// carries the tick boundary, the snapshot cadence and the input buffer
	/// on top. Their DIFFERENCE is the server-side pipeline, which is the
	/// part that responds to SNAPSHOT_DIVISOR and the send stride.
	///
	/// Deriving one from the other by subtracting a constant does not work
	/// and was measured not to: on a LAN every `rtt_ms` sample is either one
	/// tick or two, and the wire itself is a small fraction of one such
	/// quantum, so it simply is not present in the signal.
	pub wire_rtt_ms: f32,
	/// Mean absolute deviation of the same signal — the number that says
	/// whether an average RTT is trustworthy.
	pub jitter_ms: f32,
	/// Most recent single measurement.
	pub rtt_last_ms: f32,
	pub down_kbps: f32,
	pub up_kbps: f32,
	/// Snapshots accepted per second (nominally `TICK_RATE /
	/// SNAPSHOT_DIVISOR`; below that means loss or a struggling server).
	pub snapshots_per_sec: f32,
	/// Reconciles that had to rewind and replay. Steady-state this should
	/// be ~0; sustained non-zero means the server is rejecting claims.
	pub replays_per_sec: f32,
	/// Predictions awaiting a verdict, and remotes being interpolated.
	pub pending_edits: u32,
	pub remotes: u32,
	/// How far ahead of the last acked tick the client is predicting —
	/// the client-side view of the jitter-buffer depth.
	pub tick_lead: u32,
}

/// Instantaneous pipeline state (last-write gauges, not counters).
#[derive(Clone, Copy, Default)]
pub struct PipelineGauges {
	pub gen_queue: u32,
	pub mesh_queue: u32,
	pub in_flight: u32,
	/// Generated chunks resident in the pipeline store.
	pub resident: u32,
	/// Meshes drained but not yet uploaded. Engine-side backlog: the
	/// channel-side `saturated` flag cannot see it, so a starved upload
	/// loop used to be invisible.
	pub upload_backlog: u32,
	/// Chunks evicted but whose GPU buffers are still queued for release.
	pub evict_backlog: u32,
	/// Inbound net events queued past this frame's edit budget.
	pub net_backlog: u32,
	/// Meshes handed to the GPU THIS frame, and their vertex bytes — the
	/// per-frame view the run-window `meshes_uploaded` counter can't give.
	/// Paired with `outside_ms` these are the whole point: on the web the
	/// staging copy lands in another process at submit, so bytes are the
	/// only leading indicator of a hitch that no phase timing records.
	pub uploaded: u32,
	pub upload_bytes: u32,
}

/// What the renderer did last frame.
#[derive(Clone, Copy, Default)]
pub struct RenderStats {
	/// Chunks with GPU meshes.
	pub chunks_resident: u32,
	pub chunks_drawn: u32,
	pub chunks_culled: u32,
	pub water_draws: u32,
	pub draw_calls: u32,
	pub triangles: u32,
	/// The GI window's sky ceiling (R18) — the highest occupied cell in the
	/// whole window, and the early-out every non-descending ray in the
	/// resolve breaks on. It is a session HIGH-WATER MARK in practice: the
	/// coarse mirror it is scanned from is never cleared on eviction, so
	/// this climbs as terrain is explored and does not come back down. That
	/// makes it the number to watch when frame time degrades over a session
	/// — a filled window is the cost the engine actually has, and an empty
	/// one (the no-occupied-cell sentinel, where every ray breaks on its
	/// first step) is the cheapest it can ever be.
	pub sky_y: f32,
	/// Emitter cells the GI knows about. Bounded by `MAX_LIGHTS` on the
	/// GPU but NOT on the CPU: the map is only pruned when a chunk
	/// re-classifies to zero emitters, so this also ratchets.
	pub emitters: u32,
}

/// GPU pass durations from timestamp queries (a few frames latent).
#[derive(Clone, Copy, Default)]
pub struct GpuPassMs {
	/// Depth prepass over opaque solids.
	pub prepass_ms: f32,
	pub main_ms: f32,
	/// The split-off water pass (blit + prepass + color).
	pub water_ms: f32,
	pub overlay_ms: f32,
	/// RTGI probe-update compute.
	pub gi_ms: f32,
	/// The half-res GI trace.
	pub gi_trace_ms: f32,
	/// Temporal accumulation + à-trous — BOTH iterations summed. The two
	/// shared one query pair until the second silently overwrote the
	/// first, so this column reported the stride-2 pass alone.
	pub gi_denoise_ms: f32,
	/// The D32F → R32Float depth blit — a full-screen read+write that ran
	/// untimed since it was written.
	pub depth_blit_ms: f32,
	/// The viewmodel pass (its own pass, its own depth clear).
	pub viewmodel_ms: f32,
	/// The upscale pass: render resolution → the surface.
	pub upscale_ms: f32,
}

/// Where the camera is standing and when — the overlay's reproduction
/// line, and the only part of [`OverlayStats`] that is not a measurement.
///
/// It exists because a screenshot of a rendering bug is nearly useless
/// without the world it was taken in: every field here is an argument the
/// capture flags take (`--seed --pos --yaw --pitch --tod`), so a photo of
/// the panel is a runnable repro. `Metrics` cannot know any of it — the
/// engine fills this in after `overlay_stats()`.
#[derive(Clone, Copy, Default)]
pub struct WorldStats {
	/// Eye position: exactly what `--pos` sets, so it can be typed back.
	pub pos: [f32; 3],
	pub yaw_deg: f32,
	pub pitch_deg: f32,
	/// Sun angle from straight-down, i.e. `--tod`.
	pub tod_deg: f32,
	/// Printed at full round-trip precision — worldgen is a pure function
	/// of this, so a truncated seed is a different world.
	pub seed: f64,
}

/// Per-frame snapshot for the debug overlay: everything it draws.
#[derive(Clone, Default)]
pub struct OverlayStats {
	pub fps: f32,
	pub avg_ms: f32,
	pub p95_ms: f32,
	pub p99_ms: f32,
	pub worst_ms: f32,
	/// Hitches in the current report window / since start.
	pub hitches_window: u32,
	pub hitches_total: u64,
	/// Frames that missed their display slot (see `DROP_FACTOR`) — catches
	/// the single dropped frames `hitches` is too coarse to see.
	pub dropped_window: u32,
	pub dropped_total: u64,
	/// Estimated display interval backing `dropped_*`; 0.0 until known.
	pub refresh_ms: f32,
	pub last: FrameTimings,
	pub smooth: FrameTimings,
	/// None until the first timestamp readback (or unsupported).
	pub gpu: Option<GpuPassMs>,
	/// Job aggregates for the report window so far.
	pub gen_jobs: JobTiming,
	pub mesh_jobs: JobTiming,
	pub generated: u32,
	pub uploaded: u32,
	pub upload_kb: u32,
	pub stale_dropped: u32,
	pub requeued: u32,
	pub evicted: u32,
	pub upload_saturated: u32,
	pub gauges: PipelineGauges,
	/// `None` when not connected to a server.
	pub net: Option<NetStats>,
	pub render: RenderStats,
	/// Filled by the engine, not by `Metrics` — see [`WorldStats`].
	pub world: WorldStats,
	pub gpu_buffer_bytes: u64,
	pub workers: u32,
	/// Streaming budget in effect this frame (adaptive on web; the fixed
	/// config value elsewhere).
	pub stream_budget_ms: f32,
}

pub struct Summary {
	pub fps: f32,
	pub avg_ms: f32,
	pub p95_ms: f32,
	pub p99_ms: f32,
	pub worst_ms: f32,
}

pub struct Metrics {
	frame_ms: VecDeque<f32>,
	sort_scratch: Vec<f32>,
	last_frame: Option<Instant>,
	started_at: Instant,
	/// First presented frame — the time base for run elapsed / fill time
	/// (excludes GPU + window init).
	first_frame_at: Option<Instant>,
	/// Seconds after the first frame when the chunk pipeline first
	/// quiesced (initial world fill complete).
	streamed_at: Option<f32>,
	/// Full-run frame history (bench mode only; one f32 per frame) so the
	/// exit summary can compute run-level percentiles.
	run_history: Option<Vec<f32>>,
	last_report: Instant,
	report_every: Duration,
	frames_since_report: u32,
	// Per-phase timings: last frame, EMA, and report-window accumulation.
	pub last: FrameTimings,
	smooth: FrameTimings,
	win_sum: FrameTimings,
	win_frames: u32,
	hitches: u32,
	hitches_total: u64,
	// Run-lifetime aggregates (never reset; bench summary).
	run_frames: u64,
	run_gen_jobs: JobTiming,
	run_mesh_jobs: JobTiming,
	// Worker-job aggregates and event counters (reset each report window).
	pub gen_jobs: JobTiming,
	pub mesh_jobs: JobTiming,
	pub chunks_generated: u32,
	pub meshes_uploaded: u32,
	/// Mesh KiB handed over this window. Pairs with `meshes_uploaded`:
	/// count alone can't distinguish a cheap frame from an expensive one,
	/// and bytes are what the web's upload cap is actually denominated in.
	pub upload_kb: u32,
	pub stale_dropped: u32,
	pub requeued: u32,
	pub evicted: u32,
	pub upload_saturated: u32,
	// Gauges (last-write-wins).
	pub gauges: PipelineGauges,
	/// Connection health, written each frame while connected and cleared
	/// on disconnect. `None` = single player.
	pub net: Option<NetStats>,
	pub render: RenderStats,
	pub gpu: Option<GpuPassMs>,
	pub gpu_buffer_bytes: u64,
	pub workers: u32,
	/// Streaming budget in effect (last-write gauge; see the engine's
	/// adaptive controller).
	pub stream_budget_ms: f32,
	/// Log a per-phase breakdown of every frame past the drop bar —
	/// attributes pacing spikes without wading through CSVs. Set by the
	/// launcher (desktop: `DIG_SPIKELOG`; web: `?spikelog`) rather than
	/// read from the environment here, which wasm has none of.
	spike_log: bool,
	/// Estimated display interval, tracked from the fastest frames in the
	/// window (see [`Metrics::refresh_ms`]).
	refresh_ms: f32,
	/// Frames past `DROP_FACTOR` × the estimated display interval — i.e.
	/// visibly dropped ones. `HITCH_MS` is coarse enough that a single
	/// dropped frame never registered at runtime.
	dropped: u32,
	dropped_total: u64,
	csv: Option<std::io::BufWriter<std::fs::File>>,
}

/// Schema note: columns are only ever APPENDED to a group, never
/// reordered, but old CSVs still can't be diffed naively against new ones
/// (`BENCH.md` carries the same warning for the `gpu_water_ms` insertion).
const CSV_HEADER: &str = "t_s,fps,avg_ms,p95_ms,p99_ms,worst_ms,hitches,dropped,refresh_ms,\
busy_ms,out_ms,acq_ms,upd_ms,net_ms,sim_ms,sav_ms,mnt_ms,gi_ms,upl_ms,prep_ms,enc_ms,\
gpu_prepass_ms,gpu_main_ms,gpu_water_ms,gpu_overlay_ms,\
gpu_gi_ms,gpu_gi_trace_ms,gpu_gi_denoise_ms,gpu_depth_blit_ms,gpu_viewmodel_ms,gpu_upscale_ms,\
gen_n,gen_avg_ms,gen_max_ms,mesh_n,mesh_avg_ms,mesh_max_ms,\
uploads,upl_kb,stale,requeue,evict,sat,resident,gen_q,mesh_q,in_flight,upl_q,evict_q,\
drawn,culled,water,draws,tris,gpu_bytes,budget_ms,sky_y,emitters";

impl Metrics {
	/// `spike_log` comes from the launcher: core is env-free by convention,
	/// and `std::env` on wasm would silently answer None forever.
	pub fn new(csv_path: Option<&std::path::Path>, spike_log: bool) -> Self {
		let csv = csv_path.and_then(|p| {
			use std::io::Write;
			let open = std::fs::File::create(p).and_then(|f| {
				let mut w = std::io::BufWriter::new(f);
				writeln!(w, "{CSV_HEADER}")?;
				Ok(w)
			});
			match open {
				Ok(w) => Some(w),
				Err(e) => {
					log::warn!("metrics csv {}: {e} — disabled", p.display());
					None
				}
			}
		});
		let now = Instant::now();
		Self {
			frame_ms: VecDeque::with_capacity(WINDOW),
			sort_scratch: Vec::with_capacity(WINDOW),
			last_frame: None,
			started_at: now,
			first_frame_at: None,
			streamed_at: None,
			run_history: None,
			last_report: now,
			report_every: Duration::from_secs(5),
			frames_since_report: 0,
			last: FrameTimings::default(),
			smooth: FrameTimings::default(),
			win_sum: FrameTimings::default(),
			win_frames: 0,
			hitches: 0,
			hitches_total: 0,
			run_frames: 0,
			run_gen_jobs: JobTiming::default(),
			run_mesh_jobs: JobTiming::default(),
			gen_jobs: JobTiming::default(),
			mesh_jobs: JobTiming::default(),
			chunks_generated: 0,
			meshes_uploaded: 0,
			upload_kb: 0,
			stale_dropped: 0,
			requeued: 0,
			evicted: 0,
			upload_saturated: 0,
			gauges: PipelineGauges::default(),
			net: None,
			render: RenderStats::default(),
			gpu: None,
			gpu_buffer_bytes: 0,
			workers: 0,
			stream_budget_ms: 0.0,
			spike_log,
			refresh_ms: 0.0,
			dropped: 0,
			dropped_total: 0,
			csv,
		}
	}

	/// Estimated display interval; 0.0 until the window has enough frames.
	pub fn refresh_ms(&self) -> f32 {
		self.refresh_ms
	}

	/// Re-estimate the display interval from the low end of the window.
	/// Cheap enough per frame only because `sort_scratch` is reused and the
	/// window is bounded; called from the report path instead.
	fn update_refresh(&mut self) {
		if self.frame_ms.len() < 30 {
			return;
		}
		self.sort_scratch.clear();
		self.sort_scratch.extend(self.frame_ms.iter().copied());
		self.sort_scratch
			.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
		let i = ((self.sort_scratch.len() as f32 * REFRESH_PCTL) as usize)
			.min(self.sort_scratch.len() - 1);
		self.refresh_ms = self.sort_scratch[i].clamp(REFRESH_MIN_MS, REFRESH_MAX_MS);
	}

	/// Call once per presented frame with that frame's phase timings.
	/// `total_ms` is overwritten with the frame-to-frame delta.
	/// Suspend/resume safe: call `pause()` when rendering stops so the gap
	/// isn't counted as a frame.
	pub fn frame(&mut self, mut timings: FrameTimings) {
		let now = Instant::now();
		if self.first_frame_at.is_none() {
			self.first_frame_at = Some(now);
		}
		if let Some(last) = self.last_frame {
			timings.total_ms = (now - last).as_secs_f32() * 1000.0;
			// Only derivable here: `total_ms` isn't known until now. Clamped
			// because the two measured terms are sampled at slightly
			// different points and a fast frame can round them past it.
			timings.outside_ms = (timings.total_ms - timings.busy_ms - timings.acquire_ms).max(0.0);
			if self.frame_ms.len() == WINDOW {
				self.frame_ms.pop_front();
			}
			self.frame_ms.push_back(timings.total_ms);
			if let Some(h) = &mut self.run_history {
				h.push(timings.total_ms);
			}
			if timings.total_ms > HITCH_MS {
				self.hitches += 1;
				self.hitches_total += 1;
			}
			if self.refresh_ms > 0.0 && timings.total_ms > self.refresh_ms * DROP_FACTOR {
				self.dropped += 1;
				self.dropped_total += 1;
			}
			// Per-spike phase attribution (see `spike_log`), on the SAME bar
			// as `dropped` — so every counted drop gets exactly one logged
			// breakdown and the counter leads straight to a cause. It was a
			// separate factor barely above the estimate, which under vsync
			// sat below the AVERAGE frame (see `DROP_FACTOR` for why the
			// estimate reads low) and logged every frame instead of the
			// anomalies.
			let spike_over = if self.refresh_ms > 0.0 {
				self.refresh_ms * DROP_FACTOR
			} else {
				8.4
			};
			if self.spike_log && timings.total_ms > spike_over {
				log::info!(
					"spike f{} total {:.2} busy {:.2} out {:.2} | acq {:.2} upd {:.2} (net {:.2} sim {:.2} sav {:.2}) mnt {:.2} gi {:.2} upl {:.2} prep {:.2} enc {:.2} | gpu pre {:.2} main {:.2} water {:.2} gi {:.2} trace {:.2} den {:.2} | draws {} tris {}k | up {} ({}k) upl_q {} evict_q {} budget {:.1}",
					self.run_frames,
					timings.total_ms,
					timings.busy_ms,
					timings.outside_ms,
					timings.acquire_ms,
					timings.update_ms,
					timings.net_ms,
					timings.sim_ms,
					timings.persist_ms,
					timings.maintain_ms,
					timings.gi_ms,
					timings.upload_ms,
					timings.prep_ms,
					timings.encode_ms,
					self.gpu.as_ref().map_or(0.0, |g| g.prepass_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.main_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.water_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.gi_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.gi_trace_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.gi_denoise_ms),
					self.render.draw_calls,
					self.render.triangles / 1000,
					self.gauges.uploaded,
					self.gauges.upload_bytes / 1024,
					self.gauges.upload_backlog,
					self.gauges.evict_backlog,
					self.stream_budget_ms,
				);
			}
			self.last = timings;
			self.smooth.ema(&timings);
			self.win_sum.add(&timings);
			self.win_frames += 1;
			self.frames_since_report += 1;
			self.run_frames += 1;
		}
		self.last_frame = Some(now);

		if now - self.last_report >= self.report_every {
			self.update_refresh();
			self.report();
			self.last_report = now;
		}
	}

	pub fn pause(&mut self) {
		self.last_frame = None;
	}

	pub fn summary(&mut self) -> Option<Summary> {
		self.sort_scratch.clear();
		self.sort_scratch.extend(self.frame_ms.iter().copied());
		Self::summarize(&mut self.sort_scratch)
	}

	/// Sorts `frames` in place and computes the percentile summary.
	fn summarize(frames: &mut [f32]) -> Option<Summary> {
		if frames.is_empty() {
			return None;
		}
		frames.sort_by(|a, b| a.partial_cmp(b).unwrap());
		let avg = frames.iter().sum::<f32>() / frames.len() as f32;
		let pct = |p: f32| frames[((frames.len() as f32 * p) as usize).min(frames.len() - 1)];
		Some(Summary {
			fps: 1000.0 / avg,
			avg_ms: avg,
			p95_ms: pct(0.95),
			p99_ms: pct(0.99),
			worst_ms: *frames.last().unwrap(),
		})
	}

	/// Bench mode: retain every frame time so [`Self::bench_summary`] can
	/// report run-level percentiles instead of last-window ones.
	pub fn record_run_history(&mut self) {
		self.run_history = Some(Vec::with_capacity(2048));
	}

	/// Record that the chunk pipeline has fully quiesced. The first call
	/// stamps the fill time (initial world fill); later calls are no-ops.
	pub fn mark_streamed(&mut self) {
		if self.streamed_at.is_none() {
			let base = self.first_frame_at.unwrap_or(self.started_at);
			self.streamed_at = Some(base.elapsed().as_secs_f32());
		}
	}

	/// Snapshot for the debug overlay. Safe before the first frame
	/// (returns zeros).
	pub fn overlay_stats(&mut self) -> OverlayStats {
		let (fps, avg_ms, p95_ms, p99_ms, worst_ms) = match self.summary() {
			Some(s) => (s.fps, s.avg_ms, s.p95_ms, s.p99_ms, s.worst_ms),
			None => (0.0, 0.0, 0.0, 0.0, 0.0),
		};
		OverlayStats {
			fps,
			avg_ms,
			p95_ms,
			p99_ms,
			worst_ms,
			hitches_window: self.hitches,
			hitches_total: self.hitches_total,
			dropped_window: self.dropped,
			dropped_total: self.dropped_total,
			refresh_ms: self.refresh_ms,
			last: self.last,
			smooth: self.smooth,
			gpu: self.gpu,
			gen_jobs: self.gen_jobs,
			mesh_jobs: self.mesh_jobs,
			generated: self.chunks_generated,
			uploaded: self.meshes_uploaded,
			upload_kb: self.upload_kb,
			stale_dropped: self.stale_dropped,
			requeued: self.requeued,
			evicted: self.evicted,
			upload_saturated: self.upload_saturated,
			gauges: self.gauges,
			net: self.net,
			render: self.render,
			// Not a measurement and not `Metrics`' to know: the engine
			// stamps it on the way to the overlay.
			world: WorldStats::default(),
			gpu_buffer_bytes: self.gpu_buffer_bytes,
			workers: self.workers,
			stream_budget_ms: self.stream_budget_ms,
		}
	}

	/// One-line, machine-diffable run summary for benchmark exits.
	/// With run history recorded, percentiles cover the whole run minus a
	/// warmup prefix (whose worst frame is reported separately); otherwise
	/// they fall back to the last WINDOW frames. Elapsed/fps/fill are
	/// measured from the first presented frame.
	pub fn bench_summary(&mut self) -> Option<String> {
		const WARMUP_FRAMES: usize = 60;
		let elapsed = self
			.first_frame_at
			.unwrap_or(self.started_at)
			.elapsed()
			.as_secs_f32();
		// Frame-pacing signals: the first counts frames that wouldn't fit a
		// high-refresh vsync slot (budget-fit, for uncapped runs); the
		// second counts frames half again past that slot — a visibly
		// dropped frame when vsync-paced.
		const VSYNC_120HZ_MS: f32 = 8.4;
		const DROP_MS: f32 = 12.5;
		let frames = self.run_frames;
		let (s, warmup, miss) = match self.run_history.take().filter(|h| h.len() > WARMUP_FRAMES) {
			Some(mut h) => {
				let worst = h[..WARMUP_FRAMES].iter().fold(0.0f32, |a, &b| a.max(b));
				let run = &mut h[WARMUP_FRAMES..];
				let over = run.iter().filter(|&&ms| ms > VSYNC_120HZ_MS).count();
				let drop = run.iter().filter(|&&ms| ms > DROP_MS).count();
				let s = Self::summarize(run)?;
				(
					s,
					format!(" (warmup worst {worst:.1})"),
					format!(" | over-8.3ms {over} over-12.5ms {drop}"),
				)
			}
			None => (self.summary()?, String::new(), String::new()),
		};
		let fill = self
			.streamed_at
			.map_or_else(|| "n/a".to_owned(), |t| format!("{t:.2}s"));
		let mut generation = self.run_gen_jobs;
		generation.merge(self.gen_jobs);
		let mut mesh = self.run_mesh_jobs;
		mesh.merge(self.mesh_jobs);
		let per_sec = |n: u32| n as f32 / elapsed.max(0.001);
		Some(format!(
			"bench: frames {frames} in {elapsed:.1}s ({:.1} fps) | fill {fill} | avg {:.2} p95 {:.2} p99 {:.2} worst {:.2} ms{warmup}{miss} | hitches {} | resident {} | gen {} jobs avg {:.2} max {:.2} ms ({:.0}/s) | mesh {} jobs avg {:.2} max {:.2} ms ({:.0}/s)",
			frames as f32 / elapsed.max(0.001),
			s.avg_ms,
			s.p95_ms,
			s.p99_ms,
			s.worst_ms,
			self.hitches_total,
			self.gauges.resident,
			generation.count,
			generation.avg_ms(),
			generation.max_ms,
			per_sec(generation.count),
			mesh.count,
			mesh.avg_ms(),
			mesh.max_ms,
			per_sec(mesh.count),
		))
	}

	/// Fill `out` with the last `n` frame totals, oldest first (for the
	/// overlay's frame-time graph).
	pub fn history_tail(&self, n: usize, out: &mut Vec<f32>) {
		out.clear();
		let skip = self.frame_ms.len().saturating_sub(n);
		out.extend(self.frame_ms.iter().skip(skip).copied());
	}

	fn report(&mut self) {
		let elapsed = self.last_report.elapsed().as_secs_f32();
		let true_fps = if elapsed > 0.0 {
			self.frames_since_report as f32 / elapsed
		} else {
			0.0
		};
		let Some(s) = self.summary() else {
			self.frames_since_report = 0;
			return;
		};
		log::info!(
			"{true_fps:5.1} fps | avg {:5.2} p95 {:5.2} p99 {:5.2} worst {:6.2} ms | busy {:5.2} out {:5.2} | hitch {} drop {} | gen {} up {} ({} KiB) | gen q {} mesh q {} fly {} upl q {} | resident {} | gpu {:.1} MB",
			s.avg_ms,
			s.p95_ms,
			s.p99_ms,
			s.worst_ms,
			if self.win_frames > 0 {
				self.win_sum.busy_ms / self.win_frames as f32
			} else {
				0.0
			},
			if self.win_frames > 0 {
				self.win_sum.outside_ms / self.win_frames as f32
			} else {
				0.0
			},
			self.hitches,
			self.dropped,
			self.chunks_generated,
			self.meshes_uploaded,
			self.upload_kb,
			self.gauges.gen_queue,
			self.gauges.mesh_queue,
			self.gauges.in_flight,
			self.gauges.upload_backlog,
			self.gauges.resident,
			self.gpu_buffer_bytes as f64 / (1024.0 * 1024.0),
		);
		if let Some(w) = &mut self.csv {
			use std::io::Write;
			let t = self.started_at.elapsed().as_secs_f32();
			let win = if self.win_frames > 0 {
				self.win_sum.scaled(1.0 / self.win_frames as f32)
			} else {
				FrameTimings::default()
			};
			let gpu = self.gpu.unwrap_or_default();
			let r = &self.render;
			let _ = writeln!(
				w,
				"{t:.1},{true_fps:.1},{:.2},{:.2},{:.2},{:.2},{},{},{:.2},\
{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},\
{:.3},{:.3},{:.3},{:.3},\
{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},\
{},{:.2},{:.2},{},{:.2},{:.2},\
{},{},{},{},{},{},{},{},{},{},{},{},\
{},{},{},{},{},{},{:.2},{:.1},{}",
				s.avg_ms,
				s.p95_ms,
				s.p99_ms,
				s.worst_ms,
				self.hitches,
				self.dropped,
				self.refresh_ms,
				win.busy_ms,
				win.outside_ms,
				win.acquire_ms,
				win.update_ms,
				win.net_ms,
				win.sim_ms,
				win.persist_ms,
				win.maintain_ms,
				win.gi_ms,
				win.upload_ms,
				win.prep_ms,
				win.encode_ms,
				gpu.prepass_ms,
				gpu.main_ms,
				gpu.water_ms,
				gpu.overlay_ms,
				gpu.gi_ms,
				gpu.gi_trace_ms,
				gpu.gi_denoise_ms,
				gpu.depth_blit_ms,
				gpu.viewmodel_ms,
				gpu.upscale_ms,
				self.gen_jobs.count,
				self.gen_jobs.avg_ms(),
				self.gen_jobs.max_ms,
				self.mesh_jobs.count,
				self.mesh_jobs.avg_ms(),
				self.mesh_jobs.max_ms,
				self.meshes_uploaded,
				self.upload_kb,
				self.stale_dropped,
				self.requeued,
				self.evicted,
				self.upload_saturated,
				self.gauges.resident,
				self.gauges.gen_queue,
				self.gauges.mesh_queue,
				self.gauges.in_flight,
				self.gauges.upload_backlog,
				self.gauges.evict_backlog,
				r.chunks_drawn,
				r.chunks_culled,
				r.water_draws,
				r.draw_calls,
				r.triangles,
				self.gpu_buffer_bytes,
				self.stream_budget_ms,
				r.sky_y,
				r.emitters,
			);
			let _ = w.flush();
		}
		self.frames_since_report = 0;
		self.win_sum = FrameTimings::default();
		self.win_frames = 0;
		self.hitches = 0;
		self.dropped = 0;
		self.run_gen_jobs.merge(self.gen_jobs);
		self.run_mesh_jobs.merge(self.mesh_jobs);
		self.gen_jobs = JobTiming::default();
		self.mesh_jobs = JobTiming::default();
		self.chunks_generated = 0;
		self.meshes_uploaded = 0;
		self.upload_kb = 0;
		self.stale_dropped = 0;
		self.requeued = 0;
		self.evicted = 0;
		self.upload_saturated = 0;
	}
}

impl Drop for Metrics {
	fn drop(&mut self) {
		// Flush the partial report window so short or uncapped runs don't
		// silently lose their tail frames from the CSV.
		if self.win_frames > 0 && self.csv.is_some() {
			self.report();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ring_caps_at_window() {
		let mut m = Metrics::new(None, false);
		// First call only arms last_frame; every later call pushes one.
		for _ in 0..WINDOW + 10 {
			m.frame(FrameTimings::default());
		}
		assert_eq!(m.frame_ms.len(), WINDOW);
	}

	/// `CSV_HEADER` and the row's format string are two hand-written lists
	/// with nothing tying them together — arity is checked by the compiler
	/// only WITHIN the format string. A skew shifts every later column by
	/// one, and the failure mode is a plausible number read from the wrong
	/// field rather than an error (it cost a whole analysis pass once).
	#[test]
	fn csv_row_matches_header_width() {
		let path = std::env::temp_dir().join("digblock-metrics-csv-width.csv");
		let _ = std::fs::remove_file(&path);
		let mut m = Metrics::new(Some(&path), false);
		// Report on the next frame instead of waiting out the window.
		m.report_every = Duration::ZERO;
		m.frame(FrameTimings::default());
		m.frame(FrameTimings::default());
		drop(m);
		let text = std::fs::read_to_string(&path).unwrap();
		let mut lines = text.lines();
		let header = lines.next().expect("header written");
		let row = lines.next().expect("at least one data row");
		assert_eq!(
			header.split(',').count(),
			row.split(',').count(),
			"CSV header/row skew\nheader: {header}\nrow:    {row}"
		);
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn job_timing_math() {
		let mut j = JobTiming::default();
		assert_eq!(j.avg_ms(), 0.0);
		j.record(2.0);
		j.record(4.0);
		assert_eq!(j.count, 2);
		assert_eq!(j.avg_ms(), 3.0);
		assert_eq!(j.max_ms, 4.0);
		let mut k = JobTiming::default();
		k.record(10.0);
		j.merge(k);
		assert_eq!(j.count, 3);
		assert_eq!(j.max_ms, 10.0);
		assert_eq!(j.total_ms, 16.0);
	}

	#[test]
	fn hitch_counting() {
		let mut m = Metrics::new(None, false);
		// First call sets last_frame; deltas are measured wall-clock, so
		// synthesize hitches by injecting directly.
		m.frame(FrameTimings::default());
		m.frame_ms.push_back(50.0);
		m.hitches += 1;
		m.hitches_total += 1;
		assert_eq!(m.hitches, 1);
		assert_eq!(m.hitches_total, 1);
		let stats = m.overlay_stats();
		assert_eq!(stats.hitches_window, 1);
	}

	#[test]
	fn overlay_stats_safe_before_first_frame() {
		let mut m = Metrics::new(None, false);
		let s = m.overlay_stats();
		assert_eq!(s.fps, 0.0);
		assert_eq!(s.gen_jobs.count, 0);
		assert!(s.gpu.is_none());
	}

	#[test]
	fn streamed_marks_only_once() {
		let mut m = Metrics::new(None, false);
		assert!(m.streamed_at.is_none());
		m.mark_streamed();
		let first = m.streamed_at;
		assert!(first.is_some());
		std::thread::sleep(Duration::from_millis(2));
		m.mark_streamed();
		assert_eq!(m.streamed_at, first);
	}

	#[test]
	fn bench_summary_falls_back_below_warmup() {
		let mut m = Metrics::new(None, false);
		m.record_run_history();
		// Two frames is under the warmup threshold — must fall back to the
		// window summary instead of returning None.
		m.frame(FrameTimings::default());
		m.frame(FrameTimings::default());
		let line = m.bench_summary().expect("summary");
		assert!(line.contains("fill n/a"));
	}

	#[test]
	fn history_tail_returns_recent() {
		let mut m = Metrics::new(None, false);
		for i in 0..10 {
			m.frame_ms.push_back(i as f32);
		}
		let mut out = Vec::new();
		m.history_tail(4, &mut out);
		assert_eq!(out, vec![6.0, 7.0, 8.0, 9.0]);
		m.history_tail(100, &mut out);
		assert_eq!(out.len(), 10);
	}
}
