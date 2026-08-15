//! RTGI probe volume (GI-DESIGN.md). P1 built the voxel window the
//! rays march; P2 makes the probes real: two toroidal 6-axis
//! ambient-cube cascades (C0 2-block spacing near, C1 8-block far),
//! ping-ponged rgba16float atlases updated by an amortized compute
//! trace (spherical-Fibonacci rays, coarse-skip DDA, sun + sky +
//! emissive radiance, infinite bounce by sampling last frame's atlas
//! at hits), an r32uint flags texture carrying per-probe age and the
//! toroidal self-heal checksum, and an edit-invalidation dispatch.
//! P3 added the half-res resolve pass (gi_resolve.wgsl) — RTGI is the
//! only lighting path; there is no flag.

use digblock_core::block::BlockType;
use digblock_core::chunk::ChunkPos;
use digblock_core::gi::{opacity, GiChunk, GiOpacity};
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_GI, Q_GI_TRACE};
use crate::renderer::FrameUniforms;

/// C0 probe grid (2-block spacing near cascade; see GI-DESIGN.md).
pub const C0_DIMS: (u32, u32, u32) = (32, 24, 32);
/// C0's probe spacing in blocks — the finest scale the bounce field can
/// resolve, and therefore the granularity a moving emitter has to be
/// re-placed at (`engine::CARRY_INVAL_STEP`).
pub const C0_SPACING: u32 = 2;
/// C1 far cascade (8-block spacing).
pub const C1_DIMS: (u32, u32, u32) = (32, 20, 32);
/// Probes traced per cascade per frame (the amortization grain).
const PROBES_PER_DISPATCH: u32 = 512;
/// World height in blocks — the window's fixed Y extent (an up-ray
/// always reaches sky; the window never scrolls vertically).
const WORLD_H: u32 = 256;
/// Light-list capacity (GI-DESIGN.md): nearest emitters win.
const MAX_LIGHTS: usize = 256;

/// One entry in the light list: world position and the GI class byte
/// whose LUT row carries its cast level and its tint.
///
/// A light a player is CARRYING is one of these too. It just isn't in a
/// chunk — which is the whole of the difference, and the only reader
/// that can tell is `gi_light_margin_of`, because the margin is a
/// question about the light's CELL and a carried one has none.
pub type Emitter = (f32, f32, f32, u8);

/// Bit 8 of a light entry's packed `w` word: this light is being
/// CARRIED. The class byte occupies 0–7 and every shader reader masks,
/// exactly as the resolve's history length rides its face code.
///
/// It exists for one reason, and it is not cosmetic: `gi_light_margin`
/// is how far short of a light its shadow ray stops, derived from the
/// light's position WITHIN ITS CELL. That bound is only tolerable
/// because a placed emitter sits at a known offset inside one — a
/// torch's flame gives 1.036, the worst any of them reach. A carried
/// light hangs at feet + 1.0, so a carrier standing on a block top lands
/// on the degenerate maximum and the margin runs to 1.77, which swallows
/// a whole wall they are standing against. Pinned against the WGSL twin
/// by `the_carried_flag_matches_the_shaders`.
const LIGHT_CARRIED: u32 = 256;

/// Cosine of the sun cone's half-angle for the per-pixel shadow ray.
/// The real sun is about 0.27 degrees of angular radius; this is 0.5,
/// roughly twice that, which buys a penumbra that is sharp at contact
/// and softens with distance to the occluder — the thing a shadow map
/// never had — at no extra ray. Widen it and shadows get mushy AND
/// noisier, since one sample has to cover more solid angle.
/// Format of the two ping-ponged resolve targets — BOTH of which hold a
/// running average, which is the whole constraint. rgba8 was tried first
/// and could not: at a history length of 512 each frame contributes 1/512
/// of a difference and in 8 bits that rounds to zero, so the average
/// stopped moving. rgba16f then failed the SAME way, one format up, and it
/// took a capture harness that can see motion to notice — half-float
/// carries a 10-bit mantissa, so near a stored value of 1.0 the ulp is
/// ~1e-3 and `hi + (indirect - hi)/512` rounds away unless the difference
/// exceeds ~0.25. A quarter of the signal range never converged: every
/// pixel froze where it stood when the camera stopped, leaving a pattern
/// locked to the path walked, permanently. Measured at mean 0.988 over
/// 10.4% of pixels after 25 seconds of stillness (GI-DESIGN.md
/// "Measuring motion").
///
/// So the rule is not "use a float format", it is that the format's
/// mantissa has to carry the smallest increment the history length asks
/// for: alpha of 1/N stalls once the residual falls under ~N ulps. f32's
/// 24-bit mantissa puts that floor at ~1e-5 here, which is invisible;
/// f16's put it at a quarter of the signal.
pub const GI_ACC_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

// The a-trous tap strides, one entry per iteration. Doubling the stride
// instead of growing the kernel is the whole trick: strides 1 and 2 of a
// 5x5 B3 spline cover 11x11 for 50 taps. A third iteration (stride 4) is
// deliberately not here — it reaches 23x23, which on a half-res buffer is
// most of a shadow.
const ATROUS_STEPS: [f32; 2] = [1.0, 2.0];

const SUN_CONE_COS: f32 = 0.999_961_9; // cos(0.5 degrees)

/// How long a consumed edit sphere keeps capping the resolve's screen
/// history (frames). A lighting edit changes radiance on surfaces whose
/// face code and depth never change, and a resting pixel at n = 512 admits
/// the change at 1/512 a frame — a placed torch's pool would fade in over
/// seconds. The probes re-trace an age-reset probe on the sweep's next
/// visit (a full C0 sweep is 24576/512 = 48 frames), so two periods holds
/// the screen at the short-history rate until the cache underneath it has
/// settled, multi-bounce included.
const RESOLVE_INVAL_FRAMES: u32 = 96;
/// Screen-cap radius around an edit, in blocks: C1's invalidation radius,
/// which is also about how far a torch (light 14) reaches.
const RESOLVE_INVAL_RADIUS: f32 = 16.0;

/// Below this cos between consecutive caster directions, the sun did not
/// drift — it was TELEPORTED (`/time`). ~5.7 degrees is far more than the
/// world clock moves in a frame and far less than any admin jump.
const SUN_JUMP_COS: f32 = 0.995;
/// Sun-direction quantization for GI rays: ~1.5° so shadow work stays
/// still between steps and hysteresis glides across them.
const SUN_QUANT: f32 = 38.0;

struct Cascade {
	dims: (u32, u32, u32),
	spacing: u32,
	cursor: u32,
	/// Permutation stride for the sweep (see `schedule_stride`).
	stride: u32,
	uniform_buf: wgpu::Buffer,
	atlas: [wgpu::Texture; 2],
	/// bind[i]: atlas[i] is prev (sampled), atlas[1-i] is out (written).
	bind: [wgpu::BindGroup; 2],
}

impl Cascade {
	fn probes(&self) -> u32 {
		self.dims.0 * self.dims.1 * self.dims.2
	}
}

/// Stride for the permuted probe sweep: about the golden fraction of the
/// probe count, nudged until it is coprime with it, so `k -> k*stride mod
/// count` is a bijection and consecutive sweep steps land far apart.
/// Clamped so `wg.x * stride` cannot overflow u32 in the shader.
fn schedule_stride(count: u32) -> u32 {
	fn gcd(a: u32, b: u32) -> u32 {
		if b == 0 {
			a
		} else {
			gcd(b, a % b)
		}
	}
	let cap = u32::MAX / PROBES_PER_DISPATCH.max(1);
	let mut s = ((f64::from(count) * 0.618_033_988_749_895) as u32).clamp(1, cap.min(count.max(1)));
	// `s` and `count` share a factor at most `count` times; in practice
	// the first or second candidate lands.
	let mut tries = 0;
	while (gcd(s, count.max(1)) != 1 || s == 0) && tries < 64 {
		s = if s + 1 > cap { 1 } else { s + 1 };
		tries += 1;
	}
	if gcd(s, count.max(1)) != 1 {
		1
	} else {
		s
	}
}

pub struct GiDraw {
	probe_pipeline: wgpu::ComputePipeline,
	inval_pipeline: wgpu::ComputePipeline,
	/// One invocation sampling probes + sun visibility at the camera
	/// into `cam_buf` — the light rig for spray and the viewmodel.
	cam_pipeline: wgpu::ComputePipeline,
	cam_buf: wgpu::Buffer,
	cascades: [Cascade; 2],
	/// Which atlas is the front (last written) — flips per frame.
	ping: usize,
	frame: u32,
	window: wgpu::Texture,
	coarse: wgpu::Texture,
	/// CPU copy of the coarse texture, in upload order (x fastest, z
	/// slowest) — what the sky ceiling is recomputed from when chunks
	/// land. 51 KiB at the desktop side.
	coarse_mirror: Box<[u8]>,
	/// The mirror changed this frame; `sky_y` rescans at the next
	/// `encode`.
	coarse_mips_dirty: bool,
	/// Highest occupied cell y anywhere in the window, in blocks (R18):
	/// an ASCENDING ray above this is proven lit/sky in one float
	/// compare, so the sun march stops at the tallest local hill instead
	/// of climbing to y = 255 — recomputed with the skip mips. 255 until
	/// the first flush (conservative: never breaks a ray early on
	/// unclassified data).
	sky_y: f32,
	/// XZ extent in chunks (= draw distance): the toroidal modulus.
	window_chunks: u32,
	/// Cost-stub mask (DIG_STUB) — rides `jitter.w` into gi_resolve.wgsl,
	/// which has no frame UBO of its own to read `misc.z` from.
	stub: u32,
	emitters: FxHashMap<ChunkPos, Vec<Emitter>>,
	/// The carried lights the list was last flushed with — see
	/// `flush_lights`, which compares this frame's against it. A carried
	/// torch is not a second kind of light: it is an entry in the ONE
	/// list that happens to move, and every consumer of that list (both
	/// NEE paths, and through the probes the water surface, remote
	/// players and `cam_light`) sees it without knowing the difference.
	carried: Vec<Emitter>,
	/// Temporal state for the resolve's reprojection. `history_valid` is
	/// cleared whenever last frame's buffers cannot be trusted: the first
	/// frame, a renderer rebuild after device loss, or a sun jump (`/time`
	/// can rotate the caster 180 degrees between two frames, and without
	/// this the screen ghosts for seconds).
	prev_vp: [[f32; 4]; 4],
	prev_cam: [f32; 3],
	prev_sun: [f32; 3],
	history_valid: bool,
	frame_i: u32,
	lights_dirty: bool,
	lights_buf: wgpu::Buffer,
	/// Camera position at the last light-list flush — see flush_lights.
	lights_cam: [f32; 3],
	/// Emitters written at that flush, so a move that changes WHICH are in
	/// reach re-flushes even under `MAX_LIGHTS`.
	lights_len: usize,
	/// Positions awaiting probe invalidation (≤4 consumed/frame), each
	/// flagged with whether it came from a MOVING light rather than a
	/// world edit — see `resolve_inval` for the only thing that flag
	/// changes.
	inval_queue: Vec<([f32; 3], bool)>,
	/// Spheres the probe pass consumed, held live so the RESOLVE keeps
	/// capping screen history around them while the probes re-converge
	/// (position + remaining frames + the moving flag; ≤4, because the
	/// uniform has exactly four slots).
	///
	/// Eviction is per-KIND, and that is load-bearing rather than tidy. A
	/// world edit fires once; a carried light fires every
	/// `CARRY_INVAL_STEP` of travel, which is twice a second at
	/// `WALK_SPEED` — so against a plain newest-wins ring one walking
	/// torch-bearer holds three of the four slots continuously (2/s
	/// against `RESOLVE_INVAL_FRAMES` = 96 frames of life) and two of
	/// them turn it over faster than an edit's window. Making each stream
	/// recycle its OWN oldest slot bounds the moving one without a second
	/// constant: an edit that wins a slot keeps it for the full 96 frames
	/// however far anybody walks.
	resolve_inval: Vec<([f32; 3], u32, bool)>,
	/// The half-res resolve pass (gi_resolve.wgsl): probes + per-pixel
	/// sun DDA → the scene's two lighting inputs.
	resolve_pipeline: wgpu::RenderPipeline,
	resolve_bgl: wgpu::BindGroupLayout,
	atrous_pipeline: wgpu::RenderPipeline,
	atrous_bgl: wgpu::BindGroupLayout,
	/// One uniform per a-trous iteration — see the comment where they are
	/// created for why this cannot be a single rewritten buffer.
	atrous_buf: [wgpu::Buffer; ATROUS_STEPS.len()],
	resolve_buf: wgpu::Buffer,
	/// Bound at the resolve's empty group 1 (every slot in a pipeline
	/// layout must hold a bind group, empty or not).
	empty_bg: wgpu::BindGroup,
	/// gi_common.wgsl's group(2) — the GI world data every direct
	/// gatherer binds: the resolve, water, and players.
	gather_bgl: wgpu::BindGroupLayout,
	/// One per atlas ping; `gather_bind()` picks the front.
	gather_bind: [wgpu::BindGroup; 2],
	/// The scene pass's group 2: the two resolve outputs, textureLoad'd.
	scene_bgl: wgpu::BindGroupLayout,
	/// Last quantized sun (xyz dir + w above-horizon), set by `encode`
	/// and consumed by the resolve uniform in the same frame.
	last_sun: [f32; 4],
	/// Debug raymarch (`DIG_GI=window|probes`).
	debug_pipeline: wgpu::RenderPipeline,
	/// One per front atlas (the debug view samples C0's newest).
	debug_bind: [wgpu::BindGroup; 2],
	debug_buf: wgpu::Buffer,
	debug_probes: bool,
}

/// The 256-entry material LUT (GI-DESIGN.md): everything a ray needs
/// per class byte. Four words per entry: albedo rgb + opacity class in
/// the first, the cast-light level in the second (emission color is
/// the albedo — a torch glows its own warm tone), and the ≤2-box
/// occlusion cover of a PARTIAL class (opacity 3) packed in the third
/// and fourth (`core::gi::pack_box`; 0 = no box). Full cubes keep the
/// base-id entries; the synthetic shape classes from
/// `core::gi::shape_classes` fill 200.. and borrow their base block's
/// albedo and light, so a slab bounces wood-colored and a torch's
/// synthetic class still names its emission.
fn material_lut(layer_means: &[[u8; 3]]) -> [[u32; 4]; 256] {
	let mut lut = [[0u32; 4]; 256];
	let albedo = |b: BlockType| -> [u8; 3] {
		layer_means
			.get(b.props().tex_top as usize)
			.copied()
			.unwrap_or([255, 255, 255])
	};
	for b in BlockType::ALL {
		let id = b as usize;
		if id >= 256 || b == BlockType::Air {
			continue;
		}
		let m = albedo(b);
		let op = match opacity(b) {
			GiOpacity::Air => 0u32,
			GiOpacity::Translucent => 1,
			GiOpacity::Opaque => 2,
		};
		lut[id] = [
			u32::from(m[0]) | (u32::from(m[1]) << 8) | (u32::from(m[2]) << 16) | (op << 24),
			u32::from(b.props().light),
			0,
			0,
		];
	}
	for (i, sc) in digblock_core::gi::shape_classes().iter().enumerate() {
		let m = albedo(sc.base);
		let boxes = sc.boxes.map(|b| b.map_or(0, digblock_core::gi::pack_box));
		lut[digblock_core::gi::SHAPE_CLASS_BASE as usize + i] = [
			u32::from(m[0]) | (u32::from(m[1]) << 8) | (u32::from(m[2]) << 16) | (3 << 24),
			u32::from(sc.base.props().light),
			boxes[0],
			boxes[1],
		];
	}
	lut
}

/// The window's sky ceiling (R18): the highest coarse brick row holding
/// anything at all, as a cell y. A non-descending ray above it is proven
/// lit/sky in one float compare, which is what ends the sun march's
/// climb at the tallest local hill instead of y = 255 — proving open sky
/// at 8 blocks per brick skip was ~9 dependent texture loads per
/// fragment, most of a screen full of river's sun cost. Scanned top
/// down, so a settled world answers in one slice; -1 when the mirror is
/// still empty (rays find nothing either way).
/// The CPU twin of `gi_lateral_max` in gi_dda.wgsl — the lateral bound
/// EVERY march breaks at, in blocks from the camera. `flush_lights` uses
/// it to drop emitters no ray could reach; the two must agree or the
/// light list either hides a torch the shader would have traced or wastes
/// a `MAX_LIGHTS` slot on one it would not. Pinned by
/// `the_light_reach_matches_the_shaders_bound`.
fn lateral_max(window_chunks: u32) -> f32 {
	(window_chunks * 16) as f32 * 0.5 - 1.0
}

/// The light list's payload: every reachable emitter, at most the
/// `MAX_LIGHTS` nearest `cam`, in the 16-byte GPU entry form
/// (`[x, y, z, bitcast(class)]`).
///
/// Split out of `flush_lights` because that needs a `Gpu` to upload and
/// this is the part with a rule in it.
///
/// Carried lights — a player holding a torch — are simply more
/// candidates. There is no reservation and no separate cap, because the
/// distance rule already does the right thing at both ends: a carrier's
/// own light sits half a block from the camera and can never be the one
/// dropped however many torches a world has, and a distant player's
/// torch losing its slot to 256 nearer emitters is the correct answer
/// under the same rule that governs everything else here.
///
/// Selection, NOT ordering. The doc on `flush_lights` records why: no
/// reader cares about the order, so the only thing the distance is for
/// is deciding which ones survive the cap. Under the cap nothing
/// survives-or-doesn't and the list goes out unordered; over it,
/// `select_nth_unstable_by` partitions in O(n) rather than sorting in
/// O(n log n). That matters now in a way it did not before: a carried
/// light moves, so this runs EVERY frame while anyone is walking, over
/// every emitter within `lateral_max` — 159 blocks at draw distance 20.
fn select_lights(
	emitters: impl Iterator<Item = Emitter>,
	carried: &[Emitter],
	cam: [f32; 3],
	reach: f32,
) -> Vec<[f32; 4]> {
	let placed = emitters.map(|(x, y, z, class)| (x, y, z, u32::from(class)));
	let held = carried
		.iter()
		.map(|&(x, y, z, class)| (x, y, z, u32::from(class) | LIGHT_CARRIED));
	let mut all: Vec<(f32, [f32; 4])> = placed
		.chain(held)
		.filter(|e| {
			let (dx, dz) = (e.0 - cam[0], e.2 - cam[2]);
			dx * dx + dz * dz <= reach * reach
		})
		.map(|(x, y, z, w)| {
			let d2 = (x - cam[0]).powi(2) + (y - cam[1]).powi(2) + (z - cam[2]).powi(2);
			(d2, [x, y, z, f32::from_bits(w)])
		})
		.collect();
	if all.len() > MAX_LIGHTS {
		all.select_nth_unstable_by(MAX_LIGHTS, |a, b| a.0.total_cmp(&b.0));
		all.truncate(MAX_LIGHTS);
	}
	all.into_iter().map(|(_, e)| e).collect()
}

fn sky_ceiling(mirror: &[u8], w: usize, h: usize, d: usize) -> f32 {
	(0..h)
		.rev()
		.find(|&y| {
			(0..d).any(|z| {
				let row = (z * h + y) * w;
				mirror[row..row + w].iter().any(|&b| b != 0)
			})
		})
		.map_or(-1.0, |y| (y * 8 + 7) as f32)
}

fn atlas_tex(device: &wgpu::Device, dims: (u32, u32, u32), label: &str) -> wgpu::Texture {
	device.create_texture(&wgpu::TextureDescriptor {
		label: Some(label),
		size: wgpu::Extent3d {
			width: dims.0,
			height: dims.1,
			depth_or_array_layers: dims.2 * 6,
		},
		mip_level_count: 1,
		sample_count: 1,
		dimension: wgpu::TextureDimension::D3,
		format: wgpu::TextureFormat::Rgba16Float,
		usage: wgpu::TextureUsages::STORAGE_BINDING
			| wgpu::TextureUsages::TEXTURE_BINDING
			| wgpu::TextureUsages::COPY_SRC
			| wgpu::TextureUsages::COPY_DST,
		view_formats: &[],
	})
}

/// GiUniforms layout (must match gi_probe.wgsl): dims, cam, sun,
/// sun_color, zenith, horizon, sched, 4 inval spheres = 176 bytes.
const GI_UNIFORM_SIZE: u64 = 176;

/// The GI resolve's group 0 bind plus its two MRT attachments — one
/// argument instead of three, which is what keeps the encode signature
/// under clippy's limit as the pass grows.
pub struct ResolveTargets<'a> {
	pub bind: &'a wgpu::BindGroup,
	pub indirect_view: &'a wgpu::TextureView,
	pub sun_view: &'a wgpu::TextureView,
}

impl GiDraw {
	pub fn new(
		gpu: &Gpu,
		window_chunks: u32,
		layer_means: &[[u8; 3]],
		surface_format: wgpu::TextureFormat,
		debug_probes: bool,
		stub: u32,
	) -> Self {
		let device = &gpu.device;
		let queue = &gpu.queue;
		let side = window_chunks * 16;
		let window = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("gi voxel window"),
			size: wgpu::Extent3d {
				width: side,
				height: WORLD_H,
				depth_or_array_layers: side,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::R8Uint,
			usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let coarse = device.create_texture(&wgpu::TextureDescriptor {
			label: Some("gi coarse occupancy"),
			size: wgpu::Extent3d {
				width: side / 8,
				height: WORLD_H / 8,
				depth_or_array_layers: side / 8,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D3,
			format: wgpu::TextureFormat::R8Uint,
			usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
			view_formats: &[],
		});
		let lut = material_lut(layer_means);
		let lut_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi material lut"),
			size: (256 * 16) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue
			.write_buffer(&lut_buf, 0, bytemuck::cast_slice(&lut));
		let lights_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi light list"),
			size: (16 + MAX_LIGHTS * 16) as u64,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue.write_buffer(&lights_buf, 0, &[0u8; 16]);

		// One bind-group layout serves both compute entry points.
		let tex3 = |binding, sample_type| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::COMPUTE,
			ty: wgpu::BindingType::Texture {
				sample_type,
				view_dimension: wgpu::TextureViewDimension::D3,
				multisampled: false,
			},
			count: None,
		};
		let storage_buf = |binding| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::COMPUTE,
			ty: wgpu::BindingType::Buffer {
				ty: wgpu::BufferBindingType::Storage { read_only: true },
				has_dynamic_offset: false,
				min_binding_size: None,
			},
			count: None,
		};
		let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi probe bgl"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				tex3(1, wgpu::TextureSampleType::Uint),
				tex3(2, wgpu::TextureSampleType::Uint),
				storage_buf(3),
				storage_buf(4),
				tex3(5, wgpu::TextureSampleType::Float { filterable: false }),
				wgpu::BindGroupLayoutEntry {
					binding: 6,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::StorageTexture {
						access: wgpu::StorageTextureAccess::WriteOnly,
						format: wgpu::TextureFormat::Rgba16Float,
						view_dimension: wgpu::TextureViewDimension::D3,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 7,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::StorageTexture {
						access: wgpu::StorageTextureAccess::ReadWrite,
						format: wgpu::TextureFormat::R32Uint,
						view_dimension: wgpu::TextureViewDimension::D3,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					binding: 8,
					visibility: wgpu::ShaderStages::COMPUTE,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: false },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
			],
		});
		// The camera-light sample cam_light writes and spray/viewmodel
		// bind as a uniform (STORAGE for the compute write, UNIFORM for
		// the render reads — same 32 bytes).
		let cam_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi camera light"),
			size: 32,
			usage: wgpu::BufferUsages::STORAGE
				| wgpu::BufferUsages::UNIFORM
				| wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		gpu.queue.write_buffer(&cam_buf, 0, &[0u8; 32]);
		let window_view = window.create_view(&Default::default());
		let coarse_view = coarse.create_view(&Default::default());

		let make_cascade = |dims: (u32, u32, u32), spacing: u32, name: &str| -> Cascade {
			let atlas = [
				atlas_tex(device, dims, &format!("gi atlas {name} a")),
				atlas_tex(device, dims, &format!("gi atlas {name} b")),
			];
			let flags = device.create_texture(&wgpu::TextureDescriptor {
				label: Some(&format!("gi flags {name}")),
				size: wgpu::Extent3d {
					width: dims.0,
					height: dims.1,
					depth_or_array_layers: dims.2,
				},
				mip_level_count: 1,
				sample_count: 1,
				dimension: wgpu::TextureDimension::D3,
				format: wgpu::TextureFormat::R32Uint,
				usage: wgpu::TextureUsages::STORAGE_BINDING,
				view_formats: &[],
			});
			let flags_view = flags.create_view(&Default::default());
			let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
				label: Some(&format!("gi uniforms {name}")),
				size: GI_UNIFORM_SIZE,
				usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
				mapped_at_creation: false,
			});
			let views = [
				atlas[0].create_view(&Default::default()),
				atlas[1].create_view(&Default::default()),
			];
			let bind = std::array::from_fn(|prev| {
				device.create_bind_group(&wgpu::BindGroupDescriptor {
					label: Some(&format!("gi probe bg {name} {prev}")),
					layout: &bgl,
					entries: &[
						wgpu::BindGroupEntry {
							binding: 0,
							resource: uniform_buf.as_entire_binding(),
						},
						wgpu::BindGroupEntry {
							binding: 1,
							resource: wgpu::BindingResource::TextureView(&window_view),
						},
						wgpu::BindGroupEntry {
							binding: 2,
							resource: wgpu::BindingResource::TextureView(&coarse_view),
						},
						wgpu::BindGroupEntry {
							binding: 3,
							resource: lut_buf.as_entire_binding(),
						},
						wgpu::BindGroupEntry {
							binding: 4,
							resource: lights_buf.as_entire_binding(),
						},
						wgpu::BindGroupEntry {
							binding: 5,
							resource: wgpu::BindingResource::TextureView(&views[prev]),
						},
						wgpu::BindGroupEntry {
							binding: 6,
							resource: wgpu::BindingResource::TextureView(&views[1 - prev]),
						},
						wgpu::BindGroupEntry {
							binding: 7,
							resource: wgpu::BindingResource::TextureView(&flags_view),
						},
						wgpu::BindGroupEntry {
							binding: 8,
							resource: cam_buf.as_entire_binding(),
						},
					],
				})
			});
			Cascade {
				dims,
				spacing,
				cursor: 0,
				stride: schedule_stride(dims.0 * dims.1 * dims.2),
				uniform_buf,
				atlas,
				bind,
			}
		};
		let cascades = [
			make_cascade(C0_DIMS, C0_SPACING, "c0"),
			make_cascade(C1_DIMS, 8, "c1"),
		];

		let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi probe"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_PROBE.into()),
		});
		let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi probe layout"),
			bind_group_layouts: &[&bgl],
			push_constant_ranges: &[],
		});
		let compute = |entry: &str, label: &str| {
			device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
				label: Some(label),
				layout: Some(&layout),
				module: &module,
				entry_point: Some(entry),
				compilation_options: Default::default(),
				cache: None,
			})
		};
		let probe_pipeline = compute("update_probes", "gi probe update");
		let inval_pipeline = compute("inval", "gi invalidate");
		let cam_pipeline = compute("cam_light", "gi camera light");

		// Debug raymarch pipeline (fullscreen, no depth, swapchain target).
		let debug_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi debug uniforms"),
			size: 96,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let ftex3 = |binding, sample_type, vis| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: vis,
			ty: wgpu::BindingType::Texture {
				sample_type,
				view_dimension: wgpu::TextureViewDimension::D3,
				multisampled: false,
			},
			count: None,
		};
		let debug_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi debug bgl"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding: 0,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				ftex3(
					1,
					wgpu::TextureSampleType::Uint,
					wgpu::ShaderStages::FRAGMENT,
				),
				ftex3(
					2,
					wgpu::TextureSampleType::Uint,
					wgpu::ShaderStages::FRAGMENT,
				),
				wgpu::BindGroupLayoutEntry {
					binding: 3,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				ftex3(
					4,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::ShaderStages::FRAGMENT,
				),
			],
		});
		let debug_bind = std::array::from_fn(|i| {
			let av = cascades[0].atlas[i].create_view(&Default::default());
			device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("gi debug bg"),
				layout: &debug_bgl,
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: debug_buf.as_entire_binding(),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&window_view),
					},
					wgpu::BindGroupEntry {
						binding: 2,
						resource: wgpu::BindingResource::TextureView(&coarse_view),
					},
					wgpu::BindGroupEntry {
						binding: 3,
						resource: lut_buf.as_entire_binding(),
					},
					wgpu::BindGroupEntry {
						binding: 4,
						resource: wgpu::BindingResource::TextureView(&av),
					},
				],
			})
		});
		let debug_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi debug"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_DEBUG.into()),
		});
		let debug_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi debug layout"),
			bind_group_layouts: &[&debug_bgl],
			push_constant_ranges: &[],
		});
		let debug_pipeline = crate::pipeline_builder::build_depthless(
			device,
			surface_format,
			&crate::pipeline_builder::PipelineDesc {
				label: "gi debug",
				layout: &debug_layout,
				module: &debug_module,
				vs_entry: "vs",
				fs_entry: Some("fs"),
				vertex_buffers: &[],
				cull: None,
				depth_write: false,
				depth_compare: wgpu::CompareFunction::Always,
				depth_bias: Default::default(),
				blend: None,
				write_mask: wgpu::ColorWrites::ALL,
			},
		);

		// The half-res resolve (gi_resolve.wgsl): its own uniform + a
		// depth tap at group 0, the shared gather group at 2 (group 1
		// deliberately empty — gi_common.wgsl pins the world data to
		// group(2) because water/players have materials at 1); two MRT
		// outputs, no depth attachment, so it's built directly rather
		// than through the single-target pipeline funnel.
		let f_entry = |binding, ty| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::FRAGMENT,
			ty,
			count: None,
		};
		let ftex = |binding, sample_type, dim| {
			f_entry(
				binding,
				wgpu::BindingType::Texture {
					sample_type,
					view_dimension: dim,
					multisampled: false,
				},
			)
		};
		let resolve_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi resolve bgl"),
			entries: &[
				f_entry(
					0,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
				ftex(
					1,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				// The prepass's exact face-normal codes. Uint sample type
				// — the value is a table index, not a colour.
				ftex(
					2,
					wgpu::TextureSampleType::Uint,
					wgpu::TextureViewDimension::D2,
				),
				// Last frame's two outputs, ping-ponged in.
				ftex(
					3,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				ftex(
					4,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				// The emitter list (R20): the per-pixel NEE ray samples the
				// same buffer the probe pass does. Second storage buffer in
				// this pipeline's fragment stage (gi_lut is the first), well
				// under the WebGPU baseline limit of 8.
				f_entry(
					5,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
			],
		});
		// gi_common.wgsl's group(2): the GI world data every direct
		// gatherer binds (the resolve here, water and players through
		// their pipeline layouts). One per atlas ping. VERTEX visibility
		// is for water's per-vertex probe gather (R17) — WebGPU baseline
		// allows read-only storage and textures in the vertex stage, and
		// the extra visibility costs the fragment-only users nothing.
		let g_entry = |binding, ty| wgpu::BindGroupLayoutEntry {
			binding,
			visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
			ty,
			count: None,
		};
		let gtex = |binding, sample_type| {
			g_entry(
				binding,
				wgpu::BindingType::Texture {
					sample_type,
					view_dimension: wgpu::TextureViewDimension::D3,
					multisampled: false,
				},
			)
		};
		let gather_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi gather bgl"),
			entries: &[
				gtex(0, wgpu::TextureSampleType::Uint),
				gtex(1, wgpu::TextureSampleType::Uint),
				g_entry(
					2,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Storage { read_only: true },
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
				gtex(3, wgpu::TextureSampleType::Float { filterable: false }),
				gtex(4, wgpu::TextureSampleType::Float { filterable: false }),
			],
		});
		let gather_bind = std::array::from_fn(|i| {
			let a0 = cascades[0].atlas[i].create_view(&Default::default());
			let a1 = cascades[1].atlas[i].create_view(&Default::default());
			device.create_bind_group(&wgpu::BindGroupDescriptor {
				label: Some("gi gather bg"),
				layout: &gather_bgl,
				entries: &[
					wgpu::BindGroupEntry {
						binding: 0,
						resource: wgpu::BindingResource::TextureView(&window_view),
					},
					wgpu::BindGroupEntry {
						binding: 1,
						resource: wgpu::BindingResource::TextureView(&coarse_view),
					},
					wgpu::BindGroupEntry {
						binding: 2,
						resource: lut_buf.as_entire_binding(),
					},
					wgpu::BindGroupEntry {
						binding: 3,
						resource: wgpu::BindingResource::TextureView(&a0),
					},
					wgpu::BindGroupEntry {
						binding: 4,
						resource: wgpu::BindingResource::TextureView(&a1),
					},
				],
			})
		});
		let resolve_buf = device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("gi resolve uniforms"),
			// 2 mat4 + 10 vec4 + 4 invalidation spheres. Grew at M4 by
			// prev_view_proj, the previous camera + history-valid flag, the
			// exact caster, and the sample-sequence lane; the spheres came
			// with the deterministic sun (screen history follows a lighting
			// edit at the short rate).
			size: 352,
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let resolve_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi resolve"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_RESOLVE.into()),
		});
		let empty_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi resolve empty"),
			entries: &[],
		});
		let empty_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
			label: Some("gi resolve empty"),
			layout: &empty_bgl,
			entries: &[],
		});
		let resolve_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi resolve layout"),
			bind_group_layouts: &[&resolve_bgl, &empty_bgl, &gather_bgl],
			push_constant_ranges: &[],
		});
		let resolve_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("gi resolve"),
			layout: Some(&resolve_layout),
			vertex: wgpu::VertexState {
				module: &resolve_module,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				..Default::default()
			},
			depth_stencil: None,
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &resolve_module,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(GI_ACC_FORMAT.into()), Some(GI_ACC_FORMAT.into())],
			}),
			multiview: None,
			cache: None,
		});
		// The resolve's spatial half. It reads only what the two lighting
		// targets already carry, so its layout is a uniform and that pair —
		// no world bindings, no moments buffer (gi_atrous.wgsl derives both
		// sigmas analytically from the stored mean and sample count).
		let atrous_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi atrous bgl"),
			entries: &[
				f_entry(
					0,
					wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
				),
				ftex(
					1,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				ftex(
					2,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
			],
		});
		// One buffer per iteration rather than one rewritten between them:
		// both passes are encoded into the same submission, so a single
		// buffer would hand BOTH of them whichever stride was written last.
		// The strides are compile-time constants anyway.
		let atrous_buf: [wgpu::Buffer; ATROUS_STEPS.len()] = std::array::from_fn(|i| {
			let b = device.create_buffer(&wgpu::BufferDescriptor {
				label: Some("gi atrous uniform"),
				size: 16,
				usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
				mapped_at_creation: false,
			});
			queue.write_buffer(
				&b,
				0,
				bytemuck::cast_slice(&[ATROUS_STEPS[i], 0.0, 0.0, 0.0]),
			);
			b
		});
		let atrous_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("gi atrous"),
			source: wgpu::ShaderSource::Wgsl(digblock_assets::shader::GI_ATROUS.into()),
		});
		let atrous_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: Some("gi atrous layout"),
			bind_group_layouts: &[&atrous_bgl],
			push_constant_ranges: &[],
		});
		let atrous_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("gi atrous"),
			layout: Some(&atrous_layout),
			vertex: wgpu::VertexState {
				module: &atrous_module,
				entry_point: Some("vs"),
				compilation_options: Default::default(),
				buffers: &[],
			},
			primitive: wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleList,
				..Default::default()
			},
			depth_stencil: None,
			multisample: Default::default(),
			fragment: Some(wgpu::FragmentState {
				module: &atrous_module,
				entry_point: Some("fs"),
				compilation_options: Default::default(),
				targets: &[Some(GI_ACC_FORMAT.into()), Some(GI_ACC_FORMAT.into())],
			}),
			multiview: None,
			cache: None,
		});
		// The scene pass's group 2: both resolve outputs, textureLoad'd
		// by chunk.wgsl's bilateral upsample.
		let scene_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label: Some("gi scene bgl"),
			entries: &[
				ftex(
					0,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
				ftex(
					1,
					wgpu::TextureSampleType::Float { filterable: false },
					wgpu::TextureViewDimension::D2,
				),
			],
		});

		Self {
			probe_pipeline,
			inval_pipeline,
			cam_pipeline,
			cam_buf,
			cascades,
			ping: 0,
			frame: 0,
			window,
			coarse,
			coarse_mirror: vec![0u8; (side / 8 * (WORLD_H / 8) * (side / 8)) as usize]
				.into_boxed_slice(),
			coarse_mips_dirty: false,
			sky_y: 255.0,
			window_chunks,
			stub,
			emitters: FxHashMap::default(),
			carried: Vec::new(),
			prev_vp: [[0.0; 4]; 4],
			prev_cam: [0.0; 3],
			prev_sun: [0.0; 3],
			history_valid: false,
			frame_i: 0,
			lights_dirty: false,
			lights_buf,
			lights_cam: [f32::INFINITY; 3],
			lights_len: 0,
			inval_queue: Vec::new(),
			resolve_inval: Vec::new(),
			resolve_pipeline,
			resolve_bgl,
			resolve_buf,
			atrous_pipeline,
			atrous_bgl,
			atrous_buf,
			empty_bg,
			gather_bgl,
			gather_bind,
			scene_bgl,
			last_sun: [0.0, 1.0, 0.0, 1.0],
			debug_pipeline,
			debug_bind,
			debug_buf,
			debug_probes,
		}
	}

	pub fn scene_bgl(&self) -> &wgpu::BindGroupLayout {
		&self.scene_bgl
	}

	/// gi_common.wgsl's group(2) layout — water and players reference it
	/// in their pipeline layouts.
	pub fn gather_bgl(&self) -> &wgpu::BindGroupLayout {
		&self.gather_bgl
	}

	/// The GI world bind group reading the FRONT (just written) atlases
	/// — valid after this frame's `encode`.
	pub fn gather_bind(&self) -> &wgpu::BindGroup {
		&self.gather_bind[self.ping]
	}

	/// The 32-byte camera-light buffer `cam_light` writes — spray and
	/// the viewmodel bind it as a uniform.
	pub fn cam_light_buf(&self) -> &wgpu::Buffer {
		&self.cam_buf
	}

	/// Build the resolve's group-0 bind (uniform + the depth and
	/// face-normal taps) against the current targets — called whenever
	/// the frame targets rebuild.
	pub fn make_resolve_bind(
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
				wgpu::BindGroupEntry {
					binding: 5,
					resource: self.lights_buf.as_entire_binding(),
				},
			],
		})
	}

	/// Build one a-trous iteration's bind: its stride uniform plus the pair
	/// it reads. Iteration `i` reads whichever pair the previous stage
	/// wrote, so the caller owns the ping — there is no state here.
	pub fn make_atrous_bind(
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

	/// Encode one a-trous iteration: read one lighting pair, write the
	/// other. Both are full-screen half-res passes with no depth.
	///
	/// `query` is the caller's, one pair per ITERATION. Both used to write
	/// `Q_GI_DENOISE`, so the second silently overwrote the first and the
	/// denoise column reported the stride-2 pass alone.
	pub fn encode_atrous(
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
		});
		pass.set_pipeline(&self.atrous_pipeline);
		pass.set_bind_group(0, bind, &[]);
		pass.draw(0..3, 0..1);
	}

	/// Encode the half-res GI resolve into the two lighting targets.
	pub fn encode_resolve(
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
		let mut data = Vec::with_capacity(352);
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
		data.extend_from_slice(bytemuck::cast_slice(&frame.light_color));
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

	/// Drop the screen-space history: device loss, a resize, or anything
	/// else that leaves last frame's buffers describing another world.
	pub fn reset_history(&mut self) {
		self.history_valid = false;
	}

	/// Patch this frame's uniforms with the GI parameters the direct
	/// gatherers (water, players) read: quantized caster, cascade dims,
	/// window side. Also latches `last_sun` for this frame's `encode` /
	/// `encode_resolve`. Call BEFORE the frame UBO uploads.
	pub fn patch_frame(&mut self, frame: &mut FrameUniforms) {
		// Quantized caster: the ACTIVE light (sun by day, moon by night —
		// light_dir points toward the ground and light_color already
		// matches it), snapped so probe results hold still between steps
		// (~1.5°).
		let s = [
			-frame.light_dir[0],
			-frame.light_dir[1],
			-frame.light_dir[2],
		];
		let len = (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt().max(1e-6);
		let q = [
			(s[0] / len * SUN_QUANT).round() / SUN_QUANT,
			(s[1] / len * SUN_QUANT).round() / SUN_QUANT,
			(s[2] / len * SUN_QUANT).round() / SUN_QUANT,
		];
		let ql = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt().max(1e-6);
		let sun_up = if q[1] > 0.02 { 1.0f32 } else { 0.0 };
		self.last_sun = [q[0] / ql, q[1] / ql, q[2] / ql, sun_up];
		frame.gi_sun = self.last_sun;
		let d0 = self.cascades[0].dims;
		let d1 = self.cascades[1].dims;
		frame.gi0 = [
			d0.0 as f32,
			d0.1 as f32,
			d0.2 as f32,
			self.cascades[0].spacing as f32,
		];
		frame.gi1 = [
			d1.0 as f32,
			d1.1 as f32,
			d1.2 as f32,
			self.cascades[1].spacing as f32,
		];
		frame.gi_misc = [(self.window_chunks * 16) as f32, self.sky_y, 0.0, 0.0];
	}

	/// Upload one classified chunk into its toroidal window region (and
	/// its coarse bytes, and its emitter-list slice).
	pub fn upload_chunk(&mut self, gpu: &Gpu, pos: ChunkPos, gi: &GiChunk) {
		let wc = self.window_chunks as i32;
		let (tx, tz) = (pos.x.rem_euclid(wc) as u32, pos.z.rem_euclid(wc) as u32);
		let ty = pos.y as u32;
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture: &self.window,
				mip_level: 0,
				origin: wgpu::Origin3d {
					x: tx * 16,
					y: ty * 16,
					z: tz * 16,
				},
				aspect: wgpu::TextureAspect::All,
			},
			&gi.bytes[..],
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(16),
				rows_per_image: Some(16),
			},
			wgpu::Extent3d {
				width: 16,
				height: 16,
				depth_or_array_layers: 16,
			},
		);
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture: &self.coarse,
				mip_level: 0,
				origin: wgpu::Origin3d {
					x: tx * 2,
					y: ty * 2,
					z: tz * 2,
				},
				aspect: wgpu::TextureAspect::All,
			},
			&gi.coarse,
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(2),
				rows_per_image: Some(2),
			},
			wgpu::Extent3d {
				width: 2,
				height: 2,
				depth_or_array_layers: 2,
			},
		);
		// Keep the CPU mirror in step and mark the sky ceiling stale
		// (R18); the next `encode` rescans it.
		{
			let w = (self.window_chunks * 2) as usize;
			let h = (WORLD_H / 8) as usize;
			for (ci, &b) in gi.coarse.iter().enumerate() {
				let (dz, dy, dx) = (ci / 4, (ci / 2) % 2, ci % 2);
				let m = ((tz as usize * 2 + dz) * h + (ty as usize * 2 + dy)) * w
					+ tx as usize * 2
					+ dx;
				if self.coarse_mirror[m] != b {
					self.coarse_mirror[m] = b;
					self.coarse_mips_dirty = true;
				}
			}
		}
		let world = (
			pos.x as f32 * 16.0,
			pos.y as f32 * 16.0,
			pos.z as f32 * 16.0,
		);
		// Not the cell center: `light_origin` puts a shaped emitter's point
		// source where its geometry actually glows (the torch's flame, not
		// its handle), and answers 0.5 for every full cube.
		let list: Vec<_> = gi
			.emitters
			.iter()
			.map(|&(x, y, z, class)| {
				let o = digblock_core::gi::light_origin(class);
				(
					world.0 + f32::from(x) + o[0],
					world.1 + f32::from(y) + o[1],
					world.2 + f32::from(z) + o[2],
					class,
				)
			})
			.collect();
		let changed = if list.is_empty() {
			self.emitters.remove(&pos).is_some()
		} else {
			self.emitters.insert(pos, list);
			true
		};
		self.lights_dirty |= changed;
	}

	/// The voxel window's radius in chunks, AS BUILT.
	///
	/// The streaming side must key off this and not off the live
	/// `config.draw_dist`, which the settings screen mutates at runtime
	/// while the window texture keeps the size it was created with.
	/// Approving a chunk outside the built window hands `upload_chunk` a
	/// position whose `rem_euclid` wraps onto a DIFFERENT cell than the
	/// gather's does — silently writing one part of the world into
	/// another's slot, which is exactly the aliasing `gi_uploadable`
	/// exists to prevent.
	pub fn window_chunks(&self) -> i32 {
		self.window_chunks as i32
	}

	/// Forget an evicted chunk's emitters.
	///
	/// Nothing used to prune this map: the only removal was a chunk
	/// re-classifying to zero emitters, so every torch ever seen stayed in
	/// it for the process's life and a world swap carried the previous
	/// world's forward. `flush_lights` walks the whole map every frame
	/// before its early-out, and past `MAX_LIGHTS` a stale entry can
	/// out-sort a live one.
	///
	/// Deliberately does NOT touch the coarse mirror. The window is
	/// TOROIDAL and much smaller than residency, so an evicted chunk's
	/// `rem_euclid` slot is very likely owned by a different, live chunk
	/// by now — clearing it would erase real occupancy. (And it would buy
	/// nothing: the drill in BENCH.md 2026-08-12 rebuilt `sky_y` to the
	/// same value with frame time unmoved.)
	pub fn remove_chunk(&mut self, pos: ChunkPos) {
		if self.emitters.remove(&pos).is_some() {
			self.lights_dirty = true;
		}
	}

	/// Drop every piece of GI state that only ever GROWS, so the next
	/// upload sweep rebuilds it from what is actually resident (F7).
	///
	/// This is a diagnostic, not a fix. The coarse mirror and the emitter
	/// map are never pruned on eviction, so both ratchet as a session
	/// explores: `sky_y` climbs toward the tallest terrain seen ANYWHERE
	/// rather than the tallest currently in the window, and every ray in
	/// the resolve pays for the difference. Pair with `gi_redirty_all` on
	/// the pipeline — this clears, that re-queues.
	///
	/// If frame time recovers after pressing F7, the ratchet is real and
	/// the fix belongs in eviction, not here.
	pub fn clear_derived_state(&mut self) {
		self.coarse_mirror.fill(0);
		self.coarse_mips_dirty = true;
		self.emitters.clear();
		self.carried.clear();
		self.lights_dirty = true;
	}

	/// The sky ceiling and the emitter-cell count, for the debug overlay
	/// and the CSV. Both ratchet as the world is explored (the coarse
	/// mirror and the emitter map are never pruned on eviction), which is
	/// exactly why they are worth showing: they are the two pieces of GI
	/// state that make a settled session cost more than a fresh one.
	pub fn debug_state(&self) -> (f32, u32) {
		(
			self.sky_y,
			// Carried lights are counted too, and not for completeness:
			// this field is how you answer "is the torch in my hand
			// actually in the list" without a debugger.
			(self.emitters.values().map(Vec::len).sum::<usize>() + self.carried.len()) as u32,
		)
	}

	/// Queue an edit position for probe invalidation (age reset around
	/// it, ≤4 spheres applied per frame).
	pub fn invalidate(&mut self, pos: [f32; 3]) {
		self.queue_invalidation(pos, false);
	}

	/// The same, for a light that MOVED rather than a cell that changed —
	/// a player carrying a torch.
	///
	/// Identical to the probes, which is the whole design: re-placement,
	/// not a second mechanism. The flag rides along only so the resolve's
	/// four-slot screen-cap ring can keep the two streams from starving
	/// each other; see `resolve_inval`.
	pub fn invalidate_moving(&mut self, pos: [f32; 3]) {
		self.queue_invalidation(pos, true);
	}

	fn queue_invalidation(&mut self, pos: [f32; 3], moving: bool) {
		// Bounded: only 4 spheres are drained per frame, and an unbounded
		// queue grows without limit under any burst of edits (a fluid
		// cascade, a bulk placement) and then spends minutes replaying
		// invalidations for cells that have since changed again. Dropping
		// the OLDEST is right — the newest edits are the ones whose light
		// is most wrong.
		const INVAL_MAX: usize = 256;
		if self.inval_queue.len() >= INVAL_MAX {
			self.inval_queue.remove(0);
		}
		self.inval_queue.push((pos, moving));
	}

	/// Rebuild the flattened light list when it changed: the nearest
	/// MAX_LIGHTS emitters to the camera, count in the header.
	///
	/// Order never matters to a reader — the probes sample uniformly and
	/// the resolve's NEE scans actual positions — so a stale SELECTION
	/// only bites through the MAX_LIGHTS truncation: with more emitters
	/// than slots, "nearest to a camera that has since left" quietly
	/// drops the torches around the player. The refresh below re-selects
	/// on real movement in exactly that oversubscribed case, and stays
	/// free in the common far-under-MAX_LIGHTS world. Nothing is sorted:
	/// `select_lights` partitions, and under the cap does not even do
	/// that.
	fn flush_lights(&mut self, gpu: &Gpu, cam: [f32; 3], carried: &[Emitter]) {
		// Only emitters a march can actually REACH belong in the list.
		// Every trace breaks at `gi_lateral_max` (= side/2 − 1) of the
		// camera, so an emitter past it cannot light anything however
		// close it sorts — and with more than MAX_LIGHTS emitters it
		// displaces one that can. Residency runs to draw_dist + 1 chunks
		// while the window is draw_dist chunks on a SIDE, so "resident"
		// reaches more than twice as far as "traceable" and this is not a
		// rare edge.
		let reach = lateral_max(self.window_chunks);
		let in_reach = |e: &(f32, f32, f32, u8)| {
			let (dx, dz) = (e.0 - cam[0], e.2 - cam[2]);
			dx * dx + dz * dz <= reach * reach
		};
		// Carried lights count toward `total` like any other: they are in
		// the list the truncation applies to, so leaving them out of the
		// tally would let a full world hide one.
		let total: usize = self
			.emitters
			.values()
			.map(|v| v.iter().copied().filter(&in_reach).count())
			.sum::<usize>()
			+ carried.iter().copied().filter(&in_reach).count();
		let moved = {
			let d = [
				cam[0] - self.lights_cam[0],
				cam[1] - self.lights_cam[1],
				cam[2] - self.lights_cam[2],
			];
			d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > 16.0 * 16.0
		};
		// Any real move can change WHICH emitters are in reach, not just
		// which MAX_LIGHTS of them are nearest — so the oversubscription
		// clause is no longer the only reason to re-flush. Under the cap
		// this costs one walk and a ≤4 KiB write per 16 blocks walked.
		let stale = moved && (total > MAX_LIGHTS || total != self.lights_len);
		// A carried light moves with its carrier, so the list is stale the
		// moment one does. EXACT equality, not an epsilon, and that is the
		// load-bearing part: at rest the presentation eye is bit-identical
		// frame to frame (`TickLerp::render_pos` interpolates as
		// `prev + (curr - prev) * t`, which returns `prev` exactly when
		// `prev == curr` — pinned by `a_resting_camera_does_not_drift`,
		// because the `a*(1-t) + b*t` form it replaced does NOT and left a
		// still player re-uploading on roughly one frame in seven — and
		// the reconciliation offset snaps to zero rather than decaying
		// forever). So a player standing still with a torch keeps the
		// early-out and a room lit by it converges as deeply as one lit
		// from the floor. An epsilon would be inventing tolerance for a
		// quantity that has none, and the number chosen would quietly
		// decide when a slow creep stops lighting the room.
		//
		// Compared AFTER the reach filter, which is not a detail: remotes
		// ride snapshots out to the client's draw distance (320 blocks at
		// 20) while `reach` is 159, so an unfiltered compare lets a
		// torch-bearer nobody can be lit by defeat the early-out on every
		// single frame and re-upload a byte-identical buffer.
		let carried_moved = !carried
			.iter()
			.copied()
			.filter(&in_reach)
			.eq(self.carried.iter().copied());
		if !self.lights_dirty && !stale && !carried_moved {
			return;
		}
		self.carried.clear();
		self.carried
			.extend(carried.iter().copied().filter(&in_reach));
		self.lights_dirty = false;
		self.lights_cam = cam;
		let all = select_lights(
			self.emitters.values().flatten().copied(),
			&self.carried,
			cam,
			reach,
		);
		let count = all.len() as u32;
		self.lights_len = all.len();
		let mut bytes = Vec::with_capacity(16 + all.len() * 16);
		bytes.extend_from_slice(bytemuck::cast_slice(&[count, 0, 0, 0]));
		bytes.extend_from_slice(bytemuck::cast_slice(&all));
		gpu.queue.write_buffer(&self.lights_buf, 0, &bytes);
	}

	/// Encode this frame's probe updates (both cascades) at the head of
	/// the frame's encoder: full prev→out copy, then the amortized
	/// trace over PROBES_PER_DISPATCH probes, then edit invalidation.
	pub fn encode(
		&mut self,
		gpu: &Gpu,
		encoder: &mut wgpu::CommandEncoder,
		timer: Option<&GpuTimer>,
		frame: &FrameUniforms,
		carried: &[Emitter],
	) {
		let cam = [frame.cam_pos[0], frame.cam_pos[1], frame.cam_pos[2]];
		self.flush_lights(gpu, cam, carried);
		// Rescan the sky ceiling when the mirror moved (R18).
		if self.coarse_mips_dirty {
			self.coarse_mips_dirty = false;
			let (w, h) = ((self.window_chunks * 2) as usize, (WORLD_H / 8) as usize);
			self.sky_y = sky_ceiling(&self.coarse_mirror, w, h, w);
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
			// The sweep is PERMUTED, not contiguous. `cursor` walks the
			// sweep index; the probe it selects is `(k * stride) % count`
			// with stride coprime to count, so a full cycle still touches
			// every probe exactly once while consecutive dispatches land
			// far apart. A contiguous cursor made convergence sweep the
			// volume as a visible band — dark regions appearing and
			// clearing at random-looking moments while flying.
			//
			// The base is reduced here because `cursor * stride` would
			// overflow u32 in the shader; `wg.x * stride` cannot, since
			// the stride is clamped against PROBES_PER_DISPATCH.
			let base = ((u64::from(c.cursor) * u64::from(c.stride)) % u64::from(c.probes())) as u32;
			data.extend_from_slice(bytemuck::cast_slice(&[
				base,
				self.frame,
				MAX_LIGHTS as u32,
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

	/// Draw the debug raymarch over the whole target (`DIG_GI=`).
	pub fn encode_debug(
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
		// misc.x: 1 = shade hits with the C0 probe field (DIG_GI=probes).
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
		});
		pass.set_pipeline(&self.debug_pipeline);
		// The front atlas is the one the flip just published.
		pass.set_bind_group(0, &self.debug_bind[self.ping], &[]);
		pass.draw(0..3, 0..1);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `flush_lights` drops emitters outside the reach every march breaks
	/// at, so its bound has to BE the shader's. Read `gi_lateral_max` out
	/// of gi_dda.wgsl rather than restating it: a hand-copied twin is how
	/// this codebase found the same stepper bug three times, and drifting
	/// here silently hides a torch the shader would have traced.
	#[test]
	fn the_light_reach_matches_the_shaders_bound() {
		let src = digblock_assets::shader::GI_DDA;
		let body = src
			.split("fn gi_lateral_max(side: f32) -> f32 {")
			.nth(1)
			.expect("gi_dda.wgsl must declare gi_lateral_max(side)");
		let expr = body
			.split(';')
			.next()
			.expect("gi_lateral_max must have a return")
			.replace("return", "")
			.replace(char::is_whitespace, "");
		assert_eq!(
			expr, "side*0.5-1.0",
			"gi_lateral_max changed shape — update `lateral_max` to match"
		);
		for wc in [8u32, 20, 32] {
			let side = (wc * 16) as f32;
			assert_eq!(lateral_max(wc), side * 0.5 - 1.0, "window {wc}");
		}
	}

	/// A carried torch is an entry in the ordinary light list, so the
	/// only thing that needs pinning is that the ordinary rules land it
	/// in the right place: nearest-first sorting must make the carrier's
	/// own light unloseable, and the reach filter must drop a carrier who
	/// walked out of the window exactly as it drops a placed torch.
	#[test]
	fn a_carried_light_is_a_light_like_any_other() {
		let cam = [100.0, 64.0, 100.0];
		let reach = lateral_max(20);
		let class = digblock_core::gi::gi_class(digblock_core::chunk::make_cell(
			digblock_core::block::BlockType::Torch,
			0,
		));
		// A full list of placed torches, every one of them in reach and
		// none of them at the camera.
		let placed: Vec<_> = (0..MAX_LIGHTS)
			.map(|i| (cam[0] + 2.0 + i as f32 * 0.01, cam[1], cam[2], class))
			.collect();
		let mine = digblock_core::gi::carried_origin(glam::Vec3::from_array(cam));
		let carried = [(mine.x, mine.y, mine.z, class)];
		let out = select_lights(placed.iter().copied(), &carried, cam, reach);
		assert_eq!(out.len(), MAX_LIGHTS, "the list still fills exactly once");
		let entry = out
			.iter()
			.find(|e| [e[0], e[1], e[2]] == mine.to_array())
			.expect(
				"the light in your own hand is the nearest thing there is — a \
				 full world must not be able to truncate it away",
			);
		assert_eq!(
			entry[3].to_bits() & 255,
			u32::from(class),
			"a carried torch carries the same class byte a placed one does, \
			 so tint and level come from one LUT entry"
		);
		assert_eq!(
			entry[3].to_bits() & LIGHT_CARRIED,
			LIGHT_CARRIED,
			"and the carried flag on top, which is the only thing that \
			 distinguishes it — see LIGHT_CARRIED for why the margin needs it"
		);
		// The placed ones must NOT wear it, or every torch in the world
		// stops stopping its own shadow ray short of its own cell.
		assert!(
			out.iter()
				.filter(|e| [e[0], e[1], e[2]] != mine.to_array())
				.all(|e| e[3].to_bits() & LIGHT_CARRIED == 0),
			"a placed torch is not a carried one"
		);
		// Out past the reach every march breaks at: dropped, like any
		// other emitter no ray could have traced.
		let far = [(cam[0] + reach + 1.0, cam[1], cam[2], class)];
		assert!(select_lights(std::iter::empty(), &far, cam, reach).is_empty());
	}

	/// The carried flag is packed here and unpacked in gi_dda.wgsl, which
	/// both NEE passes take as a prelude. Nothing but agreement makes the
	/// two halves one thing, so agreement is what gets pinned.
	#[test]
	fn the_carried_flag_matches_the_shaders() {
		let decl = format!("const GI_LIGHT_CARRIED = {LIGHT_CARRIED}u;");
		assert!(
			digblock_assets::shader::GI_DDA.contains(&decl),
			"gi_dda.wgsl must declare `{decl}`"
		);
		// And it has to stay clear of the class byte it shares the word
		// with, which is what makes the mask legal at all. The other side
		// needs no assert: `Emitter`'s class is a u8.
		assert_eq!(LIGHT_CARRIED & 255, 0);
	}

	/// The ceiling must sit at the TOP of the highest occupied brick (a
	/// ceiling one brick low un-shadows every hilltop), and an empty
	/// mirror must answer "nothing anywhere" rather than 255 (which
	/// would disable the early-out forever on an ocean world).
	#[test]
	fn the_sky_ceiling_tops_the_highest_brick() {
		let (w, h, d) = (40usize, 32, 40);
		let mut mirror = vec![0u8; w * h * d];
		assert_eq!(sky_ceiling(&mirror, w, h, d), -1.0);
		mirror[(17 * h + 9) * w + 33] = 0b01;
		assert_eq!(sky_ceiling(&mirror, w, h, d), (9 * 8 + 7) as f32);
		// A higher EMITTER-only brick raises it too — the ceiling gates
		// hits of any kind.
		mirror[(2 * h + 24) * w + 5] = 0b10;
		assert_eq!(sky_ceiling(&mirror, w, h, d), (24 * 8 + 7) as f32);
	}

	/// The sweep must be a PERMUTATION, not merely a shuffle: over one
	/// full cycle every probe has to be traced exactly once, or some
	/// probes never converge and others burn budget twice. That is the
	/// whole reason the stride has to be coprime with the count, and it
	/// is the kind of property that fails silently — the picture just
	/// keeps a stale patch somewhere.
	#[test]
	fn the_permuted_sweep_visits_every_probe_exactly_once() {
		for dims in [C0_DIMS, C1_DIMS, (1, 1, 1), (7, 5, 3), (16, 16, 16)] {
			let count = dims.0 * dims.1 * dims.2;
			let stride = schedule_stride(count);
			let mut seen = vec![false; count as usize];
			for k in 0..count {
				let pi = ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
				assert!(!seen[pi as usize], "{dims:?}: probe {pi} traced twice");
				seen[pi as usize] = true;
			}
			assert!(
				seen.iter().all(|&s| s),
				"{dims:?}: a probe was never traced"
			);
		}
	}

	/// `wg.x * stride` is computed in u32 IN THE SHADER, where an
	/// overflow silently wraps and the permutation stops being one.
	#[test]
	fn the_stride_cannot_overflow_the_shaders_multiply() {
		for dims in [C0_DIMS, C1_DIMS, (64, 64, 64), (1, 1, 1)] {
			let count = dims.0 * dims.1 * dims.2;
			let stride = schedule_stride(count);
			assert!(
				u64::from(stride) * u64::from(PROBES_PER_DISPATCH) <= u64::from(u32::MAX),
				"{dims:?}: stride {stride} overflows"
			);
		}
	}

	/// The history format must be able to REPRESENT the increment its
	/// history length asks for. Twice now it could not: rgba8 first, then
	/// rgba16f, both of which let `hi + (x - hi)/N_MAX` round to zero and
	/// froze the running average at whatever it held — permanently, per
	/// pixel, in a pattern locked to wherever the player had walked. It
	/// took a capture harness that can see motion to notice either time,
	/// because a still has already converged.
	///
	/// So the two knobs are pinned against each other. Blending at alpha
	/// 1/N stalls once the residual falls under about N ulps of the stored
	/// value; below one 8-bit output LSB that is invisible, and above it
	/// the picture keeps a stale ghost. Raising N_MAX or dropping the
	/// format now fails here instead.
	#[test]
	fn history_format_resolves_the_accumulator() {
		// N_MAX lives in the shader; read it rather than duplicating it,
		// so the two cannot drift.
		let src = digblock_assets::shader::GI_RESOLVE;
		let n_max: f32 = src
			.split("const N_MAX = ")
			.nth(1)
			.and_then(|s| s.split(';').next())
			.and_then(|s| s.trim().parse().ok())
			.expect("gi_resolve.wgsl must declare `const N_MAX = <f32>;`");

		// Explicit mantissa bits per format — the property under test, not
		// something to infer from a byte count.
		let mantissa_bits = match GI_ACC_FORMAT {
			wgpu::TextureFormat::Rgba32Float => 23,
			wgpu::TextureFormat::Rgba16Float => 10,
			wgpu::TextureFormat::Rgba8Unorm => 8,
			other => panic!("unknown accumulator format {other:?} — add its mantissa here"),
		};
		// Around a stored value of 1.0 (radiance and visibility both sit
		// there), an update rounds away once d/N < ulp/2.
		let stall = n_max * 2f32.powi(-(mantissa_bits + 1));
		let one_lsb = 1.0 / 255.0;
		assert!(
			stall < one_lsb,
			"{GI_ACC_FORMAT:?} with N_MAX {n_max} freezes the average while it is still \
			 {stall:.4} out (one 8-bit LSB is {one_lsb:.4}) — the running mean stops moving \
			 and every pixel keeps whatever it held when the camera stopped"
		);
	}

	/// Read a `const NAME = <number>;` out of a WGSL source (the pins
	/// below hold shader constants against Rust ones, so they read the
	/// shader rather than duplicating the number).
	fn wgsl_f32(src: &str, name: &str) -> f32 {
		let pat = format!("const {name} = ");
		let at = src
			.find(&pat)
			.unwrap_or_else(|| panic!("{name}: not declared"));
		let rest = &src[at + pat.len()..];
		let end = rest
			.find(';')
			.unwrap_or_else(|| panic!("{name}: unterminated"));
		rest[..end]
			.trim()
			.parse()
			.unwrap_or_else(|e| panic!("{name}: not a number: {e}"))
	}

	/// The sun target's g channel packs `code + 256 * n`, and every reader
	/// unpacks with integer bit-ops — which only works while the packed
	/// integer is EXACT in the accumulator format. rgba32f holds integers
	/// to 2^24; someone dropping the target to rgba16f "because the sun
	/// history is short now" would corrupt the pack (and the indirect
	/// counter inside it, which still climbs to N_MAX) long before the
	/// average itself froze.
	#[test]
	fn packed_metadata_stays_exact() {
		let n_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_MAX");
		let mantissa_bits = match GI_ACC_FORMAT {
			wgpu::TextureFormat::Rgba32Float => 23,
			wgpu::TextureFormat::Rgba16Float => 10,
			wgpu::TextureFormat::Rgba8Unorm => 8,
			other => panic!("unknown accumulator format {other:?} — add its mantissa here"),
		};
		let packed_max = 255.0 + 256.0 * n_max;
		let exact_to = 2f32.powi(mantissa_bits + 1);
		assert!(
			packed_max < exact_to,
			"code + 256*N_MAX ({packed_max}) does not round-trip in {GI_ACC_FORMAT:?} \
			 (integers are exact only below {exact_to}) — the face code and history \
			 length come back corrupted"
		);
	}

	/// The resolve turns the occluder distance into a penumbra width with
	/// SUN_TAN, which must be the tangent of the SAME cone the per-pixel
	/// direction offset samples (SUN_CONE_COS) — otherwise the dithered
	/// edge band and the reconstruction kernel disagree about how wide the
	/// penumbra is, and one of them shows.
	#[test]
	fn the_penumbra_knows_the_cone() {
		let sun_tan = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "SUN_TAN");
		let want = SUN_CONE_COS.acos().tan();
		let rel = (sun_tan - want).abs() / want;
		assert!(
			rel < 0.01,
			"SUN_TAN ({sun_tan}) is not tan(acos(SUN_CONE_COS)) = {want} — the cone and \
			 the reconstruction width have drifted apart"
		);
	}

	/// The widest penumbra the resolve will store (PEN_MAX) must fit
	/// inside what the filter can actually spread: R taps per side, per
	/// iteration, at that iteration's stride, scaled by at most
	/// SUN_STRIDE_MAX. Widening one constant without the other leaves
	/// shadows that claim a width the kernel never delivers.
	#[test]
	fn the_penumbra_fits_the_filter() {
		let pen_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PEN_MAX");
		let stride_max = wgsl_f32(digblock_assets::shader::GI_ATROUS, "SUN_STRIDE_MAX");
		let reach: f32 = ATROUS_STEPS.iter().map(|s| s * stride_max).sum();
		assert!(
			pen_max <= reach,
			"PEN_MAX ({pen_max}) exceeds the filter's combined support ({reach} texels: \
			 strides {ATROUS_STEPS:?} x SUN_STRIDE_MAX {stride_max})"
		);
	}

	/// The disocclusion prior primes a fresh pixel's history from the
	/// probe cache. It must stay under both caps that could otherwise be
	/// asked to correct it: a primed pixel that is immediately moving
	/// lives at N_MOVING, and the sun alpha rides min(n, N_SUN) — a prior
	/// claiming more confidence than either would slow the very signals
	/// the short caps exist to keep responsive.
	#[test]
	fn the_disocclusion_prior_stays_under_the_caps() {
		let n_prior = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_PRIOR");
		let n_moving = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_MOVING");
		let n_sun = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_SUN");
		assert!(n_prior > 1.0, "a prior of 1 is no prior at all");
		assert!(
			n_prior <= n_sun && n_prior < n_moving,
			"N_PRIOR ({n_prior}) must stay under N_SUN ({n_sun}) and N_MOVING ({n_moving})"
		);
		// And its two gates must exist as real gates: a margin of ≤1
		// would mark every depth rejection as grazing (the prime never
		// fires), a sun ceiling of ≥1 would prime fully sun-lit pixels
		// (measured as washed-out AO across the walk and fly regimes).
		let margin = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PRIME_MARGIN");
		let sun_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PRIME_SUN_MAX");
		assert!(margin > 1.0, "PRIME_MARGIN ({margin}) gates nothing at ≤1");
		assert!(
			sun_max > 0.0 && sun_max < 1.0,
			"PRIME_SUN_MAX ({sun_max}) must partition lit from unlit"
		);
	}

	/// The LUT's packed sub-box words must decode in the shader to the
	/// twelfths the shape authored — `gi_boxes_hit` unpacks min from bits
	/// 0/4/8 and max from 12/16/20. The core side pins the same format
	/// from the packer's end (`box_pack_matches_the_shader_decode`); this
	/// end pins that the LUT actually carries the boxes: every synthetic
	/// class entry is opacity 3 with a nonzero first box, borrowing its
	/// base block's light level.
	#[test]
	fn the_lut_carries_the_shape_covers() {
		let lut = material_lut(&[]);
		let base = digblock_core::gi::SHAPE_CLASS_BASE as usize;
		let classes = digblock_core::gi::shape_classes();
		assert!(!classes.is_empty());
		for (i, sc) in classes.iter().enumerate() {
			let e = lut[base + i];
			assert_eq!(e[0] >> 24, 3, "synthetic class {i} is not partial");
			assert_ne!(e[2], 0, "synthetic class {i} lost its first box");
			assert_eq!(e[1], u32::from(sc.base.props().light));
		}
		// And the torch's emitting class is synthetic now — the light
		// list names classes, so the level has to ride along.
		let torch = digblock_core::gi::gi_class(BlockType::Torch as u16);
		assert_eq!(lut[torch as usize][1], 14);
	}

	/// A contiguous cursor is what made convergence sweep as a visible
	/// band. Consecutive sweep steps must land far apart in the probe
	/// grid — this is the property, not an implementation detail.
	#[test]
	fn consecutive_sweep_steps_are_not_neighbours() {
		let count = C0_DIMS.0 * C0_DIMS.1 * C0_DIMS.2;
		let stride = schedule_stride(count);
		let probe = |k: u32| ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
		let mut worst = u32::MAX;
		for k in 0..count.min(4096) {
			let d = probe(k).abs_diff(probe(k + 1));
			worst = worst.min(d.min(count - d));
		}
		assert!(
			worst > count / 8,
			"consecutive steps land {worst} apart out of {count} — that is a band"
		);
	}
}
