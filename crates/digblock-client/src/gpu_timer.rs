//! GPU pass timing via timestamp queries. Debug facility: readings lag by
//! the depth of the readback ring and are advisory (Metal counter sampling
//! has quirks); a frame is simply skipped when its readback slot is still
//! in flight.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use digblock_core::metrics::GpuPassMs;
use digblock_platform::Gpu;

/// Query pairs (begin, end) per pass.
///
/// Every GI pass gets its own bracket. The GI system shipped with only
/// `Q_GI` written and nothing harvesting it, and the resolve — the most
/// expensive pass in the frame — bracketed by nothing at all, so the
/// gap between measured GPU time and the actual frame was invisible.
/// A pass that is not in this list does not exist as far as tuning is
/// concerned.
pub(crate) const Q_PREPASS: u32 = 0;
pub(crate) const Q_MAIN: u32 = 2;
pub(crate) const Q_OVERLAY: u32 = 4;
/// Water pass (blit + prepass + water color), once the main pass splits.
pub(crate) const Q_WATER: u32 = 6;
/// RTGI probe-update compute (GI-DESIGN.md).
pub(crate) const Q_GI: u32 = 8;
/// The half-res GI trace (probe gather + sun ray).
pub(crate) const Q_GI_TRACE: u32 = 10;
/// À-trous iteration 1 (the stride-1 pass).
///
/// The two iterations need SEPARATE pairs. They used to share this one,
/// and `encode_atrous` writes it unconditionally, so the stride-2 pass
/// overwrote the stride-1 pass and the reported "denoise" number was one
/// iteration masquerading as both — the same class of blind spot the
/// header warns about, one level down from a pass.
pub(crate) const Q_GI_DENOISE: u32 = 12;
/// À-trous iteration 2 (the stride-2 pass).
pub(crate) const Q_GI_DENOISE2: u32 = 14;
/// The D32F → R32Float depth blit. A full-screen read+write of every
/// pixel that ran untimed since it was written.
pub(crate) const Q_DEPTH_BLIT: u32 = 16;
/// The viewmodel pass — its own render pass with its own depth clear.
pub(crate) const Q_VIEWMODEL: u32 = 18;
/// The upscale pass: render resolution → the surface. A straight 1:1
/// blit at the default render scale, so this is also the cost of the pass
/// boundary the restructure added.
pub(crate) const Q_UPSCALE: u32 = 20;
const QUERY_COUNT: u32 = 22;
const BUF_SIZE: u64 = QUERY_COUNT as u64 * 8;
/// Readback ring depth; deep enough to cover the buffer-map latency.
const SLOTS: usize = 3;

struct Slot {
	resolve: wgpu::Buffer,
	read: wgpu::Buffer,
	ready: Arc<AtomicBool>,
	pending: bool,
}

pub(crate) struct GpuTimer {
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
	pub(crate) fn new(gpu: &Gpu) -> Option<Self> {
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
	pub(crate) fn begin_frame(&mut self) {
		let slot = &mut self.slots[self.cursor % SLOTS];
		if slot.pending {
			if !slot.ready.load(Ordering::Relaxed) {
				self.armed = false; // still mapping — skip timing this frame
				return;
			}
			let data = slot
				.read
				.slice(..)
				.get_mapped_range()
				.expect("the readback reported ready");
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
				prepass_ms: pass(Q_PREPASS as usize),
				main_ms: pass(Q_MAIN as usize),
				water_ms: pass(Q_WATER as usize),
				overlay_ms: pass(Q_OVERLAY as usize),
				gi_ms: pass(Q_GI as usize),
				gi_trace_ms: pass(Q_GI_TRACE as usize),
				// BOTH iterations, which is what this column always claimed
				// to report and never did.
				gi_denoise_ms: pass(Q_GI_DENOISE as usize) + pass(Q_GI_DENOISE2 as usize),
				depth_blit_ms: pass(Q_DEPTH_BLIT as usize),
				viewmodel_ms: pass(Q_VIEWMODEL as usize),
				upscale_ms: pass(Q_UPSCALE as usize),
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
	pub(crate) fn writes(&self, first: u32) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
		self.armed.then_some(wgpu::RenderPassTimestampWrites {
			query_set: &self.query_set,
			beginning_of_pass_write_index: Some(first),
			end_of_pass_write_index: Some(first + 1),
		})
	}

	/// The compute-pass twin of [`writes`].
	pub(crate) fn compute_writes(
		&self,
		first: u32,
	) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
		self.armed.then_some(wgpu::ComputePassTimestampWrites {
			query_set: &self.query_set,
			beginning_of_pass_write_index: Some(first),
			end_of_pass_write_index: Some(first + 1),
		})
	}

	/// Encode resolve + copy. Call after the last pass, before submit.
	pub(crate) fn resolve(&mut self, encoder: &mut wgpu::CommandEncoder) {
		if !self.armed {
			return;
		}
		let slot = &self.slots[self.cursor % SLOTS];
		encoder.resolve_query_set(&self.query_set, 0..QUERY_COUNT, &slot.resolve, 0);
		encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.read, 0, BUF_SIZE);
	}

	/// Kick off the async readback. Call after queue.submit.
	pub(crate) fn after_submit(&mut self, gpu: &Gpu) {
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
