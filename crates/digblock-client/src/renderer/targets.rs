//! The offscreen render targets and the two resolution knobs behind
//! them: the world renders at `surface x render_scale`, GI at
//! `render_size / gi_scale_div` -- an integer divisor of the RENDER size.

use super::*;

impl Renderer {
	/// (Re)build the size-dependent frame targets when the size changes.
	/// Covers first frame, window resize, resume-with-new-size, and the
	/// device-loss rebuild (which resets `targets` to None).
	pub(super) fn ensure_targets(&mut self, gpu: &Gpu, size: (u32, u32), scale: RenderScale) {
		if self.targets.as_ref().map(|t| (t.size, t.scale)) == Some((size, scale)) {
			return;
		}
		// The old set goes first, DESTROYED: the last frame that bound it
		// is already submitted, and nothing binds it again — every group
		// naming it is rebuilt below before the next encode (`gpu_ledger`).
		if let Some(old) = self.targets.take() {
			old.retire(&mut self.ledger);
		}
		// `size` stays the SURFACE size — it is the rebuild key, and it is
		// what the overlay and every UI hit-test use. Only the world's
		// targets shrink.
		let render = scale.render_size(size);
		let extent = wgpu::Extent3d {
			width: render.0,
			height: render.1,
			depth_or_array_layers: 1,
		};
		let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("frame depth"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: DEPTH_FORMAT,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
		let depth_copy = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("solid depth copy (r32f)"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: wgpu::TextureFormat::R32Float,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let depth_copy_view = depth_copy.create_view(&wgpu::TextureViewDescriptor::default());
		let normal_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("solid face-normal codes (r8uint)"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: NORMAL_FORMAT,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let normal_view = normal_tex.create_view(&wgpu::TextureViewDescriptor::default());
		let scene = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("scene color"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: self.surface_format,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let scene_view = scene.create_view(&wgpu::TextureViewDescriptor::default());
		// The post target: water, the viewmodel and the GI debug view draw HERE while sampling
		// `scene` for refraction and SSR. Two textures rather than one, and not optional: water
		// reads the scene colour it is blending over, so one texture both written and sampled is
		// a read-write hazard, and an invalid pass poisons the whole encoder — `finish()` drops
		// EVERY pass in the frame, so the symptom is a black window with no HUD.
		let post = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("post color"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: self.surface_format,
			usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let post_view = post.create_view(&wgpu::TextureViewDescriptor::default());
		// Both group-0 combos that depend on the resize-recreated
		// textures are (re)built here: the water frame+scene group and
		// the blit group.
		let water_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("water frame + scene targets"),
			layout: &self.water_frame_bgl,
			entries: &[
				// The SAME frame UBO the solids read. Water used to get a
				// second copy of it for one field — a constant alpha — and that
				// field is gone (see water.wgsl's `alpha`), so the copy and
				// its per-frame upload went with it. The water group still
				// needs its own bind GROUP, because it also carries the
				// resize-recreated scene targets.
				wgpu::BindGroupEntry {
					binding: 0,
					resource: self.frame_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&depth_copy_view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::TextureView(&scene_view),
				},
				wgpu::BindGroupEntry {
					binding: 3,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		let blit_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("blit"),
			layout: &self.blit_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&scene_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		// What the UPSCALE samples: the post target, i.e. the finished
		// world at render resolution.
		let upscale_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("upscale"),
			layout: &self.blit_bgl,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: wgpu::BindingResource::TextureView(&post_view),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
				},
			],
		});
		let depth_blit_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("depth blit"),
			layout: &self.depth_blit_bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: wgpu::BindingResource::TextureView(&depth_view),
			}],
		});
		// The GI resolve targets at a whole-integer fraction of the RENDER
		// size — the chunk shader's bilateral upsample bridges the gap,
		// and its 4-tap gather is why the ratio must divide exactly.
		let gi = scale.gi_size(size);
		let gi_tex = |format, label| {
			gpu.device.create_texture(&wgpu::TextureDescriptor {
				label: Some(label),
				size: wgpu::Extent3d {
					width: gi.0,
					height: gi.1,
					depth_or_array_layers: 1,
				},
				mip_level_count: 1,
				sample_count: 1,
				dimension: wgpu::TextureDimension::D2,
				format,
				usage: wgpu::TextureUsages::RENDER_ATTACHMENT
					| wgpu::TextureUsages::TEXTURE_BINDING,
				view_formats: &[],
			})
		};
		// One constant with the resolve pipeline's own targets — a skew
		// here is a validation error, and the reason it is f32 is in
		// gi::GI_ACC_FORMAT.
		let acc = crate::gi::GI_ACC_FORMAT;
		let gi_indirect = [gi_tex(acc, "gi indirect 0"), gi_tex(acc, "gi indirect 1")];
		let gi_sun = [gi_tex(acc, "gi sun 0"), gi_tex(acc, "gi sun 1")];
		let gi_indirect_view = [
			gi_indirect[0].create_view(&Default::default()),
			gi_indirect[1].create_view(&Default::default()),
		];
		let gi_sun_view = [
			gi_sun[0].create_view(&Default::default()),
			gi_sun[1].create_view(&Default::default()),
		];
		let scene_bg = |i: usize| {
			gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("gi scene"),
				layout: self.gi.scene_bgl(),
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: wgpu::BindingResource::TextureView(&gi_indirect_view[i]),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&gi_sun_view[i]),
					},
				],
			})
		};
		// The scene reads [0]; the resolve reads [1], which is where last
		// frame's first a-trous iteration left the filtered image.
		let gi_scene_bg = scene_bg(0);
		let gi_resolve_bg = self.gi.make_resolve_bind(
			gpu,
			&depth_copy_view,
			&normal_view,
			&gi_indirect_view[1],
			&gi_sun_view[1],
		);
		let gi_atrous_bg = [
			self.gi
				.make_atrous_bind(gpu, 0, &gi_indirect_view[0], &gi_sun_view[0]),
			self.gi
				.make_atrous_bind(gpu, 1, &gi_indirect_view[1], &gi_sun_view[1]),
		];
		// Fresh textures hold nothing: last frame's screen history went
		// with them.
		self.gi.reset_history();
		let textures: Vec<wgpu::Texture> = [depth, depth_copy, normal_tex, scene, post]
			.into_iter()
			.chain(gi_indirect)
			.chain(gi_sun)
			.collect();
		for tex in &textures {
			self.ledger.charge_texture(GpuKind::Targets, tex);
		}
		self.targets = Some(FrameTargets {
			size,
			scale,
			textures,
			depth_view,
			depth_copy_view,
			normal_view,
			scene_view,
			post_view,
			water_bg,
			blit_bg,
			upscale_bg,
			depth_blit_bg,
			gi_indirect_view,
			gi_sun_view,
			gi_scene_bg,
			gi_resolve_bg,
			gi_atrous_bg,
		});
	}
}
