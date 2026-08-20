//! Device-lifetime wgpu state. `Renderer::new` is a pure constructor —
//! device-loss recovery is "build a new one and re-upload meshes".

use digblock_core::chunk::ChunkPos;
use digblock_core::mesher::{MeshData, quad_indices};
use digblock_core::metrics::RenderStats;
use rustc_hash::FxHashMap;

use crate::gpu_timer::{
	GpuTimer, Q_DEPTH_BLIT, Q_GI_DENOISE, Q_GI_DENOISE2, Q_MAIN, Q_OVERLAY, Q_PREPASS, Q_UPSCALE,
	Q_VIEWMODEL, Q_WATER,
};
use crate::overlay::Overlay;
use crate::sky::SkyDraw;
use crate::texture::BlockTextures;
use crate::water::WaterDraw;
use digblock_core::metrics::OverlayStats;
use digblock_platform::Gpu;

/// The depth prepass's second output: one face-normal code per pixel
/// (chunk.wgsl's `code_of`), a sentinel code where no solid drew. Uint,
/// not unorm — this is an index into a table, and it has to survive the
/// round trip bit-exactly for the bilateral upsample's equality test to
/// mean anything.
mod build;
mod chunk_pool;
mod draw;
mod frame_types;
mod targets;

pub use frame_types::stub;
pub(crate) use frame_types::{FrameScene, FrameUniforms, NameTag, RenderConfig};

const NORMAL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Uint;

/// Worst-case quads per chunk mesh: a solid chunk of the busiest shaped
/// variant (every template quad emits when neighbors don't cover them).
/// Dwarfs the old full-block checkerboard bound. The shared index buffer
/// this sizes is allocated once and stays small enough for mobile;
/// `upload_chunk` guards the cap instead of trusting it.
const MAX_QUADS: usize = 4096 * digblock_core::shapes::MAX_SHAPE_QUADS;

/// Chunk-origin stride: one `vec4f` per slot in the instance-step origin
/// stream. Was padded out to the uniform dynamic-offset alignment rather
/// than to the data — back when each draw rebound a slice of a UBO.
pub(crate) const ORIGIN_STRIDE: u64 = 16;

/// Chunk vertex-buffer size class. A remesh (an edit dirties every
/// neighbour whose shell it falls in; walking churns the rim continuously)
/// nearly always lands inside the class of the mesh it replaces, so it is
/// written over the live allocation; what does change class is recycled
/// through the free list below. Both matter most on Chrome, where every
/// buffer create and destroy is a GPU-process allocation plus a deferred
/// free that wgpu batches into periodic cleanup spikes.
const SIZE_CLASS: u64 = 16 * 1024;
/// Ceiling on retired bytes held for reuse. Rim churn at walking speed is
/// well under a MiB a frame, so this covers seconds of travel; past it
/// buffers are dropped for real, because a world swap or a draw-distance
/// cut has to give the memory back.
const POOL_BYTES_MAX: u64 = 16 * 1024 * 1024;

/// Coarse front-to-back ordering for the scene pass: one bucket per chunk
/// of column distance from the camera. Immediate-mode GPUs (what Chrome
/// usually runs on) get their early-Z rejection from draw order alone,
/// and the hash-map iteration this replaced was effectively random — pure
/// overdraw. Anything past the last bucket still draws, just unordered.
const DIST_BUCKETS: usize = 48;

/// `DeviceExt::create_buffer_init` replacement, mandatory on wasm and
/// used everywhere for uniformity. The util helper copies through a JS
/// `Uint8Array` view of wasm linear memory with wgpu-internal
/// allocations between creating the view and consuming it; if one of
/// them grows wasm memory, every live view detaches and the copy throws
/// "Cannot perform %TypedArray%.prototype.set on a detached
/// ArrayBuffer" (seen in the field from `upload_chunk` while streaming
/// grew the heap). `Queue::write_buffer` is immune: its FFI glue builds
/// the view and hands it to the synchronous GPU copy in one step, with
/// no wasm allocation in between.
fn buffer_init(gpu: &Gpu, label: &str, usage: wgpu::BufferUsages, contents: &[u8]) -> wgpu::Buffer {
	// write_buffer requires u32-aligned sizes; all callers upload
	// u32-packed data.
	debug_assert_eq!(contents.len() % 4, 0, "buffer_init needs u32-aligned data");
	let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
		label: Some(label),
		size: contents.len() as u64,
		usage: usage | wgpu::BufferUsages::COPY_DST,
		mapped_at_creation: false,
	});
	gpu.queue.write_buffer(&buf, 0, contents);
	buf
}

/// Allocation size for a mesh of `bytes`: the next whole size class.
fn size_class(bytes: u64) -> u64 {
	bytes.next_multiple_of(SIZE_CLASS).max(SIZE_CLASS)
}

/// Chunk-origin slot capacity for a draw distance. Sky chunks never
/// upload, but caves can make any buried chunk non-empty, so budget the
/// full stack of every in-range column — a slot is one `vec4f`, so even
/// the whole stack is a rounding error against the meshes. A starting
/// estimate, not a ceiling: `grow_chunk_buf` takes over before the buffer
/// fills, which is what a future runtime draw-distance change will lean
/// on.
fn chunk_slots(draw_dist: i32) -> u32 {
	let r = f64::from(draw_dist) + 1.0;
	let columns = (std::f64::consts::PI * r * r).ceil() as u32;
	(columns * digblock_core::chunk::WORLD_VCHUNK as u32)
		.next_multiple_of(256)
		.max(4096)
}

/// New capacity when `needed` exceeds the current slot count: round up
/// to the allocation granularity, but never less than double (amortized
/// growth — a handful of reallocations over a whole session).
/// Slot granularity for a regrown instance buffer. Rounding up to a
/// step rather than to `needed` keeps a run of one-slot growths from
/// reallocating on every frame.
const SLOT_GRANULARITY: u32 = 256;

fn grown_slots(current: u32, needed: u32) -> u32 {
	needed
		.next_multiple_of(SLOT_GRANULARITY)
		.max(current.saturating_mul(2))
}

pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The packed two-word chunk vertex (see `mesher::pack`), shared by the
/// solid, water, and depth-prepass pipelines.
const GEOMETRY_ATTRS: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
	format: wgpu::VertexFormat::Uint32x2,
	offset: 0,
	shader_location: 0,
}];
/// The chunk-origin instance stream (see `origin_vertex_layout`).
const ORIGIN_ATTRS: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
	format: wgpu::VertexFormat::Float32x4,
	offset: 0,
	shader_location: 1,
}];

pub(crate) fn geometry_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
	wgpu::VertexBufferLayout {
		array_stride: 8,
		step_mode: wgpu::VertexStepMode::Vertex,
		attributes: &GEOMETRY_ATTRS,
	}
}

/// Slot-indexed chunk origins as an instance-step vertex stream, bound
/// once per pass. Every chunk draws one instance at `first_instance =
/// slot`, so the vertex stage picks its own origin out and the per-chunk
/// loop is `set_vertex_buffer` + `draw_indexed` — no bind group, no
/// dynamic offset. chunk.wgsl's `vs`/`vs_depth` and water.wgsl all read
/// it at `@location(1)` and must be changed together.
pub(crate) fn origin_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
	wgpu::VertexBufferLayout {
		array_stride: ORIGIN_STRIDE,
		step_mode: wgpu::VertexStepMode::Instance,
		attributes: &ORIGIN_ATTRS,
	}
}

/// One uploaded mesh: the pooled buffer, the size class it was allocated
/// at, and the index count in use. `cap` is ≥ the live bytes — the tail
/// past them is stale and unreachable (`draw_indexed` is bounded by
/// `index_count`), which is exactly what lets a remesh write over the
/// existing allocation instead of trading it for a new one.
pub(crate) struct ChunkMesh {
	buf: wgpu::Buffer,
	cap: u64,
	index_count: u32,
}

/// A resident chunk's GPU state, owned by the renderer's flat `chunks`
/// Vec (`by_pos` maps ChunkPos back to its index). Flat because the cull
/// walks the whole set every frame: a hash map costs more in layout and
/// iteration order than the frustum test itself does.
pub(crate) struct ChunkGpu {
	pub(crate) pos: ChunkPos,
	pub(crate) solid: Option<ChunkMesh>,
	pub(crate) water: Option<ChunkMesh>,
	pub(crate) translucent: Option<ChunkMesh>,
	/// Origin-stream index; rides each draw as `first_instance`.
	pub(crate) slot: u32,
	/// Bounding-sphere center, precomputed — the cull is the hottest
	/// per-frame CPU loop in the renderer.
	pub(crate) center: glam::Vec3,
}

/// This frame's culled draw lists. The resident set is walked — and
/// frustum-tested — exactly once per frame, and the scene list carries
/// the only draw ordering the pass gets.
struct CullLists {
	/// Camera-visible solid chunks, coarsely front-to-back.
	scene: Vec<u32>,
	/// Camera-visible chunks carrying water geometry. Unordered: the
	/// water depth prepass is what decides which surface shades.
	water: Vec<u32>,
	/// Camera-visible chunks carrying blended-glass geometry, coarsely
	/// front-to-back like `scene` — the glass draw walks it in REVERSE
	/// for chunk-level back-to-front blending.
	translucent: Vec<u32>,
	/// Solid casters inside the near cascade's light frustum.
	near: Vec<u32>,
	/// Counting-sort input: (distance bucket, chunk index).
	keyed: Vec<(u8, u32)>,
	counts: [u32; DIST_BUCKETS],
	/// The translucent list's own counting-sort input — the scene's
	/// machinery reused, kept separate so one scatter pass each fills
	/// both lists from the one cull walk.
	tkeyed: Vec<(u8, u32)>,
	tcounts: [u32; DIST_BUCKETS],
}

impl Default for CullLists {
	// Hand-written: DIST_BUCKETS is past the array sizes Default covers.
	fn default() -> Self {
		Self {
			scene: Vec::new(),
			water: Vec::new(),
			translucent: Vec::new(),
			near: Vec::new(),
			keyed: Vec::new(),
			counts: [0; DIST_BUCKETS],
			tkeyed: Vec::new(),
			tcounts: [0; DIST_BUCKETS],
		}
	}
}

/// Stable counting sort of `keyed` into `out` by bucket — O(n), no
/// comparisons, no allocation after the first frame. Cheap enough to run
/// every frame, which is the point: re-sorting the resident set only when
/// the camera chunk changes would trade a flat cost for a spike at every
/// chunk boundary, and spikes are what this whole pass exists to remove.
fn scatter_by_bucket(keyed: &[(u8, u32)], counts: &[u32; DIST_BUCKETS], out: &mut Vec<u32>) {
	let mut off = [0u32; DIST_BUCKETS];
	let mut acc = 0u32;
	for (o, n) in off.iter_mut().zip(counts.iter()) {
		*o = acc;
		acc += n;
	}
	out.clear();
	out.resize(acc as usize, 0);
	for &(b, i) in keyed {
		let cursor = &mut off[b as usize];
		out[*cursor as usize] = i;
		*cursor += 1;
	}
}

pub(crate) struct Renderer {
	solid_pipeline: wgpu::RenderPipeline,
	/// Depth-only prepass over the opaque solids: fills the scene depth
	/// the GI resolve reconstructs world positions from, before any
	/// shading runs.
	prepass_pipeline: wgpu::RenderPipeline,
	/// The water depth-prepass + color subpass pair.
	water: WaterDraw,
	/// The blended-glass pipeline (Translucent bucket), drawn after
	/// water in the same pass.
	glass: crate::glass::GlassDraw,
	/// Fullscreen procedural sky, drawn after solids, before water.
	sky: SkyDraw,
	frame_buf: wgpu::Buffer,
	frame_bg: wgpu::BindGroup,
	/// Water copy of the frame UBO, carrying its own alpha (Java u_Alpha
	/// for the transparent pass); bound through `FrameTargets::water_bg`.
	/// Group 1: block texture array + sampler — one static bind group.
	materials_bg: wgpu::BindGroup,
	/// Slot-indexed chunk origins, bound as an instance-step vertex
	/// stream once per pass.
	origin_buf: wgpu::Buffer,
	/// CPU mirror of `origin_buf`, indexed by slot. Keeps `grow_chunk_buf`
	/// to a single write of exactly the allocated slots (no multi-MB
	/// zero-filled staging block) and lets a remesh skip the origin write
	/// entirely — it keeps its slot, so the origin cannot have changed.
	origins: Vec<[f32; 4]>,
	index_buf: wgpu::Buffer,
	/// Resident chunks, flat (see `ChunkGpu`); `by_pos` is the index.
	chunks: Vec<ChunkGpu>,
	by_pos: FxHashMap<ChunkPos, u32>,
	/// Retired vertex buffers by size class (`cap / SIZE_CLASS - 1`), so
	/// an evict/stream cycle recycles instead of reallocating.
	pool: Vec<Vec<wgpu::Buffer>>,
	pool_bytes: u64,
	lists: CullLists,
	free_slots: Vec<u32>,
	next_slot: u32,
	/// Capacity of `origin_buf` in slots.
	chunk_slots: u32,
	/// Total vertex-buffer bytes ALLOCATED (live meshes plus everything
	/// parked in `pool`) — the honest GPU footprint, not the subset a
	/// mesh currently fills.
	pub gpu_buffer_bytes: u64,
	/// None when the device lacks TIMESTAMP_QUERY.
	gpu_timer: Option<GpuTimer>,
	/// RTGI: the voxel window, probe cascades, and the half-res resolve
	/// — the lighting system (GI-DESIGN.md).
	gi: crate::gi::GiDraw,
	/// Draw the voxel-window debug raymarch over the scene (DIG_GI).
	debug_gi: bool,
	/// Cost-stub mask (DIG_STUB) — only the pass-level bits are read here;
	/// the rest ride the frame uniform (water) or the resolve's own
	/// uniform (GI) into the shaders.
	stub: u32,
	/// Debug overlay; always constructed (cheap) so the device-loss
	/// rebuild path recreates it for free.
	overlay: Overlay,
	/// Size-dependent frame targets, lazily (re)built by `ensure_targets`
	/// each render — one path covers first frame, resize, resume, and
	/// device loss (`Renderer::new` starts empty).
	targets: Option<FrameTargets>,
	/// Stateless waterfall spray/mist billboards (instance list fed by
	/// MeshData.impacts through upload_chunk).
	spray: crate::spray::Spray,
	/// Level-up confetti bursts, fed per-frame by the engine (the bursts
	/// themselves are world state and live there).
	confetti: crate::confetti::Confetti,
	/// The Essence Siphon's draw — motes off a vein's face, streaming into
	/// the tool. Fed per-frame by the engine, which owns the envelope.
	siphon: crate::siphon::Siphon,
	/// Remote-player boxes (multiplayer), fed per-frame by the engine.
	players: crate::players::RemotePlayersDraw,
	/// Target-block outline + dig-crack decal (scene pass).
	highlight: crate::highlight::HighlightDraw,
	/// The item in your own hand, in its own pass over the finished
	/// frame.
	viewmodel: crate::viewmodel::ViewmodelDraw,
	/// Hotbar block icons (overlay pass, between the overlay's splits).
	ui_icons: crate::ui::icons::UiIcons,
	/// Layout of `FrameTargets::water_bg` (water frame UBO + depth copy
	/// + scene color + linear sampler): the water pipelines' group 0.
	water_frame_bgl: wgpu::BindGroupLayout,
	/// Layout of `FrameTargets::blit_bg` (scene color + linear sampler).
	blit_bgl: wgpu::BindGroupLayout,
	/// Layout of `FrameTargets::depth_blit_bg` (the live depth texture).
	depth_blit_bgl: wgpu::BindGroupLayout,
	linear_sampler: wgpu::Sampler,
	/// Composites the offscreen scene into the post target at the start of
	/// the water pass. Has a depth-stencil state, so it may ONLY run in a
	/// pass that has a depth attachment — see `upscale_pipeline`.
	blit_pipeline: wgpu::RenderPipeline,
	/// The same shader, depthless, for the standalone upscale pass.
	upscale_pipeline: wgpu::RenderPipeline,
	/// Resolves the live depth into the R32Float copy between the scene
	/// and water passes (WebGPU forbids copying into a depth texture).
	depth_blit_pipeline: wgpu::RenderPipeline,
	surface_format: wgpu::TextureFormat,
}

/// Renderer-owned per-frame render targets. The platform hands over only
/// the swapchain view; depth and the offscreen scene color live here so
/// the water pass can sample both.
/// The two resolutions the world renders at, as one value so they cannot
/// drift apart.
///
/// `render` is a fraction of the surface and `gi` is a whole-integer
/// fraction of THAT. Anchoring the GI to the surface instead would give a
/// fractional ratio the moment render scale left native, and `chunk.wgsl`'s
/// 4-tap bilateral gather walks a footprint that assumes an exact integer
/// ratio — the upsample would read texels that no longer line up with the
/// pixels it is weighting. `gi_ratio` is the number the shader is handed.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct RenderScale {
	pub render: f32,
	pub gi_div: u32,
}

impl RenderScale {
	/// World render size for a surface size. At least one pixel — a zero-sized
	/// attachment is a validation error, and a minimized window can ask.
	fn render_size(self, surface: (u32, u32)) -> (u32, u32) {
		let s = self.render.clamp(0.1, 1.0);
		(
			((surface.0 as f32 * s).round() as u32).max(1),
			((surface.1 as f32 * s).round() as u32).max(1),
		)
	}

	/// GI resolve size: the render size divided by `gi_div`, rounded UP so
	/// the GI buffer always covers every render pixel. Rounding down would
	/// leave the last row/column of a non-divisible render size reading
	/// past the edge of the GI texture.
	fn gi_size(self, surface: (u32, u32)) -> (u32, u32) {
		let r = self.render_size(surface);
		let d = self.gi_div.max(1);
		(r.0.div_ceil(d).max(1), r.1.div_ceil(d).max(1))
	}
}

struct FrameTargets {
	size: (u32, u32),
	/// The scale the targets were built at — part of the rebuild key, so
	/// moving either slider re-creates them.
	scale: RenderScale,
	/// Scene-pass depth attachment; resolved into `depth_copy_view` by
	/// the depth-blit pass before water. (Views keep textures alive.)
	depth_view: wgpu::TextureView,
	/// Pre-water solid depth as R32Float, sampleable (absorption / soft
	/// shores / SSR) — a color format because WebGPU forbids copying
	/// into a depth texture, so it's filled by a fullscreen draw.
	depth_copy_view: wgpu::TextureView,
	/// Exact face-normal codes, written by the depth prepass alongside
	/// depth and read by the GI resolve. Full res: the resolve is half
	/// res and samples this at the matching full-res texel, the same way
	/// it samples the depth copy.
	normal_view: wgpu::TextureView,
	/// Offscreen scene color view: solids + sky render here; water
	/// samples it (refraction/SSR) while blending into `post_view`.
	scene_view: wgpu::TextureView,
	/// Where the world is FINISHED, at render resolution: water, the
	/// viewmodel and the GI debug view draw here over a blit of
	/// `scene_view`, and the upscale pass takes this to the surface.
	/// Separate from `scene_view` because water samples what it blends
	/// over — see the comment at its creation.
	post_view: wgpu::TextureView,
	/// Water group 0: the water frame UBO + scene targets.
	water_bg: wgpu::BindGroup,
	/// Blit group 0: scene color + linear sampler — the water pass's
	/// opening composite of `scene_view` into `post_view`.
	blit_bg: wgpu::BindGroup,
	/// Upscale group 0: `post_view` + linear sampler.
	upscale_bg: wgpu::BindGroup,
	/// Depth-blit group 0: the live depth texture.
	depth_blit_bg: wgpu::BindGroup,
	/// GI lighting pairs, half res: indirect rgb + view distance, and sun
	/// visibility + normal code + water path + history length. Format is
	/// `gi::GI_ACC_FORMAT` because both hold a RUNNING AVERAGE — rgba8 and then
	/// rgba16f each failed the same way, their mantissa too short to resolve an
	/// update of one part in the longest history the accumulator reaches.
	///
	/// The roles are FIXED, not ping-ponged, because the a-trous filter gives the
	/// frame a natural cycle:
	///
	///   [0] <- resolve            [0] -> atrous(stride 1) -> [1]
	///   [1] -> atrous(stride 2) -> [0] -> the scene pass
	///
	/// so [1] holds the once-filtered image at the end of every frame, which is
	/// exactly what the next frame wants as history (SVGF feeds the first
	/// iteration's output back). Two pairs cover it with no copy.
	gi_indirect_view: [wgpu::TextureView; 2],
	gi_sun_view: [wgpu::TextureView; 2],
	/// Scene group 2: pair [0], the one chunk.wgsl textureLoad's.
	gi_scene_bg: wgpu::BindGroup,
	/// Resolve group 0: uniform + depth/normal taps + pair [1] as history
	/// (the GI world data rides group 2 via `GiDraw::gather_bind`).
	gi_resolve_bg: wgpu::BindGroup,
	/// One per a-trous iteration, each bound to the pair it reads.
	gi_atrous_bg: [wgpu::BindGroup; 2],
}

impl Renderer {
	/// Latest GPU pass reading (a few frames latent); None when timestamps
	/// are unsupported or nothing has been read back yet.
	pub(crate) fn gpu_pass_ms(&self) -> Option<digblock_core::metrics::GpuPassMs> {
		self.gpu_timer.as_ref().and_then(|t| t.latest)
	}

	/// One linear pass over the resident set producing every list this
	/// frame draws from. `near` is None when the cached near map stands,
	/// which also skips its plane test. Returns the count culled against
	/// the camera frustum.
	fn cull(
		chunks: &[ChunkGpu],
		lists: &mut CullLists,
		cam: &[glam::Vec4; 4],
		near: Option<&[glam::Vec4; 4]>,
		cam_chunk: (i32, i32),
	) -> u32 {
		lists.water.clear();
		lists.near.clear();
		lists.keyed.clear();
		lists.counts = [0; DIST_BUCKETS];
		lists.tkeyed.clear();
		lists.tcounts = [0; DIST_BUCKETS];
		let mut culled = 0;
		for (i, c) in chunks.iter().enumerate() {
			let i = i as u32;
			let solid = c.solid.is_some();
			if let Some(planes) = near
				&& solid && sphere_in_frustum(c.center, planes)
			{
				lists.near.push(i);
			}
			if !sphere_in_frustum(c.center, cam) {
				culled += 1;
				continue;
			}
			let translucent = c.translucent.is_some();
			if solid || translucent {
				let dx = i64::from(c.pos.x - cam_chunk.0);
				let dz = i64::from(c.pos.z - cam_chunk.1);
				let b = ((dx * dx + dz * dz) as f64).sqrt() as usize;
				let b = b.min(DIST_BUCKETS - 1);
				if solid {
					lists.counts[b] += 1;
					lists.keyed.push((b as u8, i));
				}
				if translucent {
					lists.tcounts[b] += 1;
					lists.tkeyed.push((b as u8, i));
				}
			}
			if c.water.is_some() {
				lists.water.push(i);
			}
		}
		let CullLists {
			scene,
			keyed,
			counts,
			translucent,
			tkeyed,
			tcounts,
			..
		} = lists;
		scatter_by_bucket(keyed, counts, scene);
		scatter_by_bucket(tkeyed, tcounts, translucent);
		culled
	}

	/// The GI voxel window's radius in chunks, as this renderer built it.
	/// F7 drill: drop the GI's ratcheting derived state — see
	/// `GiDraw::clear_derived_state`.
	pub(crate) fn gi_clear_derived_state(&mut self) {
		self.gi.clear_derived_state();
	}

	/// `sky_y` and the emitter count, for the F7 drill's log line.
	pub(crate) fn gi_debug_state(&self) -> (f32, u32) {
		self.gi.debug_state()
	}

	pub(crate) fn gi_window_chunks(&self) -> i32 {
		self.gi.window_chunks()
	}
}

/// Everything a pass needs to draw chunks: the resident set plus the two
/// buffers bound once per pass. One borrow instead of three in the pass
/// signatures, which otherwise grow an argument per feature.
pub(crate) struct ChunkDraw<'a> {
	pub chunks: &'a [ChunkGpu],
	pub origin_buf: &'a wgpu::Buffer,
	pub index_buf: &'a wgpu::Buffer,
}

impl ChunkDraw<'_> {
	/// Bind what every chunk draw in a pass shares: the quad index buffer
	/// and the slot-indexed origin stream.
	pub(crate) fn bind(&self, pass: &mut wgpu::RenderPass<'_>) {
		pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
		pass.set_vertex_buffer(1, self.origin_buf.slice(..));
	}
}

/// One chunk draw: bind its vertex stream, then one instance at
/// `first_instance = slot` — which is how the vertex stage picks its
/// origin out of the instance-step origin buffer bound once per pass.
/// Two encoder calls where the dynamic-offset bind group cost three; on
/// wasm each is a JS hop with validation, and the depth prepass, the
/// scene, and both water subpasses each walk thousands of chunks.
/// Returns the index count drawn.
pub(crate) fn draw_chunk(pass: &mut wgpu::RenderPass<'_>, mesh: &ChunkMesh, slot: u32) -> u32 {
	pass.set_vertex_buffer(0, mesh.buf.slice(..));
	pass.draw_indexed(0..mesh.index_count, 0, slot..slot + 1);
	mesh.index_count
}

/// Chunk center in world units — the bounding sphere's origin, cached on
/// `ChunkGpu` because the cull recomputes nothing per frame.
pub(crate) fn chunk_center(pos: ChunkPos) -> glam::Vec3 {
	glam::Vec3::new(
		(pos.x * 16 + 8) as f32,
		(pos.y * 16 + 8) as f32,
		(pos.z * 16 + 8) as f32,
	)
}

/// Chunk bounding-sphere test against extracted side planes. RADIUS is the
/// chunk's half-diagonal (Java sphereInFrustumWithoutNearFar — near/far
/// deliberately ignored). One definition, so the magic constant can't
/// drift between the callers that share it.
pub(crate) fn sphere_in_frustum(center: glam::Vec3, planes: &[glam::Vec4; 4]) -> bool {
	const RADIUS: f32 = 13.86;
	planes
		.iter()
		.all(|p| p.truncate().dot(center) + p.w > -RADIUS)
}

/// Gribb–Hartmann frustum plane extraction: left/right/bottom/top only.
pub(crate) fn side_planes(m: glam::Mat4) -> [glam::Vec4; 4] {
	let r0 = m.row(0);
	let r1 = m.row(1);
	let r3 = m.row(3);
	[r3 + r0, r3 - r0, r3 + r1, r3 - r1].map(|p| {
		let len = p.truncate().length();
		if len > 0.0 { p / len } else { p }
	})
}

#[cfg(test)]
mod tests {
	/// The WGSL `Frame` struct (frame_common.wgsl) and the Rust-side
	/// `FrameUniforms` are hand-mirrored — field order and packing are
	/// load-bearing. This turns that comment into CI enforcement: naga's
	/// Layouter computes the shader-side uniform layout, and a size
	/// mismatch (reordered/added/removed field, packing drift) fails the
	/// test without needing a GPU.
	#[test]
	fn frame_layout_matches_wgsl() {
		let module = naga::front::wgsl::parse_str(digblock_assets::shader::CHUNK)
			.expect("chunk shader parses");
		let mut layouter = naga::proc::Layouter::default();
		layouter.update(module.to_ctx()).expect("layout");
		let frame = module
			.global_variables
			.iter()
			.find(|(_, v)| v.name.as_deref() == Some("frame"))
			.expect("global `frame` exists")
			.1;
		let wgsl_size = layouter[frame.ty].size as usize;
		assert_eq!(
			wgsl_size,
			std::mem::size_of::<super::FrameUniforms>(),
			"frame_common.wgsl Frame layout diverged from renderer.rs FrameUniforms"
		);
	}

	/// Every pipeline this renderer builds, validated with no GPU present.
	///
	/// The shader test in `digblock-assets` parses each WGSL file ALONE, so it
	/// can only see that a shader is well-formed. It cannot see the seam that
	/// actually breaks: a shader's bindings against the `BindGroupLayout` the
	/// Rust side hand-wrote for it, a vertex buffer's formats against the
	/// attributes the vertex stage declares, a fragment's outputs against the
	/// target formats. All three are checked at `create_render_pipeline` and
	/// nowhere earlier.
	///
	/// wgpu's noop backend stubs the HAL, but `wgpu-core` runs its full
	/// validation first, backend-independent — so this constructs the real
	/// `Renderer` on a device that does not exist and gets the real errors.
	///
	/// What it does NOT cover: naga is the permissive front end and the
	/// browser's is Tint, so a shader that passes here can still invalidate a
	/// pipeline in Chrome.
	#[cfg(not(target_arch = "wasm32"))]
	#[test]
	fn every_pipeline_builds_without_a_gpu() {
		let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
			backends: wgpu::Backends::NOOP,
			backend_options: wgpu::BackendOptions {
				noop: wgpu::NoopBackendOptions::enabled(),
				..Default::default()
			},
			..wgpu::InstanceDescriptor::new_without_display_handle()
		});
		let adapter =
			pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
				.expect("the noop backend always yields an adapter");
		let (device, queue) =
			pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
				.expect("the noop backend always yields a device");

		// Validation failures are reported out-of-band — `create_*` hands
		// back an error resource rather than panicking — so the scope is
		// what turns them into a failing test instead of a silent pass.
		let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
		let gpu = digblock_platform::Gpu {
			instance,
			adapter,
			device,
			queue,
		};
		let _renderer = super::Renderer::new(
			&gpu,
			// The surface format the desktop swapchain actually picks;
			// fragment outputs are validated against it.
			wgpu::TextureFormat::Bgra8UnormSrgb,
			&super::RenderConfig {
				draw_dist: 8,
				spray_droplets: 4,
				chunk_slots_override: None,
				debug_gi: false,
				debug_gi_probes: false,
				stub: 0,
			},
		);
		if let Some(err) = pollster::block_on(scope.pop()) {
			panic!("a pipeline failed validation:\n{err}");
		}
	}

	#[test]
	/// Growth always covers what was asked for, never shrinks below a
	/// doubling, and lands on a granularity step. Asserted as those three
	/// properties over a spread of shapes rather than as a table of
	/// products, which would only restate the arithmetic.
	fn grown_slots_sizing() {
		use super::{SLOT_GRANULARITY, grown_slots};
		for (current, needed) in [
			(4096u32, 4097u32), // the doubling floor dominates
			(8, 9),             // the granularity floor dominates
			(8192, 20000),      // a large jump past both
			(100, 512),         // an exact multiple that already doubles
			(0, 1),
			(1, SLOT_GRANULARITY),
		] {
			let got = grown_slots(current, needed);
			assert!(
				got >= needed,
				"{current}->{needed} gave {got}, short of need"
			);
			assert!(
				got >= current.saturating_mul(2),
				"{current}->{needed} gave {got}, under the doubling floor"
			);
			assert_eq!(
				got % SLOT_GRANULARITY,
				0,
				"{current}->{needed} gave {got}, off the granularity step"
			);
		}
	}

	#[test]
	fn size_classes_round_up_and_never_shrink() {
		let c = super::SIZE_CLASS;
		// Anything non-empty gets at least one class...
		assert_eq!(super::size_class(1), c);
		assert_eq!(super::size_class(c), c);
		// ...and a byte past a class boundary takes the next whole one, so
		// a remesh within the class writes in place.
		assert_eq!(super::size_class(c + 1), 2 * c);
		assert_eq!(super::size_class(3 * c), 3 * c);
		// The class index the pool is keyed by stays in range.
		assert_eq!(super::size_class(0), c);
	}

	#[test]
	fn scatter_orders_by_bucket_and_is_stable() {
		let keyed = [(2u8, 10u32), (0, 20), (2, 30), (1, 40), (0, 50)];
		let mut counts = [0u32; super::DIST_BUCKETS];
		for &(b, _) in &keyed {
			counts[b as usize] += 1;
		}
		let mut out = vec![9999u32; 3]; // reused scratch, stale contents
		super::scatter_by_bucket(&keyed, &counts, &mut out);
		// Near buckets first; ties keep their input order (the resident
		// set's own order), so the result is deterministic frame to frame.
		assert_eq!(out, vec![20, 50, 40, 10, 30]);
	}
}
