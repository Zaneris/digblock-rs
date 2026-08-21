//! The standing spiral over an arrival pad — spawn today, the waystones
//! when they land. Persistent, stateless light (waystone.wgsl): the CPU
//! owns one instance per site and ages nothing; the whole motion derives
//! from the site, its phase and the shared animation clock.
//!
//! Sites are ENGINE state set at world entry, culled by distance here on
//! the CPU (spray's radius discipline — most frames most sites are far),
//! and drawn additively in the water pass beside the other light.

use digblock_platform::Gpu;
use glam::Vec3;

/// Sites a world can mark. Spawn plus the whole waystone roster fits
/// with room; the buffer this sizes is negligible.
pub(crate) const MAX_MARKERS: usize = 16;
/// Motes per marker — keep in sync with waystone.wgsl `MOTES`, held by
/// [`the_marker_mote_count_matches_the_shader`]. Particle 0 is the
/// ground ring, so the draw call is one quad longer.
const MOTES: u32 = 24;
/// Beyond this many blocks a marker is not uploaded at all — an additive
/// speck at range costs a draw and shows nothing.
pub(crate) const MARKER_RADIUS: f32 = 96.0;
/// Bytes per instance: site + phase, radius + height + two spares.
const INSTANCE_BYTES: usize = 32;

/// One site, in the flat `[site.xyz, phase][radius, height, 0, 0]`
/// layout the vertex shader reads.
pub(crate) type MarkerInstance = [f32; 8];

/// Pack one site. `site` is the pad's floor centre at ground level;
/// `phase` staggers the beat so neighbouring markers don't pulse in step.
pub(crate) fn instance(site: [f32; 3], phase: f32, radius: f32, height: f32) -> MarkerInstance {
	[site[0], site[1], site[2], phase, radius, height, 0.0, 0.0]
}

/// The one radius and height every arrival pad's spiral stands at. One
/// pair, not per-site lanes read from anywhere else: a marker is an
/// identity, and two sites differing in silhouette would read as two
/// different things.
pub(crate) const SPIRAL_RADIUS: f32 = 1.4;
pub(crate) const SPIRAL_HEIGHT: f32 = 3.2;

/// Cull and pack the live set for this frame.
pub(crate) fn visible(sites: &[Vec3], cam: Vec3, out: &mut Vec<MarkerInstance>) {
	out.clear();
	for (i, s) in sites.iter().enumerate() {
		if s.distance(cam) <= MARKER_RADIUS && out.len() < MAX_MARKERS {
			out.push(instance(
				s.to_array(),
				i as f32 * 0.37,
				SPIRAL_RADIUS,
				SPIRAL_HEIGHT,
			));
		}
	}
}

pub(crate) struct WaystoneFx {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
}

impl WaystoneFx {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("waystone"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::WAYSTONE.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("waystone"),
			bind_group_layouts: &[Some(frame_bgl)],
			immediate_size: 0,
		});
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "waystone",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: INSTANCE_BYTES as u64,
					step_mode: wgpu::VertexStepMode::Instance,
					attributes: &[
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x4,
							offset: 0,
							shader_location: 0,
						},
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x4,
							offset: 16,
							shader_location: 1,
						},
					],
				}],
				cull: None,
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: Some(wgpu::BlendState {
					color: wgpu::BlendComponent {
						src_factor: wgpu::BlendFactor::SrcAlpha,
						dst_factor: wgpu::BlendFactor::One,
						operation: wgpu::BlendOperation::Add,
					},
					alpha: wgpu::BlendComponent {
						src_factor: wgpu::BlendFactor::Zero,
						dst_factor: wgpu::BlendFactor::One,
						operation: wgpu::BlendOperation::Add,
					},
				}),
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("waystone instances"),
			size: (MAX_MARKERS * INSTANCE_BYTES) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
		}
	}

	/// Upload this frame's visible markers. Returns the instance count.
	pub(crate) fn prepare(&self, gpu: &Gpu, markers: &[MarkerInstance]) -> u32 {
		let n = markers.len().min(MAX_MARKERS);
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&markers[..n]));
		}
		n as u32
	}

	/// Draw in the water pass beside the other additive light. Returns
	/// triangles.
	pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instances: u32) -> u32 {
		if instances == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_vertex_buffer(0, self.instance_buf.slice(..));
		pass.draw(0..6 * (MOTES + 1), 0..instances);
		instances * (MOTES + 1) * 2
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Siphon's pin: the count that sizes the draw call lives on both
	/// sides of the CPU/GPU boundary.
	#[test]
	fn the_marker_mote_count_matches_the_shader() {
		let decl = format!("const MOTES: u32 = {MOTES}u;");
		assert!(
			digblock_assets::shader::WAYSTONE.contains(&decl),
			"waystone.wgsl must declare `{decl}`"
		);
	}

	/// The cull is a cull, not a truncation with a radius attached: near
	/// sites make it in, far ones don't, and the count is exact.
	#[test]
	fn visibility_culls_by_distance() {
		let cam = Vec3::ZERO;
		let sites = vec![
			Vec3::new(10.0, 0.0, 0.0),
			Vec3::new(MARKER_RADIUS + 50.0, 0.0, 0.0),
			Vec3::new(0.0, 0.0, -20.0),
		];
		let mut out = Vec::new();
		visible(&sites, cam, &mut out);
		assert_eq!(out.len(), 2);
		assert!(
			out.iter()
				.all(|m| { Vec3::new(m[0], m[1], m[2]).distance(cam) <= MARKER_RADIUS })
		);
	}
}
