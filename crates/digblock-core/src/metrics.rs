//! Frame-time, phase, and pipeline metrics. Logged as one-line summaries,
//! optionally dumped to CSV, and snapshotted per-frame for the debug
//! overlay. GPU-free: GPU-derived numbers are fed in as plain data.

use std::collections::VecDeque;
use std::time::Duration;
use web_time::Instant;

const WINDOW: usize = 600; // ~10s at 60fps
/// A frame counts as a hitch above 2× a 60 Hz frame.
const HITCH_MS: f32 = 33.4;
/// EMA weight for the smoothed per-phase timings shown on the overlay.
const SMOOTH_ALPHA: f32 = 0.05;

/// CPU cost of one frame, split by phase. `total_ms` is filled by
/// [`Metrics::frame`] from the frame-to-frame delta; the rest are timed at
/// their call sites and handed in.
#[derive(Clone, Copy, Default)]
pub struct FrameTimings {
	/// Swapchain acquire (vsync) wait.
	pub acquire_ms: f32,
	pub update_ms: f32,
	/// `Pipeline::maintain`.
	pub maintain_ms: f32,
	/// Evict + mesh-upload loop.
	pub upload_ms: f32,
	/// Renderer encode + submit.
	pub encode_ms: f32,
	pub total_ms: f32,
}

impl FrameTimings {
	fn ema(&mut self, x: &FrameTimings) {
		let mix = |s: &mut f32, v: f32| *s = *s * (1.0 - SMOOTH_ALPHA) + v * SMOOTH_ALPHA;
		mix(&mut self.acquire_ms, x.acquire_ms);
		mix(&mut self.update_ms, x.update_ms);
		mix(&mut self.maintain_ms, x.maintain_ms);
		mix(&mut self.upload_ms, x.upload_ms);
		mix(&mut self.encode_ms, x.encode_ms);
		mix(&mut self.total_ms, x.total_ms);
	}

	fn add(&mut self, x: &FrameTimings) {
		self.acquire_ms += x.acquire_ms;
		self.update_ms += x.update_ms;
		self.maintain_ms += x.maintain_ms;
		self.upload_ms += x.upload_ms;
		self.encode_ms += x.encode_ms;
		self.total_ms += x.total_ms;
	}

	fn scaled(&self, k: f32) -> FrameTimings {
		FrameTimings {
			acquire_ms: self.acquire_ms * k,
			update_ms: self.update_ms * k,
			maintain_ms: self.maintain_ms * k,
			upload_ms: self.upload_ms * k,
			encode_ms: self.encode_ms * k,
			total_ms: self.total_ms * k,
		}
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

/// Instantaneous pipeline state (last-write gauges, not counters).
#[derive(Clone, Copy, Default)]
pub struct PipelineGauges {
	pub gen_queue: u32,
	pub mesh_queue: u32,
	pub in_flight: u32,
	/// Generated chunks resident in the pipeline store.
	pub resident: u32,
}

/// What the renderer did last frame.
#[derive(Clone, Copy, Default)]
pub struct RenderStats {
	/// Chunks with GPU meshes.
	pub chunks_resident: u32,
	pub chunks_drawn: u32,
	pub chunks_culled: u32,
	pub water_draws: u32,
	/// Near-cascade shadow casters.
	pub shadow_casters: u32,
	pub draw_calls: u32,
	pub triangles: u32,
	pub wide_refreshed: bool,
}

/// GPU pass durations from timestamp queries (2-3 frames latent).
#[derive(Clone, Copy, Default)]
pub struct GpuPassMs {
	pub shadow_ms: f32,
	pub main_ms: f32,
	/// The split-off water pass (blit + prepass + color); 0.0 until the
	/// renderer restructure lands.
	pub water_ms: f32,
	pub overlay_ms: f32,
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
	pub last: FrameTimings,
	pub smooth: FrameTimings,
	/// None until the first timestamp readback (or unsupported).
	pub gpu: Option<GpuPassMs>,
	/// Job aggregates for the report window so far.
	pub gen_jobs: JobTiming,
	pub mesh_jobs: JobTiming,
	pub generated: u32,
	pub uploaded: u32,
	pub stale_dropped: u32,
	pub requeued: u32,
	pub evicted: u32,
	pub upload_saturated: u32,
	pub gauges: PipelineGauges,
	pub render: RenderStats,
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
	/// Full-run frame history (bench mode only; ~7KB per 1800 frames) so
	/// the exit summary can compute run-level percentiles.
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
	pub stale_dropped: u32,
	pub requeued: u32,
	pub evicted: u32,
	pub upload_saturated: u32,
	// Gauges (last-write-wins).
	pub gauges: PipelineGauges,
	pub render: RenderStats,
	pub gpu: Option<GpuPassMs>,
	pub gpu_buffer_bytes: u64,
	pub workers: u32,
	/// Streaming budget in effect (last-write gauge; see the engine's
	/// adaptive controller).
	pub stream_budget_ms: f32,
	/// DIG_SPIKELOG=1: log a per-phase breakdown of every frame over the
	/// 120Hz budget — attributes pacing spikes without wading through CSVs.
	spike_log: bool,
	csv: Option<std::io::BufWriter<std::fs::File>>,
}

const CSV_HEADER: &str = "t_s,fps,avg_ms,p95_ms,p99_ms,worst_ms,hitches,\
acq_ms,upd_ms,mnt_ms,upl_ms,enc_ms,gpu_shadow_ms,gpu_main_ms,gpu_water_ms,gpu_overlay_ms,\
gen_n,gen_avg_ms,gen_max_ms,mesh_n,mesh_avg_ms,mesh_max_ms,\
uploads,stale,requeue,evict,sat,resident,gen_q,mesh_q,in_flight,\
drawn,culled,water,casters,draws,tris,gpu_bytes,budget_ms";

impl Metrics {
	pub fn new(csv_path: Option<&std::path::Path>) -> Self {
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
			stale_dropped: 0,
			requeued: 0,
			evicted: 0,
			upload_saturated: 0,
			gauges: PipelineGauges::default(),
			render: RenderStats::default(),
			gpu: None,
			gpu_buffer_bytes: 0,
			workers: 0,
			stream_budget_ms: 0.0,
			spike_log: std::env::var_os("DIG_SPIKELOG").is_some(),
			csv,
		}
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
			// Per-spike phase attribution (see `spike_log`).
			if self.spike_log && timings.total_ms > 8.4 {
				log::info!(
					"spike f{} total {:.2} | acq {:.2} upd {:.2} mnt {:.2} upl {:.2} enc {:.2} | gpu sh {:.2} main {:.2} water {:.2} | draws {} tris {}k wide {}",
					self.run_frames,
					timings.total_ms,
					timings.acquire_ms,
					timings.update_ms,
					timings.maintain_ms,
					timings.upload_ms,
					timings.encode_ms,
					self.gpu.as_ref().map_or(0.0, |g| g.shadow_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.main_ms),
					self.gpu.as_ref().map_or(0.0, |g| g.water_ms),
					self.render.draw_calls,
					self.render.triangles / 1000,
					self.render.wide_refreshed as u32,
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
			last: self.last,
			smooth: self.smooth,
			gpu: self.gpu,
			gen_jobs: self.gen_jobs,
			mesh_jobs: self.mesh_jobs,
			generated: self.chunks_generated,
			uploaded: self.meshes_uploaded,
			stale_dropped: self.stale_dropped,
			requeued: self.requeued,
			evicted: self.evicted,
			upload_saturated: self.upload_saturated,
			gauges: self.gauges,
			render: self.render,
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
		// Frame-pacing signals: over-8.3 = frames that wouldn't fit a 120Hz
		// vsync slot (budget-fit, for uncapped runs); over-12.5 = frames
		// 1.5× the slot — a visibly dropped frame when vsync-paced.
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
		let mut gen = self.run_gen_jobs;
		gen.merge(self.gen_jobs);
		let mut mesh = self.run_mesh_jobs;
		mesh.merge(self.mesh_jobs);
		let per_sec = |n: u32| n as f32 / elapsed.max(0.001);
		Some(format!(
			"bench: frames {frames} in {elapsed:.1}s ({:.1} fps) | fill {fill} | avg {:.2} p95 {:.2} p99 {:.2} worst {:.2} ms{warmup}{miss} | hitches {} | resident {} | gen {} jobs avg {:.2} max {:.2} ms ({:.0}/s) | mesh {} jobs avg {:.2} max {:.2} ms ({:.0}/s)",
			frames as f32 / elapsed.max(0.001),
			s.avg_ms, s.p95_ms, s.p99_ms, s.worst_ms,
			self.hitches_total,
			self.gauges.resident,
			gen.count, gen.avg_ms(), gen.max_ms, per_sec(gen.count),
			mesh.count, mesh.avg_ms(), mesh.max_ms, per_sec(mesh.count),
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
			"{true_fps:5.1} fps | avg {:5.2} p95 {:5.2} p99 {:5.2} worst {:6.2} ms | hitch {} | gen {} up {} | gen q {} mesh q {} fly {} | resident {} | gpu {:.1} MB",
			s.avg_ms, s.p95_ms, s.p99_ms, s.worst_ms, self.hitches,
			self.chunks_generated, self.meshes_uploaded,
			self.gauges.gen_queue, self.gauges.mesh_queue, self.gauges.in_flight,
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
				"{t:.1},{true_fps:.1},{:.2},{:.2},{:.2},{:.2},{},\
{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},\
{},{:.2},{:.2},{},{:.2},{:.2},\
{},{},{},{},{},{},{},{},{},\
{},{},{},{},{},{},{},{:.2}",
				s.avg_ms,
				s.p95_ms,
				s.p99_ms,
				s.worst_ms,
				self.hitches,
				win.acquire_ms,
				win.update_ms,
				win.maintain_ms,
				win.upload_ms,
				win.encode_ms,
				gpu.shadow_ms,
				gpu.main_ms,
				gpu.water_ms,
				gpu.overlay_ms,
				self.gen_jobs.count,
				self.gen_jobs.avg_ms(),
				self.gen_jobs.max_ms,
				self.mesh_jobs.count,
				self.mesh_jobs.avg_ms(),
				self.mesh_jobs.max_ms,
				self.meshes_uploaded,
				self.stale_dropped,
				self.requeued,
				self.evicted,
				self.upload_saturated,
				self.gauges.resident,
				self.gauges.gen_queue,
				self.gauges.mesh_queue,
				self.gauges.in_flight,
				r.chunks_drawn,
				r.chunks_culled,
				r.water_draws,
				r.shadow_casters,
				r.draw_calls,
				r.triangles,
				self.gpu_buffer_bytes,
				self.stream_budget_ms,
			);
			let _ = w.flush();
		}
		self.frames_since_report = 0;
		self.win_sum = FrameTimings::default();
		self.win_frames = 0;
		self.hitches = 0;
		self.run_gen_jobs.merge(self.gen_jobs);
		self.run_mesh_jobs.merge(self.mesh_jobs);
		self.gen_jobs = JobTiming::default();
		self.mesh_jobs = JobTiming::default();
		self.chunks_generated = 0;
		self.meshes_uploaded = 0;
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
		let mut m = Metrics::new(None);
		// First call only arms last_frame; every later call pushes one.
		for _ in 0..WINDOW + 10 {
			m.frame(FrameTimings::default());
		}
		assert_eq!(m.frame_ms.len(), WINDOW);
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
		let mut m = Metrics::new(None);
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
		let mut m = Metrics::new(None);
		let s = m.overlay_stats();
		assert_eq!(s.fps, 0.0);
		assert_eq!(s.gen_jobs.count, 0);
		assert!(s.gpu.is_none());
	}

	#[test]
	fn streamed_marks_only_once() {
		let mut m = Metrics::new(None);
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
		let mut m = Metrics::new(None);
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
		let mut m = Metrics::new(None);
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
