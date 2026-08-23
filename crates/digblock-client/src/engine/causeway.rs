//! The Causeway's client half: the plank laid under the feet as they
//! leave support, and the road coming up when the span runs out.
//!
//! The plank is PREDICTED — a round trip is several ticks and a fall is
//! immediate — so it rides the standing edit machinery with a seq of its
//! own and reverts through the same rollback as a refused place. What is
//! deliberately not predicted is RETIREMENT: online the server owns the
//! tail and its removals arrive as ordinary edits, so the two ends can
//! never disagree about which planks stand.

use super::Engine;
use digblock_core::block::BlockType;
use digblock_core::causeway::Cell;
use digblock_core::chunk::make_cell;

impl Engine {
	/// One tick's worth of road, laid from `tick_player` and nowhere
	/// else — the plank a tick asks for is only level with the ground
	/// while the feet are still on the top they walked off, which is
	/// true for that tick alone.
	pub(super) fn step_causeway(&mut self, plank: Option<Cell>) {
		let Some(c) = plank else {
			return;
		};
		let glass = make_cell(BlockType::Wardglass, 0);
		let Some(prev) = self.pipeline.cell_at(c.0, c.1, c.2) else {
			return;
		};
		self.pipeline.set_cell(c.0, c.1, c.2, glass);
		match self.session.client.as_mut() {
			Some(net) if net.ready() => {
				let tick = net.predictor.current_tick();
				// No inventory delta: conjured glass is not stock, and a
				// plank costs nothing beyond the fare already paid.
				let seq = net.edits.begin(c, prev, glass, tick, None);
				net.send_edit(
					seq,
					tick,
					self.player.pos,
					self.camera.dir(),
					digblock_net::messages::EditKind::CausewayStep,
					c,
					(0, 0, 0),
					self.hotbar.selected_index() as u8,
				);
				// The trail is a dedup list here, not a schedule: the
				// server retires the tail and broadcasts the removal.
				self.causeway.lay(c);
			}
			_ => {
				self.fluid.schedule_around(c.0, c.1, c.2);
				if let Some(old) = self.causeway.lay(c) {
					self.take_plank(old);
				}
			}
		}
	}

	/// The span ran out, or the world did. Online this only forgets the
	/// trail — the server's own lift arrives as ordinary edits, and
	/// predicting it would race them.
	pub(super) fn end_causeway(&mut self) {
		self.causeway_left = 0.0;
		let road = self.causeway.take();
		if matches!(self.session.client.as_ref(), Some(n) if n.ready()) {
			return;
		}
		for c in road {
			self.take_plank(c);
		}
	}

	/// Take one plank up offline — only if it is still the glass this
	/// road laid, so a pane somebody dug out stays dug.
	fn take_plank(&mut self, c: Cell) {
		let Some(prev) = self.pipeline.cell_at(c.0, c.1, c.2) else {
			return;
		};
		if BlockType::from_id(digblock_core::chunk::cell_id(prev)) != BlockType::Wardglass {
			return;
		}
		self.pipeline
			.set_cell(c.0, c.1, c.2, make_cell(BlockType::Air, 0));
		self.fluid.schedule_around(c.0, c.1, c.2);
	}
}
