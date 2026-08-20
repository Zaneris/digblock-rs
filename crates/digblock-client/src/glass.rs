//! Glass draw: the per-chunk translucent meshes (the Translucent mesh
//! bucket), inside the water pass right after the water draw — the depth
//! attachment there already holds solids AND the nearest water surface,
//! so glass depth-tests against both. Two commuting halves per chunk: a
//! MULTIPLY pass tints the backdrop by each texel's transmittance (what
//! makes a pane over bright sky glow with what it passes), then an
//! ADDITIVE pass lays the surface's own lit/emissive term on top. Both
//! commute across faces and panes, so no draw order matters — no sort,
//! no winding split, and a cube tinting through two faces is thickness.

use digblock_platform::Gpu;

use crate::renderer::{ChunkGpu, draw_chunk};

pub(crate) struct GlassDraw {
	/// dst x src — the transmittance multiply. Depth test on, depth
	/// WRITE off in both halves: a pane must not shadow the spray and
	/// decals drawn after it out of the depth buffer, and glass behind
	/// glass has to blend, not lose the depth fight.
	tint_pipeline: wgpu::RenderPipeline,
	/// dst + src — the surface's own light (and crystal's ink).
	shine_pipeline: wgpu::RenderPipeline,
}

impl GlassDraw {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		layout: &wgpu::PipelineLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("glass"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GLASS.into()),
		});
		// The solid stream's two-word vertices; slot 1 is the shared
		// instance-step chunk-origin stream.
		let vertex_layouts = [
			crate::renderer::geometry_vertex_layout(),
			crate::renderer::origin_vertex_layout(),
		];
		let half = |label, vs_entry, fs_entry, blend: wgpu::BlendComponent| {
			crate::pipeline_builder::build(
				device,
				Some(surface_format),
				&crate::pipeline_builder::PipelineDesc {
					label,
					layout,
					module: &shader,
					vs_entry,
					fs_entry: Some(fs_entry),
					vertex_buffers: &vertex_layouts,
					cull: None,
					depth_write: false,
					// LESS-OR-EQUAL, not Less: water emits its interface
					// face toward glass, so an aquarium wall and every
					// submerged pane are exactly COPLANAR with a water
					// quad the water prepass already wrote. Under `Less`
					// the pane lost that tie to its own backdrop and
					// vanished. Glass writes no depth and the mesher culls
					// it against every opaque neighbour, so no other
					// coplanar case can slip through on the tie.
					depth_compare: wgpu::CompareFunction::LessEqual,
					depth_bias: Default::default(),
					blend: Some(wgpu::BlendState {
						color: blend,
						alpha: wgpu::BlendComponent {
							src_factor: wgpu::BlendFactor::Zero,
							dst_factor: wgpu::BlendFactor::One,
							operation: wgpu::BlendOperation::Add,
						},
					}),
					write_mask: wgpu::ColorWrites::ALL,
				},
			)
		};
		Self {
			// result = dst x src: the fragment answers "what passes".
			tint_pipeline: half(
				"glass tint",
				"vs_tint",
				"fs_tint",
				wgpu::BlendComponent {
					src_factor: wgpu::BlendFactor::Zero,
					dst_factor: wgpu::BlendFactor::Src,
					operation: wgpu::BlendOperation::Add,
				},
			),
			// result = dst + src: the surface's own light lands on top.
			shine_pipeline: half(
				"glass shine",
				"vs",
				"fs",
				wgpu::BlendComponent {
					src_factor: wgpu::BlendFactor::One,
					dst_factor: wgpu::BlendFactor::One,
					operation: wgpu::BlendOperation::Add,
				},
			),
		}
	}

	/// Draw the culled translucent list (indices into `chunks`) BACK TO
	/// FRONT, each chunk's tint half immediately followed by its shine.
	///
	/// The two halves do NOT commute across panes: a pane's real operator
	/// is `dst*T + S`, and `f_A(f_B(x))` differs from `f_B(f_A(x))` unless
	/// the shines happen to balance. Running all tints and then all shines
	/// is order-independent, but only because every shine then escapes
	/// every filter — stack panes and the transmission falls to nothing
	/// while the additive term grows without bound, so a deep glass
	/// corridor gets BRIGHTER. Interleaving per chunk in depth order makes
	/// the composite exact whenever the panes sit in different chunks; two
	/// panes inside ONE chunk still share a tint and are the standing
	/// approximation. The cost is two pipeline binds per translucent
	/// chunk, and glass is sparse.
	///
	/// The caller has bound groups 0-2, the index buffer, and the origin
	/// stream at vertex slot 1. Returns (draws, triangles).
	pub(crate) fn draw(
		&self,
		pass: &mut wgpu::RenderPass<'_>,
		chunks: &[ChunkGpu],
		list: &[u32],
	) -> (u32, u32) {
		let (mut draws, mut tris) = (0u32, 0u32);
		// `list` is coarsely front-to-back (the shared distance scatter),
		// so walking it in reverse is back-to-front.
		for &i in list.iter().rev() {
			let chunk = &chunks[i as usize];
			let Some(mesh) = chunk.translucent.as_ref() else {
				continue;
			};
			for pipeline in [&self.tint_pipeline, &self.shine_pipeline] {
				pass.set_pipeline(pipeline);
				draws += 1;
				tris += draw_chunk(pass, mesh, chunk.slot) / 3;
			}
		}
		(draws, tris)
	}
}
