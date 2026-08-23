//! The moving probe window over resident chunks: upload, eviction,
//! and the derived state F7 clears. `sky`/`emit` ratcheting over a
//! session is real terrain entering the window, not a leak.

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
		// its handle), and answers the centre for every full cube.
		let list: Vec<_> = gi
			.emitters
			.iter()
			.map(|&(x, y, z, class)| {
				let o = digblock_core::gi::light_origin(class);
				Emitter {
					pos: [
						world.0 + f32::from(x) + o[0],
						world.1 + f32::from(y) + o[1],
						world.2 + f32::from(z) + o[2],
					],
					class,
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

	/// The voxel window's radius in chunks, AS BUILT. The streaming side must key off this and not
	/// off the live `config.draw_dist`, which the settings screen mutates at runtime while the
	/// window texture keeps the size it was created with. A chunk approved outside the built
	/// window hands `upload_chunk` a position whose `rem_euclid` wraps onto a DIFFERENT cell than
	/// the gather's — the aliasing `gi_uploadable` exists to prevent.
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
					class: digblock_core::gi::gi_class(id),
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

	/// Forget an evicted chunk's emitters: without the prune, `flush_lights` walks stale entries
	/// every frame, past `MAX_LIGHTS` one can out-sort a live emitter, and a world swap carries
	/// the previous world's forward. Deliberately does NOT touch the coarse mirror — the window is
	/// TOROIDAL and much smaller than residency, so an evicted chunk's `rem_euclid` slot is very
	/// likely owned by a live one — clearing it would erase real occupancy (BENCH.md 2026-08-12).
	pub(crate) fn remove_chunk(&mut self, pos: ChunkPos) {
		if self.emitters.remove(&pos).is_some() | self.surfaces.remove(&pos).is_some() {
			self.lights_dirty = true;
		}
	}

	/// Drop every piece of GI state that only ever GROWS, so the next upload sweep rebuilds it
	/// from what is actually resident (F7). A diagnostic, not a fix: the coarse MIRROR is never
	/// pruned on eviction (see [`Self::remove_chunk`] for why it must not be), so it ratchets as
	/// a session explores and every ray in the resolve pays for the difference. Pair with
	/// `gi_redirty_all` on the pipeline — this clears, that re-queues.
	pub(crate) fn clear_derived_state(&mut self) {
		self.coarse_mirror.fill(0);
		self.coarse_mips_dirty = true;
		self.emitters.clear();
		self.surfaces.clear();
		self.carried.clear();
		self.lights_dirty = true;
	}

	/// The sky ceiling and the emitter-cell count, for the debug overlay and the CSV. `sky_y`
	/// ratchets as the world is explored — it reads the coarse mirror, which eviction leaves
	/// alone — which is exactly why it is worth showing: it is what makes a settled session cost
	/// more than a fresh one. The emitter count tracks residency instead, since
	/// [`Self::remove_chunk`] prunes both maps, so a climb in it is real terrain in the window.
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
