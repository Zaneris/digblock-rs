//! Reusable UI framework: scale math, retained-per-frame draw commands,
//! hit regions, and drag state — the pure, headless-testable layer every
//! screen builds on. GPU consumption lives in `overlay.rs` (quads/text)
//! and `ui/icons.rs` (block-texture icons); everything here is data.

pub(crate) mod anchors;
pub mod drag;
pub mod draw;
pub mod hit;
pub mod hud;
pub mod icons;
pub mod scale;
pub mod screens;
pub mod shaped_icon;
pub mod theme;
pub(crate) mod tutorial;
pub mod widgets;

pub use draw::{DrawList, Layer};
pub use hit::HitMap;
pub use widgets::{Style, Ui};

/// One item trading places with another **in a single slot**: at `u` (0..=1 through the
/// handover), is the INCOMING item the one to draw, and how present is it (0..=1, scaling size
/// and alpha)? Outgoing shrinks away, then incoming grows in, so the slot never shows two icons
/// at once — cross-fading two 3D icons is a double exposure of their internal faces — and
/// present passes through exactly 0 at the changeover. Shared by transmute tiles and the hotbar.
pub fn handover(u: f32) -> (bool, f32) {
	use digblock_core::ease::Curve;
	if u < 0.5 {
		// The outgoing shrink stays linear: a departure should not linger.
		(false, 1.0 - 2.0 * u)
	} else {
		// The incoming grow rides the hermite: it leaves the handover at
		// zero SLOPE too (the moment of nothing stays a moment), then
		// lands softly instead of ramping.
		(true, Curve::Smooth.apply(2.0 * u - 1.0))
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
	/// Alchemy: the screen's single TRANSMUTE button.
	CraftGo,
	/// Spellbook: one spell row (index into the roster the page built).
	Spell(u8),
	/// Spellbook: the fare chip in `Spell(n)`, its own id so a tooltip can name the rect it drew.
	SpellFare(u8),
	/// One spell-bar slot (0..SPELL_SLOTS) — the spellbook's drop targets.
	SpellBar(u8),
	/// Hiscores: one board tab (overall first, then the skills in roster order).
	Board(u8),
	/// Hiscores: the pager's two ends.
	BoardPrev,
	BoardNext,
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
