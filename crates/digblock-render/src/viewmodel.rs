//! The first-person viewmodel: the item in your own hand.
//!
//! Two halves, as elsewhere in this crate. The pure half authors the item **in
//! view space** — the camera is the origin looking down −z, so there is no
//! camera basis to derive and the pose is a function of walk phase and swing
//! alone. It also carries its OWN projection, a fixed field and near plane, so
//! the item's on-screen size never moves when the world FOV or the window does,
//! and nothing this close can clip the world's near plane.
//!
//! The GPU half draws [`item_geom`] faces through the shared quad index pattern
//! in a pass of its own, over the finished frame with a freshly cleared depth
//! buffer. That is what makes the item immune to walking into a wall while still
//! occluding itself correctly — a staircase in hand is not convex, and back-face
//! culling alone would not sort it.

use glam::camera::rh::proj::directx::perspective;
use glam::{Mat4, Vec3};

use digblock_platform::Gpu;

use crate::item_geom;
use crate::ui::hud::IconKind;

// ---- pose -------------------------------------------------------------

/// Vertical field of view of the viewmodel's own projection, degrees.
/// Narrower than the world's so the item reads with less distortion at
/// this distance.
const FOV_Y_DEG: f32 = 55.0;
/// How far in front of the eye the item sits, world units.
const DIST: f32 = 1.0;
/// Item size across, world units. A cube turned to the rest yaw presents
/// its diagonal rather than a face, so its silhouette is the wider of the
/// two — at the field and distance above, that lands present without
/// being in the way.
const SIZE: f32 = 0.18;
/// Where the item rests, as a fraction of the frustum half-extents at
/// `DIST`. Fractions rather than absolute offsets: the hand should keep
/// its place in the corner on any window shape, and the item's own size
/// stays fixed so it doesn't grow on a wide monitor. The vertical rest
/// clears the hotbar, which is anchored to the bottom edge.
const REST_X_FRAC: f32 = 0.48;
const REST_Y_FRAC: f32 = -0.58;

/// Resting orientation. Yawing, then tipping toward the viewer, puts
/// the item's +x, +y and +z faces on screen — deliberately the
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

/// Idle breathing. A hand that is perfectly still reads as a decal
/// stuck to the screen, so at rest the item keeps a slow drift — but
/// this is a CLOCK, not a distance, because standing still is exactly
/// where the distance accumulator has nothing to say. It fades out as
/// the walk bob fades in, so the two never fight over the same axis.
const BREATH_SECS: f32 = 4.5;
/// Travel, as fractions of [`SIZE`]. A fraction of the bob's: this
/// should be noticed only when it stops.
const BREATH_Y: f32 = 0.055;
const BREATH_X: f32 = 0.022;
/// Peak breathing pitch, radians.
const BREATH_PITCH: f32 = 0.035;
/// The breath is a drift, not a second bob. Checked at compile time
/// because it is a relationship between two tunables, and the day
/// someone raises it past the walk's travel is the day the hand starts
/// twitching while you stand still.
const _: () = assert!(BREATH_Y * 3.0 < BOB_Y);

/// Swing travel: down-and-forward, plus a wrist rotation.
const SWING_Y: f32 = 0.42;
const SWING_Z: f32 = 0.30;
const SWING_PITCH: f32 = 0.9;
/// Seconds per swing arc — deliberately the length of a scoop, so one
/// bucketful is one stroke: the hand dips as the hold opens and the
/// water appears in it as the hand comes back up. Any shorter and the
/// arc ran out mid-hold and started a second one, which read as two
/// grabs for a single bucket. Pinned against the block table by
/// `the_swing_is_exactly_one_scoop_long` — it is a duration shared with
/// core, not a free tunable.
const SWING_SECS: f32 = 20.0 / 60.0;

/// Seconds for a whole slot swap, down and back up. Short: this sits
/// between the player and the block they meant to place.
const SWAP_SECS: f32 = 0.22;
/// How far the item drops, in units of [`SIZE`]. Enough to leave the
/// frame entirely at the rest position — the item CHANGES down there,
/// and a swap you can watch happen is worse than no animation at all.
const SWAP_DROP: f32 = 2.0;
/// Forward tip on the way down, radians.
const SWAP_PITCH: f32 = 0.5;

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

/// Idle breathing crossfaded with the walk bob. The bob is a
/// distance-driven phase with a speed-driven amplitude, so the hand
/// keeps time with the feet rather than with the clock — the same
/// reason [`crate::audio::Footsteps`] paces by distance. The breath is
/// the opposite by necessity, and takes over whatever amplitude the bob
/// gives up.
#[derive(Default)]
pub struct Bob {
	phase: f32,
	amp: f32,
	/// Seconds into the breathing cycle, wrapped — a free-running clock,
	/// never reset by starting or stopping, so easing back to rest lands
	/// mid-breath instead of restarting one.
	breath: f32,
}

impl Bob {
	/// Advance by one frame. `moved` = horizontal distance travelled,
	/// `dt` = its duration, `grounded` = the feet are on something.
	/// Airborne or standing still, the amplitude eases back to zero and
	/// the item settles into its breathing; the phase keeps its place so
	/// resuming a walk doesn't jump.
	///
	/// `grounded` is the same gate [`crate::audio::Footsteps`] uses, for
	/// the same reason: the bob is the hand keeping time with the feet,
	/// and feet that aren't touching anything take no steps. Sailing
	/// through the air — or swimming, which is also off the ground — is
	/// distance travelled that the walk did not do, and letting it feed
	/// the stride swung the hand at a full walking pace through a jump.
	pub fn advance(&mut self, moved: f32, dt: f32, grounded: bool) {
		if dt <= 0.0 {
			return;
		}
		// Off the ground there is no stride: the phase freezes where it
		// is (so landing resumes the swing rather than restarting it) and
		// the amplitude eases out through the same speed term as a stop.
		let moved = if grounded { moved } else { 0.0 };
		self.phase = (self.phase + moved / BOB_STRIDE).fract();
		self.breath = (self.breath + dt / BREATH_SECS).fract();
		let target = (moved / dt / BOB_FULL_SPEED).clamp(0.0, 1.0);
		let k = (BOB_ATTACK * dt).min(1.0);
		self.amp += (target - self.amp) * k;
	}

	/// This frame's contribution to the pose.
	pub fn pose(&self) -> Pose {
		let t = self.phase * std::f32::consts::TAU;
		let walk = Pose {
			// Lateral at the stride rate, vertical at twice it: one dip
			// per footfall, one sway per pair.
			offset: Vec3::new(BOB_X * t.sin(), -BOB_Y * (2.0 * t).cos().abs(), 0.0) * self.amp,
			pitch: 0.0,
			roll: BOB_ROLL * t.sin() * self.amp,
		};
		// The breath rides the amplitude the bob isn't using, so the sum
		// of the two envelopes is always one and neither can stack on top
		// of the other at a jog.
		let rest = 1.0 - self.amp;
		let b = self.breath * std::f32::consts::TAU;
		// A quarter-cycle apart: the drift traces a slow ellipse rather
		// than sliding up and down one line.
		let breath = Pose {
			offset: Vec3::new(BREATH_X * b.cos(), BREATH_Y * b.sin(), 0.0) * rest,
			pitch: BREATH_PITCH * b.sin() * rest,
			roll: 0.0,
		};
		combine(walk, breath)
	}

	/// World swap.
	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

/// The collect swing: exactly one arc per scoop, started when the hold
/// opens. Collecting water is a scoop, not a mining hold — it
/// deliberately does NOT draw the block-breaking crack decal, so this is
/// the only feedback that the tool is doing something.
///
/// Nothing here chains arcs off the button being down. It used to, and
/// with an arc shorter than the hold that always produced a second
/// stroke: the first ran out a few ticks before the scoop landed, a new
/// one began, and the bucket filled during it. The arc IS the scoop
/// ([`SWING_SECS`]), so the hold has room for one and only one.
#[derive(Default)]
pub struct Swing {
	/// Seconds into the current arc; `None` between arcs.
	t: Option<f32>,
}

impl Swing {
	/// Advance the arc in flight, if any. One arc always runs to
	/// completion, so releasing the button mid-swing returns the hand
	/// smoothly instead of snapping it back.
	pub fn advance(&mut self, dt: f32) {
		self.t = self.t.map(|t| t + dt).filter(|t| *t < SWING_SECS);
	}

	/// Kick off an arc now — a scoop hold just opened. Restarting an arc
	/// already in flight is correct and intended: the only way to get
	/// here mid-swing is a hold that retargeted, which restarted the
	/// scoop timer this arc is measuring.
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

/// The slot swap: the item drops out of frame, CHANGES at the bottom,
/// and the new one rises in its place. Owning `shown` is the whole
/// point — the drawn item lags the selection by half a swap, which is
/// what makes it a swap rather than a dip with a jump-cut in the middle.
pub struct Swap {
	/// Seconds into the current swap; at or past [`SWAP_SECS`] = at rest.
	t: f32,
	shown: Option<IconKind>,
}

impl Default for Swap {
	fn default() -> Self {
		Self {
			t: SWAP_SECS,
			shown: None,
		}
	}
}

/// Are these the same ITEM, ignoring any state it carries? Only the
/// water tool has state, and filling it is not a slot change: the hand
/// must not put the bucket away and bring the same bucket back out
/// because a scoop landed in it.
fn same_item(a: Option<IconKind>, b: Option<IconKind>) -> bool {
	match (a, b) {
		(Some(IconKind::WaterTool { .. }), Some(IconKind::WaterTool { .. })) => true,
		_ => a == b,
	}
}

impl Swap {
	/// Feed this frame's selection. Read the item to draw back out of
	/// [`Self::shown`].
	pub fn advance(&mut self, dt: f32, selected: Option<IconKind>) {
		// A state change on the item already in hand is adopted in place
		// — no dip — and adopting it first is what keeps the comparisons
		// below reading as "a different item".
		if same_item(selected, self.shown) {
			self.shown = selected;
		}
		let half = SWAP_SECS / 2.0;
		if self.t >= SWAP_SECS {
			// At rest. An empty hand has nothing to put away, so it
			// starts at the bottom and only rises — no pause staring at
			// nothing before your first block appears.
			if selected != self.shown {
				self.t = if self.shown.is_none() { half } else { 0.0 };
			}
		} else if self.t >= half && selected != self.shown {
			// Changed again while the new item was rising. Mirror back
			// onto the way down at the SAME height, so scrolling through
			// the bar reads as one continuous dip per slot instead of
			// snapping to the top between each.
			self.t = SWAP_SECS - self.t;
		}
		if self.t < SWAP_SECS {
			self.t = (self.t + dt).min(SWAP_SECS);
			// The exchange happens out of sight, at the bottom.
			if self.t >= half {
				self.shown = selected;
			}
		}
	}

	/// The item to draw — not necessarily the one that's selected.
	pub fn shown(&self) -> Option<IconKind> {
		self.shown
	}

	pub fn pose(&self) -> Pose {
		if self.t >= SWAP_SECS {
			return Pose::default();
		}
		// Down and back on a raised cosine: zero velocity at both ends
		// AND at the bottom, so the item eases out, hangs for the
		// exchange, and eases back rather than pinging.
		let a = (1.0 - (self.t / SWAP_SECS * std::f32::consts::TAU).cos()) / 2.0;
		Pose {
			offset: Vec3::new(0.0, -SWAP_DROP * a, 0.0),
			pitch: SWAP_PITCH * a,
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
/// `height` is the geometry's height in twelfths — a door's spans both
/// its cells — used to centre it, so a door hangs in the hand like a
/// cube does.
pub fn mvp(aspect: f32, height: i32, pose: Pose) -> Mat4 {
	let half_h = (FOV_Y_DEG.to_radians() / 2.0).tan() * DIST;
	let half_w = half_h * aspect.max(0.01);
	let rest = Vec3::new(REST_X_FRAC * half_w, REST_Y_FRAC * half_h, -DIST);
	let proj = perspective(FOV_Y_DEG.to_radians(), aspect.max(0.01), 0.01, 2.0);
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

/// One vertex, in order: position in twelfths, uv, texture layer, face
/// index into the shader's normal table.
///
/// Shared with `players.rs`, which draws the SAME geometry in a remote
/// player's hand — so your hand and theirs are one build rather than two
/// that happen to match.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Vert {
	pos: [f32; 3],
	uv: [f32; 2],
	layer: u32,
	face: u32,
}

/// Enough for the widest item (a door: two stacked shape variants) with
/// room to spare.
pub(crate) const MAX_QUADS: usize = 4 * digblock_core::shapes::MAX_SHAPE_QUADS + 32;

/// Build this frame's vertex list for `kind`. Pure — the corners come
/// straight from [`item_geom`], which orders them for
/// `mesher::quad_indices`.
pub(crate) fn build(kind: IconKind, out: &mut Vec<Vert>) {
	out.clear();
	for f in item_geom::faces(kind) {
		let face = f.dir as u32;
		// The ink bit rides above the layer index rather than taking an
		// attribute of its own: this vertex feeds TWO pipelines (your own
		// hand and a remote's), so a new attribute would be a format
		// change in both, and the layer word has every bit spare.
		let layer = u32::from(f.tex_layer) | (u32::from(f.emissive) << INK_BIT);
		for i in 0..4 {
			let c = f.corners[i];
			out.push(Vert {
				pos: [c[0] as f32, c[1] as f32, c[2] as f32],
				uv: f.uv[i],
				layer,
				face,
			});
		}
	}
}

/// Where [`ItemFace::emissive`](item_geom::ItemFace::emissive) rides in
/// the vertex's layer word, clear of every texture index there will ever
/// be. Both item shaders mask it back out before they sample; pinned
/// against the WGSL twins by `the_ink_bit_matches_the_shaders`.
pub(crate) const INK_BIT: u32 = 16;

/// Everything the viewmodel pass needs for one frame.
#[derive(Clone, Copy)]
pub struct ViewmodelFrame {
	pub kind: IconKind,
	pub pose: Pose,
	/// View-space light direction (toward the ground) and its strength
	/// in w, plus the frame's direct color — the same rig the scene
	/// uses, so the hand doesn't glow at midnight. Ambient comes from
	/// the RTGI camera-light buffer, not the CPU.
	pub light_dir_view: Vec3,
	pub light_strength: f32,
	pub light_color: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
	mvp: [[f32; 4]; 4],
	/// xyz = light direction in item space; w = strength.
	light: [f32; 4],
	/// rgb = the frame's direct color; w = 1 when the item is fullbright
	/// ink (see [`uniforms`]).
	light_color: [f32; 4],
}

/// The frame's uniform block.
///
/// Pure, and separate from `prepare`, for two reasons: a test can ask
/// what actually reaches the GPU without a device, and the emissive flag
/// is DERIVED here from the drawn item rather than plumbed through
/// `ViewmodelFrame` — so it is structurally impossible to draw a torch
/// and forget to light it.
fn uniforms(aspect: f32, f: &ViewmodelFrame) -> Uniforms {
	let light = light_in_item_space(f.light_dir_view, f.pose);
	let emissive = crate::ui::hud::icon_block(f.kind).is_some_and(|b| b.props().emissive);
	Uniforms {
		mvp: mvp(aspect, item_geom::height(f.kind), f.pose).to_cols_array_2d(),
		light: [light.x, light.y, light.z, f.light_strength],
		light_color: [
			f.light_color[0],
			f.light_color[1],
			f.light_color[2],
			f32::from(emissive),
		],
	}
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
		cam_light_buf: &wgpu::Buffer,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("viewmodel"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::VIEWMODEL.into()),
		});
		let uniform = |binding| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
			ty: wgpu::BindingType::Buffer {
				ty: wgpu::BufferBindingType::Uniform,
				has_dynamic_offset: false,
				min_binding_size: None,
			},
			count: None,
		};
		// Binding 1 is the RTGI camera-light buffer (gi_probe's
		// cam_light): the hand is at the camera, so that sample IS its
		// ambient + sun visibility.
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("viewmodel"),
			entries: &[uniform(0), uniform(1)],
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
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: uniform_buf.as_entire_binding(),
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: cam_light_buf.as_entire_binding(),
				},
			],
		});
		// Group 0 is our own transform + light rig; group 1 is the
		// shared block material array, exactly as the players pass uses
		// it for their held cubes.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("viewmodel"),
			bind_group_layouts: &[Some(&bgl), Some(materials_bgl)],
			immediate_size: 0,
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
			let u = uniforms(aspect, f);
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
mod tests;
