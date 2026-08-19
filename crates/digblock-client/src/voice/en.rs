//! English. One arm per [`Msg`], matched exhaustively so a new message
//! cannot ship without words, and every arm free to order its own
//! arguments — that freedom is the whole point of the enum above it.
//!
//! Two voices live here on purpose: the flat lower-case refusals a dig
//! speaks, and the sentence-case lines the session speaks. A refusal is
//! the game answering your hand; the rest is the game telling you
//! something happened.

use super::Msg;
use digblock_net::protocol::PlayerId;

pub(super) fn line(msg: &Msg) -> String {
	match msg {
		// Names the block and the level that opens it: a refusal that
		// only says no is one the player has to go and look up. WHICH
		// skill it names is what tells the two level gates apart, so one
		// sentence serves both — Mining is "too hard for you", Alchemy
		// "you could not make one".
		Msg::DigRefused(req) => format!(
			"{} needs {} level {}",
			req.block.name().to_lowercase(),
			req.skill.display(),
			req.level
		),
		Msg::DigProtected => "this block is holding firm".into(),
		Msg::PlaceProtected => "nothing will stay put here".into(),
		Msg::NeedsSiphon => "this vein needs an essence siphon".into(),
		Msg::DigRefusedByServer => "the server refused that dig".into(),

		Msg::LevelUp { skill, level } => format!("{} is now level {level}.", skill.display()),
		Msg::RemoteLevelUp { who, skill, level } => {
			format!("{who} reached {} level {level}.", skill.display())
		}
		Msg::VeinsRevealed => "New veins shimmer into view.".into(),

		Msg::Connected => "Connected to server.".into(),
		Msg::LeftServer => "Left the server.".into(),
		Msg::LeftWorld => "Left the world.".into(),
		Msg::Joined { name } => format!("{name} joined the server."),
		Msg::Left { name } => format!("{name} left the server."),
		Msg::ConnectRefused { reason } => format!("Server refused: {reason}."),
		Msg::Disconnected { reason } => format!("Disconnected: {reason}."),
		Msg::Teleported => "Teleported.".into(),
		Msg::ReturnedToSpawn => "Returned to spawn.".into(),

		// Bracketed and neutral, so a server line cannot be mistaken for
		// a player's.
		Msg::ServerNotice { text } => format!("[server] {text}"),
		Msg::Chat { name, text } => format!("{name}: {text}"),
		Msg::CommandHint { usage, desc } => format!("{usage} - {desc}"),
	}
}

pub(super) fn unknown_player(id: PlayerId) -> String {
	format!("player {id}")
}

pub(super) fn me() -> String {
	"me".into()
}
