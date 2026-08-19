//! Transparent-water draw: the depth-prepass + color two-subpass pair
//! over the per-chunk water meshes. The pass structure around it (the
//! scene blit first, spray after) stays in the Renderer — this owns
//! only the two pipelines; the chunk list comes from the renderer's
//! one-per-frame cull, shared with the scene pass.

use digblock_platform::Gpu;

use crate::renderer::{ChunkGpu, draw_chunk};

pub(crate) struct WaterDraw {
	/// Depth-only prepass over the water geometry: writes the nearest
	/// water depth, color masked off. Stacked water/air interfaces
	/// (fall-sheet backsides, pool surfaces behind them) then fail the
	/// color pass's depth test, so a connected body of water reads as
	/// one surface instead of accumulated alpha layers.
	depth_pipeline: wgpu::RenderPipeline,
	/// Color pass: blended, double-sided (the dropped surface is
	/// visible from below), depth test on but depth WRITE off — fixes
	/// the Java artifact of water writing depth in the transparent
	/// pass.
	color_pipeline: wgpu::RenderPipeline,
}

impl WaterDraw {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		layout: &wgpu::PipelineLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("water"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::WATER.into()),
		});
		// Water vertices are two u32 words (see mesher MeshData); slot 1 is
		// the shared instance-step chunk-origin stream.
		let vertex_layouts = [
			crate::renderer::geometry_vertex_layout(),
			crate::renderer::origin_vertex_layout(),
		];
		let color_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "water",
				layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &vertex_layouts,
				cull: None,
				depth_write: false,
				// LessEqual against the prepass depth: only the nearest
				// water surface shades (Equal in effect, tolerant of
				// cross-pipeline depth invariance).
				depth_compare: wgpu::CompareFunction::LessEqual,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState::ALPHA_BLENDING),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let depth_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "water depth",
				layout,
				module: &shader,
				// Position-only vertex entry: the color pass's vs now runs
				// the probe gather per vertex (R17), and the prepass must
				// not pay that twice for geometry it only depth-tests.
				vs_entry: "vs_depth",
				fs_entry: Some("fs_depth"),
				vertex_buffers: &vertex_layouts,
				cull: None,
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::empty(),
			},
		);
		Self {
			depth_pipeline,
			color_pipeline,
		}
	}

	/// The two-subpass draw loop (prepass then color) over the renderer's
	/// culled water list (indices into `chunks`). The caller has bound
	/// groups 0-2, the index buffer, and the origin stream at vertex slot
	/// 1. Returns (water draws, triangles).
	pub(crate) fn draw(
		&self,
		pass: &mut wgpu::RenderPass<'_>,
		chunks: &[ChunkGpu],
		list: &[u32],
	) -> (u32, u32) {
		let (mut draws, mut tris) = (0u32, 0u32);
		for pipeline in [&self.depth_pipeline, &self.color_pipeline] {
			pass.set_pipeline(pipeline);
			for &i in list {
				let chunk = &chunks[i as usize];
				let Some(mesh) = chunk.water.as_ref() else {
					continue;
				};
				draws += 1;
				tris += draw_chunk(pass, mesh, chunk.slot) / 3;
			}
		}
		(draws, tris)
	}
}
