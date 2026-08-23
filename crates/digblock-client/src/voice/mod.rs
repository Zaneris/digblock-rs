//! Everything the game SAYS to the player, as messages rather than
//! sentences: a variant carries the block, the skill, the level or the
//! name, and the WORDS are chosen at draw time by the locale
//! ([`en::line`]). That is what makes a second language a data change —
//! a sentence baked at the call site cannot be reordered or re-cased.
//!
//! `Engine::push_feed` takes a [`Msg`], so a string literal cannot reach
//! the feed. Core NAMES things (`BlockType::name`, `Skill::display`), the
//! client SPEAKS. Screens still own their labels, which are layout as
//! much as text; they move here variant by variant when translation
//! starts, and the enum is shaped to take them.

mod en;

use digblock_core::skills::Skill;
use digblock_core::unlock::Requirement;
use digblock_net::protocol::PlayerId;

/// Which spell a refusal answers. A refusal that cannot name its spell says the wrong thing about
/// two spells out of three — the ward's fare refused as a waystone's passage, kindle's rung as a
/// locked port. So the spell is an ARGUMENT, and every caller that knows which one it cast passes
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Spell {
	Waystone,
	Wisp,
	Kindle,
	Ward,
	Causeway,
}

/// One line the game speaks. Arguments stay TYPED and separate — never
/// pre-formatted into the message — so a locale is free to reorder them,
/// case them its own way, or leave one out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Msg {
	// ---- refusals, the four a dig or a place can meet ----
	DigRefused(Requirement),
	/// A protected cell and its opposite number — two messages, since one line for both misleads.
	DigProtected,
	PlaceProtected,
	/// A revealed vein with the wrong tool in hand — cleared by switching hands, not by levelling.
	NeedsSiphon,
	/// A server refusal the client cannot turn into a gate: the cell is gone, or the two disagree.
	DigRefusedByServer,

	/// A cast channel broken by moving, digging or opening a surface — the spell let go.
	CastInterrupted,
	/// A spell beyond the caster's Arcana, and the one they tried.
	SpellLocked(Spell),
	/// A spell refused for its fare, before any debit.
	SpellFareShort(Spell),
	/// The same two, as the SERVER refuses them — `CastRefusal` carries no spell to name.
	WaystoneLocked,
	CastCantPay,
	/// A conjuring with no place to land: no cell for the light, or a ward closing on a player.
	CastBadTarget,

	// ---- progression ----
	LevelUp {
		skill: Skill,
		level: u32,
	},
	RemoteLevelUp {
		who: String,
		skill: Skill,
		level: u32,
	},
	VeinsRevealed,

	// ---- the session ----
	Connected,
	LeftServer,
	LeftWorld,
	Joined {
		name: String,
	},
	Left {
		name: String,
	},
	ConnectRefused {
		reason: String,
	},
	Disconnected {
		reason: String,
	},
	Teleported,
	ReturnedToSpawn,

	// ---- text that arrives as text: nothing on this side knows what it says ----
	ServerNotice {
		text: String,
	},
	/// An admin command's answer, typed all the way across the wire so the words are chosen here.
	Admin(digblock_net::messages::AdminReply),
	/// Another player's chat: the NAME and the TEXT are theirs, only the punctuation is ours.
	Chat {
		name: String,
		text: String,
	},
	/// Tab completion's echo of a server-advertised command — both halves theirs, the join ours.
	CommandHint {
		usage: String,
		desc: String,
	},
}

/// The line to draw for `msg`, in the player's language. One locale today; adding another is a
/// `fn line(&Msg) -> Option<String>` beside [`en`] and an arm here that falls back to English, a
/// partial translation being the normal state of a shipped one. English itself may never be
/// partial: `en::line` matches exhaustively, so a new variant fails the BUILD until it has words.
pub(crate) fn render(msg: &Msg) -> String {
	en::line(msg)
}

/// What to call a player whose name has not reached us. A NAME rather
/// than a line, but the locale owns it for the same reason.
pub(crate) fn unknown_player(id: PlayerId) -> String {
	en::unknown_player(id)
}

/// What to call yourself, when your own echo arrives before your name is
/// settled.
pub(crate) fn me() -> String {
	en::me()
}

#[cfg(test)]
mod tests;
