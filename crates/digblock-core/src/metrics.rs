//! Frame-time, phase, and pipeline metrics. Logged as one-line summaries,
//! optionally dumped to CSV, and snapshotted per-frame for the debug
//! overlay. GPU-free: GPU-derived numbers are fed in as plain data.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
	csv: Option<std::io::BufWriter<std::fs::File>>,
}

const CSV_HEADER: &str = "t_s,fps,avg_ms,p95_ms,p99_ms,worst_ms,hitches,\
acq_ms,upd_ms,mnt_ms,upl_ms,enc_ms,gpu_shadow_ms,gpu_main_ms,gpu_overlay_ms,\
gen_n,gen_avg_ms,gen_max_ms,mesh_n,mesh_avg_ms,mesh_max_ms,\
uploads,stale,requeue,evict,sat,resident,gen_q,mesh_q,in_flight,\
drawn,culled,water,casters,draws,tris,gpu_bytes";

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
			csv,
		}
	}

	/// Call once per presented frame with that frame's phase timings.
	/// `total_ms` is overwritten with the frame-to-frame delta.
	/// Suspend/resume safe: call `pause()` when rendering stops so the gap
	/// isn't counted as a frame.
	pub fn frame(&mut self, mut timings: FrameTimings) {
		let now = Instant::now();
		if let Some(last) = self.last_frame {
			timings.total_ms = (now - last).as_secs_f32() * 1000.0;
			if self.frame_ms.len() == WINDOW {
				self.frame_ms.pop_front();
			}
			self.frame_ms.push_back(timings.total_ms);
			if timings.total_ms > HITCH_MS {
				self.hitches += 1;
				self.hitches_total += 1;
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
		if self.frame_ms.is_empty() {
			return None;
		}
		self.sort_scratch.clear();
		self.sort_scratch.extend(self.frame_ms.iter().copied());
		self.sort_scratch.sort_by(|a, b| a.partial_cmp(b).unwrap());
		let sorted = &self.sort_scratch;
		let avg = sorted.iter().sum::<f32>() / sorted.len() as f32;
		let pct = |p: f32| sorted[((sorted.len() as f32 * p) as usize).min(sorted.len() - 1)];
		Some(Summary {
			fps: 1000.0 / avg,
			avg_ms: avg,
			p95_ms: pct(0.95),
			p99_ms: pct(0.99),
			worst_ms: *sorted.last().unwrap(),
		})
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
		}
	}

	/// One-line, machine-diffable run summary for benchmark exits.
	/// Percentiles cover the last WINDOW frames; fps/jobs cover the run.
	pub fn bench_summary(&mut self) -> Option<String> {
		let elapsed = self.started_at.elapsed().as_secs_f32();
		let frames = self.run_frames;
		let s = self.summary()?;
		let mut gen = self.run_gen_jobs;
		gen.merge(self.gen_jobs);
		let mut mesh = self.run_mesh_jobs;
		mesh.merge(self.mesh_jobs);
		Some(format!(
			"bench: frames {frames} in {elapsed:.1}s ({:.1} fps) | avg {:.2} p95 {:.2} p99 {:.2} worst {:.2} ms | hitches {} | gen {} jobs avg {:.2} max {:.2} ms | mesh {} jobs avg {:.2} max {:.2} ms",
			frames as f32 / elapsed.max(0.001),
			s.avg_ms, s.p95_ms, s.p99_ms, s.worst_ms,
			self.hitches_total,
			gen.count, gen.avg_ms(), gen.max_ms,
			mesh.count, mesh.avg_ms(), mesh.max_ms,
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
{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},\
{},{:.2},{:.2},{},{:.2},{:.2},\
{},{},{},{},{},{},{},{},{},\
{},{},{},{},{},{},{}",
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
