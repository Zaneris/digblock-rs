//! The chunk mesh slot allocator: one growable vertex buffer carved
//! into size classes, plus the per-slot origin instance data. Slots are
//! recycled rather than freed, so residency churn costs no allocation.

use super::*;

impl Renderer {
	/// Replace `origin_buf` with a larger one and repopulate it from the
	/// CPU mirror in a single write of exactly the allocated slots — free
	/// slots keep whatever they held, since no draw ever names them. Every
	/// pass binds the origin stream fresh each frame.
	pub(super) fn grow_chunk_buf(&mut self, gpu: &Gpu, needed_slots: u32) {
		let old = self.chunk_slots;
		let new_slots = grown_slots(old, needed_slots);
		let origin_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk origins"),
			size: u64::from(new_slots) * ORIGIN_STRIDE,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		if !self.origins.is_empty() {
			gpu.queue
				.write_buffer(&origin_buf, 0, bytemuck::cast_slice(&self.origins));
		}
		self.ledger
			.charge_buffer(GpuKind::ChunkOrigins, &origin_buf);
		// The frame that is still reading the old stream was submitted
		// before this; the destroy queues behind it (`gpu_ledger`).
		let old_buf = std::mem::replace(&mut self.origin_buf, origin_buf);
		self.ledger.retire_buffer(GpuKind::ChunkOrigins, old_buf);
		self.chunk_slots = new_slots;
		log::info!(
			"chunk origin buffer grown: {old} -> {new_slots} slots ({} KiB), {} slots live",
			u64::from(new_slots) * ORIGIN_STRIDE / 1024,
			self.origins.len()
		);
	}

	/// Claim an origin slot for a newly resident chunk and publish its
	/// origin. Growth fires while there is still headroom rather than at
	/// overflow: the reallocate-and-repopulate lands in one frame, and the
	/// path that reaches it — a runtime draw-distance increase — is already
	/// the frame doing the most other work.
	pub(super) fn alloc_slot(&mut self, gpu: &Gpu, pos: ChunkPos) -> u32 {
		let slot = match self.free_slots.pop() {
			Some(s) => s,
			None => {
				let s = self.next_slot;
				self.next_slot += 1;
				self.origins.push([0.0; 4]);
				if u64::from(self.next_slot) * 20 >= u64::from(self.chunk_slots) * 17 {
					self.grow_chunk_buf(gpu, self.next_slot + 1);
				}
				s
			}
		};
		debug_assert!(slot < self.chunk_slots, "origin slot out of range");
		let (ox, oy, oz) = pos.origin();
		let i = slot as usize;
		self.origins[i] = [ox, oy, oz, 0.0];
		gpu.queue.write_buffer(
			&self.origin_buf,
			u64::from(slot) * ORIGIN_STRIDE,
			bytemuck::cast_slice(&self.origins[i..=i]),
		);
		slot
	}

	/// Take a buffer of at least `bytes` from the free list, or make one.
	pub(super) fn acquire(&mut self, gpu: &Gpu, bytes: u64) -> ChunkMesh {
		let cap = size_class(bytes);
		let class = (cap / SIZE_CLASS - 1) as usize;
		if let Some(buf) = self.pool.get_mut(class).and_then(Vec::pop) {
			self.pool_bytes -= cap;
			return ChunkMesh {
				buf,
				cap,
				index_count: 0,
			};
		}
		let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("chunk verts"),
			size: cap,
			usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		self.ledger.charge_buffer(GpuKind::ChunkVerts, &buf);
		ChunkMesh {
			buf,
			cap,
			index_count: 0,
		}
	}

	/// Park a retired buffer for reuse, or free it once the pool is at its
	/// ceiling. Safe to reuse immediately: the next write goes through the
	/// queue, which orders it behind any in-flight frame still reading it.
	/// Past the ceiling the buffer is DESTROYED, not dropped: a drop frees
	/// nothing on the web until the JS wrapper is collected (`gpu_ledger`).
	pub(super) fn recycle(&mut self, mesh: ChunkMesh) {
		if self.pool_bytes + mesh.cap > POOL_BYTES_MAX {
			self.ledger.retire_buffer(GpuKind::ChunkVerts, mesh.buf);
			return;
		}
		let class = (mesh.cap / SIZE_CLASS - 1) as usize;
		if self.pool.len() <= class {
			self.pool.resize_with(class + 1, Vec::new);
		}
		self.pool_bytes += mesh.cap;
		self.pool[class].push(mesh.buf);
	}

	/// Write `verts` over `prev`'s buffer when they fit its size class —
	/// the common case for a remesh of the same chunk, and the whole point
	/// of the classes. Anything else takes a pooled buffer of the right
	/// class and retires the old one. Empty verts release it outright.
	pub(super) fn store_mesh(
		&mut self,
		gpu: &Gpu,
		prev: Option<ChunkMesh>,
		verts: &[u32],
	) -> Option<ChunkMesh> {
		// All vertices are two u32 words (see mesher::pack). The shared
		// index buffer covers MAX_QUADS; a mesh past it would make
		// draw_indexed over-read (a wgpu validation error). Truncate
		// loudly — visible dropped faces beat a device loss, and the cap
		// is sized to be unreachable.
		let mut verts = verts;
		let quads = (verts.len() / (4 * 2)).min(MAX_QUADS);
		if verts.len() / (4 * 2) > MAX_QUADS {
			log::error!(
				"chunk mesh exceeds MAX_QUADS ({} > {MAX_QUADS}); truncating",
				verts.len() / (4 * 2)
			);
			verts = &verts[..MAX_QUADS * 8];
		}
		if verts.is_empty() {
			if let Some(old) = prev {
				self.recycle(old);
			}
			return None;
		}
		let bytes = std::mem::size_of_val(verts) as u64;
		let mut mesh = match prev {
			Some(m) if m.cap >= bytes => m,
			other => {
				if let Some(old) = other {
					self.recycle(old);
				}
				self.acquire(gpu, bytes)
			}
		};
		gpu.queue
			.write_buffer(&mesh.buf, 0, bytemuck::cast_slice(verts));
		mesh.index_count = quads as u32 * 6;
		Some(mesh)
	}

	/// Upload (or replace) a chunk's mesh. Empty meshes remove the entry —
	/// unlike the Java, a re-mesh to zero faces cannot leave a stale VBO.
	pub(crate) fn upload_chunk(&mut self, gpu: &Gpu, pos: ChunkPos, mesh: &MeshData) {
		if mesh.is_empty() {
			self.remove_chunk(pos);
			return;
		}
		self.spray.set_chunk(pos, pos.origin(), &mesh.impacts);
		// The vein faces this mesh exposed, on the same rail and for the
		// same reason: the mesher is the one pass that knows which of them
		// this player can see (`mesher::lights`). Unconditionally, empty
		// list included — a completed extraction produces exactly that,
		// and "no faces here any more" has to reach the light list.
		self.gi.set_chunk_lights(pos, &mesh.lights);
		// A remesh keeps its entry, its slot (so the origin needs no
		// rewrite) and, whenever the new mesh fits, its buffers.
		let idx = match self.by_pos.get(&pos) {
			Some(&i) => i as usize,
			None => {
				let slot = self.alloc_slot(gpu, pos);
				let idx = self.chunks.len();
				self.chunks.push(ChunkGpu {
					pos,
					solid: None,
					water: None,
					translucent: None,
					slot,
					center: chunk_center(pos),
				});
				self.by_pos.insert(pos, idx as u32);
				idx
			}
		};
		let prev = self.chunks[idx].solid.take();
		self.chunks[idx].solid = self.store_mesh(gpu, prev, &mesh.solid);
		let prev = self.chunks[idx].water.take();
		self.chunks[idx].water = self.store_mesh(gpu, prev, &mesh.water);
		let prev = self.chunks[idx].translucent.take();
		self.chunks[idx].translucent = self.store_mesh(gpu, prev, &mesh.translucent);
	}

	/// RTGI: forward one classified chunk into the voxel window.
	pub(crate) fn gi_upload(&mut self, gpu: &Gpu, pos: ChunkPos, gi: &digblock_core::gi::GiChunk) {
		self.gi.upload_chunk(gpu, pos, gi);
	}

	/// RTGI: rebuild the voxel window for a new draw distance — see
	/// `GiDraw::resize_window`; the caller re-dirties every resident chunk.
	pub(crate) fn gi_resize_window(&mut self, gpu: &Gpu, window_chunks: u32) {
		self.gi.resize_window(gpu, window_chunks, &mut self.ledger);
	}

	/// RTGI: queue probe invalidation around an edited cell.
	pub(crate) fn gi_invalidate(&mut self, pos: [f32; 3]) {
		self.gi.invalidate(pos);
	}

	/// The same, for a light a player is CARRYING that has moved or gone
	/// out. Identical to the probes; the two are told apart only so a
	/// walking carrier's stream cannot evict a world edit from the
	/// resolve's four screen-cap slots (`GiDraw::resolve_inval`).
	pub(crate) fn gi_invalidate_moving(&mut self, pos: [f32; 3]) {
		self.gi.invalidate_moving(pos);
	}

	pub(crate) fn remove_chunk(&mut self, pos: ChunkPos) {
		self.spray.remove_chunk(pos);
		// Before the early return: GI residency is its own set, and an
		// un-meshed chunk still owns an emitter entry.
		self.gi.remove_chunk(pos);
		let Some(idx) = self.by_pos.remove(&pos) else {
			return;
		};
		let old = self.chunks.swap_remove(idx as usize);
		// swap_remove filled the hole with the last entry (unless the hole
		// WAS the last) — retarget its index.
		if let Some(moved) = self.chunks.get(idx as usize) {
			self.by_pos.insert(moved.pos, idx);
		}
		self.free_slots.push(old.slot);
		for mesh in [old.solid, old.water, old.translucent]
			.into_iter()
			.flatten()
		{
			self.recycle(mesh);
		}
	}

	/// Drop every uploaded chunk mesh (world swap: the multiplayer
	/// handshake replaces the locally-seeded world with the server's).
	pub(crate) fn clear_chunks(&mut self) {
		for chunk in std::mem::take(&mut self.chunks) {
			self.spray.remove_chunk(chunk.pos);
			self.free_slots.push(chunk.slot);
			for mesh in [chunk.solid, chunk.water, chunk.translucent]
				.into_iter()
				.flatten()
			{
				self.recycle(mesh);
			}
		}
		self.by_pos.clear();
		// A world swap used to carry the OLD world's emitters and coarse
		// occupancy forward into the new one. Here — unlike a single
		// eviction — wiping the mirror is safe and correct, because every
		// slot is about to be rewritten by the incoming world anyway.
		self.gi.clear_derived_state();
	}
}
