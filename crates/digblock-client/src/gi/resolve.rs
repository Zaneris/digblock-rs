//! Resolving probes to screen and the a-trous filter over the result.
//! The ping-pong roles are FIXED; swapping them inverts the history.

use super::*;

/// The resolve's shipped ray budgets — DDA iterations for the per-pixel final gather and the sun
/// march, the frame's two largest (BENCH.md) and so the tier knob
/// (`EngineConfig::gi_gather_iters`/`gi_sun_iters`). The shaders' loop bounds are CAPACITIES and
/// stay WGSL literals, held to these by `budgets_fit_the_shaders_capacities`; a retune is measured
/// against GI-DESIGN.md's bars, since a shorter march silently un-shadows what lies past the cut.
pub(crate) const GI_GATHER_ITERS: u32 = 96;
pub(crate) const GI_SUN_ITERS: u32 = 320;

impl GiDraw {
	/// Build the resolve's group-0 bind (uniform + the depth and
	/// face-normal taps) against the current targets — called whenever
	/// the frame targets rebuild.
	pub(crate) fn make_resolve_bind(
		&self,
		gpu: &Gpu,
		depth_copy_view: &wgpu::TextureView,
		normal_view: &wgpu::TextureView,
		hist_indirect: &wgpu::TextureView,
		hist_sun: &wgpu::TextureView,
	) -> wgpu::BindGroup {
		gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi resolve bg"),
			layout: &self.resolve_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: self.resolve_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(depth_copy_view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(normal_view),
				},
				wgpu::BindGroupEntry {
					binding: 3,
					resource: wgpu::BindingResource::TextureView(hist_indirect),
				},
				wgpu::BindGroupEntry {
					binding: 4,
					resource: wgpu::BindingResource::TextureView(hist_sun),
				},
			],
		})
	}

	/// Build one a-trous iteration's bind: its stride uniform plus the pair
	/// it reads. Iteration `i` reads whichever pair the previous stage
	/// wrote, so the caller owns the ping — there is no state here.
	pub(crate) fn make_atrous_bind(
		&self,
		gpu: &Gpu,
		iter: usize,
		src_indirect: &wgpu::TextureView,
		src_sun: &wgpu::TextureView,
	) -> wgpu::BindGroup {
		gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi atrous bg"),
			layout: &self.atrous_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: self.atrous_buf[iter].as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(src_indirect),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(src_sun),
				},
			],
		})
	}

	/// Encode one a-trous iteration: read one lighting pair, write the other. Both are full-screen
	/// half-res passes with no depth. `query` is the caller's and must be one pair per ITERATION —
	/// sharing `Q_GI_DENOISE` across both lets the second overwrite the first, and the denoise
	/// column then reports the stride-2 pass alone.
	pub(crate) fn encode_atrous(
		&self,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		query: u32,
		bind: &wgpu::BindGroup,
		indirect_view: &wgpu::TextureView,
		sun_view: &wgpu::TextureView,
	) {
		let attach = |view| {
			Some(wgpu::RenderPassColorAttachment {
				view,
				resolve_target: None,
				ops: wgpu::Operations {
					load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
					store: wgpu::StoreOp::Store,
				},
				depth_slice: None,
			})
		};
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("gi atrous"),
			color_attachments: &[attach(indirect_view), attach(sun_view)],
			depth_stencil_attachment: None,
			timestamp_writes: timer.and_then(|t| t.writes(query)),
			occlusion_query_set: None,
			multiview_mask: None,
		});
		pass.set_pipeline(&self.atrous_pipeline);
		pass.set_bind_group(0, bind, &[]);
		pass.draw(0..3, 0..1);
	}

	/// Encode the half-res GI resolve into the two lighting targets.
	pub(crate) fn encode_resolve(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		frame: &FrameUniforms,
		out: ResolveTargets<'_>,
	) {
		let ResolveTargets {
			bind,
			indirect_view,
			sun_view,
		} = out;
		let side = (self.window_chunks * 16) as f32;
		let mut data = Vec::with_capacity(368);
		for col in frame.inv_view_proj {
			data.extend_from_slice(bytemuck::cast_slice(&col));
		}
		data.extend_from_slice(bytemuck::cast_slice(&[
			frame.cam_pos[0],
			frame.cam_pos[1],
			frame.cam_pos[2],
			side,
		]));
		let d0 = self.cascades[0].dims;
		let d1 = self.cascades[1].dims;
		data.extend_from_slice(bytemuck::cast_slice(&[
			d0.0 as f32,
			d0.1 as f32,
			d0.2 as f32,
			self.cascades[0].spacing as f32,
		]));
		data.extend_from_slice(bytemuck::cast_slice(&[
			d1.0 as f32,
			d1.1 as f32,
			d1.2 as f32,
			self.cascades[1].spacing as f32,
		]));
		data.extend_from_slice(bytemuck::cast_slice(&self.last_sun));
		// rgb the caster colour; w its twilight STRENGTH, which the scene
		// pass applies to the sun lane through `light_dir.w`. The
		// chromatic residual is a finished radiance on the indirect lane
		// and gets no such multiply, so it has to carry the strength
		// itself — without it the two lanes stop summing to the
		// transmission anywhere but full sun. (`light_color.w` is the
		// animation clock; the resolve has no use for it.)
		data.extend_from_slice(bytemuck::cast_slice(&[
			frame.light_color[0],
			frame.light_color[1],
			frame.light_color[2],
			frame.light_dir[3],
		]));
		data.extend_from_slice(bytemuck::cast_slice(&frame.zenith));
		data.extend_from_slice(bytemuck::cast_slice(&frame.horizon));
		for col in self.prev_vp {
			data.extend_from_slice(bytemuck::cast_slice(&col));
		}
		data.extend_from_slice(bytemuck::cast_slice(&[
			self.prev_cam[0],
			self.prev_cam[1],
			self.prev_cam[2],
			f32::from(self.history_valid),
		]));
		// The EXACT caster, unquantized. Quantization exists so probes
		// hold still between updates; on a per-pixel ray it is the
		// lattice resonance, so the resolve takes the real direction and
		// jitters it instead.
		let e = [
			-frame.light_dir[0],
			-frame.light_dir[1],
			-frame.light_dir[2],
		];
		let el = (e[0] * e[0] + e[1] * e[1] + e[2] * e[2]).sqrt().max(1e-6);
		data.extend_from_slice(bytemuck::cast_slice(&[
			e[0] / el,
			e[1] / el,
			e[2] / el,
			self.last_sun[3],
		]));
		data.extend_from_slice(bytemuck::cast_slice(&[
			self.frame_i as f32,
			SUN_CONE_COS,
			// The sky ceiling (R18) — see `sky_y`.
			self.sky_y,
			// The cost-stub mask (DIG_STUB), exact in f32 well past the
			// bits it uses. The resolve does not bind the frame UBO, so it
			// cannot read `frame.misc.z` the way water.wgsl does.
			self.stub as f32,
		]));
		// The live invalidation spheres, decayed here because this runs
		// exactly once per frame. w = 0 marks an empty slot.
		for i in 0..4 {
			let s = self.resolve_inval.get(i).map_or([0.0f32; 4], |(p, _, _)| {
				[p[0], p[1], p[2], RESOLVE_INVAL_RADIUS]
			});
			data.extend_from_slice(bytemuck::cast_slice(&s));
		}
		for e in &mut self.resolve_inval {
			e.1 -= 1;
		}
		self.resolve_inval.retain(|e| e.1 > 0);
		// The ray budgets (`rz.budget`), exact in f32 far past any real
		// iteration count.
		data.extend_from_slice(bytemuck::cast_slice(&[
			self.gather_iters as f32,
			self.sun_iters as f32,
			0.0,
			0.0,
		]));
		gpu.queue.write_buffer(&self.resolve_buf, 0, &data);
		let attach = |view| {
			Some(wgpu::RenderPassColorAttachment {
				view,
				resolve_target: None,
				ops: wgpu::Operations {
					load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
					store: wgpu::StoreOp::Store,
				},
				depth_slice: None,
			})
		};
		let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
			label: Some("gi resolve"),
			color_attachments: &[attach(indirect_view), attach(sun_view)],
			depth_stencil_attachment: None,
			timestamp_writes: timer.and_then(|t| t.writes(Q_GI_TRACE)),
			occlusion_query_set: None,
			multiview_mask: None,
		});
		pass.set_pipeline(&self.resolve_pipeline);
		pass.set_bind_group(0, bind, &[]);
		pass.set_bind_group(1, &self.empty_bg, &[]);
		pass.set_bind_group(2, self.gather_bind(), &[]);
		pass.draw(0..3, 0..1);
		drop(pass);
		// Latch this frame as next frame's history. A large caster swing
		// between two frames is `/time`, not the sky turning, and screen
		// history that survives it ghosts for seconds.
		let jumped = e[0] * self.prev_sun[0] + e[1] * self.prev_sun[1] + e[2] * self.prev_sun[2]
			< SUN_JUMP_COS;
		self.history_valid = !jumped;
		self.prev_sun = [e[0] / el, e[1] / el, e[2] / el];
		self.prev_vp = frame.view_proj;
		self.prev_cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
		self.frame_i = self.frame_i.wrapping_add(1);
	}
}
