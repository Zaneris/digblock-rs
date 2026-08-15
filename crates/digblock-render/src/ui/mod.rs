//! Reusable UI framework: scale math, retained-per-frame draw commands,
//! hit regions, and drag state — the pure, headless-testable layer every
//! screen builds on. GPU consumption lives in `overlay.rs` (quads/text)
//! and `ui/icons.rs` (block-texture icons); everything here is data.

pub mod drag;
pub mod draw;
pub mod hit;
pub mod hud;
pub mod icons;
pub mod scale;
pub mod screens;
pub mod shaped_icon;
pub mod theme;
pub mod widgets;

pub use draw::{DrawList, Layer};
pub use hit::HitMap;
pub use widgets::{Style, Ui};

/// One item trading places with another **in a single slot**: at `u`
/// (0..=1 through the handover), is the INCOMING item the one to draw,
/// and how present is it (0..=1, scaling both its size and its alpha)?
///
/// The outgoing item shrinks away over the first half and the incoming
/// one grows in over the second, so the slot never shows two icons at
/// once — cross-fading two overlapping 3D icons is a double exposure of
/// their internal faces, and the effect wanted is a rotation, not a
/// dissolve. Present passes through exactly 0 at the changeover, which
/// is what lets everything else about the exchange (which tool the rail
/// lists, which member a family tile names) happen unseen.
///
/// Shared by the transmute tab's rotating family tiles and the hotbar's
/// tool slot: a swap should look like a swap wherever it happens.
pub fn handover(u: f32) -> (bool, f32) {
	if u < 0.5 {
		(false, 1.0 - 2.0 * u)
	} else {
		(true, 2.0 * u - 1.0)
	}
}

impl From<[f32; 4]> for Rect {
	fn from([x, y, w, h]: [f32; 4]) -> Self {
		Self { x, y, w, h }
	}
}

/// Slot identity shared by the in-game HUD (whose hotbar rects double
/// as drop targets), the inventory screen, and the alchemy screen —
/// one id space so the frame keeps a single hit map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvId {
	/// One cell of the inventory grid (index into the built grid list).
	Grid(u16),
	/// One hotbar slot.
	Bar(u8),
	/// Alchemy: one member tile of the selected transmute family.
	Craft(u8),
	/// Alchemy: one transmute family tile (index into `FAMILIES`).
	Family(u8),
	/// Alchemy: the transmute tab's single TRANSMUTE button.
	CraftGo,
	/// Alchemy: one of the two tab-strip buttons (0 extract, 1 transmute).
	Tab(u8),
	/// Alchemy: the extraction drop target (a pointer, not a container).
	ExtractSlot,
	/// Alchemy: the bucket-charge tile beside the grid (drag source; the
	/// charge can't live in the grid — water is not slottable).
	BucketTile,
}

/// Pixel-space rectangle, y-down from the top-left (the overlay's
/// coordinate space).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
	pub x: f32,
	pub y: f32,
	pub w: f32,
	pub h: f32,
}

impl Rect {
	pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
		Self { x, y, w, h }
	}

	pub fn contains(&self, x: f32, y: f32) -> bool {
		x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
	}

	pub fn arr(self) -> [f32; 4] {
		[self.x, self.y, self.w, self.h]
	}

	/// Center a `w`×`h` box inside this rect.
	pub fn centered(&self, w: f32, h: f32) -> Self {
		Self::new(
			self.x + (self.w - w) / 2.0,
			self.y + (self.h - h) / 2.0,
			w,
			h,
		)
	}
}
