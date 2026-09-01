//! The keybinding table: every rebindable verb, its saved key, and the
//! lookup the input dispatch runs per key event. Enter and Escape are
//! deliberately absent — chat and menu doors never move (the Controls
//! page lists the chat door for completeness, greyed) — and a few keys
//! (Tab, Backquote, F7, the arrow aliases) keep fixed FALLBACK behavior
//! that a player's binding shadows rather than conflicts with. Identity
//! for persistence is always the name string, never a discriminant.

use digblock_platform::input::Key;

/// Every rebindable action, in menu display order. Plain variants so a
/// value indexes [`Bindings::keys`] directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
	MoveForward,
	MoveBack,
	MoveLeft,
	MoveRight,
	Jump,
	Sneak,
	Fly,
	OpenInventory,
	OpenAlchemy,
	OpenCharacter,
	OpenHiscores,
	OpenSpellbook,
	HotbarSlot1,
	HotbarSlot2,
	HotbarSlot3,
	HotbarSlot4,
	HotbarSlot5,
	HotbarSlot6,
	HotbarSlot7,
	HotbarSlot8,
	HotbarSlot9,
	CastSpell1,
	CastSpell2,
	CastSpell3,
	ToolToggle,
	Screenshot,
}

impl Action {
	pub const ALL: [Action; 26] = [
		Action::MoveForward,
		Action::MoveBack,
		Action::MoveLeft,
		Action::MoveRight,
		Action::Jump,
		Action::Sneak,
		Action::Fly,
		Action::OpenInventory,
		Action::OpenAlchemy,
		Action::OpenCharacter,
		Action::OpenHiscores,
		Action::OpenSpellbook,
		Action::HotbarSlot1,
		Action::HotbarSlot2,
		Action::HotbarSlot3,
		Action::HotbarSlot4,
		Action::HotbarSlot5,
		Action::HotbarSlot6,
		Action::HotbarSlot7,
		Action::HotbarSlot8,
		Action::HotbarSlot9,
		Action::CastSpell1,
		Action::CastSpell2,
		Action::CastSpell3,
		Action::ToolToggle,
		Action::Screenshot,
	];

	/// Stable persistence name — the settings-file contract. Renaming a
	/// variant later means keeping its old string as a `parse` alias.
	pub fn name(self) -> &'static str {
		match self {
			Action::MoveForward => "move_forward",
			Action::MoveBack => "move_back",
			Action::MoveLeft => "move_left",
			Action::MoveRight => "move_right",
			Action::Jump => "jump",
			Action::Sneak => "sneak",
			Action::Fly => "fly",
			Action::OpenInventory => "inventory",
			Action::OpenAlchemy => "alchemy",
			Action::OpenCharacter => "character",
			Action::OpenHiscores => "hiscores",
			Action::OpenSpellbook => "spellbook",
			Action::HotbarSlot1 => "hotbar_1",
			Action::HotbarSlot2 => "hotbar_2",
			Action::HotbarSlot3 => "hotbar_3",
			Action::HotbarSlot4 => "hotbar_4",
			Action::HotbarSlot5 => "hotbar_5",
			Action::HotbarSlot6 => "hotbar_6",
			Action::HotbarSlot7 => "hotbar_7",
			Action::HotbarSlot8 => "hotbar_8",
			Action::HotbarSlot9 => "hotbar_9",
			Action::CastSpell1 => "spell_1",
			Action::CastSpell2 => "spell_2",
			Action::CastSpell3 => "spell_3",
			Action::ToolToggle => "tool_toggle",
			Action::Screenshot => "screenshot",
		}
	}

	pub fn parse(s: &str) -> Option<Action> {
		Action::ALL.into_iter().find(|a| a.name() == s)
	}

	/// The Controls page row text.
	pub fn label(self) -> &'static str {
		match self {
			Action::MoveForward => "Walk Forward",
			Action::MoveBack => "Walk Back",
			Action::MoveLeft => "Walk Left",
			Action::MoveRight => "Walk Right",
			Action::Jump => "Jump",
			Action::Sneak => "Sneak",
			Action::Fly => "Fly Toggle",
			Action::OpenInventory => "Inventory",
			Action::OpenAlchemy => "Alchemy",
			Action::OpenCharacter => "Character",
			Action::OpenHiscores => "Hiscores",
			Action::OpenSpellbook => "Spellbook",
			Action::HotbarSlot1 => "Hotbar Slot 1",
			Action::HotbarSlot2 => "Hotbar Slot 2",
			Action::HotbarSlot3 => "Hotbar Slot 3",
			Action::HotbarSlot4 => "Hotbar Slot 4",
			Action::HotbarSlot5 => "Hotbar Slot 5",
			Action::HotbarSlot6 => "Hotbar Slot 6",
			Action::HotbarSlot7 => "Hotbar Slot 7",
			Action::HotbarSlot8 => "Hotbar Slot 8",
			Action::HotbarSlot9 => "Hotbar Slot 9",
			Action::CastSpell1 => "Cast Spell 1",
			Action::CastSpell2 => "Cast Spell 2",
			Action::CastSpell3 => "Cast Spell 3",
			Action::ToolToggle => "Tool Ability",
			Action::Screenshot => "Screenshot",
		}
	}

	/// `HotbarSlotN` as the digit `select_digit` speaks (1..=9).
	pub fn hotbar_digit(self) -> Option<u8> {
		let i = self as usize;
		let first = Action::HotbarSlot1 as usize;
		let last = Action::HotbarSlot9 as usize;
		(first..=last).contains(&i).then(|| (i - first) as u8 + 1)
	}

	fn idx(self) -> usize {
		self as usize
	}
}

/// One saved key per action; `None` is unbound. Lookups are linear scans
/// over 26 entries, run once per key event.
#[derive(Clone, PartialEq, Debug)]
pub struct Bindings {
	keys: [Option<Key>; Action::ALL.len()],
}

impl Default for Bindings {
	fn default() -> Self {
		let mut b = Bindings {
			keys: [None; Action::ALL.len()],
		};
		let defaults: [(Action, Key); 26] = [
			(Action::MoveForward, Key::W),
			(Action::MoveBack, Key::S),
			(Action::MoveLeft, Key::A),
			(Action::MoveRight, Key::D),
			(Action::Jump, Key::Space),
			(Action::Sneak, Key::Shift),
			(Action::Fly, Key::F),
			(Action::OpenInventory, Key::I),
			(Action::OpenAlchemy, Key::K),
			(Action::OpenCharacter, Key::C),
			(Action::OpenHiscores, Key::H),
			(Action::OpenSpellbook, Key::J),
			(Action::HotbarSlot1, Key::Digit(1)),
			(Action::HotbarSlot2, Key::Digit(2)),
			(Action::HotbarSlot3, Key::Digit(3)),
			(Action::HotbarSlot4, Key::Digit(4)),
			(Action::HotbarSlot5, Key::Digit(5)),
			(Action::HotbarSlot6, Key::Digit(6)),
			(Action::HotbarSlot7, Key::Digit(7)),
			(Action::HotbarSlot8, Key::Digit(8)),
			(Action::HotbarSlot9, Key::Digit(9)),
			(Action::CastSpell1, Key::E),
			(Action::CastSpell2, Key::R),
			(Action::CastSpell3, Key::T),
			(Action::ToolToggle, Key::Q),
			(Action::Screenshot, Key::F8),
		];
		for (a, k) in defaults {
			b.keys[a.idx()] = Some(k);
		}
		b
	}
}

impl Bindings {
	/// The dispatch lookup: what does this key do?
	pub fn action(&self, key: Key) -> Option<Action> {
		Action::ALL
			.into_iter()
			.find(|a| self.keys[a.idx()] == Some(key))
	}

	pub fn key(&self, a: Action) -> Option<Key> {
		self.keys[a.idx()]
	}

	/// The hint/menu text for an action's key.
	pub fn label(&self, a: Action) -> &'static str {
		match self.keys[a.idx()] {
			Some(k) => k.label(),
			None => "Unbound",
		}
	}

	/// Doors that never move (and the unnameable catch-all).
	pub fn bindable(key: Key) -> bool {
		!matches!(key, Key::Enter | Key::Escape | Key::Other)
	}

	/// Steal policy: the key's previous owner (if any) is unbound and
	/// returned, so the menu can name the victim. Unbindable keys are
	/// refused (`None` returned, nothing changes).
	pub fn bind(&mut self, a: Action, key: Key) -> Option<Action> {
		if !Self::bindable(key) {
			return None;
		}
		let victim = Action::ALL
			.into_iter()
			.find(|&v| v != a && self.keys[v.idx()] == Some(key));
		if let Some(v) = victim {
			self.keys[v.idx()] = None;
		}
		self.keys[a.idx()] = Some(key);
		victim
	}

	pub fn unbind(&mut self, a: Action) {
		self.keys[a.idx()] = None;
	}

	/// Normalize an untrusted table (a hand-edited file): unbindable keys
	/// drop, and a key held by two actions stays with the one earlier in
	/// `Action::ALL` — deterministic, matching `from_names` order.
	pub fn sanitize(&mut self) {
		for i in 0..self.keys.len() {
			let Some(k) = self.keys[i] else { continue };
			if !Self::bindable(k) {
				self.keys[i] = None;
				continue;
			}
			for later in self.keys[i + 1..].iter_mut() {
				if *later == Some(k) {
					*later = None;
				}
			}
		}
	}

	/// The persisted form: every action, name to key-name, "" = unbound.
	pub fn to_names(&self) -> std::collections::BTreeMap<String, String> {
		Action::ALL
			.into_iter()
			.map(|a| {
				let key = self.keys[a.idx()].map(Key::name).unwrap_or("");
				(a.name().to_owned(), key.to_owned())
			})
			.collect()
	}

	/// Rebuild from persisted names over `base` (normally the defaults).
	/// Unknown action or key names are skipped — forward compatibility in
	/// both directions — and entries apply in `Action::ALL` order so
	/// duplicate-key steals resolve the same way every load.
	pub fn from_names<'a>(
		base: Bindings,
		entries: impl Iterator<Item = (&'a str, &'a str)>,
	) -> Bindings {
		let entries: Vec<(&str, &str)> = entries.collect();
		let mut b = base;
		for a in Action::ALL {
			let Some(&(_, key)) = entries.iter().find(|(name, _)| *name == a.name()) else {
				continue;
			};
			if key.is_empty() {
				b.keys[a.idx()] = None;
			} else if let Some(k) = Key::parse(key) {
				b.keys[a.idx()] = Some(k);
			}
		}
		b.sanitize();
		b
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_bind_every_action_without_collisions() {
		let b = Bindings::default();
		for a in Action::ALL {
			let k = b
				.key(a)
				.unwrap_or_else(|| panic!("{a:?} must have a default key"));
			assert_eq!(
				b.action(k),
				Some(a),
				"{a:?}'s default key must look back up to it"
			);
			assert!(Bindings::bindable(k));
		}
	}

	#[test]
	fn action_names_roundtrip_and_are_unique() {
		for a in Action::ALL {
			assert_eq!(Action::parse(a.name()), Some(a));
		}
		for (i, a) in Action::ALL.into_iter().enumerate() {
			for b in Action::ALL[i + 1..].iter() {
				assert_ne!(a.name(), b.name());
				assert_ne!(a.label(), b.label());
			}
		}
		assert_eq!(Action::parse("no_such_action"), None);
	}

	#[test]
	fn hotbar_digits_map_one_to_nine() {
		assert_eq!(Action::HotbarSlot1.hotbar_digit(), Some(1));
		assert_eq!(Action::HotbarSlot9.hotbar_digit(), Some(9));
		assert_eq!(Action::MoveForward.hotbar_digit(), None);
		assert_eq!(Action::CastSpell1.hotbar_digit(), None);
		for a in Action::ALL {
			if let Some(d) = a.hotbar_digit() {
				assert_eq!(Bindings::default().key(a), Some(Key::Digit(d)));
			}
		}
	}

	#[test]
	fn bind_steals_and_reports_the_victim() {
		let mut b = Bindings::default();
		// F belongs to Fly; giving it to Screenshot unbinds Fly.
		assert_eq!(b.bind(Action::Screenshot, Key::F), Some(Action::Fly));
		assert_eq!(b.key(Action::Fly), None);
		assert_eq!(b.action(Key::F), Some(Action::Screenshot));
		assert_eq!(b.label(Action::Fly), "Unbound");
		// Rebinding an action to its own key is not a steal.
		assert_eq!(b.bind(Action::Jump, Key::Space), None);
		assert_eq!(b.key(Action::Jump), Some(Key::Space));
		// Doors never move.
		assert_eq!(b.bind(Action::Jump, Key::Enter), None);
		assert_eq!(b.key(Action::Jump), Some(Key::Space));
		assert_eq!(b.bind(Action::Jump, Key::Escape), None);
		assert_eq!(b.key(Action::Jump), Some(Key::Space));
	}

	#[test]
	fn sanitize_drops_doors_and_duplicates_deterministically() {
		let mut b = Bindings::default();
		b.keys[Action::Jump.idx()] = Some(Key::Enter);
		b.keys[Action::Sneak.idx()] = Some(Key::W); // duplicates MoveForward
		b.sanitize();
		assert_eq!(b.key(Action::Jump), None);
		// MoveForward is earlier in ALL, so it keeps W.
		assert_eq!(b.key(Action::MoveForward), Some(Key::W));
		assert_eq!(b.key(Action::Sneak), None);
	}

	#[test]
	fn names_roundtrip_including_unbound() {
		let mut b = Bindings::default();
		b.bind(Action::CastSpell1, Key::G);
		b.unbind(Action::Fly);
		let names = b.to_names();
		assert_eq!(names.get("spell_1").map(String::as_str), Some("g"));
		assert_eq!(names.get("fly").map(String::as_str), Some(""));
		let back = Bindings::from_names(
			Bindings::default(),
			names.iter().map(|(k, v)| (k.as_str(), v.as_str())),
		);
		assert_eq!(back, b);
	}

	#[test]
	fn from_names_skips_unknown_entries_and_sanitizes() {
		let entries = [
			("no_such_action", "q"), // unknown action: skipped
			("jump", "no_such_key"), // unknown key: default kept
			("fly", "enter"),        // door: sanitized away
			("sneak", "w"),          // duplicate of move_forward: later loses
			("spell_1", "bracket_left"),
		];
		let b = Bindings::from_names(Bindings::default(), entries.into_iter());
		assert_eq!(b.key(Action::Jump), Some(Key::Space));
		assert_eq!(b.key(Action::Fly), None);
		assert_eq!(b.key(Action::MoveForward), Some(Key::W));
		assert_eq!(b.key(Action::Sneak), None);
		assert_eq!(b.key(Action::CastSpell1), Some(Key::BracketLeft));
	}
}
