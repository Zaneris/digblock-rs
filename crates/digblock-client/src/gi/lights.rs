//! Emitter selection and upload. `flush_lights` early-outs on an
//! equality compare, which is why `smoothing` must stay bit-exact at
//! rest and not become `Vec3::lerp`.

use super::*;

impl GiDraw {
	/// Queue an edit position for probe invalidation (age reset around
	/// it; the uniform's sphere slots bound how many apply per frame).
	pub(crate) fn invalidate(&mut self, pos: [f32; 3]) {
		self.queue_invalidation(pos, false);
	}

	/// The same, for a light that MOVED rather than a cell that changed —
	/// a player carrying a torch.
	///
	/// Identical to the probes, which is the whole design: re-placement,
	/// not a second mechanism. The flag rides along only so the resolve's
	/// four-slot screen-cap ring can keep the two streams from starving
	/// each other; see `resolve_inval`.
	pub(crate) fn invalidate_moving(&mut self, pos: [f32; 3]) {
		self.queue_invalidation(pos, true);
	}

	pub(super) fn queue_invalidation(&mut self, pos: [f32; 3], moving: bool) {
		// Bounded: only a few spheres drain per frame, and an unbounded
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
	pub(super) fn flush_lights(&mut self, gpu: &Gpu, cam: [f32; 3], carried: &[Emitter]) {
		// Only emitters a march can actually REACH belong in the list.
		// Every trace breaks at `gi_lateral_max` (= side/2 − 1) of the
		// camera, so an emitter past it cannot light anything however
		// close it sorts — and with more than MAX_LIGHTS emitters it
		// displaces one that can. Residency is a RADIUS in chunks while
		// the window is that many chunks on a SIDE, so "resident" reaches
		// well past "traceable" and this is not a rare edge.
		let reach = lateral_max(self.window_chunks);
		let in_reach = |e: &Emitter| {
			let (dx, dz) = (e.pos[0] - cam[0], e.pos[2] - cam[2]);
			dx * dx + dz * dz <= reach * reach
		};
		// Carried lights count toward `total` like any other: they are in
		// the list the truncation applies to, so leaving them out of the
		// tally would let a full world hide one.
		//
		// Chunks outside the reach are skipped WHOLE, before their entries
		// are touched: a vein contributes a rectangle per exposed face and
		// caves are made of exposed faces, so the map is far larger than
		// the torch-only one this walk was written for, and most of it is
		// nowhere a march can go.
		let reachable = |m: &'_ FxHashMap<ChunkPos, Vec<Emitter>>| -> usize {
			m.iter()
				.filter(|(p, _)| chunk_in_reach(**p, cam, reach))
				.map(|(_, v)| v.iter().copied().filter(&in_reach).count())
				.sum()
		};
		let total: usize = reachable(&self.emitters)
			+ reachable(&self.surfaces)
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
		// this costs one walk and one small buffer write each time the
		// camera clears the refresh distance above.
		let stale = moved && (total > MAX_LIGHTS || total != self.lights_len);
		// A carried light moves with its carrier, so the list is stale the
		// moment one does. EXACT equality, not an epsilon, and that is the
		// load-bearing part: at rest the presentation eye is bit-identical
		// frame to frame (`TickLerp::render_pos` returns `prev` exactly when
		// `prev == curr` — pinned by `a_resting_camera_does_not_drift` — and
		// the reconciliation offset snaps to zero rather than decaying
		// forever). So a player standing still with a torch keeps the early-out
		// and a room lit by it converges as deeply as one lit from the floor.
		// An epsilon would invent tolerance for a quantity that has none, and
		// the number chosen would quietly decide when a slow creep stops
		// lighting the room.
		//
		// Compared AFTER the reach filter, which is not a detail: remotes ride
		// snapshots out to the client's whole draw distance while `reach` is a
		// fraction of it, so an unfiltered compare lets a torch-bearer nobody
		// can be lit by defeat the early-out every frame.
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
			self.emitters
				.iter()
				.chain(self.surfaces.iter())
				.filter(|(p, _)| chunk_in_reach(**p, cam, reach))
				.flat_map(|(_, v)| v.iter().copied()),
			&self.carried,
			cam,
			reach,
		);
		let count = all.len() as u32;
		self.lights_len = all.len();
		let mut bytes = Vec::with_capacity(16 + all.len() * 32);
		bytes.extend_from_slice(bytemuck::cast_slice(&[count, 0, 0, 0]));
		bytes.extend_from_slice(bytemuck::cast_slice(&all));
		gpu.queue.write_buffer(&self.lights_buf, 0, &bytes);
	}
}
