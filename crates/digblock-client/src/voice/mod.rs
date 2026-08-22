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

/// One line the game speaks. Arguments stay TYPED and separate — never
/// pre-formatted into the message — so a locale is free to reorder them,
/// case them its own way, or leave one out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Msg {
	// ---- refusals, the four a dig or a place can meet ----
	/// A level gate: the requirement names the block, the skill and the
	/// level, so the line can say all three.
	DigRefused(Requirement),
	/// A permanently protected cell, and its opposite number. Two
	/// messages and not one: "won't break" and "can't build here" are
	/// different situations, and one sentence for both reads as a bug.
	DigProtected,
	PlaceProtected,
	/// A revealed vein with the wrong tool in hand — the one refusal a
	/// player clears by switching hands rather than by levelling.
	NeedsSiphon,
	/// A refusal the server named a reason for that this client cannot
	/// turn into a gate — the cell is no longer resident, or the two
	/// disagree about what stands there.
	DigRefusedByServer,

	/// A cast channel broken by moving, digging or opening a surface —
	/// the player did something else, so the spell let go.
	CastInterrupted,
	/// A port the server refused as beyond the caster's Arcana. Rare on
	/// an honest client — the spellbook greys what it can predict.
	WaystoneLocked,
	/// A port refused for the fare, before any debit.
	CastCantPay,
	/// A kindle with nowhere to land — no aim, or a cell that cannot
	/// hold conjured light.
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

	// ---- text that arrives as text ----
	/// A free-form server line. What remains here after admin replies
	/// were typed: text a server operator authored, which nothing on
	/// this side can translate because nothing on this side knows what
	/// it says.
	ServerNotice {
		text: String,
	},
	/// An admin command's answer. Typed all the way across the wire, so
	/// the words are chosen here like every other line a player reads.
	Admin(digblock_net::messages::AdminReply),
	/// Another player's chat. The NAME and the TEXT are theirs; only the
	/// punctuation between them is ours.
	Chat {
		name: String,
		text: String,
	},
	/// Tab completion's echo of a server-advertised command. Both halves
	/// arrive from the server; the join is ours.
	CommandHint {
		usage: String,
		desc: String,
	},
}

/// The line to draw for `msg`, in the player's language.
///
/// One locale today. Adding another is a `fn line(&Msg) -> Option<String>`
/// beside [`en`] and an arm here that falls back to English — a partial
/// translation is the normal state of a shipped one. English itself may
/// never be partial: `en::line` matches exhaustively, so a new variant
/// fails the BUILD until it has words.
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
