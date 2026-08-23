//! Teleport light: the cast channel's winding aura, the departure
//! collapse, and the arrival bloom — deliberately NOT confetti. Nothing
//! here falls or flutters; a jump is made of light, and it moves on a
//! helix (teleport.wgsl).
//!
//! The CPU owns each effect's origin, mode, age (or progress) and an
//! envelope; every mote's flight is a pure function of those and the
//! shared animation clock — [`crate::confetti`]'s ownership (effects are
//! ENGINE state a world swap forgets), [`crate::siphon`]'s additive light
//! and envelope. Nothing to save on suspend, nothing to rebuild on device
//! loss but the pipeline.

use digblock_platform::Gpu;

/// Simultaneous effects: the caster's own channel, a handful of bursts
/// (a departure and an arrival per jump, short-lived — [`LIFE`]), and
/// the kindled orbs within [`KINDLED_RADIUS`].
pub(crate) const MAX_FX: usize = 24;
/// How many of [`MAX_FX`] the standing orbs may claim, the rest being RESERVED for the channel
/// aura and the live bursts. `Teleport::prepare` truncates in FILL order and kindling has no
/// cooldown, so without a reserve a player lighting a mine shaft would cast with no channel aura —
/// the one thing on screen that says a channel is running. Orbs compete for their share by
/// DISTANCE ([`nearest_orbs`]), or one sixty blocks behind evicts one two blocks ahead.
pub(crate) const MAX_KINDLED_FX: usize = 12;
/// Beyond this many blocks a kindled orb is not uploaded at all — the
/// marker's rule; its LIGHT still reaches the GI list at any range.
pub(crate) const KINDLED_RADIUS: f32 = 64.0;
/// The kindled orb's near ramp, in blocks: nothing at all inside [`KINDLED_NEAR_DARK`], full
/// weight from [`KINDLED_NEAR_FULL`] out. A light stands in the cell its caster's EYE was in, so
/// their own orb sits a fraction of a block away, and a camera-facing additive quad that close
/// straddles the near plane and whites out the screen. The dark distance must cover the whole host
/// cell — `an_orb_in_your_own_cell_never_draws` pins it; the LIGHT reaches the room either way.
pub(crate) const KINDLED_NEAR_DARK: f32 = 1.0;
pub(crate) const KINDLED_NEAR_FULL: f32 = 2.0;
/// Motes per effect — keep in sync with teleport.wgsl `MOTES`, held by
/// [`the_teleport_mote_count_matches_the_shader`]. Particle 0 is the
/// column of light, so the draw call is one quad longer.
const MOTES: u32 = 48;
/// Seconds a burst lives — the shader's weight curves reach zero exactly
/// here, so dropping an effect at this age can never cut a visible mote.
pub(crate) const LIFE: f32 = 1.15;
/// Bytes per instance: origin + mode, t + envelope + seed + spare, tint.
const INSTANCE_BYTES: usize = 48;

/// The channel aura's fade — siphon's envelope shape: quick to open so a
/// cast reads at once, slow enough shutting that an interrupt dissolves
/// rather than pops.
pub(crate) const FADE_IN_SECS: f32 = 0.15;
pub(crate) const FADE_OUT_SECS: f32 = 0.35;

/// The effect's kind, packed into the instance's mode lane.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
	Departure,
	Arrival,
	Channel,
	/// A standing kindled orb; `t` is its age, the envelope its last seconds.
	Kindled,
}

/// One effect, in the flat `[origin.xyz, mode][t, envelope, seed, 0]
/// [tint.rgb, 0]` layout the vertex shader reads.
pub(crate) type FxInstance = [f32; 12];

/// The one colour Arcana casts in — the marker spiral's violet, warmed a
/// step toward white so a jump reads hotter than a standing site.
pub(crate) const TINT: [f32; 3] = [0.62, 0.55, 1.0];

/// Pack one effect. `t` is a burst's age in seconds or the channel's
/// progress (0..=1); `envelope` scales the whole effect (bursts pass 1).
pub(crate) fn instance(
	origin: [f32; 3],
	mode: Mode,
	t: f32,
	envelope: f32,
	seed: f32,
) -> FxInstance {
	let m = match mode {
		Mode::Departure => 0.0,
		Mode::Arrival => 1.0,
		Mode::Channel => 2.0,
		Mode::Kindled => 3.0,
	};
	[
		origin[0], origin[1], origin[2], m, t, envelope, seed, 0.0, TINT[0], TINT[1], TINT[2], 0.0,
	]
}

/// How much of a kindled orb draws at this distance from the camera —
/// the near ramp [`KINDLED_NEAR_DARK`] describes, folded into the
/// instance's envelope so the sprite dissolves as the camera closes on
/// it rather than popping at a boundary.
pub(crate) fn near_fade(dist: f32) -> f32 {
	((dist - KINDLED_NEAR_DARK) / (KINDLED_NEAR_FULL - KINDLED_NEAR_DARK)).clamp(0.0, 1.0)
}

/// The indices of the [`MAX_KINDLED_FX`] nearest orbs, nearest first, out of one distance per
/// standing light, in list order. A fixed-size insertion rather than a sort: the standing-light
/// list is world state and belongs to nobody's camera, so selecting from it must not reorder it,
/// and a frame must not allocate to draw. Ties keep list order, which makes the pick pure.
pub(crate) fn nearest_orbs(d2: impl Iterator<Item = f32>) -> ([u32; MAX_KINDLED_FX], usize) {
	let mut key = [f32::INFINITY; MAX_KINDLED_FX];
	let mut idx = [0u32; MAX_KINDLED_FX];
	let mut n = 0usize;
	for (i, d) in d2.enumerate() {
		if n == MAX_KINDLED_FX && d >= key[n - 1] {
			continue;
		}
		let at = key[..n].iter().position(|&k| d < k).unwrap_or(n);
		let end = n.min(MAX_KINDLED_FX - 1);
		key.copy_within(at..end, at + 1);
		idx.copy_within(at..end, at + 1);
		key[at] = d;
		idx[at] = i as u32;
		n = (n + 1).min(MAX_KINDLED_FX);
	}
	(idx, n)
}

pub(crate) struct Teleport {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
}

impl Teleport {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("teleport"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::TELEPORT.into()),
		});
		// No camera-light group, siphon's reason: arcane light is its own
		// light, and a mote lit by the room would go out in a night jump.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("teleport"),
			bind_group_layouts: &[Some(frame_bgl)],
			immediate_size: 0,
		});
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "teleport",
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
						wgpu::VertexAttribute {
							format: wgpu::VertexFormat::Float32x4,
							offset: 32,
							shader_location: 2,
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
			label: Some("teleport instances"),
			size: (MAX_FX * INSTANCE_BYTES) as u64,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
		}
	}

	/// Upload this frame's live effects (engine-owned). Truncation is by
	/// FILL order, which is why the caller fills by priority — see
	/// [`MAX_KINDLED_FX`]. Returns the instance count.
	pub(crate) fn prepare(&self, gpu: &Gpu, fx: &[FxInstance]) -> u32 {
		let n = fx.len().min(MAX_FX);
		if n > 0 {
			gpu.queue
				.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&fx[..n]));
		}
		n as u32
	}

	/// Draw in the water pass beside siphon (frame bind group already
	/// bound at 0). Returns triangles.
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

	/// The count that sizes the draw call lives on both sides of the
	/// CPU/GPU boundary — siphon's pin, same reason.
	#[test]
	fn the_teleport_mote_count_matches_the_shader() {
		let decl = format!("const MOTES: u32 = {MOTES}u;");
		assert!(
			digblock_assets::shader::TELEPORT.contains(&decl),
			"teleport.wgsl must declare `{decl}`"
		);
	}

	/// A burst outlives nothing: the shader's weight curves are authored
	/// to reach zero AT `LIFE`, and the wgsl must agree on the number.
	#[test]
	fn the_burst_life_matches_the_shader() {
		let decl = format!("const LIFE: f32 = {LIFE};");
		assert!(
			digblock_assets::shader::TELEPORT.contains(&decl),
			"teleport.wgsl must declare `{decl}`"
		);
	}

	/// The caster's own orb never draws. Their light stands in the cell their EYE occupies, and the
	/// farthest an eye can be from that cell's centre is half a cell's diagonal — derived here
	/// rather than written down, so a wider ramp is free and a narrower one fails. Inside that,
	/// the billboard straddles the near plane and covers the screen.
	#[test]
	fn an_orb_in_your_own_cell_never_draws() {
		let half_diagonal = glam::Vec3::splat(0.5).length();
		assert_eq!(near_fade(half_diagonal), 0.0, "the far corner of the cell");
		assert_eq!(near_fade(0.0), 0.0, "and standing on it");
		// Outside, it opens up monotonically and reaches full weight.
		let mut prev = 0.0;
		let mut steps = 0;
		let mut d = 0.0;
		while d < KINDLED_NEAR_FULL * 2.0 {
			let f = near_fade(d);
			assert!(f >= prev, "the ramp went back down at {d}");
			assert!((0.0..=1.0).contains(&f), "{f} at {d}");
			prev = f;
			d += 0.05;
			steps += 1;
		}
		assert!(steps > 10, "the sweep proved nothing");
		assert_eq!(near_fade(KINDLED_NEAR_FULL), 1.0);
		assert_eq!(near_fade(KINDLED_RADIUS), 1.0, "and stays there");
	}

	/// Orbs claim their own share of the list and claim it by DISTANCE,
	/// so a mine shaft's worth of standing lights can neither evict the
	/// channel aura nor let a far orb push out a near one.
	#[test]
	fn the_orb_budget_keeps_the_nearest_and_leaves_room() {
		const {
			assert!(
				MAX_KINDLED_FX < MAX_FX,
				"the reserve has to leave something behind"
			)
		};
		// Farthest first, so a pick that ignored distance would keep
		// exactly the wrong ones.
		let n = MAX_KINDLED_FX * 3;
		let d2: Vec<f32> = (0..n).map(|i| (n - i) as f32).collect();
		let (idx, kept) = nearest_orbs(d2.iter().copied());
		assert_eq!(kept, MAX_KINDLED_FX, "the share is spent");
		for w in idx[..kept].windows(2) {
			assert!(
				d2[w[0] as usize] <= d2[w[1] as usize],
				"the pick is not nearest-first"
			);
		}
		let worst = d2[idx[kept - 1] as usize];
		let dropped = (0..n).filter(|i| !idx[..kept].contains(&(*i as u32)));
		for i in dropped {
			assert!(d2[i] >= worst, "a nearer orb was dropped for a farther");
		}
		// Fewer than the share: everything draws, still nearest-first.
		let few = [4.0f32, 1.0, 9.0];
		let (idx, kept) = nearest_orbs(few.iter().copied());
		assert_eq!(kept, few.len());
		assert_eq!(&idx[..kept], &[1, 0, 2]);
		// And nothing standing is nothing picked.
		assert_eq!(nearest_orbs(std::iter::empty()).1, 0);
	}

	/// The lanes the vertex shader reads by name.
	#[test]
	fn an_effect_packs_where_the_shader_looks() {
		let i = instance([1.0, 2.0, 3.0], Mode::Channel, 0.7, 0.9, 5.0);
		assert_eq!(&i[0..3], &[1.0, 2.0, 3.0], "origin");
		assert_eq!(i[3], 2.0, "mode rides the origin's spare lane");
		assert_eq!(i[4], 0.7, "t");
		assert_eq!(i[5], 0.9, "envelope");
		assert_eq!(i[6], 5.0, "seed");
		assert_eq!(&i[8..11], &TINT, "tint");
	}
}
