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

use digblock_core::block::BlockType;
use digblock_core::skills::Skill;
use digblock_core::unlock::Requirement;
use digblock_net::messages::AdminReply;
use digblock_net::protocol::PlayerId;
use std::ops::Range;

/// What a piece of a line IS, so the feed can colour STRUCTURE rather
/// than words. The locale divides its own sentence: where a command ends
/// and the note explaining it begins is a fact about the language, and a
/// caller that split on punctuation would be guessing at one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ink {
	/// The feed entry's own colour — chat keeps its sender's tint.
	Plain,
	Command,
	Arg,
	Note,
}

/// A spoken line and where its colour changes. `runs` covers `text` end
/// to end in order, so a caller wraps the whole string ONCE and still
/// knows what each row it gets back is made of.
#[derive(Clone, Debug)]
pub(crate) struct Line {
	pub text: String,
	pub runs: Vec<(Range<usize>, Ink)>,
}

impl Line {
	/// One undivided run, which is what all but the catalog rows speak.
	pub(crate) fn plain(text: String) -> Self {
		Self {
			runs: vec![(0..text.len(), Ink::Plain)],
			text,
		}
	}

	/// Concatenate inked pieces, merging neighbours that share an ink so
	/// a row draws in as few runs as it has colours.
	pub(crate) fn parts(parts: &[(&str, Ink)]) -> Self {
		let mut text = String::new();
		let mut runs: Vec<(Range<usize>, Ink)> = Vec::with_capacity(parts.len());
		for &(s, ink) in parts {
			let start = text.len();
			text.push_str(s);
			match runs.last_mut() {
				Some((r, prev)) if *prev == ink => r.end = text.len(),
				_ => runs.push((start..text.len(), ink)),
			}
		}
		Self { text, runs }
	}

	/// The pieces of one wrapped row, clipped to it and in order — one
	/// draw each. `row` is a byte range of `text` as `font::wrap` hands
	/// them back, which may skip the space a soft break ate.
	pub(crate) fn row(&self, row: Range<usize>) -> impl Iterator<Item = (&str, Ink)> {
		self.runs.iter().filter_map(move |(r, ink)| {
			let (a, b) = (r.start.max(row.start), r.end.min(row.end));
			(a < b).then(|| (&self.text[a..b], *ink))
		})
	}
}

/// Which spell a refusal answers. A refusal that cannot name its spell says the wrong thing about
/// two spells out of three — the sphere's fare refused as a waystone's passage, kindle's rung as a
/// locked port. So the spell is an ARGUMENT, and every caller that knows which one it cast passes
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Spell {
	Waystone,
	Wisp,
	Kindle,
	Sphere,
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
	/// A conjuring with no place to land: no cell for the light, or a sphere closing on a player.
	CastBadTarget,
	/// The Sphere with nothing in hand to build it of.
	SphereNoBlock,
	/// The Sphere with a block no shell can be built of — named here, unnamed from the server's refusal.
	SphereBadBlock(Option<BlockType>),
	/// The Sphere with too few of the held block: (block, the shell's count, the stock), or none from the server.
	SphereBlocksShort(Option<(BlockType, u32, u32)>),

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
	/// The siphon has just been made: every surface source fades into the
	/// world at once.
	SourcesRevealed,
	/// A Mining rung opened a whole dig family (`unlock::family_opened`).
	NowDiggable(digblock_core::block::DigFamily),
	// ---- the session ----
	Connected,
	/// The server just joined is not on the master's trusted list.
	UnrankedServer,
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

	// ---- the tutorial ----
	/// One step's instruction; `touch` picks the finger wording, and
	/// `jump_button` mirrors the touch jump-mode setting (only the Jump
	/// step's touch wording reads it: button vs. double-tap).
	TutorialStep {
		step: crate::tutorial::StepId,
		touch: bool,
		jump_button: bool,
	},
	/// The completion line (finished or skipped alike).
	TutorialDone,
	/// The siphon chapter's completion line.
	TutorialSiphonDone,

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
pub(crate) fn render(msg: &Msg) -> Line {
	match msg {
		// The two divided lines are lifted out rather than given a second
		// template: `en::command_hint` is the one place the row's words
		// live, and `en::line` takes its text for everything that only
		// wants a sentence.
		Msg::CommandHint { usage, desc } | Msg::Admin(AdminReply::CommandHint { usage, desc }) => {
			en::command_hint(usage, desc)
		}
		_ => Line::plain(en::line(msg)),
	}
}

/// The plain sentence for `msg`, for surfaces that draw one string
/// (the tutorial panel) rather than an inked [`Line`].
pub(crate) fn plain(msg: &Msg) -> String {
	en::line(msg)
}

/// One catalog row for a caller that holds the advertised pair rather
/// than a [`Msg`] — the suggestion list under the chat box, which offers
/// commands the server never replied about.
pub(crate) fn command_hint(usage: &str, desc: &str) -> Line {
	en::command_hint(usage, desc)
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
