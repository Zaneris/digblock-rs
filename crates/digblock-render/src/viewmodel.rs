//! The first-person viewmodel: the item in your own hand. Remote
//! players have carried a held-item cube since protocol v10; this is
//! the same idea for the player who can't see themselves.
//!
//! Two halves, as elsewhere in this crate. The pure half authors the
//! item **in view space** — the camera is the origin looking down −z,
//! so there is no camera basis to derive, no yaw/pitch to reconstruct,
//! and the pose is a function of walk phase and swing alone. It also
//! carries its OWN projection (a fixed 55° field, near 0.01) so the
//! item's on-screen size never moves when the world FOV or the window
//! does, and so nothing this close can clip the world's near plane.
//!
//! The GPU half draws [`item_geom`] faces through the shared quad index
//! pattern in a pass of its own, over the finished frame with a freshly
//! cleared depth buffer. That is what makes the item immune to walking
//! into a wall while still occluding itself correctly — a staircase in
//! hand is not convex, and back-face culling alone would not sort it.

use glam::{Mat4, Vec3};

use digblock_platform::Gpu;

use crate::item_geom;
use crate::ui::hud::IconKind;

// ---- pose -------------------------------------------------------------

/// Vertical field of view of the viewmodel's own projection, degrees.
/// Narrower than the world's 75° so the item reads with less distortion
/// at this distance.
const FOV_Y_DEG: f32 = 55.0;
/// How far in front of the eye the item sits, world units.
const DIST: f32 = 1.0;
/// Item size across, world units. A cube turned 45° presents its
/// diagonal, so its silhouette is ~√2 of this — at the field and
/// distance above that lands around a quarter of the viewport height,
/// which is present without being in the way.
const SIZE: f32 = 0.18;
/// Where the item rests, as a fraction of the frustum half-extents at
/// `DIST`. Fractions rather than absolute offsets: the hand should keep
/// its place in the corner on any window shape, and the item's own size
/// stays fixed so it doesn't grow on a wide monitor. The vertical rest
/// clears the hotbar, which is anchored to the bottom edge.
const REST_X_FRAC: f32 = 0.58;
const REST_Y_FRAC: f32 = -0.48;

/// Resting orientation. Yawing −45° then tipping 20° toward the viewer
/// puts the item's +x, +y and +z faces on screen — deliberately the
/// same three the inventory icon shows, so an item looks like itself in
/// the hand and in the bar.
const REST_YAW: f32 = -std::f32::consts::FRAC_PI_4;
const REST_PITCH: f32 = 0.349; // 20°

/// Walk-bob travel, as fractions of [`SIZE`].
const BOB_X: f32 = 0.30;
const BOB_Y: f32 = 0.22;
/// Peak roll of the bob, radians.
const BOB_ROLL: f32 = 0.10;
/// Horizontal distance per bob cycle, blocks. One cycle is a two-step
/// stride, so this is twice the footstep cadence and the item swings
/// once per pair of footfalls rather than jittering on each.
const BOB_STRIDE: f32 = 2.2;
/// Speed at which the bob reaches full travel, blocks/s (the walk
/// speed) — creeping forward barely moves the hand.
const BOB_FULL_SPEED: f32 = 4.0;
/// How fast the bob amplitude follows the speed, per second. Slow
/// enough that a single dropped frame can't snap the item.
const BOB_ATTACK: f32 = 6.0;

/// Swing travel: down-and-forward, plus a wrist rotation.
const SWING_Y: f32 = 0.42;
const SWING_Z: f32 = 0.30;
const SWING_PITCH: f32 = 0.9;
/// Seconds per swing arc.
const SWING_SECS: f32 = 0.30;

/// The item's placement this frame, in view space, relative to rest.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
	/// Offset from the rest position, in units of [`SIZE`]: +x right,
	/// +y up, +z toward the viewer.
	pub offset: Vec3,
	/// Extra rotation about the view-space x axis (a wrist flick).
	pub pitch: f32,
	/// Extra rotation about the view-space z axis (the walk's weight
	/// shift).
	pub roll: f32,
}

/// Walk bob: a distance-driven phase with a speed-driven amplitude, so
/// the hand keeps time with the feet rather than with the clock — the
/// same reason [`crate::audio::Footsteps`] paces by distance.
#[derive(Default)]
pub struct Bob {
	phase: f32,
	amp: f32,
}

impl Bob {
	/// Advance by one frame. `moved` = horizontal distance travelled,
	/// `dt` = its duration. Airborne or standing still, the amplitude
	/// eases back to zero and the item settles; the phase keeps its
	/// place so resuming a walk doesn't jump.
	pub fn advance(&mut self, moved: f32, dt: f32) {
		if dt <= 0.0 {
			return;
		}
		self.phase = (self.phase + moved / BOB_STRIDE).fract();
		let target = (moved / dt / BOB_FULL_SPEED).clamp(0.0, 1.0);
		let k = (BOB_ATTACK * dt).min(1.0);
		self.amp += (target - self.amp) * k;
	}

	/// This frame's contribution to the pose.
	pub fn pose(&self) -> Pose {
		let t = self.phase * std::f32::consts::TAU;
		Pose {
			// Lateral at the stride rate, vertical at twice it: one dip
			// per footfall, one sway per pair.
			offset: Vec3::new(BOB_X * t.sin(), -BOB_Y * (2.0 * t).cos().abs(), 0.0) * self.amp,
			pitch: 0.0,
			roll: BOB_ROLL * t.sin() * self.amp,
		}
	}

	/// World swap.
	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

/// The collect swing: one arc, restarted while the hold continues.
/// Collecting water is a scoop, not a mining hold — it deliberately
/// does NOT draw the block-breaking crack decal, so this is the only
/// feedback that the tool is doing something.
#[derive(Default)]
pub struct Swing {
	/// Seconds into the current arc; `None` between arcs.
	t: Option<f32>,
}

impl Swing {
	/// Advance, restarting the arc while `active`. One arc always runs
	/// to completion, so releasing the button mid-swing returns the
	/// hand smoothly instead of snapping it back.
	pub fn advance(&mut self, dt: f32, active: bool) {
		self.t = match self.t {
			// Mid-arc: run it out, whatever the button is doing.
			Some(t) if t + dt < SWING_SECS => Some(t + dt),
			// Between arcs: another one only while the hold continues.
			_ => active.then_some(0.0),
		};
	}

	/// Kick off an arc now (a completed scoop, so the last one lands
	/// even if the button came up on the same frame).
	pub fn strike(&mut self) {
		self.t = Some(0.0);
	}

	pub fn pose(&self) -> Pose {
		let Some(t) = self.t else {
			return Pose::default();
		};
		// Out and back on a half sine: no discontinuity at either end,
		// so arcs can chain while the button is held.
		let a = (t / SWING_SECS * std::f32::consts::PI).sin();
		Pose {
			offset: Vec3::new(0.0, -SWING_Y * a, SWING_Z * a),
			pitch: SWING_PITCH * a,
			roll: 0.0,
		}
	}

	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

/// Sum of the poses in play this frame.
pub fn combine(a: Pose, b: Pose) -> Pose {
	Pose {
		offset: a.offset + b.offset,
		pitch: a.pitch + b.pitch,
		roll: a.roll + b.roll,
	}
}

/// The full transform for the viewmodel: item twelfths → clip space.
/// `height` is the geometry's height in twelfths (24 for a door), used
/// to centre it, so a door hangs in the hand like a cube does.
pub fn mvp(aspect: f32, height: i32, pose: Pose) -> Mat4 {
	let half_h = (FOV_Y_DEG.to_radians() / 2.0).tan() * DIST;
	let half_w = half_h * aspect.max(0.01);
	let rest = Vec3::new(REST_X_FRAC * half_w, REST_Y_FRAC * half_h, -DIST);
	let proj = Mat4::perspective_rh(FOV_Y_DEG.to_radians(), aspect.max(0.01), 0.01, 2.0);
	// Right to left: centre the twelfths on the origin, shrink to
	// SIZE, turn to the resting orientation, apply this frame's flick,
	// then park it in the corner.
	let model = Mat4::from_translation(rest + pose.offset * SIZE)
		* Mat4::from_rotation_z(pose.roll)
		* Mat4::from_rotation_x(REST_PITCH + pose.pitch)
		* Mat4::from_rotation_y(REST_YAW)
		* Mat4::from_scale(Vec3::splat(SIZE / 12.0))
		* Mat4::from_translation(Vec3::new(-6.0, -height as f32 / 2.0, -6.0));
	proj * model
}

/// The light direction rotated OUT of view space and into the item's
/// own frame, so the shader can shade against the raw face normals it
/// already has — no normal matrix, and the item darkens with the sun
/// like everything else.
pub fn light_in_item_space(light_dir_view: Vec3, pose: Pose) -> Vec3 {
	let rot = Mat4::from_rotation_z(pose.roll)
		* Mat4::from_rotation_x(REST_PITCH + pose.pitch)
		* Mat4::from_rotation_y(REST_YAW);
	// Orthonormal, so the inverse is the transpose.
	rot.transpose().transform_vector3(light_dir_view)
}

// ---- geometry ---------------------------------------------------------

/// One vertex: position in twelfths, uv, texture layer, face index into
/// the shader's normal table. 24 B.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vert {
	pos: [f32; 3],
	uv: [f32; 2],
	layer: u32,
	face: u32,
}

/// Enough for the widest item (a door: two stacked shape variants) with
/// room to spare.
const MAX_QUADS: usize = 4 * digblock_core::shapes::MAX_SHAPE_QUADS + 32;

/// Build this frame's vertex list for `kind`. Pure — the corners come
/// straight from [`item_geom`], which orders them for
/// `mesher::quad_indices`.
fn build(kind: IconKind, out: &mut Vec<Vert>) {
	out.clear();
	for f in item_geom::faces(kind) {
		let face = f.dir as u32;
		for i in 0..4 {
			let c = f.corners[i];
			out.push(Vert {
				pos: [c[0] as f32, c[1] as f32, c[2] as f32],
				uv: f.uv[i],
				layer: u32::from(f.tex_layer),
				face,
			});
		}
	}
}

/// Everything the viewmodel pass needs for one frame.
#[derive(Clone, Copy)]
pub struct ViewmodelFrame {
	pub kind: IconKind,
	pub pose: Pose,
	/// View-space light direction (toward the ground) and its strength
	/// in w, plus the frame's ambient and direct colors — the same rig
	/// the scene uses, so the hand doesn't glow at midnight.
	pub light_dir_view: Vec3,
	pub light_strength: f32,
	pub ambient: [f32; 3],
	pub light_color: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
	mvp: [[f32; 4]; 4],
	/// xyz = light direction in item space; w = strength.
	light: [f32; 4],
	ambient: [f32; 4],
	light_color: [f32; 4],
}

// ---- draw -------------------------------------------------------------

pub struct ViewmodelDraw {
	pipeline: wgpu::RenderPipeline,
	uniform_buf: wgpu::Buffer,
	bind_group: wgpu::BindGroup,
	vertex_buf: wgpu::Buffer,
	scratch: Vec<Vert>,
	quads: u32,
}

impl ViewmodelDraw {
	pub fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		materials_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("viewmodel"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::VIEWMODEL.into()),
		});
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("viewmodel"),
			entries: &[wgpu::BindGroupLayoutEntry {
				binding: 0,
				visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
				ty: wgpu::BindingType::Buffer {
					ty: wgpu::BufferBindingType::Uniform,
					has_dynamic_offset: false,
					min_binding_size: None,
				},
				count: None,
			}],
		});
		let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("viewmodel uniforms"),
			size: std::mem::size_of::<Uniforms>() as u64,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("viewmodel"),
			layout: &bgl,
			entries: &[wgpu::BindGroupEntry {
				binding: 0,
				resource: uniform_buf.as_entire_binding(),
			}],
		});
		// Group 0 is our own transform + light rig; group 1 is the
		// shared block material array, exactly as the players pass uses
		// it for their held cubes.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("viewmodel"),
			bind_group_layouts: &[&bgl, materials_bgl],
			push_constant_ranges: &[],
		});
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "viewmodel",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: std::mem::size_of::<Vert>() as u64,
					step_mode: wgpu::VertexStepMode::Vertex,
					attributes: &[
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x3,
							offset: 0,
							shader_location: 0,
						},
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x2,
							offset: 12,
							shader_location: 1,
						},
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Uint32,
							offset: 20,
							shader_location: 2,
						},
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Uint32,
							offset: 24,
							shader_location: 3,
						},
					],
				}],
				// The item is a handful of boxes, and its pass owns a
				// cleared depth buffer: cull the backs, sort the rest
				// with depth like any other solid.
				cull: Some(wgpu::Face::Back),
				depth_write: true,
				depth_compare: wgpu::CompareFunction::Less,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);
		let vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("viewmodel vertices"),
			size: (MAX_QUADS * 4 * std::mem::size_of::<Vert>()) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			uniform_buf,
			bind_group,
			vertex_buf,
			scratch: Vec::with_capacity(MAX_QUADS * 4),
			quads: 0,
		}
	}

	/// Rebuild and upload this frame's item. `None` (empty hand, a
	/// screen open, the menu up) skips the pass entirely.
	pub fn prepare(&mut self, gpu: &Gpu, aspect: f32, frame: Option<&ViewmodelFrame>) {
		let Some(f) = frame else {
			self.quads = 0;
			return;
		};
		let mut scratch = std::mem::take(&mut self.scratch);
		build(f.kind, &mut scratch);
		let quads = (scratch.len() / 4).min(MAX_QUADS);
		self.quads = quads as u32;
		if quads > 0 {
			gpu.queue.write_buffer(
				&self.vertex_buf,
				0,
				bytemuck::cast_slice(&scratch[..quads * 4]),
			);
			let light = light_in_item_space(f.light_dir_view, f.pose);
			let u = Uniforms {
				mvp: mvp(aspect, item_geom::height(f.kind), f.pose).to_cols_array_2d(),
				light: [light.x, light.y, light.z, f.light_strength],
				ambient: [f.ambient[0], f.ambient[1], f.ambient[2], 0.0],
				light_color: [f.light_color[0], f.light_color[1], f.light_color[2], 0.0],
			};
			gpu.queue
				.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));
		}
		self.scratch = scratch;
	}

	pub fn is_empty(&self) -> bool {
		self.quads == 0
	}

	/// Draw into the viewmodel pass. `index_buf` is the shared quad
	/// index buffer — the same [3,1,0,3,0,2] pattern every chunk uses,
	/// which is why [`item_geom`] orders its corners the mesher's way.
	pub fn draw(
		&self,
		pass: &mut wgpu::RenderPass<'_>,
		index_buf: &wgpu::Buffer,
		materials_bg: &wgpu::BindGroup,
	) -> u32 {
		if self.quads == 0 {
			return 0;
		}
		pass.set_pipeline(&self.pipeline);
		pass.set_bind_group(0, &self.bind_group, &[]);
		pass.set_bind_group(1, materials_bg, &[]);
		pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
		pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
		pass.draw_indexed(0..self.quads * 6, 0, 0..1);
		self.quads * 2
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::block::BlockType;

	const ASPECT: f32 = 16.0 / 9.0;

	/// Project an item-space point (twelfths) to normalized device
	/// coordinates, so the tests can ask where the thing actually is.
	fn ndc(aspect: f32, height: i32, pose: Pose, p: [f32; 3]) -> [f32; 3] {
		let c = mvp(aspect, height, pose) * glam::Vec4::new(p[0], p[1], p[2], 1.0);
		[c.x / c.w, c.y / c.w, c.z / c.w]
	}

	fn centre(aspect: f32, height: i32, pose: Pose) -> [f32; 3] {
		ndc(aspect, height, pose, [6.0, height as f32 / 2.0, 6.0])
	}

	#[test]
	fn the_item_rests_in_the_lower_right_and_stays_on_screen() {
		for aspect in [1.0f32, 4.0 / 3.0, 16.0 / 9.0, 21.0 / 9.0] {
			for height in [12, 24] {
				let c = centre(aspect, height, Pose::default());
				assert!(c[0] > 0.1 && c[0] < 0.95, "x {} at {aspect}", c[0]);
				assert!(c[1] < -0.1 && c[1] > -0.95, "y {} at {aspect}", c[1]);
				assert!(c[2] > 0.0 && c[2] < 1.0, "in front of the eye: {}", c[2]);
			}
		}
	}

	/// The whole point of the item's own projection: the world's FOV,
	/// the window shape and the near plane are all somebody else's
	/// problem. Its on-screen HEIGHT must not move with the aspect.
	#[test]
	fn on_screen_size_is_independent_of_the_window_shape() {
		let span = |aspect: f32| {
			let lo = ndc(aspect, 12, Pose::default(), [6.0, 0.0, 6.0]);
			let hi = ndc(aspect, 12, Pose::default(), [6.0, 12.0, 6.0]);
			(hi[1] - lo[1]).abs()
		};
		let want = span(16.0 / 9.0);
		for aspect in [1.0f32, 4.0 / 3.0, 21.0 / 9.0] {
			assert!(
				(span(aspect) - want).abs() < 1e-4,
				"{aspect}: {} vs {want}",
				span(aspect)
			);
		}
	}

	/// A door is twice as tall as a cube; both hang centred on the same
	/// spot rather than a door sinking out of frame.
	#[test]
	fn tall_items_stay_centred_on_the_rest_position() {
		let cube = centre(ASPECT, 12, Pose::default());
		let door = centre(ASPECT, 24, Pose::default());
		for i in 0..3 {
			assert!((cube[i] - door[i]).abs() < 1e-5, "axis {i}");
		}
	}

	/// Icon and hand must agree about which way an item faces: both
	/// show +x, +y and +z. Checked through the transform rather than by
	/// asserting the constants back at themselves.
	#[test]
	fn the_hand_shows_the_same_three_faces_as_the_icon() {
		let m = mvp(ASPECT, 12, Pose::default());
		let origin = m * glam::Vec4::new(6.0, 6.0, 6.0, 1.0);
		let o = glam::Vec3::new(origin.x, origin.y, origin.z) / origin.w;
		for axis in [
			glam::Vec3::X * 6.0,
			glam::Vec3::Y * 6.0,
			glam::Vec3::Z * 6.0,
		] {
			let p = m * (glam::Vec4::from((glam::Vec3::new(6.0, 6.0, 6.0) + axis, 1.0)));
			let p = glam::Vec3::new(p.x, p.y, p.z) / p.w;
			assert!(
				p.z < o.z,
				"{axis:?} should come toward the viewer ({} vs {})",
				p.z,
				o.z
			);
		}
	}

	#[test]
	fn the_bob_settles_when_you_stop_and_never_leaves_its_box() {
		let mut bob = Bob::default();
		let dt = 1.0 / 60.0;
		// Walk for a second: the amplitude comes up and the pose moves.
		for _ in 0..60 {
			bob.advance(4.0 * dt, dt);
		}
		let walking = bob.pose();
		assert!(bob.amp > 0.9, "full travel at walk speed: {}", bob.amp);
		// Bounded travel, always — a bob that can grow is a bob that
		// eventually posts the item off-screen.
		for _ in 0..600 {
			bob.advance(4.0 * dt, dt);
			let p = bob.pose();
			assert!(p.offset.x.abs() <= BOB_X + 1e-5);
			assert!(p.offset.y.abs() <= BOB_Y + 1e-5);
			assert!(p.roll.abs() <= BOB_ROLL + 1e-5);
		}
		// Standing still: it eases back to rest, and stays there.
		for _ in 0..180 {
			bob.advance(0.0, dt);
		}
		let still = bob.pose();
		assert!(bob.amp < 0.01, "settled: {}", bob.amp);
		assert!(still.offset.length() < 0.01);
		assert!(walking.offset.length() > 0.05, "it did move while walking");
	}

	#[test]
	fn a_swing_runs_to_completion_and_repeats_while_held() {
		let mut s = Swing::default();
		let dt = 1.0 / 60.0;
		assert_eq!(s.pose(), Pose::default(), "idle hands are still");
		// Held: arcs chain, and the pose never rests mid-hold.
		let mut peak: f32 = 0.0;
		for _ in 0..120 {
			s.advance(dt, true);
			peak = peak.max(s.pose().offset.y.abs());
		}
		assert!(peak > SWING_Y * 0.9, "the arc reaches its travel: {peak}");
		// Released mid-arc: the current one still finishes, then stops.
		s.strike();
		s.advance(dt, false);
		assert!(s.pose() != Pose::default(), "mid-arc, still swinging");
		for _ in 0..60 {
			s.advance(dt, false);
		}
		assert_eq!(s.pose(), Pose::default(), "and then it rests");
	}

	/// Every item must fit the buffer the pass allocates — a door is
	/// the worst case, and silently truncating one would drop faces.
	#[test]
	fn every_item_fits_the_vertex_buffer() {
		let mut v = Vec::new();
		for kind in [
			IconKind::Cube(BlockType::Dirt),
			IconKind::Shaped(BlockType::WoodStairs),
			IconKind::Shaped(BlockType::WoodSlab),
			IconKind::Shaped(BlockType::WoodDoor),
			IconKind::WaterTool,
		] {
			build(kind, &mut v);
			assert_eq!(v.len() % 4, 0, "{kind:?} emits whole quads");
			assert!(v.len() / 4 <= MAX_QUADS, "{kind:?}: {} quads", v.len() / 4);
			assert!(!v.is_empty(), "{kind:?} drew nothing");
		}
	}

	/// The shading rig has to survive the trip into item space: a light
	/// coming from above the camera must still read as "from above" in
	/// whatever frame the shader dots it against.
	#[test]
	fn the_light_direction_keeps_its_length_through_the_rotation() {
		let d = Vec3::new(0.3, -0.9, 0.2).normalize();
		for pose in [
			Pose::default(),
			Pose {
				offset: Vec3::ZERO,
				pitch: 0.5,
				roll: -0.2,
			},
		] {
			let l = light_in_item_space(d, pose);
			assert!((l.length() - 1.0).abs() < 1e-5, "{}", l.length());
		}
	}
}
