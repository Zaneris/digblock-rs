//! One funnel for every render-pipeline descriptor in the crate. The
//! engine's pipelines differ only along a handful of axes (entry
//! points, cull, depth mode, blend, write mask); everything else —
//! depth format, triangle topology, no multisampling, no multiview —
//! is engine-wide policy and lives here so a descriptor site can't
//! drift on an axis it never meant to touch.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::renderer::DEPTH_FORMAT;

pub(crate) struct PipelineDesc<'a> {
	pub label: &'a str,
	pub layout: &'a wgpu::PipelineLayout,
	pub module: &'a wgpu::ShaderModule,
	pub vs_entry: &'a str,
	/// None = depth-only (no fragment stage at all).
	pub fs_entry: Option<&'a str>,
	pub vertex_buffers: &'a [wgpu::VertexBufferLayout<'a>],
	pub cull: Option<wgpu::Face>,
	pub depth_write: bool,
	pub depth_compare: wgpu::CompareFunction,
	pub depth_bias: wgpu::DepthBiasState,
	pub blend: Option<wgpu::BlendState>,
	pub write_mask: wgpu::ColorWrites,
}

/// Build a pipeline; `color_format` None means no color target (the
/// depth-only shadow pass). A `fs_entry` with `color_format` Some and
/// an empty `write_mask` is the depth-prepass shape (rasterize, write
/// nothing).
pub(crate) fn build(
	device: &wgpu::Device,
	color_format: Option<wgpu::TextureFormat>,
	desc: &PipelineDesc<'_>,
) -> wgpu::RenderPipeline {
	build_inner(device, color_format, desc, true)
}

/// A pipeline whose creation was watched by error scopes, so a failure
/// costs the caller's draw instead of the frame: an invalid pipeline
/// poisons the whole encoder that binds it, and the frame's one `finish`
/// then drops EVERY pass — a black window (targets.rs). A draw the frame
/// can live without builds through [`build_guarded`] and binds only what
/// [`GuardedPipeline::pipeline`] hands back.
pub(crate) struct GuardedPipeline {
	label: String,
	pipeline: wgpu::RenderPipeline,
	verdict: Verdict,
}

enum Verdict {
	// The scope's answer is a promise on WebGPU, so the verdict starts
	// unresolved and the pipeline is withheld until it lands — at most
	// the few boot frames the browser takes to answer.
	Pending(Pin<Box<dyn Future<Output = Option<wgpu::Error>>>>),
	Valid,
	// The message is for `build_failed` — the test build is its only
	// reader; the running engine already logged it at the transition.
	#[cfg_attr(not(test), expect(dead_code))]
	Invalid(String),
}

/// [`build`] under error scopes. Validation AND Internal: a descriptor
/// bug is the former, but a driver refusing to compile the module — the
/// failure only the affected device can produce — reaches WebGPU as the
/// latter.
pub(crate) fn build_guarded(
	device: &wgpu::Device,
	color_format: Option<wgpu::TextureFormat>,
	desc: &PipelineDesc<'_>,
) -> GuardedPipeline {
	let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
	let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
	let pipeline = build(device, color_format, desc);
	// Popped eagerly, in reverse push order (native enforces LIFO); only
	// the ANSWERS are deferred, so later builds can push scopes of their
	// own.
	let v = validation.pop();
	let i = internal.pop();
	GuardedPipeline {
		label: desc.label.to_owned(),
		pipeline,
		verdict: Verdict::Pending(Box::pin(async move {
			match v.await {
				Some(e) => Some(e),
				None => i.await,
			}
		})),
	}
}

impl GuardedPipeline {
	/// Drive the verdict. Cheap once resolved; call from a per-frame
	/// `&mut` site — nothing else wakes the future.
	pub(crate) fn poll(&mut self) {
		let Verdict::Pending(fut) = &mut self.verdict else {
			return;
		};
		let mut cx = Context::from_waker(Waker::noop());
		if let Poll::Ready(err) = fut.as_mut().poll(&mut cx) {
			self.verdict = match err {
				None => Verdict::Valid,
				Some(e) => {
					let what = format!("pipeline '{}' failed to build: {e}", self.label);
					log::error!("{what} — its draw is disabled");
					Verdict::Invalid(what)
				}
			};
		}
	}

	/// The pipeline, once the scopes have cleared it.
	pub(crate) fn pipeline(&self) -> Option<&wgpu::RenderPipeline> {
		match self.verdict {
			Verdict::Valid => Some(&self.pipeline),
			_ => None,
		}
	}

	/// Test hook: the guard swallows exactly the errors the suite's
	/// outer scope would otherwise report, so the pipeline test must ask
	/// each guard directly.
	#[cfg(test)]
	pub(crate) fn build_failed(&mut self) -> Option<String> {
		self.poll();
		match &self.verdict {
			Verdict::Invalid(what) => Some(what.clone()),
			_ => None,
		}
	}
}

/// The one pass shape with no depth attachment at all: the depth-resolve
/// blit, whose color target IS the depth copy. `desc.depth_*` are
/// ignored.
pub(crate) fn build_depthless(
	device: &wgpu::Device,
	color_format: wgpu::TextureFormat,
	desc: &PipelineDesc<'_>,
) -> wgpu::RenderPipeline {
	build_inner(device, Some(color_format), desc, false)
}

fn build_inner(
	device: &wgpu::Device,
	color_format: Option<wgpu::TextureFormat>,
	desc: &PipelineDesc<'_>,
	depth: bool,
) -> wgpu::RenderPipeline {
	let targets = [color_format.map(|format| wgpu::ColorTargetState {
		format,
		blend: desc.blend,
		write_mask: desc.write_mask,
	})];
	// wgpu can leave a vertex slot unbound; this engine never wants to,
	// so filling every slot a descriptor names is one more engine-wide
	// policy the funnel owns and no call site spells.
	let buffers: Vec<_> = desc.vertex_buffers.iter().cloned().map(Some).collect();
	device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
		label: Some(desc.label),
		layout: Some(desc.layout),
		vertex: wgpu::VertexState {
			module: desc.module,
			entry_point: Some(desc.vs_entry),
			compilation_options: Default::default(),
			buffers: &buffers,
		},
		primitive: wgpu::PrimitiveState {
			topology: wgpu::PrimitiveTopology::TriangleList,
			cull_mode: desc.cull,
			..Default::default()
		},
		depth_stencil: depth.then(|| wgpu::DepthStencilState {
			format: DEPTH_FORMAT,
			// Both are `Some` unconditionally: wgpu wants them present
			// whenever the attachment has a depth aspect, and this funnel
			// only builds a depth state when there IS one.
			depth_write_enabled: Some(desc.depth_write),
			depth_compare: Some(desc.depth_compare),
			stencil: Default::default(),
			bias: desc.depth_bias,
		}),
		multisample: Default::default(),
		fragment: desc.fs_entry.map(|entry| wgpu::FragmentState {
			module: desc.module,
			entry_point: Some(entry),
			compilation_options: Default::default(),
			targets: if color_format.is_some() {
				&targets
			} else {
				&[]
			},
		}),
		multiview_mask: None,
		cache: None,
	})
}
