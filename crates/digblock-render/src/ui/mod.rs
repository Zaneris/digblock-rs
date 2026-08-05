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
pub mod widgets;

pub use draw::{DrawList, Layer};
pub use hit::HitMap;
pub use widgets::{Style, Ui};

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
	/// Alchemy: a convertible holding's "break down" button (index into
	/// the built convertibles list).
	Convert(u8),
	/// Alchemy: a craft row's button (index into `shapes::CRAFTABLE`).
	Craft(u8),
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
