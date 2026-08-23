//! RTGI probe volume (GI-DESIGN.md). P1 built the voxel window the
//! rays march; P2 makes the probes real: two toroidal 6-axis
//! ambient-cube cascades (C0 fine and near, C1 coarse and far),
//! ping-ponged rgba16float atlases updated by an amortized compute
//! trace (spherical-Fibonacci rays, coarse-skip DDA, sun + sky +
//! emissive radiance, infinite bounce by sampling last frame's atlas
//! at hits), an r32uint flags texture carrying per-probe age and the
//! toroidal self-heal checksum, and an edit-invalidation dispatch.
//! P3 added the half-res resolve pass (gi_resolve.wgsl) — RTGI is the
//! only lighting path; there is no flag.

use digblock_core::block::BlockType;
use digblock_core::chunk::ChunkPos;
use digblock_core::gi::{GiChunk, GiOpacity, opacity};
use digblock_platform::Gpu;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{GpuTimer, Q_GI, Q_GI_TRACE};
use crate::renderer::FrameUniforms;

/// C0 probe grid (the near cascade; see GI-DESIGN.md).
mod build;
mod debug;
mod lights;
mod probes;
mod resolve;
mod window;

pub(crate) use build::GiConfig;
pub(crate) use resolve::{GI_GATHER_ITERS, GI_SUN_ITERS};

pub(crate) const C0_DIMS: (u32, u32, u32) = (32, 24, 32);
/// C0's probe spacing in blocks — the finest scale the bounce field can
/// resolve, and therefore the granularity a moving emitter has to be
/// re-placed at (`engine::CARRY_INVAL_STEP`).
pub(crate) const C0_SPACING: u32 = 2;
/// C1 far cascade (the coarse spacing).
pub(crate) const C1_DIMS: (u32, u32, u32) = (32, 20, 32);
/// Probes traced per cascade per frame (the amortization grain).
const PROBES_PER_DISPATCH: u32 = 512;
/// World height in blocks — the window's fixed Y extent (an up-ray
/// always reaches sky; the window never scrolls vertically).
const WORLD_H: u32 = 256;
/// Light-list capacity (GI-DESIGN.md): the buffer allocation, the probe uniform and the shader
/// clamps are all sized to it at compile time. The RUNTIME cap (`Settings::max_lights`) can only
/// sit at or below it, which is what keeps a slider move a header rewrite instead of a
/// reallocation. `pub(crate)` so settings derives its slider maximum from the one capacity.
pub(crate) const MAX_LIGHTS: usize = 256;

/// How far ahead of the camera the selection anchor leads, in blocks — the shader's `LIGHT_RANGE`,
/// pinned by `the_selection_lead_matches_the_shaders_light_range`. Under oversubscription the
/// survivors are the nearest to the SEGMENT from eye to this far ahead, so vein glow behind the
/// player loses its slots before the torches in front, while anything close to the eye — a carried
/// light above all — still measures from the eye. The reach filter stays camera-centred.
const SELECT_AHEAD: f32 = 24.0;

/// One entry in the light list: world position and the GI class byte whose LUT row carries its
/// cast level and its tint. A light a player is CARRYING is one of these too — it just isn't in a
/// chunk, and the only readers that can tell are `gi_light_margin_of`, because the margin is a
/// question about the light's CELL and a carried one has none, and [`Self::level`], which only a
/// carried light ever sets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Emitter {
	/// Where the light radiates from: a point for a lamp, the CENTRE of the rectangle for a
	/// [`Self::face`] emitter. One field for both because everything that reasons about a light
	/// without shading it — the reach filter, the distance selection, the reservoir's candidate
	/// score — wants a representative point and nothing more.
	pub pos: [f32; 3],
	pub class: u8,
	/// Cast level for THIS entry, or 0 to take the class's own. A placed emitter is always 0: a
	/// torch is a torch wherever it stands, so one number in its LUT row serves every copy. The
	/// exception is a tool whose wielder has earned a brighter one (`core::gi::carried_light`) — a
	/// property of the HOLDER and therefore of the entry, since there is no second class to give
	/// it and the same pick in two different hands must be free to cast differently.
	pub level: u8,
	/// Draw this entry's tint pulled toward white
	/// (`core::gi::CarriedLight::whitened`). Per entry for the same
	/// reason the level is: the class byte is shared by every copy of the
	/// tool in the world, and this one belongs to who is holding it.
	pub whitened: bool,
	/// The rectangle this light radiates from, for a SURFACE emitter — a revealed vein's exposed
	/// face, merged by `mesher::lights`. `None` is a point source. This is the one thing about an
	/// entry that changes how the estimator WORKS rather than what it is worth: a point
	/// approximating a patch of glowing rock is a hotspot at the wall and has no cosine at its own
	/// surface, so it spills light around the corner of the rock it sits IN (GI-DESIGN.md R31).
	pub face: Option<Face>,
}

/// An axis-aligned emissive rectangle: which way it faces and how big it is. Its position is the
/// [`Emitter::pos`] it rides with. Voxel faces are axis-aligned by construction, so this is four
/// numbers rather than a basis — and the shader rebuilds the two tangents the same way
/// `mesher::lights::tangents` does, in ascending axis order, so the packer and the sampler cannot
/// disagree about which extent is which.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Face {
	/// The axis the outward normal lies along: 0, 1 or 2 for x, y, z.
	pub axis: u8,
	/// Outward normal points UP that axis rather than down it.
	pub positive: bool,
	/// Full extent in blocks along the two tangent axes, ascending.
	pub ext: [f32; 2],
}

/// The flag riding above the class byte in a light entry's packed `w` word: this light is being
/// CARRIED. It exists for one reason — `gi_light_margin` is how far short of a light its shadow
/// ray stops, derived from the light's position WITHIN ITS CELL, and a carried light hangs on its
/// carrier's capsule axis instead, landing on the degenerate maximum where the margin swallows a
/// whole wall. Pinned against the WGSL twin by `the_carried_flag_matches_the_shaders`.
const LIGHT_CARRIED: u32 = 256;

/// The LUT's tinted-transmission op — a see-through class whose rgb
/// filter rides word `.z`. Named because THREE shaders branch on it and
/// nothing else pins them together; `the_lut_ops_match_the_shaders`
/// holds the four of them to one value.
const LUT_OP_TINTED: u32 = 4;

/// Where [`Emitter::level`] rides in the same packed word, above the class byte and the carried
/// flag. Zero means "no override" — the LUT row's own level stands — so a placed emitter's word is
/// bit-identical to what it was before per-entry levels existed. Four bits, which is every level
/// the game authors: `emits` runs to the brightest lamp and no further.
/// Pinned against the WGSL twin by `the_carried_flag_matches_the_shaders`.
const LIGHT_LEVEL_SHIFT: u32 = 9;
pub(crate) const LIGHT_LEVEL_MASK: u32 = 15;

/// [`Emitter::whitened`]'s flag, above the level field — the entry's second per-holder property,
/// packed rather than classed for the same reason. HOW FAR toward white is not here:
/// `GI_LIGHT_WHITEN` lives in gi_dda.wgsl, the only thing that applies it (`gi_light_tint`, shared
/// so both NEE passes agree), and a mirror here would be a second copy of a value with one user.
/// Pinned against the WGSL twin by `the_carried_flag_matches_the_shaders`.
const LIGHT_WHITENED: u32 = 8192;

/// [`Emitter::face`]'s flag: this entry is a RECTANGLE, and the second half of its pair carries
/// the extents and the facing. It is what makes the light list heterogeneous — punctual and area
/// sources in one array — and every reader branches on it exactly once, inside `gi_light_sample`.
/// Punctual entries leave it clear and their second vec4 unused, so a torch's word is unchanged.
/// Pinned against the WGSL twin by `the_carried_flag_matches_the_shaders`.
const LIGHT_QUAD: u32 = 16384;

/// Format of the two ping-ponged resolve targets — BOTH of which hold a running average, which is
/// the whole constraint. rgba8 could not: at full history each frame contributes one `N_MAX`-th of
/// a difference, which rounds to zero in 8 bits. rgba16f failed the SAME way one format up, every
/// pixel freezing where it stood when the camera stopped. The rule is not "use a float format" but
/// that the mantissa must carry the history's smallest increment: 1/N stalls under ~N ulps.
pub(crate) const GI_ACC_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

// The a-trous tap strides, one entry per iteration. Doubling the stride
// instead of growing the kernel is the whole trick: these strides of a B3
// spline kernel cover a footprint far wider than the taps they cost. A
// further iteration is deliberately not here — its reach on a half-res
// buffer is most of a shadow.
const ATROUS_STEPS: [f32; 2] = [1.0, 2.0];

/// Cosine of the sun cone's half-angle for the per-pixel shadow ray. Deliberately wider than the
/// real sun's disc: it buys a penumbra sharp at contact that softens with distance to the occluder
/// at no extra ray, and widening it further makes shadows mushy AND noisier at one sample.
const SUN_CONE_COS: f32 = 0.999_961_9; // cos(0.5 degrees)

/// How long a consumed edit sphere keeps capping the resolve's screen history (frames). A lighting
/// edit changes radiance on surfaces whose face code and depth never change, and a resting pixel
/// at full history admits the change at one `N_MAX`-th a frame — a placed torch's pool would fade
/// in over seconds. The probes re-trace an age-reset probe on the sweep's next visit, so two full
/// sweep periods hold the screen at the short-history rate until the cache underneath settles.
const RESOLVE_INVAL_FRAMES: u32 = 96;
/// Screen-cap radius around an edit, in blocks: C1's invalidation radius,
/// which is also about how far a torch reaches.
const RESOLVE_INVAL_RADIUS: f32 = 16.0;

/// Below this cos between consecutive caster directions, the sun did not
/// drift — it was TELEPORTED (`/time`). The threshold sits far above what
/// the world clock moves in a frame and far below any admin jump.
const SUN_JUMP_COS: f32 = 0.995;
/// Sun-direction quantization for GI rays: coarse enough that shadow work
/// stays still between steps, fine enough that hysteresis glides across
/// them.
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
		if b == 0 { a } else { gcd(b, a % b) }
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
	if gcd(s, count.max(1)) != 1 { 1 } else { s }
}

pub(crate) struct GiDraw {
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
	/// land. One byte per coarse brick, so it stays small.
	coarse_mirror: Box<[u8]>,
	/// The mirror changed this frame; `sky_y` rescans at the next
	/// `encode`.
	coarse_mips_dirty: bool,
	/// Highest occupied cell y anywhere in the window, in blocks (R18):
	/// an ASCENDING ray above this is proven lit/sky in one float
	/// compare, so the sun march stops at the tallest local hill instead
	/// of climbing to the top of the world — recomputed with the skip
	/// mips. Sits at the world ceiling until the first flush
	/// (conservative: never breaks a ray early on unclassified data).
	sky_y: f32,
	/// XZ extent in chunks (= draw distance): the toroidal modulus.
	window_chunks: u32,
	/// Cost-stub mask (DIG_STUB) — rides `jitter.w` into gi_resolve.wgsl,
	/// which has no frame UBO of its own to read `misc.z` from.
	stub: u32,
	/// The resolve's ray budgets (`resolve::GI_GATHER_ITERS`/
	/// `GI_SUN_ITERS` unless the config's tier lever moved them) — ride
	/// `rz.budget` the same way the stub mask rides `jitter.w`.
	gather_iters: u32,
	sun_iters: u32,
	emitters: FxHashMap<ChunkPos, Vec<Emitter>>,
	/// Surface emitters per chunk — a revealed vein's exposed faces, merged
	/// into rectangles by the MESHER and delivered on its output rather
	/// than derived here (`mesher::lights` says why at length; the short
	/// version is that the chunk store holds Stone where a vein is, so
	/// `classify_chunk` cannot see one).
	///
	/// A second map beside `emitters` rather than entries in it, because
	/// the two arrive on different paths and at different times: one rides
	/// the GI classify sweep, the other the mesh job. `flush_lights` chains
	/// them, and the light list neither knows nor cares which is which.
	surfaces: FxHashMap<ChunkPos, Vec<Emitter>>,
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
	/// Selection anchor (`select_anchor`) at the last light-list flush —
	/// see flush_lights. An anchor rather than the camera position so a
	/// heading swing while oversubscribed re-flushes on the same rule a
	/// walk does: either one moves the anchor.
	lights_anchor: [f32; 3],
	/// Emitters written at that flush, so a move that changes WHICH are in
	/// reach re-flushes even under the cap.
	lights_len: usize,
	/// The runtime cap the list was last flushed under
	/// (`Settings::max_lights`, ≤ `MAX_LIGHTS`). Tracked so a slider move
	/// re-flushes in BOTH directions — without it a raised cap would not
	/// re-admit the lights it dropped until the anchor next moved.
	light_cap: usize,
	/// Positions awaiting probe invalidation (the drain is bounded per
	/// frame), each flagged with whether it came from a MOVING light
	/// rather than a world edit — see `resolve_inval` for the only thing
	/// that flag changes.
	inval_queue: Vec<([f32; 3], bool)>,
	/// Spheres the probe pass consumed, held live so the RESOLVE keeps capping
	/// screen history around them while the probes re-converge.
	///
	/// Eviction is per-KIND, and that is load-bearing rather than tidy. A world
	/// edit fires once; a carried light fires every `CARRY_INVAL_STEP` of
	/// travel, which at `WALK_SPEED` is several times a second — so against a
	/// plain newest-wins ring one walking torch-bearer holds nearly every slot
	/// continuously and turns them over faster than an edit's window. Making
	/// each stream recycle its OWN oldest slot bounds the moving one without a
	/// second constant.
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

/// The material LUT (GI-DESIGN.md): one entry per class byte, holding everything a ray needs —
/// albedo rgb plus the opacity class, the cast-light level (emission colour is the albedo, so a
/// torch glows its own warm tone), and the two-box occlusion cover of a PARTIAL class
/// (`core::gi::pack_box`). Full cubes keep the base-id entries; the synthetic shape classes above
/// `core::gi::SHAPE_CLASS_BASE` borrow their base block's albedo and light.
fn material_lut(layer_means: &[[u8; 4]]) -> [[u32; 4]; 256] {
	let mut lut = [[0u32; 4]; 256];
	let albedo = |b: BlockType| -> [u8; 4] {
		layer_means
			.get(b.props().tex_top as usize)
			.copied()
			.unwrap_or([255, 255, 255, 255])
	};
	// Word 1: the cast level in the low byte, the cast COLOUR above it. The colour is normally
	// just the albedo — one authored texture deciding both how a thing looks and what its light
	// looks like is the rule R30 was built on. An ore seam cannot honour it: an ore texture is a
	// wide grey field with a few bright flecks, so its mean IS stone, and the problem is the
	// texture's composition rather than the average taken over it. Still one table: `ore_flecks`.
	let emit_word = |b: BlockType| -> u32 {
		let t = b
			.node_flecks()
			.map_or_else(|| albedo(b), |(core, _)| [core[0], core[1], core[2], 255]);
		u32::from(b.props().light)
			| u32::from(t[0]) << 8
			| u32::from(t[1]) << 16
			| u32::from(t[2]) << 24
	};
	for b in BlockType::ALL {
		let id = b as usize;
		if id >= 256 || b == BlockType::Air {
			continue;
		}
		let m = albedo(b);
		// Op 3 is the synthetic shape classes' (below); 4 is tinted
		// transmission, whose rgb filter rides word .z — free for every
		// full-cube class, since only synthetic classes carry boxes.
		let op = match opacity(b) {
			GiOpacity::Air => 0u32,
			GiOpacity::Translucent => 1,
			GiOpacity::Opaque => 2,
			GiOpacity::Tinted => LUT_OP_TINTED,
		};
		// What a ray keeps crossing one cell of this class, per channel:
		// the texture's mean alpha is the coverage and its alpha-weighted
		// mean colour is what the covered part passes — T = 1 − ā·(1 − a).
		// DERIVED from the same texture the surface wears, so retinting
		// the pane retints its light with no second table.
		let tinted = |m: [u8; 4]| -> u32 {
			let t = |c: u8| -> u32 {
				let cover = u32::from(m[3]);
				255 - (cover * (255 - u32::from(c))) / 255
			};
			t(m[0]) | (t(m[1]) << 8) | (t(m[2]) << 16)
		};
		lut[id] = [
			u32::from(m[0]) | (u32::from(m[1]) << 8) | (u32::from(m[2]) << 16) | (op << 24),
			emit_word(b),
			if op == 4 { tinted(m) } else { 0 },
			0,
		];
	}
	for (i, sc) in digblock_core::gi::shape_classes().iter().enumerate() {
		let m = albedo(sc.base);
		let boxes = sc.boxes.map(|b| b.map_or(0, digblock_core::gi::pack_box));
		lut[digblock_core::gi::SHAPE_CLASS_BASE as usize + i] = [
			u32::from(m[0]) | (u32::from(m[1]) << 8) | (u32::from(m[2]) << 16) | (3 << 24),
			emit_word(sc.base),
			boxes[0],
			boxes[1],
		];
	}
	lut
}

/// The CPU twin of `gi_lateral_max` in gi_dda.wgsl — the lateral bound EVERY march breaks at, in
/// blocks from the camera. `flush_lights` uses it to drop emitters no ray could reach; the two
/// must agree or the light list either hides a torch the shader would have traced or wastes a
/// `MAX_LIGHTS` slot on one it would not. Pinned by `the_light_reach_matches_the_shaders_bound`.
fn lateral_max(window_chunks: u32) -> f32 {
	(window_chunks * 16) as f32 * 0.5 - 1.0
}

/// The light list's payload: every reachable emitter, at most `cap` nearest to the forward-leading
/// segment (see `SELECT_AHEAD`), in the GPU entry form. Carried lights are simply more candidates,
/// with no reservation: a carrier's own light sits at the near end of the segment and can never be
/// the one dropped. Selection, NOT ordering — no reader cares about order, so the cap is settled by
/// `select_nth_unstable_by` in O(n), which matters because a carried light moves every frame.
fn select_lights(
	emitters: impl Iterator<Item = Emitter>,
	carried: &[Emitter],
	cam: [f32; 3],
	fwd: [f32; 3],
	reach: f32,
	cap: usize,
) -> Vec<[f32; 8]> {
	let entry = |e: Emitter, carried: u32| {
		let w = u32::from(e.class)
			| carried | (u32::from(e.level) & LIGHT_LEVEL_MASK) << LIGHT_LEVEL_SHIFT
			| if e.whitened { LIGHT_WHITENED } else { 0 }
			| if e.face.is_some() { LIGHT_QUAD } else { 0 };
		// The second half is the rectangle, or zero. A punctual light's
		// pair is [pos, class] then four zeros, so the only thing that
		// changed for a torch is the stride.
		let f = e.face.map_or([0.0; 4], |f| {
			[
				f.ext[0],
				f.ext[1],
				f32::from_bits(u32::from(f.axis) | u32::from(f.positive) << 2),
				0.0,
			]
		});
		(
			e.pos,
			[
				e.pos[0],
				e.pos[1],
				e.pos[2],
				f32::from_bits(w),
				f[0],
				f[1],
				f[2],
				f[3],
			],
		)
	};
	let placed = emitters.map(|e| entry(e, 0));
	let held = carried.iter().map(|&e| entry(e, LIGHT_CARRIED));
	let mut all: Vec<(f32, [f32; 8])> = placed
		.chain(held)
		.filter(|(p, _)| {
			let (dx, dz) = (p[0] - cam[0], p[2] - cam[2]);
			dx * dx + dz * dz <= reach * reach
		})
		.map(|(p, e)| {
			// Distance to the eye→anchor SEGMENT, not to a point ahead:
			// the eye stays on the key's zero set, which is what keeps
			// the carrier's-own-light guarantee above intact while a
			// candidate ahead of the camera still outranks its mirror
			// image behind it.
			let t =
				((p[0] - cam[0]) * fwd[0] + (p[1] - cam[1]) * fwd[1] + (p[2] - cam[2]) * fwd[2])
					.clamp(0.0, SELECT_AHEAD);
			let d2 = (p[0] - (cam[0] + fwd[0] * t)).powi(2)
				+ (p[1] - (cam[1] + fwd[1] * t)).powi(2)
				+ (p[2] - (cam[2] + fwd[2] * t)).powi(2);
			(d2, e)
		})
		.collect();
	if all.len() > cap {
		all.select_nth_unstable_by(cap, |a, b| a.0.total_cmp(&b.0));
		all.truncate(cap);
	}
	all.into_iter().map(|(_, e)| e).collect()
}

/// The per-frame inputs to the light-list flush, handed over WITH the frame
/// (see `FrameScene::carried` for why an out-of-order setter would be a
/// hazard): the camera's look direction, the carried emitters, and the
/// player's cap.
pub(crate) struct LightInputs<'a> {
	pub fwd: [f32; 3],
	pub carried: &'a [Emitter],
	pub cap: usize,
}

/// Where the light selection measures from: the far end of the segment the
/// ranking key uses. One function so `flush_lights`' staleness test and the
/// key cannot lead by two different distances — a heading swing is a move of
/// exactly this point.
fn select_anchor(cam: [f32; 3], fwd: [f32; 3]) -> [f32; 3] {
	[
		cam[0] + fwd[0] * SELECT_AHEAD,
		cam[1] + fwd[1] * SELECT_AHEAD,
		cam[2] + fwd[2] * SELECT_AHEAD,
	]
}

/// Could any cell of this chunk be within `reach` of `cam` laterally? The per-EMITTER reach test
/// decides the light list; this is the same question asked of a whole chunk first, so a chunk well
/// outside the traceable window costs one compare instead of one per entry it holds — a revealed
/// vein contributes a rectangle per exposed face, and caves are made of those. Conservative: it
/// measures to the chunk's nearest EDGE, so it can only admit a chunk the emitter test rejects.
fn chunk_in_reach(pos: ChunkPos, cam: [f32; 3], reach: f32) -> bool {
	let n = digblock_core::chunk::CHUNK_SIZE as f32;
	let axis = |lo: f32, c: f32| (lo - c).max(c - (lo + n)).max(0.0);
	let dx = axis(pos.x as f32 * n, cam[0]);
	let dz = axis(pos.z as f32 * n, cam[2]);
	dx * dx + dz * dz <= reach * reach
}

/// The window's sky ceiling (R18): the highest coarse brick row holding anything at all, as a cell
/// y. A non-descending ray above it is proven lit/sky in one float compare, which ends the sun
/// march at the tallest local hill instead of the top of the world — proving open sky one brick
/// skip at a time cost a dependent texture load per skip. Negative when the mirror is still empty.
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

/// GiUniforms layout (must match gi_probe.wgsl), in order: dims, cam,
/// sun, sun_color, zenith, horizon, sched, then the inval spheres.
const GI_UNIFORM_SIZE: u64 = 176;

/// The GI resolve's group 0 bind plus its two MRT attachments — one
/// argument instead of three, which is what keeps the encode signature
/// under clippy's limit as the pass grows.
pub(crate) struct ResolveTargets<'a> {
	pub bind: &'a wgpu::BindGroup,
	pub indirect_view: &'a wgpu::TextureView,
	pub sun_view: &'a wgpu::TextureView,
}

impl GiDraw {
	pub(crate) fn scene_bgl(&self) -> &wgpu::BindGroupLayout {
		&self.scene_bgl
	}

	/// gi_common.wgsl's group(2) layout — water and players reference it
	/// in their pipeline layouts.
	pub(crate) fn gather_bgl(&self) -> &wgpu::BindGroupLayout {
		&self.gather_bgl
	}

	/// The GI world bind group reading the FRONT (just written) atlases
	/// — valid after this frame's `encode`.
	pub(crate) fn gather_bind(&self) -> &wgpu::BindGroup {
		&self.gather_bind[self.ping]
	}

	/// The camera-light buffer `cam_light` writes — spray and the
	/// viewmodel bind it as a uniform.
	pub(crate) fn cam_light_buf(&self) -> &wgpu::Buffer {
		&self.cam_buf
	}

	/// Drop the screen-space history: device loss, a resize, or anything
	/// else that leaves last frame's buffers describing another world.
	pub(crate) fn reset_history(&mut self) {
		self.history_valid = false;
	}

	/// Patch this frame's uniforms with the GI parameters the direct
	/// gatherers (water, players) read: quantized caster, cascade dims,
	/// window side. Also latches `last_sun` for this frame's `encode` /
	/// `encode_resolve`. Call BEFORE the frame UBO uploads.
	pub(crate) fn patch_frame(&mut self, frame: &mut FrameUniforms) {
		// Quantized caster: the ACTIVE light (sun by day, moon by night —
		// light_dir points toward the ground and light_color already
		// matches it), snapped by `SUN_QUANT` so probe results hold still
		// between steps.
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
}

#[cfg(test)]
mod tests;
