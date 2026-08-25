//! English. One arm per [`Msg`], matched exhaustively so a new message
//! cannot ship without words, and every arm free to order its own
//! arguments — that freedom is the whole point of the enum above it.
//!
//! Two voices live here on purpose: the flat lower-case refusals a dig
//! speaks, and the sentence-case lines the session speaks. A refusal is
//! the game answering your hand; the rest is the game telling you
//! something happened.

use super::{Ink, Line, Msg, Spell};
use digblock_core::block::Essence;
use digblock_net::messages::{AdminReply, AdminUsage, NoPlayer};
use digblock_net::protocol::PlayerId;

/// One catalog row — `/help`'s and Tab's alike — divided so the feed can
/// colour the command, the arguments it takes and the note apart. The
/// usage stem arrives as one token plus the rest; a stem with no
/// arguments simply has no middle piece.
pub(super) fn command_hint(usage: &str, desc: &str) -> Line {
	let (cmd, args) = usage.split_once(' ').unwrap_or((usage, ""));
	let sep = (" - ", Ink::Note);
	if args.is_empty() {
		Line::parts(&[(cmd, Ink::Command), sep, (desc, Ink::Note)])
	} else {
		Line::parts(&[
			(cmd, Ink::Command),
			(" ", Ink::Arg),
			(args, Ink::Arg),
			sep,
			(desc, Ink::Note),
		])
	}
}

pub(super) fn line(msg: &Msg) -> String {
	match msg {
		// Names the block and the level that opens it: a refusal that
		// only says no is one the player has to go and look up. WHICH
		// skill it names is what tells the two level gates apart, so one
		// sentence serves both — Mining is "too hard for you", Alchemy
		// "you could not make one".
		Msg::DigRefused(req) => format!(
			"{} needs {} level {}",
			req.block.name(),
			req.skill.name(),
			req.level
		),
		Msg::DigProtected => "this block is holding firm".into(),
		Msg::PlaceProtected => "nothing will stay put here".into(),
		Msg::NeedsSiphon => "this vein needs an essence siphon".into(),
		Msg::DigRefusedByServer => "the server refused that dig".into(),
		Msg::CastInterrupted => "the channel breaks".into(),
		Msg::SpellLocked(s) => format!("{} is beyond your arcana", spell(*s)),
		// The waystone's fare is a PASSAGE — you are buying the journey,
		// not the spell — and the conjurings are not, so the sentence
		// splits on the argument rather than papering over it.
		Msg::SpellFareShort(Spell::Waystone) => "you cannot pay the passage".into(),
		Msg::SpellFareShort(s) => format!("you cannot pay for {}", spell(*s)),
		// Spell-blind on purpose: the wire says only that the server
		// refused. Naming a spell here would name the wrong one.
		Msg::WaystoneLocked => "that casting is beyond your arcana".into(),
		Msg::CastCantPay => "you cannot pay that fare".into(),
		Msg::CastBadTarget => "nothing can be conjured here".into(),
		Msg::SphereNoBlock => "hold a block to raise the sphere from".into(),
		Msg::SphereBadBlock(Some(b)) => format!("{} cannot be raised into a sphere", b.name()),
		Msg::SphereBadBlock(None) => "that cannot be raised into a sphere".into(),
		Msg::SphereBlocksShort(Some((b, need, have))) => {
			format!("the sphere needs {need} {} and you hold {have}", b.name())
		}
		Msg::SphereBlocksShort(None) => "you do not hold enough blocks for the sphere".into(),

		Msg::LevelUp { skill, level } => format!("{} is now level {level}.", skill.name()),
		Msg::RemoteLevelUp { who, skill, level } => {
			format!("{who} reached {} level {level}.", skill.name())
		}
		Msg::VeinsRevealed => "New veins shimmer into view.".into(),

		Msg::Connected => "Connected to server.".into(),
		Msg::UnrankedServer => {
			"This server is not on the hiscores: XP earned here stays here.".into()
		}
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
		Msg::CommandHint { usage, desc } => command_hint(usage, desc).text,
		Msg::Admin(reply) => admin(reply),
	}
}

/// What to call a spell inside a refusal — a noun phrase the sentences
/// above drop in, so each of them reads as one line rather than as a
/// template with a name stapled on.
fn spell(s: Spell) -> &'static str {
	match s {
		Spell::Waystone => "that waystone",
		Spell::Wisp => "the wisp",
		Spell::Kindle => "conjured light",
		Spell::Sphere => "the sphere",
		Spell::Causeway => "the causeway",
	}
}

/// The usage line for one command. The `/give` line names the essences
/// out of `Essence::ALL` at the moment it is drawn, which is why the
/// advertisement and `parse_essence` cannot come to disagree — they read
/// one vocabulary from opposite ends of the wire.
fn usage(u: AdminUsage) -> String {
	match u {
		AdminUsage::Tp => "usage: /tp <x> <z> [n|s|e|w] (~ or ~<d> = relative)".into(),
		AdminUsage::SetSpawn => "usage: /set-spawn (aim at the block to stand on)".into(),
		AdminUsage::SetSpawnDir => {
			"usage: /set-spawn-dir [n|s|e|w] (no argument = your facing)".into()
		}
		AdminUsage::LockSpawn => "usage: /lock-spawn [on|off]".into(),
		AdminUsage::Goto => "usage: /goto <name>".into(),
		AdminUsage::Bring => "usage: /bring <name>".into(),
		AdminUsage::Give => {
			let names = Essence::ALL.map(|e| e.ident()).join("|");
			format!("usage: /give <name> <{names}> [n]")
		}
		AdminUsage::Time => {
			"usage: /time <hh:mm> (24h; 6:00 sunrise, 12:00 noon, 18:00 sunset)".into()
		}
		AdminUsage::Reset => "usage: /reset <name>".into(),
		AdminUsage::Ban => "usage: /ban <name>".into(),
		AdminUsage::BanName => "usage: /ban <name> (a player display name)".into(),
		AdminUsage::Unban => "usage: /unban <name-or-ip>".into(),
	}
}

/// An essence's label by wire index. An index the catalog does not know
/// is a server ahead of this client; naming it plainly beats dropping
/// the line.
fn essence(index: u16) -> String {
	Essence::try_from_index(index).map_or_else(|| format!("essence {index}"), |e| e.name().into())
}

fn cell(c: &[i32; 3]) -> String {
	format!("{} {} {}", c[0], c[1], c[2])
}

/// Admin replies. Lower case and terse like the refusals: this is the
/// game answering a command, not telling you something happened.
fn admin(r: &AdminReply) -> String {
	match r {
		AdminReply::Usage(u) => usage(*u),
		AdminReply::CommandHint { usage, desc } => command_hint(usage, desc).text,
		AdminReply::UnknownCommand { typed } => format!("unknown command {typed} - try /help"),
		AdminReply::CompassHint => "compass: e = sunrise (-x), n = -z".into(),
		AdminReply::NoSuchPlayer { name, why } => match why {
			NoPlayer::NotAName => format!("{name} is not a player display name"),
			NoPlayer::StillJoining => format!("{name} is still joining"),
			NoPlayer::NotOnline => format!("{name} is not online"),
			NoPlayer::Ambiguous { count } => format!("{count} players named {name}"),
		},
		AdminReply::ThatIsYou => "that's you".into(),
		AdminReply::Teleported { x, z } => format!("teleported to {x} {z}"),
		AdminReply::TeleportedTo { name } => format!("teleported to {name}"),
		AdminReply::Brought { name } => format!("brought {name}"),
		AdminReply::TeleportedYouBy { admin } => format!("{admin} teleported you"),
		AdminReply::NotLookingAtBlock => "not looking at a block".into(),
		AdminReply::NotLookingAtTopFace => {
			"not looking at the top of a block - aim at the surface to stand on".into()
		}
		AdminReply::SpawnNotClear { clear, capsule } => format!(
			"not a valid spawn - needs solid ground {clear} blocks out, \
			 with {capsule} clear and dry above it"
		),
		AdminReply::SpawnPointSet { cell: c } => format!("spawn point set to {}", cell(c)),
		AdminReply::NotFacingAnywhere => {
			"not facing anywhere - look along the ground, or name a direction".into()
		}
		AdminReply::SpawnFacingSet { compass } => format!("players will spawn facing {compass}"),
		AdminReply::SpawnLocked {
			cell: c,
			region,
			already,
		} => {
			let [x0, x1, y0, y1, z0, z1] = region;
			let head = if *already { "already " } else { "" };
			format!(
				"{head}spawn locked at {} (protecting x {x0}..{x1}, y {y0}..{y1}, z {z0}..{z1})",
				cell(c)
			)
		}
		AdminReply::SpawnSetButUnlocked { cell: c } => format!(
			"spawn is set at {} but unlocked (its cells can be dug)",
			cell(c)
		),
		AdminReply::NoClearSpawnToLock => "no clear spawn to lock — nowhere within range is \
			 intact; fill in the excavation and try again"
			.into(),
		AdminReply::SpawnAlreadyUnlocked => {
			"spawn is unlocked (it moves aside if players dig it out)".into()
		}
		AdminReply::SpawnUnlocked => "spawn unlocked - its cells are editable again".into(),
		AdminReply::NotAnEssence { typed } => {
			format!("{typed} is not an essence - {}", usage(AdminUsage::Give))
		}
		AdminReply::Gave {
			name,
			essence: e,
			gained,
			asked,
			held,
		} => {
			let what = essence(*e);
			// The cap is only worth mentioning when it bit.
			let short = if gained < asked {
				format!(" ({name} is at the {what} cap; {asked} asked)")
			} else {
				String::new()
			};
			format!("gave {name} {gained} {what} - now {held}{short}")
		}
		AdminReply::WereGiven { essence: e, gained } => {
			format!("you were given {gained} {}", essence(*e))
		}
		AdminReply::TimeIs { hour, minute } => format!("time is {hour:02}:{minute:02}"),
		AdminReply::TimeSet { hour, minute } => format!("time set to {hour:02}:{minute:02}"),
		AdminReply::ResetPlayer { name } => {
			format!("reset {name} - skills zeroed, inventory wiped")
		}
		AdminReply::YouWereReset => "your skills and inventory were reset".into(),
		AdminReply::NotBanningYourself => "not banning yourself".into(),
		AdminReply::SharesYourIp { name } => format!("{name} shares your IP - not banning it"),
		AdminReply::Banned { name, ip } => format!("banned {name} ({ip})"),
		AdminReply::NoBans => "no bans".into(),
		AdminReply::BanEntry { name, ip } => format!("{name} - {ip}"),
		AdminReply::Unbanned { arg, removed } => {
			format!("unbanned {arg} ({removed} entry(s))")
		}
		AdminReply::NoBanMatches { arg } => format!("no ban matches {arg}"),
		AdminReply::LegacyCharacterClaimed => {
			"your pre-account character on this server has been added to your account".into()
		}
		AdminReply::CommandsAreAdminOnly => "commands are admin-only".into(),
	}
}

pub(super) fn unknown_player(id: PlayerId) -> String {
	format!("player {id}")
}

pub(super) fn me() -> String {
	"me".into()
}
