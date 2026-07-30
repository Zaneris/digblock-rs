//! Frame-time and pipeline metrics. Logged as one-line summaries and
//! optionally dumped to CSV so milestone-over-milestone progression is
//! graphable.

use std::time::{Duration, Instant};

const WINDOW: usize = 600; // ~10s at 60fps

pub struct Metrics {
	frame_ms: Vec<f32>,
	last_frame: Option<Instant>,
	last_report: Instant,
	report_every: Duration,
	// Pipeline counters (reset each report window). Unused until M5.
	pub chunks_generated: u32,
	pub chunks_meshed: u32,
	pub meshes_uploaded: u32,
	pub resident_chunks: u32,
	pub queue_depth: u32,
	pub gpu_buffer_bytes: u64,
	csv: Option<std::io::BufWriter<std::fs::File>>,
}

pub struct Summary {
	pub fps: f32,
	pub avg_ms: f32,
	pub p95_ms: f32,
	pub p99_ms: f32,
	pub worst_ms: f32,
}

impl Metrics {
	pub fn new(csv_path: Option<&std::path::Path>) -> Self {
		let csv = csv_path.and_then(|p| {
			let mut w = std::io::BufWriter::new(std::fs::File::create(p).ok()?);
			use std::io::Write;
			writeln!(
				w,
				"t_s,fps,avg_ms,p95_ms,p99_ms,worst_ms,chunks_gen,chunks_meshed,uploads,resident,queue,gpu_bytes"
			)
			.ok()?;
			Some(w)
		});
		Self {
			frame_ms: Vec::with_capacity(WINDOW),
			last_frame: None,
			last_report: Instant::now(),
			report_every: Duration::from_secs(5),
			chunks_generated: 0,
			chunks_meshed: 0,
			meshes_uploaded: 0,
			resident_chunks: 0,
			queue_depth: 0,
			gpu_buffer_bytes: 0,
			csv: None.or(csv),
		}
	}

	/// Call once per presented frame. Suspend/resume safe: call `pause()`
	/// when rendering stops so the gap isn't counted as a frame.
	pub fn frame(&mut self) {
		let now = Instant::now();
		if let Some(last) = self.last_frame {
			let ms = (now - last).as_secs_f32() * 1000.0;
			if self.frame_ms.len() == WINDOW {
				self.frame_ms.remove(0);
			}
			self.frame_ms.push(ms);
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

	pub fn summary(&self) -> Option<Summary> {
		if self.frame_ms.is_empty() {
			return None;
		}
		let mut sorted = self.frame_ms.clone();
		sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
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

	fn report(&mut self) {
		let Some(s) = self.summary() else { return };
		log::info!(
			"{:5.1} fps | avg {:5.2} p95 {:5.2} p99 {:5.2} worst {:6.2} ms | gen {} mesh {} up {} | resident {} queue {} | gpu {:.1} MB",
			s.fps, s.avg_ms, s.p95_ms, s.p99_ms, s.worst_ms,
			self.chunks_generated, self.chunks_meshed, self.meshes_uploaded,
			self.resident_chunks, self.queue_depth,
			self.gpu_buffer_bytes as f64 / (1024.0 * 1024.0),
		);
		if let Some(w) = &mut self.csv {
			use std::io::Write;
			let t = self.last_report.elapsed().as_secs_f32();
			let _ = writeln!(
				w,
				"{t:.1},{:.1},{:.2},{:.2},{:.2},{:.2},{},{},{},{},{},{}",
				s.fps,
				s.avg_ms,
				s.p95_ms,
				s.p99_ms,
				s.worst_ms,
				self.chunks_generated,
				self.chunks_meshed,
				self.meshes_uploaded,
				self.resident_chunks,
				self.queue_depth,
				self.gpu_buffer_bytes,
			);
			let _ = w.flush();
		}
		self.chunks_generated = 0;
		self.chunks_meshed = 0;
		self.meshes_uploaded = 0;
	}
}
