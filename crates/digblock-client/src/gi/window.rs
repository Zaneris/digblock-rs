//! The moving probe window over resident chunks: upload, eviction,
//! and the derived state F7 clears. Both `sky` and `emit` follow
//! residency up AND down: eviction prunes the coarse mirror (see
//! `mirror::CoarseMirror` for why that is safe) as well as the light maps.

use super::*;

impl GiDraw {
	/// Upload one classified chunk into its toroidal window region (and
	/// its coarse bytes, and its emitter-list slice).
	pub(crate) fn upload_chunk(&mut self, gpu: &Gpu, pos: ChunkPos, gi: &GiChunk) {
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
				bytes_per_row: Some(16 * digblock_core::gi::BYTES_PER_CELL as u32),
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
			bytemuck::cast_slice(&gi.coarse),
			wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(2 * 4),
				rows_per_image: Some(2),
			},
			wgpu::Extent3d {
				width: 2,
				height: 2,
				depth_or_array_layers: 2,
			},
		);
		// Keep the CPU mirror in step and mark the sky ceiling stale
		// (R18); the next `encode` re-reads it.
		if self.coarse_mirror.write(pos, &gi.coarse) {
			self.coarse_mips_dirty = true;
		}
		let world = (
			pos.x as f32 * 16.0,
			pos.y as f32 * 16.0,
			pos.z as f32 * 16.0,
		);
		// Not the cell center: `light_origin` puts a shaped emitter's point
		// source where its geometry actually glows (the torch's flame, not
		// its handle), and answers the centre for every full cube.
		let list: Vec<_> = gi
			.emitters
			.iter()
			.map(|e| {
				let o = digblock_core::gi::light_origin(e.cover);
				Emitter {
					pos: [
						world.0 + f32::from(e.at[0]) + o[0],
						world.1 + f32::from(e.at[1]) + o[1],
						world.2 + f32::from(e.at[2]) + o[2],
					],
					class: e.look,
					// Never overridden: a placed torch's level and tint are
					// its row's, and every copy of it in the world agrees
					// by construction.
					level: 0,
					whitened: false,
					// A point source. Surface emitters never reach this
					// list at all — `classify_chunk` declines to place one,
					// because the cells it would place them for are mostly
					// buried and it cannot see which are not.
					face: None,
				}
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

	/// The voxel window's side in chunks, AS BUILT. The streaming side keys off this and not off
	/// the live `config.draw_dist` — it is what tells that side a slider move has yet to be
	/// rebuilt (`resize_window`), and a chunk approved outside the built window hands
	/// `upload_chunk` a position whose `rem_euclid` wraps onto a DIFFERENT cell than the
	/// gather's — the aliasing `gi_uploadable` exists to prevent.
	pub(crate) fn window_chunks(&self) -> i32 {
		self.window_chunks as i32
	}

	/// Take one chunk's surface emitters from a finished mesh job (`MeshData::lights`) — the vein
	/// rectangles this player can see. Called on every mesh result, empty list included, because a
	/// completed extraction produces exactly "no vein faces any more" and it must reach the list.
	/// The same path carries a tier lighting up as its reveal level is crossed — the crossing
	/// re-meshes, the re-mesh stamps the ore, and no threshold logic lives in the renderer.
	pub(crate) fn set_chunk_lights(&mut self, pos: ChunkPos, packed: &[u32]) {
		use digblock_core::mesher::lights;
		let world = (
			pos.x as f32 * 16.0,
			pos.y as f32 * 16.0,
			pos.z as f32 * 16.0,
		);
		let list: Vec<Emitter> = packed
			.iter()
			.map(|&w| {
				let (cell, dir, span_u, span_v, id) = lights::unpack(w);
				let (au, av) = lights::tangents(dir);
				let an = 3 - au - av;
				let off = dir.offset();
				let positive = [off.0, off.1, off.2][an] > 0;
				let mut c = [
					world.0 + cell.0 as f32,
					world.1 + cell.1 as f32,
					world.2 + cell.2 as f32,
				];
				// The rectangle sits ON the cell boundary the face is: the
				// far side of the cell for an outward-positive normal, the
				// near side for a negative one. Centred across its span in
				// both tangents.
				c[an] += f32::from(u8::from(positive));
				c[au] += span_u as f32 * 0.5;
				c[av] += span_v as f32 * 0.5;
				Emitter {
					pos: c,
					class: digblock_core::gi::gi_look(id),
					// A vein's brightness and colour are its tier's, the
					// same in every hand and every wall — so both come from
					// the LUT row and neither rides the entry.
					level: 0,
					whitened: false,
					face: Some(Face {
						axis: an as u8,
						positive,
						ext: [span_u as f32, span_v as f32],
					}),
				}
			})
			.collect();
		let changed = if list.is_empty() {
			self.surfaces.remove(&pos).is_some()
		} else {
			self.surfaces.insert(pos, list);
			true
		};
		self.lights_dirty |= changed;
	}

	/// Forget an evicted chunk: its emitters — without the prune, `flush_lights` walks stale
	/// entries every frame, past `MAX_LIGHTS` one can out-sort a live emitter, and a world swap
	/// carries the previous world's forward — and its coarse words, so the sky ceiling can come
	/// back down. The mirror prunes only a slot the chunk still owns: the window is TOROIDAL and
	/// an evicted chunk's `rem_euclid` slot may already hold the chunk that replaced it, and
	/// clearing that would erase real occupancy (the reason the mirror was once never pruned at
	/// all, and `sky_y` ratcheted over a session).
	pub(crate) fn remove_chunk(&mut self, pos: ChunkPos) {
		if self.emitters.remove(&pos).is_some() | self.surfaces.remove(&pos).is_some() {
			self.lights_dirty = true;
		}
		if self.coarse_mirror.evict(pos) {
			self.coarse_mips_dirty = true;
		}
	}

	/// Drop every piece of derived GI state, so the next upload sweep rebuilds it from what is
	/// actually resident (F7, and an arrival). Pair with `gi_redirty_all` on the pipeline — this
	/// clears, that re-queues.
	pub(crate) fn clear_derived_state(&mut self) {
		self.coarse_mirror.clear();
		self.coarse_mips_dirty = true;
		self.emitters.clear();
		self.surfaces.clear();
		self.carried.clear();
		self.lights_dirty = true;
	}

	/// The sky ceiling and the emitter-cell count, for the debug overlay and the CSV. Both track
	/// residency — [`Self::remove_chunk`] prunes the mirror and both light maps — so a climb in
	/// either is real terrain in the window, and a settled session costs what a fresh one does.
	pub(crate) fn debug_state(&self) -> (f32, u32) {
		(
			self.sky_y,
			// Carried lights are counted too, and not for completeness:
			// this field is how you answer "is the torch in my hand
			// actually in the list" without a debugger.
			(self.emitters.values().map(Vec::len).sum::<usize>()
				+ self.surfaces.values().map(Vec::len).sum::<usize>()
				+ self.carried.len()) as u32,
		)
	}
}
