//! What the frame needs that is not the world: the dig highlight, the
//! siphon beam, the viewmodel's bob, and the medium the eye is in.

use super::*;

impl Engine {
	/// Crosshair target for the highlight overlay: the cell, the box the outline wraps (the
	/// shape's own, so a slab or a door is boxed at its own size), and dig progress — `None` =
	/// idle outline. The water tool never reports progress in either state, so the outline stays
	/// and the crack never appears: empty it marks the source the scoop will take, charged the
	/// block the pour will land against, which is why the outline survives `diggable` saying no.
	pub(crate) fn dig_progress(&self) -> Option<crate::highlight::HighlightTarget> {
		let hit = self.target.as_ref()?;
		let boxed =
			|progress| crate::highlight::HighlightTarget::new(hit.cell, hit.cell_value, progress);
		if self.water_tool_selected() {
			return (self.inventory.bucket_full() || self.diggable(hit)).then(|| boxed(None));
		}
		if !self.diggable(hit) {
			return None;
		}
		// A field extraction shows the outline and NO crack — the water
		// tool's precedent. Nothing is breaking: the seam is being drawn
		// out of rock that stays standing, and a spreading web would say
		// the opposite of what is happening.
		if matches!(
			self.node_dig_mode(hit.cell),
			digblock_core::worldgen::nodes::DigMode::Extract(_)
		) {
			return Some(boxed(None));
		}
		// The DECAYED progress, so a released hold's web visibly retreats
		// (and a resumed one picks up from the retreated web) rather than
		// snapping between full and gone.
		let progress = self.config.debug_crack.or_else(|| {
			self.dig
				.as_ref()
				.filter(|d| d.cell == hit.cell)
				.map(|d| (f32::from(d.banked()) / f32::from(d.required.max(1))).min(1.0))
		});
		Some(boxed(progress))
	}

	/// The vein the siphon is drawing from this frame: the hit, the tier, and how far through the
	/// hold it is. Deliberately narrower than `dig_progress`'s extraction branch, which answers on
	/// AIM: merely looking at a seam with the siphon out draws nothing, because the stream is the
	/// tool working rather than the tool being held. So it wants the button down and the hold live
	/// — a released hold decays and the draw fades with it, as a retreating crack does for a dig.
	pub(super) fn siphon_target(&self) -> Option<(RayHit, BlockType, f32)> {
		let hit = self.target?;
		if !self.diggable(&hit) {
			return None;
		}
		let digblock_core::worldgen::nodes::DigMode::Extract(ore) = self.node_dig_mode(hit.cell)
		else {
			return None;
		};
		// The capture pin answers on aim alone, exactly as `debug_crack`
		// does, and for the same reason: standing in a hold is the one
		// thing a headless run cannot do.
		let progress = match self.config.debug_siphon {
			Some(p) => p,
			None => {
				let d = self
					.dig
					.as_ref()
					.filter(|d| d.cell == hit.cell && d.idle == 0)?;
				f32::from(d.banked()) / f32::from(d.required.max(1))
			}
		};
		Some((hit, ore, progress.clamp(0.0, 1.0)))
	}

	/// Age the essence draw: in while the siphon is working a vein, out once it stops.
	///
	/// Runs after the dig stepping, because what it reads is that step's result. The geometry is
	/// rewritten every live frame rather than latched, so following a seam from one cell to the
	/// next is one continuous stream — the envelope carries over — instead of a stutter of fades.
	pub(super) fn step_siphon(&mut self, dt: f32) {
		let Some((hit, ore, progress)) = self.siphon_target() else {
			// Nothing to draw from: run whatever was running out, and drop
			// it at the bottom — which is also how the flare a completed
			// draw ends on gets spent.
			if let Some(s) = self.vfx.siphon.as_mut() {
				s.envelope = crate::siphon::envelope_step(s.envelope, false, dt);
			}
			if self.vfx.siphon.is_some_and(|s| s.envelope <= 0.0) {
				self.vfx.siphon = None;
			}
			return;
		};
		let envelope =
			crate::siphon::envelope_step(self.vfx.siphon.map_or(0.0, |s| s.envelope), true, dt);
		let c = cell_center(hit.cell);
		let n = hit.normal;
		// `Extract` names one of the four tiers, so the palette is always
		// there; white is a visible wrong answer rather than a panic on a
		// path that only draws pixels.
		let (core, _) = ore.node_flecks().unwrap_or(([255; 3], [255; 3]));
		self.vfx.siphon = Some(SiphonDraw {
			source: Vec3::new(
				c[0] + n.0 as f32 * 0.5,
				c[1] + n.1 as f32 * 0.5,
				c[2] + n.2 as f32 * 0.5,
			),
			normal: n,
			tint: core.map(|v| f32::from(v) / 255.0),
			progress,
			envelope,
		});
	}

	/// Advance WHICH item is in the hand, and the swap dip between two. Runs on three paths: the
	/// interactive update's tail, where it must follow `reconcile_layout`, and before each
	/// scripted-flight return, since the viewmodel draws `Swap::shown()` and a carried light hangs
	/// off it — a harness that skips this can neither draw a held item nor measure one. The bob
	/// and the swing stay interactive-only, driven by motion the scripted camera does not have.
	pub(super) fn advance_hand(&mut self, dt: f32) {
		let full = self.inventory.bucket_full();
		self.input.tool_swap.advance(dt, self.hotbar.active_tool());
		let selected = self.hotbar.selected_item().and_then(|i| match i {
			// `HotbarItem::WaterTool` names the SLOT, not what is in it,
			// so the hand has to ask which tool is active — otherwise
			// every tool in the game is held as a bucket. `shown` rather
			// than `active_tool`, so the hand and the slot's icon change
			// over together — and a BARE slot (no vessel crafted yet)
			// holds nothing, so the hand is empty on it.
			HotbarItem::WaterTool => self
				.input
				.tool_swap
				.shown()
				.map(|t| crate::ui::hud::tool_icon(t, full)),
			other => Some(crate::ui::hud::item_icon(other, full).0),
		});
		self.vfx.view_swap.advance(dt, selected);
	}

	/// This frame's held item, if the hand has anything in it. The pose
	/// is authored in view space by `viewmodel.rs`, so all this does is
	/// pick the item and hand over the frame's light rig.
	pub(super) fn viewmodel_frame(
		&self,
		frame: &FrameUniforms,
	) -> Option<crate::viewmodel::ViewmodelFrame> {
		// The SWAP's item, not the hotbar's: mid-swap the hand is still
		// holding the one it hasn't put away yet.
		let kind = self.vfx.view_swap.shown()?;
		// The light rig is world-space; the item lives in view space, so
		// rotate the direction through the same view matrix the scene
		// used. Translation is irrelevant for a direction.
		let view = self.camera.view();
		let d = frame.light_dir;
		let light_dir_view = view.transform_vector3(Vec3::new(d[0], d[1], d[2]));
		Some(crate::viewmodel::ViewmodelFrame {
			kind,
			pose: [
				self.vfx.view_bob.pose(),
				self.vfx.view_swing.pose(),
				self.vfx.view_swap.pose(),
				self.vfx.view_cast.pose(),
			]
			.into_iter()
			.fold(Default::default(), crate::viewmodel::combine),
			light_dir_view,
			light_strength: d[3],
			light_color: [
				frame.light_color[0],
				frame.light_color[1],
				frame.light_color[2],
			],
		})
	}

	/// Camera eye below the VISIBLE water surface? (None = unloaded = dry.) The cell test alone
	/// flips at the lattice boundary, but the rendered surface of a resting source sits
	/// `WATER_SURFACE_DROP` below the cell top, so without the offset the underwater atmosphere
	/// pops in while the camera is still visibly above water. `FLIP_EARLY` adds a crossing margin:
	/// the near plane clips the surface pane first, and without it a crossing shades as neither.
	pub(super) fn eye_in_water(&self) -> bool {
		const SURFACE_DROP: f32 = digblock_core::player::WATER_SURFACE_DROP;
		const FLIP_EARLY: f32 = 0.1;
		let c = self.camera.pos;
		let (x, y, z) = (c.x.floor() as i32, c.y.floor() as i32, c.z.floor() as i32);
		if !self
			.pipeline
			.block_at(x, y, z)
			.is_some_and(|b| b.is_water())
		{
			return false;
		}
		// Water above too → fully submerged; otherwise this is the
		// surface cell and the drop (minus the crossing margin) applies.
		self.pipeline
			.block_at(x, y + 1, z)
			.is_some_and(|b| b.is_water())
			|| c.y - y as f32 <= 1.0 - SURFACE_DROP + FLIP_EARLY
	}

	/// Height of the water surface directly above the camera (top of the
	/// contiguous water column, minus the rendered drop), measured from the
	/// EYE: the shaders' path-length clamp, which an absolute height turns
	/// into a hundred blocks of water over every ray. Submerged only.
	pub(super) fn water_surface_above(&self) -> f32 {
		let c = self.camera.pos;
		let (x, z) = (c.x.floor() as i32, c.z.floor() as i32);
		let mut y = c.y.floor() as i32;
		let top = y + 256;
		while y < top
			&& self
				.pipeline
				.block_at(x, y + 1, z)
				.is_some_and(|b| b.is_water())
		{
			y += 1;
		}
		y as f32 + 1.0 - digblock_core::player::WATER_SURFACE_DROP - c.y
	}

	pub(super) fn frame_uniforms(&self, cam_fx: &crate::camera_fx::CamOffset) -> FrameUniforms {
		let aspect = self.ui.size.0 as f32 / self.ui.size.1.max(1) as f32;
		let vp = self.camera.view_proj_with(aspect, cam_fx);
		let caster = self.sun.caster();
		let atmos = digblock_core::sky::Atmosphere::sample(self.sun.elevation());
		// Directions toward the bodies in the sky (opposite the light).
		let sun = -self.sun.sun_dir();
		let moon = -self.sun.moon_dir();
		let cam = self.camera.pos;
		// The sky is the sky, submerged or not. Substituting a hand-tuned deep-water palette
		// for the atmosphere told the GI rays the sky was green — the attenuation belongs on
		// the PATH through the water (the sun DDA's transmittance today, M2's participating
		// medium for the view ray), never in the radiance of the source.
		let submerged = self.eye_in_water();
		let (horizon, zenith) = (atmos.horizon, atmos.zenith);
		FrameUniforms {
			view_proj: vp.to_cols_array_2d(),
			inv_view_proj: vp.inverse().to_cols_array_2d(),
			light_dir: [caster.dir.x, caster.dir.y, caster.dir.z, caster.strength],
			light_color: [
				atmos.light_color[0],
				atmos.light_color[1],
				atmos.light_color[2],
				// Animation clock (water scroll/foam) in the spare lane.
				self.time_secs,
			],
			clear_color: [horizon[0], horizon[1], horizon[2], 0.0],
			cam_pos: [cam.x, cam.y, cam.z, 0.0],
			sun_dir: [sun.x, sun.y, sun.z, atmos.star_intensity],
			moon_dir: [moon.x, moon.y, moon.z, atmos.glow_strength],
			zenith: [zenith[0], zenith[1], zenith[2], 0.0],
			horizon: [horizon[0], horizon[1], horizon[2], 0.0],
			// The medium's rim distance: where the STREAMED CYLINDER ends,
			// which is draw_dist chunks of CHUNK_SIZE blocks — NOT the
			// camera far plane, which is a larger multiple of draw_dist for
			// legacy reasons and therefore sits well beyond anywhere terrain
			// exists at all. Passing the far plane here (M2 did) put the rim
			// ramp outside the streamed cylinder entirely, so the
			// term never fired and the streaming edge was never covered.
			misc: [
				1.0,
				(self.config.draw_dist * digblock_core::chunk::CHUNK_SIZE as i32) as f32,
				// The cost-stub mask (DIG_STUB), exact in f32 well past the
				// bits it uses. Zero in every normal run.
				self.config.stub as f32,
				// The render:GI divisor chunk.wgsl's bilateral upsample
				// walks its 2x2 neighbourhood with.
				self.config.gi_scale_div as f32,
			],
			water_misc: [
				f32::from(submerged),
				f32::from(self.config.ssr),
				// Water-surface height above the camera: the shaders'
				// underwater path-length clamp (unused while dry).
				if submerged {
					self.water_surface_above()
				} else {
					0.0
				},
				0.0,
			],
			// The renderer's GiDraw fills these (quantized caster, cascade
			// dims, window side) via patch_frame each frame.
			gi_sun: [0.0; 4],
			gi0: [0.0; 4],
			gi1: [0.0; 4],
			gi_misc: [0.0; 4],
		}
	}
}
