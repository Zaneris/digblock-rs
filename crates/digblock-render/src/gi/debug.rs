//! The probe visualization `--gi-debug` draws. Nothing here runs in a
//! normal frame.

use super::*;

impl GiDraw {
	/// Draw the debug raymarch over the whole target (`DIG_GI=`).
	pub(crate) fn encode_debug(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		target: &wgpu::TextureView,
		inv_view_proj: [[f32; 4]; 4],
		cam_pos: [f32; 3],
	) {
		let side = (self.window_chunks * 16) as f32;
		let mut data = Vec::with_capacity(96);
		for col in inv_view_proj {
			data.extend_from_slice(bytemuck::cast_slice(&col));
		}
		data.extend_from_slice(bytemuck::cast_slice(&[
			cam_pos[0], cam_pos[1], cam_pos[2], side,
		]));
		// misc.x: nonzero shades hits with the C0 probe field
		// (DIG_GI=probes).
		let mode = if self.debug_probes { 1.0f32 } else { 0.0 };
		data.extend_from_slice(bytemuck::cast_slice(&[
			mode,
			C0_DIMS.0 as f32,
			C0_DIMS.1 as f32,
			C0_DIMS.2 as f32,
		]));
		gpu.queue.write_buffer(&self.debug_buf, 0, &data);
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("gi debug"),
			color_attachments: &[Some(wgpu::RenderPassColorAttachment {
				view: target,
				depth_slice: None,
				resolve_target: None,
				ops: wgpu::Operations {
					load: wgpu::LoadOp::Load,
					store: wgpu::StoreOp::Store,
				},
			})],
			depth_stencil_attachment: None,
			timestamp_writes: None,
			occlusion_query_set: None,
			multiview_mask: None,
		});
		pass.set_pipeline(&self.debug_pipeline);
		// The front atlas is the one the flip just published.
		pass.set_bind_group(0, &self.debug_bind[self.ping], &[]);
		pass.draw(0..3, 0..1);
	}
}
