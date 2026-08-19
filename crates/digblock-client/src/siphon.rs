//! The essence draw: what the Essence Siphon looks like while it works. Motes
//! of the vein's own colour lift off the rock face and stream into the tool,
//! over a kindle on the face they leave from.
//!
//! It exists because a field draw is the one action in the game that takes a
//! block apart without breaking it, so the feedback every other hold uses says
//! the wrong thing: the dig crack is a web of fractures spreading across rock
//! about to fall, and a drawn seam leaves the mountain standing.
//! `Engine::dig_progress` declines to draw one here; this is what it declines in
//! favour of.
//!
//! The CPU owns one instance and an envelope; every mote's flight is a pure
//! function of that instance and the shared animation clock, in siphon.wgsl —
//! [`crate::spray`]'s arrangement and [`crate::confetti`]'s packing. There is no
//! particle state to lose on suspend and nothing to rebuild on device loss but
//! the pipeline.
//!
//! The motes converge on `digblock_core::gi::carried_origin` — the point a light
//! in a hand radiates from — handed over by the engine rather than recomputed,
//! so the stream ends exactly where the siphon's own green glow hangs. Deriving
//! the drop here would put a second copy of a core constant on the far side of
//! the CPU/GPU boundary.
//!
//! One documented v1 limit: this is YOUR draw only. A snapshot carries a
//! remote's held item but nothing about a hold, so watching somebody else work a
//! seam shows their glowing tool and no stream. Fixing it is a protocol field
//! for a cosmetic.

use digblock_platform::Gpu;

/// Motes per draw — keep in sync with siphon.wgsl `MOTES`, which is what
/// [`the_mote_count_matches_the_shader`] holds. Particle 0 is the face
/// kindle, so the draw call is one quad longer than this.
const MOTES: u32 = 40;
/// Bytes per instance: source + progress, destination + envelope, tint +
/// face code.
const INSTANCE_BYTES: wgpu::BufferAddress = 48;

/// Seconds the stream takes to reach full strength when a draw starts,
/// and to die away once one stops. Short enough that a tap on the button
/// reads as a tap; long enough that neither end is a cut.
pub(crate) const FADE_IN_SECS: f32 = 0.18;
pub(crate) const FADE_OUT_SECS: f32 = 0.42;

/// What the envelope is set to the instant a draw COMPLETES.
///
/// One number covers both endings, which is why it is expressed this way
/// rather than as a separate burst: a released hold begins its fade from
/// wherever it had got to, and a completed one begins from here, so a
/// finished extraction flares and then dies on the ordinary fade-out
/// instead of blinking off the frame the cell turns spent. Above 1 on
/// purpose — the envelope multiplies the motes' colour, so the flare is
/// allowed to clip.
pub(crate) const FLARE: f32 = 2.4;

/// Advance the envelope by `dt`, with `live` saying whether the siphon is
/// working a vein this frame.
///
/// Three behaviours, and the middle one is why this is a function rather
/// than two lines at the call site: a flare still running when the NEXT
/// draw begins settles back to full instead of being held there. It
/// belongs to the vein that earned it, and carrying it forward would hand
/// the next one a brightness it never paid for.
pub(crate) fn envelope_step(current: f32, live: bool, dt: f32) -> f32 {
	if live && current <= 1.0 {
		(current + dt / FADE_IN_SECS).min(1.0)
	} else {
		// Live, a flare settles no further than full; dead, it runs all
		// the way out and the caller drops the draw at the bottom.
		(current - dt / FADE_OUT_SECS).max(f32::from(u8::from(live)))
	}
}

/// One live draw, in the flat layout the vertex shader reads:
/// `[source.xyz, progress][dest.xyz, envelope][tint.rgb, face code]`.
pub(crate) type DrawInstance = [f32; 12];

/// Which face of the cell a draw comes out of, as `axis * 2 + positive`
/// — decoded by siphon.wgsl's `face_normal`, its twin.
///
/// A code rather than a vector because the instance has exactly one lane
/// spare and a raycast normal only ever holds one of six values. A
/// normal that is somehow not an axis falls to -z, which is a visible
/// wrong face rather than a division by zero in the shader's basis.
fn face_code(normal: (i32, i32, i32)) -> f32 {
	let (axis, positive) = if normal.0 != 0 {
		(0, normal.0 > 0)
	} else if normal.1 != 0 {
		(1, normal.1 > 0)
	} else {
		(2, normal.2 > 0)
	};
	f32::from(axis * 2 + u8::from(positive))
}

/// Pack one draw. `source` is the centre of the face the motes lift off,
/// `dest` where they are drawn to, `progress` how far through the hold
/// (0..1) and `envelope` the fade/flare strength.
pub(crate) fn instance(
	source: [f32; 3],
	dest: [f32; 3],
	tint: [f32; 3],
	face: (i32, i32, i32),
	progress: f32,
	envelope: f32,
) -> DrawInstance {
	[
		source[0],
		source[1],
		source[2],
		progress,
		dest[0],
		dest[1],
		dest[2],
		envelope,
		tint[0],
		tint[1],
		tint[2],
		face_code(face),
	]
}

pub(crate) struct Siphon {
	pipeline: wgpu::RenderPipeline,
	instance_buf: wgpu::Buffer,
}

impl Siphon {
	pub(crate) fn new(
		gpu: &Gpu,
		surface_format: wgpu::TextureFormat,
		frame_bgl: &wgpu::BindGroupLayout,
	) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("siphon"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::SIPHON.into()),
		});
		// No camera-light group, unlike spray and confetti: essence is its
		// own light. A mote lit by the room would go out in exactly the
		// tunnels this effect exists for.
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("siphon"),
			bind_group_layouts: &[Some(frame_bgl)],
			immediate_size: 0,
		});
		// Additive, and the only pipeline here that is: the motes are
		// added to the pixel behind them rather than laid over it. Depth:
		// occluded by terrain, never occludes anything itself.
		let pipeline = crate::pipeline_builder::build(
			device,
			Some(surface_format),
			&crate::pipeline_builder::PipelineDesc {
				label: "siphon",
				layout: &layout,
				module: &shader,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[wgpu::VertexBufferLayout {
					array_stride: INSTANCE_BYTES,
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
					// The destination alpha is left exactly as it was:
					// nothing downstream reads it, and an additive effect
					// has no business claiming coverage it does not have.
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
			label: Some("siphon instance"),
			size: INSTANCE_BYTES,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		Self {
			pipeline,
			instance_buf,
		}
	}

	/// Upload this frame's draw, or nothing. Returns the instance count —
	/// one or zero, since a player holds one button.
	pub(crate) fn prepare(&self, gpu: &Gpu, draw: Option<&DrawInstance>) -> u32 {
		let Some(d) = draw else {
			return 0;
		};
		gpu.queue
			.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(d));
		1
	}

	/// Draw alongside the spray and confetti billboards (same pass, frame
	/// bind group already bound at 0). Returns triangles.
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
	/// CPU/GPU boundary, so the "keep in sync" comment is mechanical
	/// rather than a request. Getting it wrong draws part of a stream, or
	/// a bank of degenerate quads, and neither fails anywhere.
	#[test]
	fn the_mote_count_matches_the_shader() {
		let decl = format!("const MOTES: u32 = {MOTES}u;");
		assert!(
			digblock_assets::shader::SIPHON.contains(&decl),
			"siphon.wgsl must declare `{decl}`"
		);
	}

	/// Six faces, six codes, and each one names the axis and sign the
	/// shader's `face_normal` reconstructs. Mirrored here rather than
	/// asserted against the shader text because it is the DECODE that has
	/// to agree, not the spelling.
	#[test]
	fn every_face_normal_survives_the_round_trip() {
		let decode = |code: f32| {
			let code = code as u32;
			let axis = code / 2;
			let s = if code & 1 == 1 { 1 } else { -1 };
			(
				if axis == 0 { s } else { 0 },
				if axis == 1 { s } else { 0 },
				if axis == 2 { s } else { 0 },
			)
		};
		let faces = [
			(1, 0, 0),
			(-1, 0, 0),
			(0, 1, 0),
			(0, -1, 0),
			(0, 0, 1),
			(0, 0, -1),
		];
		let mut seen = Vec::new();
		for n in faces {
			let code = face_code(n);
			assert_eq!(decode(code), n, "face {n:?} decodes back to itself");
			assert!(
				(0.0..6.0).contains(&code),
				"face {n:?} code {code} in range"
			);
			seen.push(code as u32);
		}
		seen.sort_unstable();
		seen.dedup();
		assert_eq!(seen.len(), 6, "six faces, six codes");
	}

	/// Run an envelope for `secs` at a plausible frame step and report
	/// where it lands.
	fn run(mut e: f32, live: bool, secs: f32) -> f32 {
		let dt = 1.0 / 60.0;
		for _ in 0..(secs / dt) as u32 {
			e = envelope_step(e, live, dt);
		}
		e
	}

	/// Both ends of an ordinary draw: it opens over the fade-in and shuts
	/// over the fade-out, and neither runs past its bound. The out end is
	/// what the caller's drop test keys on, so an envelope that stalled
	/// above zero would leave a dead draw uploaded every frame forever.
	#[test]
	fn a_draw_opens_and_shuts_inside_its_own_windows() {
		assert!(
			run(0.0, true, FADE_IN_SECS * 0.4) < 0.75,
			"not open at once"
		);
		assert_eq!(run(0.0, true, FADE_IN_SECS * 1.5), 1.0, "open, and capped");
		assert!(
			run(1.0, false, FADE_OUT_SECS * 0.4) > 0.25,
			"not gone at once"
		);
		assert_eq!(
			run(1.0, false, FADE_OUT_SECS * 1.5),
			0.0,
			"shut, and floored"
		);
	}

	/// The flare is a real, visible ending rather than a value that gets
	/// clamped away on the next frame: it outlasts a plain release, and it
	/// still reaches zero. And a flare caught by a NEW draw settles to
	/// exactly full — the brightness belongs to the vein that earned it.
	#[test]
	fn a_completed_draw_flares_longer_than_a_released_one() {
		let released = run(1.0, false, FADE_OUT_SECS * 0.6);
		let flared = run(FLARE, false, FADE_OUT_SECS * 0.6);
		assert!(
			flared > released && flared > 1.0,
			"still above full while a release is already dim: {flared} vs {released}"
		);
		assert_eq!(run(FLARE, false, FADE_OUT_SECS * FLARE * 1.2), 0.0);
		// Picked up by the next draw: down to full, and no further.
		assert_eq!(run(FLARE, true, FADE_OUT_SECS * FLARE * 1.2), 1.0);
	}

	/// The lanes the vertex shader reads by name. A packing this flat is
	/// exactly the kind that gets reordered by an edit somewhere else.
	#[test]
	fn a_draw_packs_where_the_shader_looks() {
		let i = instance(
			[1.0, 2.0, 3.0],
			[4.0, 5.0, 6.0],
			[0.25, 0.5, 0.75],
			(0, 1, 0),
			0.6,
			FLARE,
		);
		assert_eq!(&i[0..3], &[1.0, 2.0, 3.0], "source");
		assert_eq!(i[3], 0.6, "progress rides the source's spare lane");
		assert_eq!(&i[4..7], &[4.0, 5.0, 6.0], "destination");
		assert_eq!(i[7], FLARE, "envelope rides the destination's");
		assert_eq!(&i[8..11], &[0.25, 0.5, 0.75], "tint");
		assert_eq!(i[11], face_code((0, 1, 0)), "and the face code the tint's");
	}
}
