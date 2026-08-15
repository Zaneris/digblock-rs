//! Persistent-edit overlay: the client-side mirror of the server's
//! journal index shape (last-write-wins cells grouped by chunk column).
//! Installed into a `Pipeline`, it re-applies edits whenever a column
//! (re)generates — which is what makes hard eviction safe: a chunk that
//! streams back in gets its edits back. Kept separate from the server's
//! `JournalIndex` (digblock-persist) because core cannot depend on the
//! net crate's `BlockEdit`; the two share shape, not code.

use rustc_hash::FxHashMap;

/// Final cell value per edited cell within one chunk column.
pub type CellEdits = FxHashMap<(i32, i32, i32), u16>;

#[derive(Default)]
pub struct EditOverlay {
	columns: FxHashMap<(i32, i32), CellEdits>,
}

impl EditOverlay {
	/// Record the final cell value at a world position (LWW). True if this
	/// cell had never been recorded — which is a MESH-visible transition,
	/// not bookkeeping: `ore::revealed` short-circuits on "has this cell
	/// been touched?", so the first record on a cell is what stops its
	/// column showing a vein there.
	pub fn record(&mut self, x: i32, y: i32, z: i32, cell: u16) -> bool {
		self.columns
			.entry((x.div_euclid(16), z.div_euclid(16)))
			.or_default()
			.insert((x, y, z), cell)
			.is_none()
	}

	pub fn column(&self, cx: i32, cz: i32) -> Option<&CellEdits> {
		self.columns.get(&(cx, cz))
	}

	/// Distinct edited cells.
	pub fn len(&self) -> usize {
		self.columns.values().map(|c| c.len()).sum()
	}

	pub fn is_empty(&self) -> bool {
		self.columns.is_empty()
	}

	/// Every edit as `(x, y, z, cell)`, in arbitrary order.
	pub fn iter(&self) -> impl Iterator<Item = (i32, i32, i32, u16)> + '_ {
		self.columns
			.values()
			.flat_map(|c| c.iter().map(|(&(x, y, z), &cell)| (x, y, z, cell)))
	}
}

impl FromIterator<((i32, i32, i32), u16)> for EditOverlay {
	fn from_iter<I: IntoIterator<Item = ((i32, i32, i32), u16)>>(iter: I) -> Self {
		let mut overlay = Self::default();
		for ((x, y, z), cell) in iter {
			overlay.record(x, y, z, cell);
		}
		overlay
	}
}
