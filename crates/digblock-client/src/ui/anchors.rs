//! Named on-screen elements the tutorial points at, in ONE id space for
//! both halves of the contract: a step highlights `AnchorId::X` and its
//! gate fires from the same identity riding the UI events. Hit-mapped
//! elements answer through the frame's own [`super::HitMap`] — the rect
//! that highlights is the rect that drew, by construction — and the
//! pure-geometry touch buttons (which never register hits) answer
//! through the same `*_rect` functions that draw and claim them.

use super::hud::SafeArea;
use super::{HitMap, InvId, Rect};

/// A named thing on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AnchorId {
	/// Any hit-registered element: a hotbar slot, a grid cell, a tile.
	Ui(InvId),
	/// The touch HUD's corner menu button (route to every screen).
	CornerMenu,
	/// The touch HUD's place button.
	PlaceButton,
	/// The touch HUD's jump button (when the setting shows it).
	JumpButton,
}

/// Where `id` is this frame, or None when it is not on screen (hit maps
/// only carry what actually drew).
pub(crate) fn anchor_rect(
	id: AnchorId,
	hits: &HitMap<InvId>,
	size: (u32, u32),
	scale: f32,
	insets: &SafeArea,
) -> Option<Rect> {
	match id {
		AnchorId::Ui(i) => hits.rect_of(i),
		AnchorId::CornerMenu => Some(super::hud::corner_button_rect(size, scale, insets)),
		AnchorId::PlaceButton => Some(super::hud::place_button_rect(size, scale, insets)),
		AnchorId::JumpButton => Some(super::hud::jump_button_rect(size, scale, insets)),
	}
}
