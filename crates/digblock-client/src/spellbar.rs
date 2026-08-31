//! The spell bar: three assignable slots bottom-left of the gameplay
//! HUD, filled by dragging rows out of the spellbook and fired by hotkey
//! (desktop) or tap (touch). This module owns the `InvId::Spell` id
//! space — the persistent key a slot stores — and the bar's per-frame
//! view; the spellbook imports the ids so the two cannot drift. The
//! assignments are character state: they ride the wire beside the item
//! layout and persist server-side (or in the local `WorldSave`).

use digblock_core::unlock;
use digblock_core::worldgen::SurfaceKind;

use crate::ui::screens::spellbook::SpellView;

pub(crate) const SPELL_SLOTS: usize = 3;
/// The empty sentinel — 0 is taken by the Spawn Waystone.
pub(crate) const EMPTY_SPELL: u8 = u8::MAX;

/// The non-port ids in the `InvId::Spell` space — ports use `1 + kind
/// index`, which tops out well below these. Slot 102 was the retired
/// Mason's Arc stance and stays unassigned forever.
pub(crate) const SLOT_HOME: u8 = 0;
pub(crate) const SLOT_WISP: u8 = 100;
pub(crate) const SLOT_KINDLE: u8 = 101;
pub(crate) const SLOT_SPHERE: u8 = 103;
pub(crate) const SLOT_CAUSEWAY: u8 = 104;

/// The `SurfaceKind` behind a port id (`1 + declaration index`) — only
/// for ids that name a LADDERED port (a kind with a waystone level).
pub(crate) fn port_kind(id: u8) -> Option<SurfaceKind> {
	let idx = id.checked_sub(1)? as usize;
	let k = *SurfaceKind::ALL.get(idx)?;
	unlock::waystone(k).map(|_| k)
}

/// Whether a stored byte names a spell at all — the read-side gate for
/// wire and save bytes (unknown ids, 102 included, read as empty).
pub(crate) fn valid_spell_id(id: u8) -> bool {
	matches!(
		id,
		SLOT_HOME | SLOT_WISP | SLOT_KINDLE | SLOT_SPHERE | SLOT_CAUSEWAY
	) || port_kind(id).is_some()
}

/// The voice's name for a refusal about this spell — teleports of every
/// stripe are the waystone.
pub(crate) fn voice_spell(id: u8) -> crate::voice::Spell {
	match id {
		SLOT_WISP => crate::voice::Spell::Wisp,
		SLOT_KINDLE => crate::voice::Spell::Kindle,
		SLOT_SPHERE => crate::voice::Spell::Sphere,
		SLOT_CAUSEWAY => crate::voice::Spell::Causeway,
		_ => crate::voice::Spell::Waystone,
	}
}

/// The bar's name for a spell — the tooltip's noun. Ports reuse the
/// spellbook's own labels, so the bar and the book agree.
pub(crate) fn spell_name(id: u8) -> &'static str {
	match id {
		SLOT_HOME => "Spawn Waystone",
		SLOT_WISP => "Wisp",
		SLOT_KINDLE => "Kindle",
		SLOT_SPHERE => "Sphere",
		SLOT_CAUSEWAY => "Causeway",
		id => crate::ui::screens::spellbook::port_label(id.saturating_sub(1)),
	}
}

/// The sprite for a spell: a materials-array layer and a tint. Every
/// port draws the one near-white monolith tinted by its essence's core
/// colour — seven ports, one texture.
pub(crate) fn spell_icon(id: u8) -> (u16, u32) {
	use digblock_core::block::tex;
	const WHITE: u32 = 0xFFFF_FFFF;
	match id {
		SLOT_HOME => (tex::SPELL_HOME, WHITE),
		SLOT_WISP => (tex::SPELL_WISP, WHITE),
		SLOT_KINDLE => (tex::SPELL_KINDLE, WHITE),
		SLOT_SPHERE => (tex::SPELL_SPHERE, WHITE),
		SLOT_CAUSEWAY => (tex::SPELL_CAUSEWAY, WHITE),
		id => {
			let tint = port_kind(id)
				.and_then(digblock_core::alchemy::cast_price)
				.map_or(WHITE, |(essence, _)| {
					let ([r, g, b], _) = essence.palette();
					0xFF00_0000 | u32::from(b) << 16 | u32::from(g) << 8 | u32::from(r)
				});
			(tex::SPELL_PORT, tint)
		}
	}
}

/// The three assignments. Plain data, no readiness — that is the frame's
/// [`SpellBarView`] business.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpellBar {
	slots: [u8; SPELL_SLOTS],
}

impl Default for SpellBar {
	fn default() -> Self {
		Self {
			slots: [EMPTY_SPELL; SPELL_SLOTS],
		}
	}
}

impl SpellBar {
	pub(crate) fn get(&self, i: usize) -> Option<u8> {
		let id = *self.slots.get(i)?;
		(id != EMPTY_SPELL).then_some(id)
	}

	/// Assign `spell` to `slot`, clearing any OTHER slot that held it —
	/// one spell lives in one slot.
	pub(crate) fn assign(&mut self, slot: usize, spell: u8) {
		if slot >= SPELL_SLOTS || !valid_spell_id(spell) {
			return;
		}
		for s in &mut self.slots {
			if *s == spell {
				*s = EMPTY_SPELL;
			}
		}
		self.slots[slot] = spell;
	}

	pub(crate) fn swap(&mut self, a: usize, b: usize) {
		if a < SPELL_SLOTS && b < SPELL_SLOTS {
			self.slots.swap(a, b);
		}
	}

	pub(crate) fn clear(&mut self, slot: usize) {
		if slot < SPELL_SLOTS {
			self.slots[slot] = EMPTY_SPELL;
		}
	}

	pub(crate) fn to_wire(self) -> [u8; SPELL_SLOTS] {
		self.slots
	}

	/// Forgiving decode for wire and save bytes: pad short, drop extras,
	/// and read unknown ids (the retired 102 included) as empty.
	pub(crate) fn from_wire(bytes: &[u8]) -> Self {
		let mut bar = Self::default();
		for (slot, &b) in bar.slots.iter_mut().zip(bytes.iter()) {
			if valid_spell_id(b) {
				*slot = b;
			}
		}
		// A duplicated spell (a hand-edited save) keeps its FIRST slot.
		for i in 0..SPELL_SLOTS {
			let Some(id) = bar.get(i) else { continue };
			for later in bar.slots[i + 1..].iter_mut() {
				if *later == id {
					*later = EMPTY_SPELL;
				}
			}
		}
		bar
	}
}

/// One slot as this frame draws it.
pub struct SlotView {
	pub spell: Option<u8>,
	/// Activating it now would start the cast.
	pub ready: bool,
	/// The teleport countdown, drawn as a corner badge while it runs.
	pub badge: Option<String>,
	/// The hover tooltip: the name, with the not-ready reason in parens.
	pub tooltip: String,
}

/// The bar's per-frame view: readiness resolved through the SAME
/// [`SpellView`] the spellbook rows are built from, so the bar can never
/// promise what the book would refuse.
pub struct SpellBarView {
	pub slots: [SlotView; SPELL_SLOTS],
	/// Shell-reported safe-area insets, carried so the pages naming a
	/// hovered slot need no second new parameter.
	pub insets: crate::ui::hud::SafeArea,
}

impl SpellBarView {
	/// A bar with nothing in it — screens under test, and any caller
	/// with no live engine behind it.
	pub fn empty() -> Self {
		Self {
			slots: std::array::from_fn(|_| SlotView {
				spell: None,
				ready: false,
				badge: None,
				tooltip: String::new(),
			}),
			insets: crate::ui::hud::SafeArea::default(),
		}
	}

	pub(crate) fn build(
		bar: &SpellBar,
		view: &SpellView,
		cooldown_secs: u32,
		insets: crate::ui::hud::SafeArea,
	) -> Self {
		let slots = std::array::from_fn(|i| match bar.get(i) {
			None => SlotView {
				spell: None,
				ready: false,
				badge: None,
				tooltip: String::new(),
			},
			Some(id) => {
				let (ready, note) = view.slot_info(id);
				let teleport = id == SLOT_HOME || port_kind(id).is_some();
				let badge =
					(teleport && !ready && cooldown_secs > 0).then(|| format!("{cooldown_secs}s"));
				let tooltip = match &note {
					Some(note) => format!("{} ({note})", spell_name(id)),
					None => spell_name(id).to_string(),
				};
				SlotView {
					spell: Some(id),
					ready,
					badge,
					tooltip,
				}
			}
		});
		Self { slots, insets }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn assign_dedupes_and_refuses_junk() {
		let mut bar = SpellBar::default();
		bar.assign(0, SLOT_WISP);
		bar.assign(2, SLOT_WISP);
		assert_eq!(bar.get(0), None, "one spell lives in one slot");
		assert_eq!(bar.get(2), Some(SLOT_WISP));
		bar.assign(1, 102); // the retired stance
		assert_eq!(bar.get(1), None);
		bar.assign(9, SLOT_HOME); // out of range
		assert_eq!(bar.to_wire(), [EMPTY_SPELL, EMPTY_SPELL, SLOT_WISP]);
	}

	#[test]
	fn swap_and_clear() {
		let mut bar = SpellBar::default();
		bar.assign(0, SLOT_HOME);
		bar.assign(1, SLOT_SPHERE);
		bar.swap(0, 1);
		assert_eq!(bar.get(0), Some(SLOT_SPHERE));
		assert_eq!(bar.get(1), Some(SLOT_HOME));
		bar.clear(0);
		assert_eq!(bar.get(0), None);
	}

	#[test]
	fn from_wire_pads_truncates_and_drops_junk() {
		// Short: the missing slots read empty.
		let bar = SpellBar::from_wire(&[SLOT_WISP]);
		assert_eq!(bar.to_wire(), [SLOT_WISP, EMPTY_SPELL, EMPTY_SPELL]);
		// Long: extras drop. The port id is a REAL one from the roster —
		// not every SurfaceKind is a laddered port.
		let port = 1 + crate::ui::screens::spellbook::all_ports()[0].idx;
		let bar = SpellBar::from_wire(&[SLOT_HOME, port, SLOT_CAUSEWAY, SLOT_WISP, SLOT_KINDLE]);
		assert_eq!(bar.to_wire(), [SLOT_HOME, port, SLOT_CAUSEWAY]);
		// Junk ids (retired 102, out-of-space 200, a port index past the
		// roster) read as empty; a duplicate keeps its first slot.
		let bar = SpellBar::from_wire(&[102, 200, SLOT_WISP]);
		assert_eq!(bar.to_wire(), [EMPTY_SPELL, EMPTY_SPELL, SLOT_WISP]);
		let bar = SpellBar::from_wire(&[SLOT_WISP, SLOT_WISP, SLOT_WISP]);
		assert_eq!(bar.to_wire(), [SLOT_WISP, EMPTY_SPELL, EMPTY_SPELL]);
	}

	#[test]
	fn every_spellbook_id_is_valid_and_named() {
		let mut ids = vec![
			SLOT_HOME,
			SLOT_WISP,
			SLOT_KINDLE,
			SLOT_SPHERE,
			SLOT_CAUSEWAY,
		];
		for port in crate::ui::screens::spellbook::all_ports() {
			ids.push(1 + port.idx);
		}
		for id in ids {
			assert!(valid_spell_id(id), "{id} must be assignable");
			assert!(!spell_name(id).is_empty());
			let (layer, tint) = spell_icon(id);
			assert!(layer >= digblock_core::block::tex::SPELL_WISP);
			assert_ne!(tint, 0);
		}
		// Every port wears the shared monolith, each in its own tint.
		let tints: Vec<u32> = crate::ui::screens::spellbook::all_ports()
			.iter()
			.map(|p| spell_icon(1 + p.idx).1)
			.collect();
		assert!(tints.len() > 1);
	}
}
