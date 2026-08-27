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

	/// The same, for a light that MOVED rather than a cell that changed — a player carrying a
	/// torch. Identical to the probes, which is the whole design: re-placement, not a second
	/// mechanism. The flag rides along only so the resolve's screen-cap ring can keep the two
	/// streams from starving each other; see `resolve_inval`.
	pub(crate) fn invalidate_moving(&mut self, pos: [f32; 3]) {
		self.queue_invalidation(pos, true);
	}

	pub(super) fn queue_invalidation(&mut self, pos: [f32; 3], moving: bool) {
		// Bounded: only a few spheres drain per frame, and an unbounded queue grows without
		// limit under any burst of edits, then spends minutes replaying invalidations for cells
		// that have since changed again. Dropping the OLDEST is right — the newest edits are the
		// ones whose light is most wrong.
		const INVAL_MAX: usize = 256;
		if self.inval_queue.len() >= INVAL_MAX {
			self.inval_queue.remove(0);
		}
		self.inval_queue.push((pos, moving));
	}

	/// Rebuild the flattened light list when it changed: the `cap` nearest emitters to the
	/// selection anchor, count in the header. Order never matters to a reader, so a stale
	/// SELECTION bites only through the truncation — "nearest to an anchor that has since left"
	/// drops the torches around the player. Nothing is sorted: `select_lights` partitions. `cap`
	/// is `Settings::max_lights`; the buffer and shader clamps stay sized to `MAX_LIGHTS`.
	pub(super) fn flush_lights(&mut self, gpu: &Gpu, cam: [f32; 3], lights: LightInputs<'_>) {
		let LightInputs {
			fwd,
			carried,
			cap,
			sky_floor,
		} = lights;
		let cap = cap.clamp(1, MAX_LIGHTS);
		if cap != self.light_cap {
			self.light_cap = cap;
			self.lights_dirty = true;
		}
		// The floor is quantized (whole-block heights, recomputed per camera
		// chunk), so this fires on arming, disarming and real tally moves —
		// never on frame-to-frame jitter.
		if sky_floor != self.lights_floor {
			self.lights_floor = sky_floor;
			self.lights_dirty = true;
		}
		// Only emitters a march can actually REACH belong in the list. Every trace breaks at
		// `gi_lateral_max` of the camera, so an emitter past it cannot light anything however
		// close it sorts — and with more than MAX_LIGHTS emitters it displaces one that can.
		// Residency is a RADIUS in chunks while the window is that many chunks on a SIDE, so
		// "resident" reaches well past "traceable" and this is not a rare edge.
		let reach = lateral_max(self.window_chunks);
		let in_reach = |e: &Emitter| {
			let (dx, dz) = (e.pos[0] - cam[0], e.pos[2] - cam[2]);
			dx * dx + dz * dz <= reach * reach
		};
		// Carried lights count toward the tally like any other: they are in the list the
		// truncation applies to, so leaving them out would let a full world hide one. Chunks
		// outside the reach are skipped WHOLE, before their entries are touched — a vein
		// contributes a rectangle per exposed face and caves are made of exposed faces, so most
		// of the map is nowhere a march can go.
		let reachable = |m: &'_ FxHashMap<ChunkPos, Vec<Emitter>>| -> usize {
			m.iter()
				.filter(|(p, _)| chunk_in_reach(**p, cam, reach))
				.map(|(_, v)| v.iter().copied().filter(&in_reach).count())
				.sum()
		};
		// The anchor, not the camera: a pure heading swing moves it too,
		// so an oversubscribed list refreshes when the player turns
		// around, on the same rule and the same distance a walk uses. At
		// rest both position and heading are bit-identical frame to
		// frame, so the early-out below survives unchanged.
		let anchor = select_anchor(cam, fwd);
		let moved = {
			let d = [
				anchor[0] - self.lights_anchor[0],
				anchor[1] - self.lights_anchor[1],
				anchor[2] - self.lights_anchor[2],
			];
			d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > 16.0 * 16.0
		};
		// Any real move can change WHICH emitters are in reach, not just which `cap` of them are
		// nearest — so the oversubscription clause is not the only reason to re-flush. Under the
		// cap this costs one walk and one small buffer write each time the anchor clears the
		// refresh distance above. The tally rides INSIDE `moved`, and not beside it: it walks
		// both emitter maps, and a frame that has not moved would pay for an answer it discards.
		let stale = moved && {
			let total = reachable(&self.emitters)
				+ reachable(&self.surfaces)
				+ carried.iter().copied().filter(&in_reach).count();
			total > cap || total != self.lights_len
		};
		// A carried light moves with its carrier, so the list is stale the moment one does.
		// EXACT equality, not an epsilon: at rest the presentation eye is bit-identical frame to
		// frame (pinned by `a_resting_camera_does_not_drift`), so a standing player keeps the
		// early-out, and an epsilon would decide when a slow creep stops lighting a room.
		// Compared AFTER the reach filter, or a torch-bearer nobody can be lit by defeats it.
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
		self.lights_anchor = anchor;
		let all = select_lights(
			self.emitters
				.iter()
				.chain(self.surfaces.iter())
				.filter(|(p, _)| chunk_in_reach(**p, cam, reach))
				.flat_map(|(_, v)| v.iter().copied()),
			&self.carried,
			cam,
			fwd,
			reach,
			sky_floor,
			cap,
		);
		let count = all.len() as u32;
		self.lights_len = all.len();
		let mut bytes = Vec::with_capacity(16 + all.len() * 32);
		bytes.extend_from_slice(bytemuck::cast_slice(&[count, 0, 0, 0]));
		bytes.extend_from_slice(bytemuck::cast_slice(&all));
		gpu.queue.write_buffer(&self.lights_buf, 0, &bytes);
	}
}
