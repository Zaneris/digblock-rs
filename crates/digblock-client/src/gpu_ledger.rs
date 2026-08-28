//! The GPU memory this client allocates, by kind, and the ONE way a big buffer or texture is
//! let go: `retire_*`, which DESTROYS it rather than dropping it.
//!
//! Dropping a `wgpu::Buffer`/`wgpu::Texture` frees nothing on the WebGPU backend — its `Drop`
//! is a no-op, and the GPU allocation lives until V8 happens to collect the JS wrapper, which it
//! does late and in bursts, because the wrapper is a few bytes and V8 cannot see the megabytes
//! behind it. A session that walks (chunk buffers past the pool ceiling), resizes or toggles
//! fullscreen (a whole target set), or moves the draw-distance slider (the 100 MiB voxel window)
//! strands all of it in Chrome's GPU process. `destroy()` is the standard fix and is safe with
//! in-flight frames on both backends: WebGPU orders it on the device timeline after every earlier
//! submit, and wgpu-core queues the raw handle behind pending submissions. The one rule is that
//! nothing submitted LATER may still bind the object, which each retirement site keeps by
//! replacing the bind groups that named it in the same breath.
//!
//! `retired` is the number that must stay flat while the player stands still: it is what the
//! old drops would have stranded, and the overlay's `mem` line shows it beside the live total.

use digblock_core::metrics::GpuMemory;

/// What an allocation is for — the overlay's breakdown and, for the ledger's own arithmetic,
/// the bucket a retirement credits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GpuKind {
	/// Chunk vertex buffers, live and parked in the pool.
	ChunkVerts,
	/// The chunk-origin instance stream.
	ChunkOrigins,
	/// The size-dependent frame targets: depth, normals, scene, post, GI history.
	Targets,
	/// The GI voxel window and its coarse occupancy.
	GiWindow,
	/// The probe atlases and flags.
	GiAtlas,
	/// The block texture array and other fixed textures.
	Textures,
}

const KINDS: usize = 6;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct GpuLedger {
	live: [u64; KINDS],
	retired: u64,
}

impl GpuLedger {
	pub(crate) fn charge_buffer(&mut self, kind: GpuKind, buf: &wgpu::Buffer) {
		self.charge(kind, buf.size());
	}

	pub(crate) fn charge_texture(&mut self, kind: GpuKind, tex: &wgpu::Texture) {
		self.charge(kind, texture_bytes(tex));
	}

	/// Destroy a buffer now and move its bytes from live to retired.
	pub(crate) fn retire_buffer(&mut self, kind: GpuKind, buf: wgpu::Buffer) {
		let bytes = buf.size();
		buf.destroy();
		self.credit(kind, bytes);
	}

	/// Destroy a texture now and move its bytes from live to retired. Views and bind groups that
	/// named it may still be alive in the caller's hands; they are dropped, not destroyed, and
	/// must not be bound again.
	pub(crate) fn retire_texture(&mut self, kind: GpuKind, tex: wgpu::Texture) {
		let bytes = texture_bytes(&tex);
		tex.destroy();
		self.credit(kind, bytes);
	}

	fn charge(&mut self, kind: GpuKind, bytes: u64) {
		self.live[kind as usize] += bytes;
	}

	fn credit(&mut self, kind: GpuKind, bytes: u64) {
		let live = &mut self.live[kind as usize];
		debug_assert!(*live >= bytes, "{kind:?}: retiring more than was charged");
		*live = live.saturating_sub(bytes);
		self.retired += bytes;
	}

	pub(crate) fn live(&self, kind: GpuKind) -> u64 {
		self.live[kind as usize]
	}

	pub(crate) fn live_total(&self) -> u64 {
		self.live.iter().sum()
	}

	pub(crate) fn retired(&self) -> u64 {
		self.retired
	}

	/// The metrics view: the live total, its three big buckets, and the retired total.
	pub(crate) fn snapshot(&self) -> GpuMemory {
		GpuMemory {
			live: self.live_total(),
			mesh: self.live(GpuKind::ChunkVerts) + self.live(GpuKind::ChunkOrigins),
			gi: self.live(GpuKind::GiWindow) + self.live(GpuKind::GiAtlas),
			targets: self.live(GpuKind::Targets),
			retired: self.retired,
		}
	}
}

/// A texture's footprint from its own descriptor fields.
pub(crate) fn texture_bytes(tex: &wgpu::Texture) -> u64 {
	texture_bytes_of(
		tex.size(),
		tex.dimension(),
		tex.format(),
		tex.mip_level_count(),
	)
}

/// The bytes a texture of this shape occupies: every mip level, at the format's copy size per
/// texel. Uncompressed formats only — every texture this client makes is one — with the
/// depth-only formats wgpu declines to size (`Depth24Plus`) taken as four bytes, the width every
/// backend actually gives them.
fn texture_bytes_of(
	size: wgpu::Extent3d,
	dimension: wgpu::TextureDimension,
	format: wgpu::TextureFormat,
	mips: u32,
) -> u64 {
	let bpp = u64::from(format.block_copy_size(None).unwrap_or(4));
	(0..mips)
		.map(|level| {
			let e = size.mip_level_size(level, dimension);
			u64::from(e.width) * u64::from(e.height) * u64::from(e.depth_or_array_layers) * bpp
		})
		.sum()
}

#[cfg(test)]
mod tests {
	use super::*;
	use wgpu::{Extent3d, TextureDimension as Dim, TextureFormat as Fmt};

	const MIB: u64 = 1024 * 1024;

	#[test]
	fn the_voxel_window_at_draw_20_is_one_hundred_mib() {
		let side = crate::gi::window_chunks_for(20) * 16;
		let size = Extent3d {
			width: side,
			height: 256,
			depth_or_array_layers: side,
		};
		assert_eq!(texture_bytes_of(size, Dim::D3, Fmt::R8Uint, 1), 100 * MIB);
	}

	#[test]
	fn a_gi_history_texel_is_sixteen_bytes_and_a_target_set_sums_its_formats() {
		let px = Extent3d {
			width: 1000,
			height: 500,
			depth_or_array_layers: 1,
		};
		assert_eq!(
			texture_bytes_of(px, Dim::D2, crate::gi::GI_ACC_FORMAT, 1),
			500_000 * 16
		);
		// The five screen-sized targets: depth 4 + depth copy 4 + normal 1 + scene 4 + post 4.
		let screen: u64 = [
			Fmt::Depth32Float,
			Fmt::R32Float,
			Fmt::R8Uint,
			Fmt::Bgra8UnormSrgb,
			Fmt::Bgra8UnormSrgb,
		]
		.into_iter()
		.map(|f| texture_bytes_of(px, Dim::D2, f, 1))
		.sum();
		assert_eq!(screen, 500_000 * 17);
	}

	#[test]
	fn mip_levels_and_array_layers_are_counted() {
		let size = Extent3d {
			width: 64,
			height: 64,
			depth_or_array_layers: 3,
		};
		// 64² + 32² + 16² + 8² + 4² + 2² + 1² texels, three layers, four bytes.
		let texels = 4096 + 1024 + 256 + 64 + 16 + 4 + 1;
		assert_eq!(
			texture_bytes_of(size, Dim::D2, Fmt::Rgba8Unorm, 7),
			texels * 3 * 4
		);
	}

	#[test]
	fn the_ledger_moves_bytes_from_live_to_retired() {
		let mut l = GpuLedger::default();
		l.charge(GpuKind::Targets, 70 * MIB);
		l.charge(GpuKind::ChunkVerts, 16 * MIB);
		l.charge(GpuKind::GiWindow, 100 * MIB);
		assert_eq!(l.live_total(), 186 * MIB);
		assert_eq!(l.retired(), 0);
		l.credit(GpuKind::Targets, 70 * MIB);
		assert_eq!(l.live(GpuKind::Targets), 0);
		assert_eq!(l.live_total(), 116 * MIB);
		assert_eq!(l.retired(), 70 * MIB);
		let s = l.snapshot();
		assert_eq!(
			(s.live, s.mesh, s.gi, s.targets, s.retired),
			(116 * MIB, 16 * MIB, 100 * MIB, 0, 70 * MIB)
		);
	}
}
