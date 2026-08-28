//! The probe trace and the accumulate/blend pass order. Pass order is
//! an invariant, not a preference -- `GI-DESIGN.md` `## Invariants`.

use super::*;

impl GiDraw {
	/// Encode this frame's probe updates (both cascades) at the head of
	/// the frame's encoder: full prev→out copy, then the amortized
	/// trace over PROBES_PER_DISPATCH probes, then edit invalidation.
	pub(crate) fn encode(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		frame: &FrameUniforms,
		lights: LightInputs<'_>,
	) {
		let cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
		self.flush_lights(gpu, cam, lights);
		// Re-read the sky ceiling when the mirror moved (R18) — a 32-row
		// scan the mirror keeps current word by word.
		if self.coarse_mips_dirty {
			self.coarse_mips_dirty = false;
			self.sky_y = self.coarse_mirror.sky_y();
		}
		// The quantized caster was latched by `patch_frame` (which the
		// renderer calls before the frame uploads, every frame).
		let spheres: Vec<[f32; 4]> = {
			let take = self.inval_queue.len().min(4);
			let drained: Vec<([f32; 3], bool)> = self.inval_queue.drain(..take).collect();
			// What the probes start re-tracing this frame, the resolve's
			// screen history must stop trusting for a while — same spheres,
			// held live across RESOLVE_INVAL_FRAMES. Four slots, and a full
			// ring evicts the oldest entry OF THE SAME KIND so a walking
			// carrier's stream cannot push a world edit out early; see
			// `resolve_inval`.
			for &(p, moving) in &drained {
				if self.resolve_inval.len() >= 4 {
					let victim = self
						.resolve_inval
						.iter()
						.position(|e| e.2 == moving)
						.unwrap_or(0);
					self.resolve_inval.remove(victim);
				}
				self.resolve_inval.push((p, RESOLVE_INVAL_FRAMES, moving));
			}
			(0..4)
				.map(|i| {
					drained
						.get(i)
						.map(|(p, _)| [p[0], p[1], p[2], 0.0]) // radius per cascade below
						.unwrap_or([0.0; 4])
				})
				.collect()
		};
		let side = (self.window_chunks * 16) as f32;
		let has_inval = spheres.iter().any(|s| s != &[0.0; 4]);
		for ci in 0..2 {
			let c = &self.cascades[ci];
			let mut data = Vec::with_capacity(GI_UNIFORM_SIZE as usize);
			let dims = [c.dims.0, c.dims.1, c.dims.2, c.spacing];
			data.extend_from_slice(bytemuck::cast_slice(&dims));
			data.extend_from_slice(bytemuck::cast_slice(&[cam[0], cam[1], cam[2], side]));
			data.extend_from_slice(bytemuck::cast_slice(&self.last_sun));
			data.extend_from_slice(bytemuck::cast_slice(&frame.light_color));
			// zenith.w carries the sky ceiling (R18) — the probe kernel
			// only ever reads the color's rgb.
			data.extend_from_slice(bytemuck::cast_slice(&[
				frame.zenith[0],
				frame.zenith[1],
				frame.zenith[2],
				self.sky_y,
			]));
			data.extend_from_slice(bytemuck::cast_slice(&frame.horizon));
			// The sweep is PERMUTED, not contiguous: `cursor` walks the sweep index and the probe
			// it selects is `(k * stride) % count` with stride coprime to count, so a full cycle
			// touches every probe once while consecutive dispatches land far apart. A contiguous
			// cursor made convergence sweep the volume as a visible band. The base is reduced
			// here because `cursor * stride` would overflow u32 in the shader.
			let base = ((u64::from(c.cursor) * u64::from(c.stride)) % u64::from(c.probes())) as u32;
			// `sched.z`: where a probe ray starts crossing bricks by summary;
			// the stub pushes it past MAX_T so no ray ever does.
			let coarse_from = if self.stub & crate::renderer::stub::GIPROBELOD != 0 {
				u32::MAX
			} else {
				c.coarse_from
			};
			data.extend_from_slice(bytemuck::cast_slice(&[
				base,
				self.frame,
				coarse_from,
				c.stride,
			]));
			let radius = if ci == 0 { 8.0f32 } else { 16.0 };
			for s in &spheres {
				let r = if s == &[0.0; 4] { 0.0 } else { radius };
				data.extend_from_slice(bytemuck::cast_slice(&[s[0], s[1], s[2], r]));
			}
			gpu.queue.write_buffer(&c.uniform_buf, 0, &data);
			// Full prev→out copy so untouched probes carry forward.
			encoder.copy_texture_to_texture(
				self.cascades[ci].atlas[self.ping].as_image_copy(),
				self.cascades[ci].atlas[1 - self.ping].as_image_copy(),
				wgpu::Extent3d {
					width: c.dims.0,
					height: c.dims.1,
					depth_or_array_layers: c.dims.2 * 6,
				},
			);
		}
		{
			let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
				label: Some("gi probes"),
				timestamp_writes: timer.and_then(|t| t.compute_writes(Q_GI)),
			});
			pass.set_pipeline(&self.probe_pipeline);
			for c in &self.cascades {
				pass.set_bind_group(0, &c.bind[self.ping], &[]);
				pass.dispatch_workgroups(PROBES_PER_DISPATCH.min(c.probes()), 1, 1);
			}
			// Camera light for spray/viewmodel: one invocation against
			// the C0 cascade (prev_atlas = last frame's front, gi.dims
			// = C0 — the camera always sits inside C0's coverage).
			pass.set_pipeline(&self.cam_pipeline);
			pass.set_bind_group(0, &self.cascades[0].bind[self.ping], &[]);
			pass.dispatch_workgroups(1, 1, 1);
			if has_inval {
				pass.set_pipeline(&self.inval_pipeline);
				for c in &self.cascades {
					pass.set_bind_group(0, &c.bind[self.ping], &[]);
					pass.dispatch_workgroups(c.probes().div_ceil(64), 1, 1);
				}
			}
		}
		for c in &mut self.cascades {
			let n = c.probes();
			c.cursor = (c.cursor + PROBES_PER_DISPATCH.min(n)) % n;
		}
		self.ping = 1 - self.ping;
		self.frame = self.frame.wrapping_add(1);
	}
}
