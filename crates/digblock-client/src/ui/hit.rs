//! Hit regions with typed ids, filled by the same geometry pass that
//! emits draw commands — the "one layout serves render AND hit-test"
//! invariant (menu.rs's discipline), made structural.

use super::Rect;

pub struct HitMap<Id> {
	entries: Vec<(Rect, Id)>,
}

// Manual: `#[derive(Default)]` would needlessly require `Id: Default`.
impl<Id> Default for HitMap<Id> {
	fn default() -> Self {
		Self {
			entries: Vec::new(),
		}
	}
}

impl<Id: Copy + PartialEq> HitMap<Id> {
	pub fn clear(&mut self) {
		self.entries.clear();
	}

	pub fn push(&mut self, rect: Rect, id: Id) {
		self.entries.push((rect, id));
	}

	/// Topmost hit wins: later pushes overdraw earlier ones on screen,
	/// so they win the hit-test too.
	pub fn hit(&self, x: f32, y: f32) -> Option<Id> {
		self.entries
			.iter()
			.rev()
			.find(|(r, _)| r.contains(x, y))
			.map(|(_, id)| *id)
	}

	pub fn rect_of(&self, id: Id) -> Option<Rect> {
		self.entries
			.iter()
			.find(|(_, entry)| *entry == id)
			.map(|(r, _)| *r)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn last_pushed_wins_and_rect_of_finds() {
		let mut hits: HitMap<u8> = HitMap::default();
		hits.push(Rect::new(0.0, 0.0, 100.0, 100.0), 1);
		hits.push(Rect::new(25.0, 25.0, 50.0, 50.0), 2);
		assert_eq!(hits.hit(50.0, 50.0), Some(2), "topmost wins");
		assert_eq!(hits.hit(10.0, 10.0), Some(1));
		assert_eq!(hits.hit(200.0, 200.0), None);
		assert_eq!(hits.rect_of(2), Some(Rect::new(25.0, 25.0, 50.0, 50.0)));
		hits.clear();
		assert_eq!(hits.hit(50.0, 50.0), None);
	}
}
