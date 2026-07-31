//! GPU pass timing via timestamp queries. Debug facility: readings are
//! 2-3 frames latent and advisory (Metal counter sampling has quirks);
//! a frame is simply skipped when its readback slot is still in flight.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use digblock_core::metrics::GpuPassMs;
use digblock_platform::Gpu;

/// Query pairs (begin, end) per pass.
pub const Q_SHADOW_WIDE: u32 = 0;
pub const Q_SHADOW_NEAR: u32 = 2;
pub const Q_MAIN: u32 = 4;
pub const Q_OVERLAY: u32 = 6;
const QUERY_COUNT: u32 = 8;
const BUF_SIZE: u64 = QUERY_COUNT as u64 * 8;
/// Readback ring depth; covers the 2-3 frame map latency.
const SLOTS: usize = 3;

struct Slot {
	resolve: wgpu::Buffer,
	read: wgpu::Buffer,
	ready: Arc<AtomicBool>,
	pending: bool,
}

pub struct GpuTimer {
	query_set: wgpu::QuerySet,
	slots: Vec<Slot>,
	cursor: usize,
	period_ns: f32,
	/// Most recent complete reading.
	pub latest: Option<GpuPassMs>,
	/// Whether this frame's passes should write timestamps (false while
	/// the current slot is still mapping).
	armed: bool,
}

impl GpuTimer {
	/// None if the device lacks TIMESTAMP_QUERY.
	pub fn new(gpu: &Gpu) -> Option<Self> {
		if !gpu
			.device
			.features()
			.contains(wgpu::Features::TIMESTAMP_QUERY)
		{
			return None;
		}
		let query_set = gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
			label: Some("pass timestamps"),
			ty: wgpu::QueryType::Timestamp,
			count: QUERY_COUNT,
		});
		let slots = (0..SLOTS)
			.map(|i| Slot {
				resolve: gpu.device.create_buffer(&wgpu::BufferDescriptor {
					label: Some("ts resolve"),
					size: BUF_SIZE,
					usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
					mapped_at_creation: false,
				}),
				read: gpu.device.create_buffer(&wgpu::BufferDescriptor {
					label: Some(&format!("ts read {i}")),
					size: BUF_SIZE,
					usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
					mapped_at_creation: false,
				}),
				ready: Arc::new(AtomicBool::new(false)),
				pending: false,
			})
			.collect();
		Some(Self {
			query_set,
			slots,
			cursor: 0,
			period_ns: gpu.queue.get_timestamp_period(),
			latest: None,
			armed: false,
		})
	}

	/// Call at frame start: harvests the current slot if its readback
	/// landed, and decides whether this frame writes timestamps.
	pub fn begin_frame(&mut self) {
		let slot = &mut self.slots[self.cursor % SLOTS];
		if slot.pending {
			if !slot.ready.load(Ordering::Relaxed) {
				self.armed = false; // still mapping — skip timing this frame
				return;
			}
			let data = slot.read.slice(..).get_mapped_range();
			let ticks: &[u64] = bytemuck::cast_slice(&data);
			let pass = |i: usize| -> f32 {
				let (b, e) = (ticks[i], ticks[i + 1]);
				// Unwritten queries resolve to 0; reject nonsense.
				if b == 0 || e <= b {
					0.0
				} else {
					(e - b) as f32 * self.period_ns / 1.0e6
				}
			};
			let reading = GpuPassMs {
				shadow_ms: pass(Q_SHADOW_WIDE as usize) + pass(Q_SHADOW_NEAR as usize),
				main_ms: pass(Q_MAIN as usize),
				overlay_ms: pass(Q_OVERLAY as usize),
			};
			drop(data);
			slot.read.unmap();
			slot.ready.store(false, Ordering::Relaxed);
			slot.pending = false;
			if reading.main_ms > 0.0 {
				self.latest = Some(reading);
			}
		}
		self.armed = true;
	}

	/// Pass descriptor timestamp writes for the pass starting at `first`,
	/// or None when timing is skipped this frame.
	pub fn writes(&self, first: u32) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
		self.armed.then_some(wgpu::RenderPassTimestampWrites {
			query_set: &self.query_set,
			beginning_of_pass_write_index: Some(first),
			end_of_pass_write_index: Some(first + 1),
		})
	}

	/// Encode resolve + copy. Call after the last pass, before submit.
	pub fn resolve(&mut self, encoder: &mut wgpu::CommandEncoder) {
		if !self.armed {
			return;
		}
		let slot = &self.slots[self.cursor % SLOTS];
		encoder.resolve_query_set(&self.query_set, 0..QUERY_COUNT, &slot.resolve, 0);
		encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.read, 0, BUF_SIZE);
	}

	/// Kick off the async readback. Call after queue.submit.
	pub fn after_submit(&mut self, gpu: &Gpu) {
		if self.armed {
			let slot = &mut self.slots[self.cursor % SLOTS];
			let ready = Arc::clone(&slot.ready);
			slot.read
				.slice(..)
				.map_async(wgpu::MapMode::Read, move |r| {
					if r.is_ok() {
						ready.store(true, Ordering::Relaxed);
					}
				});
			slot.pending = true;
			self.cursor += 1;
		}
		// Non-blocking: keeps map_async callbacks draining every frame.
		let _ = gpu.device.poll(wgpu::PollType::Poll);
	}
}
