//! The GI half of streaming: which chunks are dirty, which are inside
//! the probe window, and the classification handed to the uploader.

use super::*;

impl Pipeline {
	/// Drain the chunks whose GI-window region is stale (RTGI). The
	/// caller owns budgeting: it takes everything and queues the excess,
	/// so a burst (arrival column, cold fill) never re-dirties here.
	pub fn take_gi_dirty(&mut self) -> Vec<ChunkPos> {
		self.gi_dirty.drain().collect()
	}

	/// Is this chunk inside the GI window centered on the last
	/// `gi_recenter` call? The upload drain MUST check this: a chunk
	/// outside the half-extent would alias onto an in-window column in
	/// the toroidal texture and stamp far terrain over near.
	pub fn gi_uploadable(&self, pos: ChunkPos, window_chunks: i32) -> bool {
		self.gi_center
			.is_some_and(|c| gi_in_window(pos, c, window_chunks))
	}

	/// Scroll the GI window: when the camera crosses a chunk boundary,
	/// re-dirty every resident chunk that just ENTERED the half-extent —
	/// its window texels still hold whatever far column aliased there
	/// while it was out of range. The strip that left needs nothing:
	/// reads are lateral-capped to the half-extent, so its stale texels
	/// are unreachable until it re-enters (and re-dirties here).
	pub fn gi_recenter(&mut self, center: ChunkPos, window_chunks: i32) {
		let c = (center.x, center.z);
		let old = self.gi_center;
		if old == Some(c) {
			return;
		}
		self.gi_center = Some(c);
		for &pos in self.store.keys() {
			if gi_in_window(pos, c, window_chunks)
				&& !old.is_some_and(|o| gi_in_window(pos, o, window_chunks))
			{
				self.gi_dirty.insert(pos);
			}
		}
	}

	/// Drain edited cells for probe invalidation (RTGI).
	pub fn take_gi_edits(&mut self) -> Vec<(i32, i32, i32)> {
		std::mem::take(&mut self.gi_edits)
	}

	/// Classify one resident chunk for the GI window. None = not (or no
	/// longer) resident — the caller drops the entry; if the region ever
	/// becomes visible again it arrives, and arriving re-dirties it.
	pub fn classify_gi(&self, pos: ChunkPos) -> Option<crate::gi::GiChunk> {
		match &self.store.get(&pos)?.state {
			ChunkState::Ready(data) => Some(crate::gi::classify_chunk(data)),
			_ => None,
		}
	}

	/// Re-dirty every resident chunk's GI-window region (device-loss
	/// rebuild: the fresh window is empty and all GI data is derived).
	pub fn gi_redirty_all(&mut self) {
		self.gi_dirty.extend(self.store.keys().copied());
	}
}
