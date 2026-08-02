//! Transparent-water draw: the depth-prepass + color two-subpass pair
//! over the per-chunk water meshes. The pass structure around it (the
//! scene blit first, spray after) stays in the Renderer — this owns
//! only the two pipelines and the reused chunk list.

use digblock_core::chunk::ChunkPos;
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

use crate::renderer::{ChunkGpu, CHUNK_STRIDE};

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
	/// Reused water chunk list (was a fresh alloc every frame). The
	/// depth prepass makes draw order irrelevant, so no sort key.
	scratch: Vec<ChunkPos>,
}

impl WaterDraw {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		layout: &wgpu::PipelineLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("water"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::WATER.into()),
		});
		// Water vertices are two u32 words (see mesher MeshData).
		let vertex_layout = wgpu::VertexBufferLayout {
			array_stride: 8,
			step_mode: wgpu::VertexStepMode::Vertex,
			attributes: &[wgpu::VertexAttribute {
				format: wgpu::VertexFormat::Uint32x2,
				offset: 0,
				shader_location: 0,
			}],
		};
		let color_pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "water",
				layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: std::slice::from_ref(&vertex_layout),
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
				vs_entry: "vs",
				fs_entry: Some("fs_depth"),
				vertex_buffers: std::slice::from_ref(&vertex_layout),
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
			scratch: Vec::new(),
		}
	}

	/// Rebuild the water chunk list, shared by both subpasses of
	/// `draw`. The depth prepass makes draw order irrelevant: no
	/// sorting.
	pub fn build_list(
		&mut self,
		chunks: &FxHashMap<ChunkPos, ChunkGpu>,
		in_frustum: impl Fn(&ChunkPos) -> bool,
	) {
		self.scratch.clear();
		self.scratch.extend(
			chunks
				.iter()
				.filter(|(pos, c)| c.water.is_some() && in_frustum(pos))
				.map(|(pos, _)| *pos),
		);
	}

	/// The two-subpass draw loop (prepass then color) over the built
	/// list. The caller has bound groups 0-2 and the index buffer.
	/// Returns (water draws, triangles).
	pub fn draw(
		&self,
		pass: &mut wgpu::RenderPass<'_>,
		chunks: &FxHashMap<ChunkPos, ChunkGpu>,
		chunk_bg: &wgpu::BindGroup,
	) -> (u32, u32) {
		let (mut draws, mut tris) = (0u32, 0u32);
		for pipeline in [&self.depth_pipeline, &self.color_pipeline] {
			pass.set_pipeline(pipeline);
			for pos in &self.scratch {
				let Some(chunk) = chunks.get(pos) else {
					continue;
				};
				if let Some((buf, index_count)) = &chunk.water {
					pass.set_bind_group(
						3,
						chunk_bg,
						&[(u64::from(chunk.slot) * CHUNK_STRIDE) as u32],
					);
					pass.set_vertex_buffer(0, buf.slice(..));
					pass.draw_indexed(0..*index_count, 0, 0..1);
					draws += 1;
					tris += *index_count / 3;
				}
			}
		}
		(draws, tris)
	}
}
