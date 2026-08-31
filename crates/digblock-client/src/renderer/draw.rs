//! The frame: five passes into borrowed views. The renderer never sees
//! the surface, so it cannot hold a stale swapchain texture across a
//! suspend.

use super::*;

impl Renderer {
	/// Encode the frame in one command encoder, fixed order — probes →
	/// depth prepass → depth blit → GI resolve → scene → water →
	/// viewmodel → overlay.
	pub(crate) fn render(
		&mut self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		scene: &FrameScene<'_>,
	) -> RenderStats {
		let FrameScene {
			uniforms: ref frame,
			view_proj,
			size,
			scale,
			ui_scale,
			players,
			held,
			carried,
			cam_forward,
			light_cap,
			sky_floor,
			overlay,
			confetti,
			bursts,
			siphon,
			teleport,
			markers,
			feed,
			target: highlight_target,
			viewmodel,
			ui,
			chat,
			nametags,
			binds,
		} = *scene;
		self.ensure_targets(gpu, size, scale);
		// Remote players re-upload every frame (a handful of instances —
		// cheaper than the change detection it replaces).
		self.players.set_instances(gpu, players);
		self.players.set_held(gpu, held);
		self.highlight.set_target(gpu, highlight_target);
		let aspect = size.0 as f32 / size.1.max(1) as f32;
		self.viewmodel.prepare(gpu, aspect, viewmodel.as_ref());
		let mut stats = RenderStats {
			chunks_resident: self.chunks.len() as u32,
			..Default::default()
		};
		// GI parameters into the frame uniforms (quantized caster, cascade
		// dims, window side) — before the UBO uploads below.
		let mut frame = *frame;
		self.gi.patch_frame(&mut frame);
		let frame = &frame;
		if let Some(t) = self.gpu_timer.as_mut() {
			t.begin_frame();
		}
		self.overlay.prepare(
			gpu,
			size,
			ui_scale,
			ui,
			overlay,
			feed,
			chat.as_ref(),
			nametags,
			binds,
		);
		self.ui_icons.prepare(gpu, size, ui);
		gpu.queue
			.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(frame));
		// One cull for the whole frame: scene and water lists out of a
		// single walk of the flat resident set.
		let cam_planes = side_planes(view_proj);
		let cam_chunk = (
			(frame.cam_pos[0] / 16.0).floor() as i32,
			(frame.cam_pos[2] / 16.0).floor() as i32,
		);
		stats.chunks_culled =
			Self::cull(&self.chunks, &mut self.lists, &cam_planes, None, cam_chunk);
		let spray_n = self.spray.prepare(
			gpu,
			glam::Vec3::new(frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]),
		);
		let confetti_n = self.confetti.prepare(gpu, confetti);
		let bursts_n = self.bursts.prepare(gpu, bursts);
		let siphon_n = self.siphon.prepare(gpu, siphon.as_ref());
		let teleport_n = self.teleport.prepare(gpu, teleport);
		let marker_n = self.waystone_fx.prepare(gpu, markers);

		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor {
				label: Some("frame"),
			});

		// RTGI probe update first: compute at the head of the encoder, so
		// every later pass that reads the atlas sees this frame's write.
		self.gi.encode(
			gpu,
			&mut encoder,
			self.gpu_timer.as_ref(),
			frame,
			crate::gi::LightInputs {
				fwd: cam_forward,
				carried,
				cap: light_cap,
				sky_floor,
			},
		);
		// AFTER the encode, not before it: `carried` is folded into the
		// light list inside `flush_lights`, so reading these earlier
		// reported the PREVIOUS frame's emitters — and zero on frame 0,
		// which is every `--frames 1` capture and exactly the run the
		// count exists to answer.
		(stats.sky_y, stats.emitters) = self.gi.debug_state();

		let draw = ChunkDraw {
			chunks: &self.chunks,
			origin_buf: &self.origin_buf,
			index_buf: &self.index_buf,
		};

		{
			let targets = self.targets.as_ref().expect("ensured above");
			// Depth prepass: opaque solids only, no shading — the depth
			// the GI resolve reconstructs world positions from.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("depth prepass"),
					// Cleared to a sentinel code no real face carries,
					// so a texel the prepass never wrote can never
					// match a surface in the upsample's equality test.
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.normal_view,
						resolve_target: None,
						ops: wgpu::Operations {
							load: wgpu::LoadOp::Clear(wgpu::Color {
								r: 255.0,
								g: 0.0,
								b: 0.0,
								a: 0.0,
							}),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Clear(1.0),
							store: wgpu::StoreOp::Store,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_PREPASS)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				pass.set_pipeline(&self.prepass_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				draw.bind(&mut pass);
				// NORASTER empties the depth buffer as well as the pass, so
				// the resolve downstream takes the sky early-out everywhere
				// — an upper bound on geometry, not a prepass measurement.
				if self.stub & stub::NORASTER == 0 {
					for &i in &self.lists.scene {
						let chunk = &draw.chunks[i as usize];
						let Some(mesh) = chunk.solid.as_ref() else {
							continue;
						};
						stats.triangles += draw_chunk(&mut pass, mesh, chunk.slot) / 3;
					}
				}
			}

			// Solid depth becomes sampleable R32Float — moved BEFORE the
			// scene pass, because the GI resolve consumes it; water and
			// the highlight decal read the same copy later. A fullscreen
			// draw, not copy_texture_to_texture — WebGPU forbids
			// depth32float as a copy destination.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("depth blit"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.depth_copy_view,
						resolve_target: None,
						ops: wgpu::Operations {
							// The blit overwrites every pixel.
							load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: None,
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_DEPTH_BLIT)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				pass.set_pipeline(&self.depth_blit_pipeline);
				pass.set_bind_group(0, &targets.depth_blit_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;
			}

			// GI resolve: probes + the per-pixel sun ray → the two
			// half-res lighting inputs the scene pass upsamples.
			self.gi.encode_resolve(
				gpu,
				&mut encoder,
				self.gpu_timer.as_ref(),
				frame,
				crate::gi::ResolveTargets {
					bind: &targets.gi_resolve_bg,
					indirect_view: &targets.gi_indirect_view[0],
					sun_view: &targets.gi_sun_view[0],
				},
			);
			// The spatial half. Two edge-stopped a-trous iterations, [0] -> [1] -> [0],
			// leaving the scene reading [0] and the once-filtered image parked in [1] as next
			// frame's history. Without this the resolve's only estimator is time, which forces
			// a history deep enough to smear under motion — see gi_atrous.wgsl.
			// DIG_STUB=giatrous drops both: the scene reads the RAW resolve out of [0].
			for (i, bind) in targets.gi_atrous_bg.iter().enumerate() {
				if self.stub & stub::GIATROUS != 0 {
					break;
				}
				let out = 1 - i;
				self.gi.encode_atrous(
					&mut encoder,
					self.gpu_timer.as_ref(),
					// One query pair per iteration — see encode_atrous.
					if i == 0 { Q_GI_DENOISE } else { Q_GI_DENOISE2 },
					bind,
					&targets.gi_indirect_view[out],
					&targets.gi_sun_view[out],
				);
				stats.draw_calls += 1;
			}

			// Scene pass: solids + sky into the offscreen color target;
			// depth arrives from the prepass (Load, LessEqual).
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("scene"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.scene_view,
						resolve_target: None,
						ops: wgpu::Operations {
							// Fog color, matching the sky horizon — belt and
							// braces for far-plane precision edges (the sky
							// pass covers every non-terrain pixel anyway).
							load: wgpu::LoadOp::Clear(wgpu::Color {
								r: f64::from(frame.clear_color[0]),
								g: f64::from(frame.clear_color[1]),
								b: f64::from(frame.clear_color[2]),
								a: 1.0,
							}),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Load,
							store: wgpu::StoreOp::Store,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_MAIN)),
					occlusion_query_set: None,
					multiview_mask: None,
				});

				pass.set_pipeline(&self.solid_pipeline);
				pass.set_bind_group(0, &self.frame_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, &targets.gi_scene_bg, &[]);
				// Origins for every chunk, bound once — each draw selects
				// its own with first_instance.
				draw.bind(&mut pass);
				// The clean raster stub: the prepass above still ran, so
				// depth and face codes are intact and the GI is untouched.
				if self.stub & (stub::NOSCENE | stub::NORASTER) == 0 {
					for &i in &self.lists.scene {
						let chunk = &draw.chunks[i as usize];
						let Some(mesh) = chunk.solid.as_ref() else {
							continue;
						};
						stats.triangles += draw_chunk(&mut pass, mesh, chunk.slot) / 3;
						stats.chunks_drawn += 1;
					}
				}

				// Remote players: opaque, depth-writing, before the sky so
				// they occlude and fog like terrain (frame_bg is bound at
				// 0). They gather the probe field per fragment, so their
				// group 2 is the GI world data, not the resolve outputs.
				pass.set_bind_group(2, self.gi.gather_bind(), &[]);
				let player_tris = self.players.draw(&mut pass, &self.index_buf);
				if player_tris > 0 {
					stats.draw_calls += self.players.draw_calls();
					stats.triangles += player_tris;
				}

				// Sky after solids (depth test discards occluded fragments —
				// near-free on TBDR); frame_bg is still bound at 0.
				self.sky.draw(&mut pass);
				stats.draw_calls += 1;
			}

			// Water pass, in place on the scene target: transparent water in two
			// subpasses — a depth-only prepass records the nearest water
			// surface, then the colour pass (no depth write, double-sided)
			// shades only fragments at that depth, one blended layer per pixel.
			// Depth is discarded at the end: nothing after this reads it.
			//
			// It targets `post_view` at RENDER resolution and one upscale pass
			// at the end takes that to the surface. The opening blit cannot be
			// dropped: water SAMPLES `scene_view`, so the pass needs a
			// destination that is not its source.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("water"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.post_view,
						resolve_target: None,
						ops: wgpu::Operations {
							// The blit overwrites every pixel; Clear beats
							// Load on TBDR.
							load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Load,
							store: wgpu::StoreOp::Discard,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_WATER)),
					occlusion_query_set: None,
					multiview_mask: None,
				});

				pass.set_pipeline(&self.blit_pipeline);
				pass.set_bind_group(0, &targets.blit_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;

				pass.set_bind_group(0, &targets.water_bg, &[]);
				pass.set_bind_group(1, &self.materials_bg, &[]);
				pass.set_bind_group(2, self.gi.gather_bind(), &[]);
				draw.bind(&mut pass);
				// DIG_STUB=nowater keeps the blit — that is what composites
				// the scene into the post target — and drops only the water
				// geometry, which bounds everything the water pass costs.
				if self.stub & stub::NOWATER == 0 {
					let (water_draws, water_tris) =
						self.water.draw(&mut pass, draw.chunks, &self.lists.water);
					stats.water_draws += water_draws;
					stats.triangles += water_tris;
				}

				// Blended glass, over the finished water: the depth
				// attachment already holds solids AND the nearest water
				// surface, so a pane behind either loses the test and one
				// in front blends over both. Water's groups 0-2 are
				// exactly the ones the glass pipeline shares.
				if self.stub & stub::NOGLASS == 0 {
					let (glass_draws, glass_tris) =
						self.glass
							.draw(&mut pass, draw.chunks, &self.lists.translucent);
					stats.draw_calls += glass_draws;
					stats.triangles += glass_tris;
				}

				// Target highlight/crack decal, over the finished water so
				// a box on a source (or a submerged block) survives the
				// water's depth tint; it occludes itself against the solid
				// depth copy in-shader (water_bg is still bound at 0).
				let highlight_tris = self.highlight.draw(&mut pass);
				if highlight_tris > 0 {
					stats.draw_calls += 1;
					stats.triangles += highlight_tris;
				}

				// Spray billboards over the finished water (blends over
				// sheet and pool alike). Water left its frame+scene group
				// bound at 0; spray wants the plain frame group.
				pass.set_bind_group(0, &self.frame_bg, &[]);
				let live = u32::from(self.stub & stub::SPRAY == 0) * spray_n;
				let spray_tris = self.spray.draw(&mut pass, live);
				stats.draw_calls += u32::from(spray_tris > 0);
				stats.triangles += spray_tris;

				// Level-up confetti, over the same finished world as the
				// spray it shares a pass (and a bind group) with. It
				// carries one group spray does not: the GI world data its
				// flakes run the shared NEE over.
				let confetti_tris =
					self.confetti
						.draw(&mut pass, confetti_n, self.gi.gather_bind());
				stats.draw_calls += u32::from(confetti_tris > 0);
				stats.triangles += confetti_tris;
				// And the burst rack, through the same groups.
				let burst_tris = self.bursts.draw(&mut pass, bursts_n, self.gi.gather_bind());
				stats.draw_calls += u32::from(burst_tris > 0);
				stats.triangles += burst_tris;

				// The essence draw last of the three, because it is the
				// only ADDITIVE one: it has to land on top of whatever the
				// alpha-blended billboards left, not underneath it.
				let siphon_tris = self.siphon.draw(&mut pass, siphon_n);
				stats.draw_calls += u32::from(siphon_tris > 0);
				stats.triangles += siphon_tris;

				// The rest of the additive light: teleport effects and
				// the standing markers, on the siphon's own rules.
				let teleport_tris = self.teleport.draw(&mut pass, teleport_n);
				stats.draw_calls += u32::from(teleport_tris > 0);
				stats.triangles += teleport_tris;
				let marker_tris = self.waystone_fx.draw(&mut pass, marker_n);
				stats.draw_calls += u32::from(marker_tris > 0);
				stats.triangles += marker_tris;
			}

			// The item in your own hand, over the finished world. Its own
			// pass for one reason: it clears the depth buffer first. The
			// item is a metre from the eye and would otherwise be sliced
			// The voxel-window debug raymarch paints over the finished
			// scene (overlay still lands on top) — the P1 gate capture.
			if self.debug_gi {
				let cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
				self.gi.encode_debug(
					gpu,
					&mut encoder,
					&targets.post_view,
					frame.inv_view_proj,
					cam,
				);
			}
			// open by the wall you walk into — but it still needs depth
			// among its OWN faces, because a staircase in hand is not
			// convex and back-face culling alone can't sort it. The water
			// pass discarded this attachment, so reusing it costs nothing.
			if !self.viewmodel.is_empty() {
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("viewmodel"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: &targets.post_view,
						resolve_target: None,
						ops: wgpu::Operations {
							load: wgpu::LoadOp::Load,
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
						view: &targets.depth_view,
						depth_ops: Some(wgpu::Operations {
							load: wgpu::LoadOp::Clear(1.0),
							store: wgpu::StoreOp::Discard,
						}),
						stencil_ops: None,
					}),
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_VIEWMODEL)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				let tris = self
					.viewmodel
					.draw(&mut pass, &self.index_buf, &self.materials_bg);
				stats.draw_calls += 1;
				stats.triangles += tris;
			}

			// THE UPSCALE. Everything above rendered at RENDER resolution
			// into `scene_view`; this is the one place it becomes the
			// surface. At native render scale it is a straight 1:1 blit —
			// the same fullscreen draw that used to open the water pass —
			// so the default frame gains a pass boundary and nothing else.
			//
			// The overlay deliberately comes AFTER: UI, text and name tags
			// are drawn at native resolution straight onto the swapchain,
			// so nothing the player reads is ever resampled.
			{
				let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
					label: Some("upscale"),
					color_attachments: &[Some(wgpu::RenderPassColorAttachment {
						view: target,
						resolve_target: None,
						ops: wgpu::Operations {
							// Covers every pixel; Clear beats Load on TBDR.
							load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
							store: wgpu::StoreOp::Store,
						},
						depth_slice: None,
					})],
					depth_stencil_attachment: None,
					timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_UPSCALE)),
					occlusion_query_set: None,
					multiview_mask: None,
				});
				pass.set_pipeline(&self.upscale_pipeline);
				pass.set_bind_group(0, &targets.upscale_bg, &[]);
				pass.draw(0..3, 0..1);
				stats.draw_calls += 1;
			}
		}

		// HUD + debug overlay: own pass over the finished frame —
		// LoadOp::Load, no depth. Always encoded now that the crosshair is
		// part of it; with the debug panel hidden it's a handful of quads.
		{
			let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label: Some("overlay"),
				color_attachments: &[Some(wgpu::RenderPassColorAttachment {
					view: target,
					resolve_target: None,
					ops: wgpu::Operations {
						load: wgpu::LoadOp::Load,
						store: wgpu::StoreOp::Store,
					},
					depth_slice: None,
				})],
				depth_stencil_attachment: None,
				timestamp_writes: self.gpu_timer.as_ref().and_then(|t| t.writes(Q_OVERLAY)),
				occlusion_query_set: None,
				multiview_mask: None,
			});
			// Sandwich: backdrops → main icons → content (counts, panel,
			// feed, menu) → ghost icon → topmost ghost quads/text. One
			// pass, one timer bracket; empty stages cost nothing.
			self.overlay.draw_below(&mut pass);
			self.ui_icons.draw_main(&mut pass, &self.materials_bg);
			self.overlay.draw_mid(&mut pass);
			self.ui_icons.draw_ghost(&mut pass, &self.materials_bg);
			self.overlay.draw_above(&mut pass);
		}

		if let Some(t) = self.gpu_timer.as_mut() {
			t.resolve(&mut encoder);
		}
		gpu.queue.submit([encoder.finish()]);
		if let Some(t) = self.gpu_timer.as_mut() {
			t.after_submit(gpu);
		}
		stats.draw_calls += stats.chunks_drawn + stats.water_draws;
		stats
	}
}
