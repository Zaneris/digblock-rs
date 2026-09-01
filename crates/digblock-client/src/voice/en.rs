//! English. One arm per [`Msg`], matched exhaustively so a new message
//! cannot ship without words, and every arm free to order its own
//! arguments — that freedom is the whole point of the enum above it.
//!
//! One voice: sentence case throughout. Refusals stay terse and
//! unpunctuated (the game answering your hand); announcements keep
//! their full stops (the game telling you something happened).

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

pub(super) fn line(msg: &Msg, binds: &crate::bindings::Bindings) -> String {
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
		Msg::DigProtected => "This block is holding firm".into(),
		Msg::PlaceProtected => "Nothing will stay put here".into(),
		Msg::NeedsSiphon => "This vein needs an essence siphon".into(),
		Msg::DigRefusedByServer => "The server refused that dig".into(),
		Msg::CastInterrupted => "The channel breaks".into(),
		Msg::SpellSlotEmpty => "That spell slot is empty. Drag a spell from the spellbook.".into(),
		Msg::TeleportWait(secs) => format!("Return in {secs}s."),
		Msg::PickOrientation(o) => {
			use digblock_core::pick::DigOrientation as O;
			match o {
				O::Horizontal => "Pick swing: sideways".into(),
				O::Vertical => "Pick swing: up and down".into(),
				O::Plane => "Pick swing: full plane".into(),
			}
		}
		Msg::SpellLocked(s) => format!("{} is beyond your arcana", spell_cap(*s)),
		// The waystone's fare is a PASSAGE — you are buying the journey,
		// not the spell — and the conjurings are not, so the sentence
		// splits on the argument rather than papering over it.
		Msg::SpellFareShort(Spell::Waystone) => "You cannot pay the passage".into(),
		Msg::SpellFareShort(s) => format!("You cannot pay for {}", spell(*s)),
		// Spell-blind on purpose: the wire says only that the server
		// refused. Naming a spell here would name the wrong one.
		Msg::WaystoneLocked => "That casting is beyond your arcana".into(),
		Msg::CastCantPay => "You cannot pay that fare".into(),
		Msg::CastBadTarget => "Nothing can be conjured here".into(),
		Msg::SphereNoBlock => "Hold a block to raise the sphere from".into(),
		Msg::SphereBadBlock(Some(b)) => format!("{} cannot be raised into a sphere", b.name()),
		Msg::SphereBadBlock(None) => "That cannot be raised into a sphere".into(),
		Msg::SphereBlocksShort(Some((b, need, have))) => {
			format!("The sphere needs {need} {} and you hold {have}", b.name())
		}
		Msg::SphereBlocksShort(None) => "You do not hold enough blocks for the sphere".into(),

		Msg::LevelUp { skill, level } => format!("{} is now level {level}.", skill.name()),
		Msg::RemoteLevelUp { who, skill, level } => {
			format!("{who} reached {} level {level}.", skill.name())
		}
		Msg::VeinsRevealed => "New veins shimmer into view.".into(),
		Msg::SourcesRevealed => "The siphon hums: essence sources fade into the world.".into(),
		// Exhaustive over the family, so a future rung cannot ship mute;
		// the two entry families never level INTO reach, but they still
		// answer with honest words.
		Msg::NowDiggable(f) => {
			use digblock_core::block::DigFamily as F;
			match f {
				F::Fixed | F::Soft => "You can now dig the soft ground.".into(),
				F::Wood => "You can now dig wood.".into(),
				F::Rock => "You can now dig rock.".into(),
			}
		}

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
		// The tutorial names UI elements by their on-screen labels, so
		// Transmute and Tools carry their button casing. Key names come
		// from the bindings table, so a rebinding moves the words with it.
		Msg::TutorialStep {
			step, touch: false, ..
		} => {
			use crate::bindings::Action as A;
			use crate::tutorial::StepId as S;
			use digblock_platform::Key;
			match step {
				S::Move => format!(
					"Walk around with {} {} {} {}",
					binds.label(A::MoveForward),
					binds.label(A::MoveLeft),
					binds.label(A::MoveBack),
					binds.label(A::MoveRight)
				),
				S::Look => "Look around with the mouse".into(),
				S::Jump => format!("Jump with {}", binds.label(A::Jump)),
				S::Dig => "Hold the left mouse button on a block to dig it".into(),
				S::SwitchSlot => format!(
					"Press {} or scroll to try another hotbar slot",
					binds.label(A::HotbarSlot2)
				),
				S::SelectSlot => format!(
					"Press {} to come back to your first slot",
					binds.label(A::HotbarSlot1)
				),
				S::Place => "Right-click a surface to place what you dug".into(),
				S::OpenMenu => format!("Press {} to open the menu", Key::Escape.label()),
				S::OpenInventory => format!(
					"Press {} to open your inventory",
					binds.label(A::OpenInventory)
				),
				S::CloseInventory => {
					format!("Press {} to close your inventory", Key::Escape.label())
				}
				S::OpenBench => format!(
					"You can afford an essence siphon: press {} to open the bench",
					binds.label(A::OpenAlchemy)
				),
				S::PickTools => "Select the Tools family".into(),
				S::PickSiphon => "Select the Essence Siphon".into(),
				S::CraftSiphon => "Press Transmute to craft the siphon".into(),
				S::SelectSiphon => format!(
					"Press {} until the siphon is in hand",
					binds.label(A::HotbarSlot9)
				),
			}
		}
		Msg::TutorialStep {
			step,
			touch: true,
			jump_button,
		} => {
			use crate::tutorial::StepId as S;
			match step {
				S::Move => "Drag the left side of the screen to walk".into(),
				S::Look => "Drag the right side of the screen to look".into(),
				S::Jump if *jump_button => "Tap the jump button".into(),
				S::Jump => "Double-tap the right side to jump".into(),
				S::Dig => "Hold a finger on a block to dig it".into(),
				S::SwitchSlot => "Tap another hotbar slot".into(),
				S::SelectSlot => "Tap your first hotbar slot".into(),
				S::Place => "Aim at a surface and tap the place button".into(),
				S::OpenMenu => "Tap the corner button to open the menu".into(),
				S::OpenInventory => "In the menu, tap Inventory".into(),
				S::CloseInventory => "Tap the X in the corner to close your inventory".into(),
				S::OpenBench => {
					"You can afford an essence siphon: open the bench from the menu".into()
				}
				S::PickTools => "Tap the Tools family".into(),
				S::PickSiphon => "Tap the Essence Siphon".into(),
				S::CraftSiphon => "Tap Transmute to craft the siphon".into(),
				S::SelectSiphon => "Tap the last slot until the siphon is in hand".into(),
			}
		}
		Msg::TutorialDone => "You know the essentials: this world is yours".into(),
		Msg::TutorialSiphonDone => "That is everything: go siphon some essence".into(),
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

/// The same noun phrase opening a sentence, so its article carries the
/// capital instead of the template bolting one on.
fn spell_cap(s: Spell) -> &'static str {
	match s {
		Spell::Waystone => "That waystone",
		Spell::Wisp => "The wisp",
		Spell::Kindle => "Conjured light",
		Spell::Sphere => "The sphere",
		Spell::Causeway => "The causeway",
	}
}

/// The usage line for one command. The `/give` line names the essences
/// out of `Essence::ALL` at the moment it is drawn, which is why the
/// advertisement and `parse_essence` cannot come to disagree — they read
/// one vocabulary from opposite ends of the wire.
fn usage(u: AdminUsage) -> String {
	match u {
		AdminUsage::Tp => "Usage: /tp <x> <z> [n|s|e|w] (~ or ~<d> = relative)".into(),
		AdminUsage::SetSpawn => "Usage: /set-spawn (aim at the block to stand on)".into(),
		AdminUsage::SetSpawnDir => {
			"Usage: /set-spawn-dir [n|s|e|w] (no argument = your facing)".into()
		}
		AdminUsage::LockSpawn => "Usage: /lock-spawn [on|off]".into(),
		AdminUsage::Goto => "Usage: /goto <name>".into(),
		AdminUsage::Bring => "Usage: /bring <name>".into(),
		AdminUsage::Give => {
			let names = Essence::ALL.map(|e| e.ident()).join("|");
			format!("Usage: /give <name> <{names}> [n]")
		}
		AdminUsage::Time => {
			"Usage: /time <hh:mm> (24h; 6:00 sunrise, 12:00 noon, 18:00 sunset)".into()
		}
		AdminUsage::Reset => "Usage: /reset <name>".into(),
		AdminUsage::Ban => "Usage: /ban <name>".into(),
		AdminUsage::BanName => "Usage: /ban <name> (a player display name)".into(),
		AdminUsage::Unban => "Usage: /unban <name-or-ip>".into(),
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

/// Admin replies. Terse and unpunctuated like the refusals: this is
/// the game answering a command, not telling you something happened.
fn admin(r: &AdminReply) -> String {
	match r {
		AdminReply::Usage(u) => usage(*u),
		AdminReply::CommandHint { usage, desc } => command_hint(usage, desc).text,
		AdminReply::UnknownCommand { typed } => format!("Unknown command {typed} - try /help"),
		AdminReply::CompassHint => "Compass: e = sunrise (-x), n = +z".into(),
		AdminReply::NoSuchPlayer { name, why } => match why {
			NoPlayer::NotAName => format!("{name} is not a player display name"),
			NoPlayer::StillJoining => format!("{name} is still joining"),
			NoPlayer::NotOnline => format!("{name} is not online"),
			NoPlayer::Ambiguous { count } => format!("{count} players named {name}"),
		},
		AdminReply::ThatIsYou => "That's you".into(),
		AdminReply::Teleported { x, z } => format!("Teleported to {x} {z}"),
		AdminReply::TeleportedTo { name } => format!("Teleported to {name}"),
		AdminReply::Brought { name } => format!("Brought {name}"),
		AdminReply::TeleportedYouBy { admin } => format!("{admin} teleported you"),
		AdminReply::NotLookingAtBlock => "Not looking at a block".into(),
		AdminReply::NotLookingAtTopFace => {
			"Not looking at the top of a block - aim at the surface to stand on".into()
		}
		AdminReply::SpawnNotClear { clear, capsule } => format!(
			"Not a valid spawn - needs solid ground {clear} blocks out, \
			 with {capsule} clear and dry above it"
		),
		AdminReply::SpawnPointSet { cell: c } => format!("Spawn point set to {}", cell(c)),
		AdminReply::NotFacingAnywhere => {
			"Not facing anywhere - look along the ground, or name a direction".into()
		}
		AdminReply::SpawnFacingSet { compass } => format!("Players will spawn facing {compass}"),
		AdminReply::SpawnLocked {
			cell: c,
			region,
			already,
		} => {
			let [x0, x1, y0, y1, z0, z1] = region;
			let head = if *already {
				"Spawn already locked"
			} else {
				"Spawn locked"
			};
			format!(
				"{head} at {} (protecting x {x0}..{x1}, y {y0}..{y1}, z {z0}..{z1})",
				cell(c)
			)
		}
		AdminReply::SpawnSetButUnlocked { cell: c } => format!(
			"Spawn is set at {} but unlocked (its cells can be dug)",
			cell(c)
		),
		AdminReply::NoClearSpawnToLock => "No clear spawn to lock: nowhere within range is \
			 intact; fill in the excavation and try again"
			.into(),
		AdminReply::SpawnAlreadyUnlocked => {
			"Spawn is unlocked (it moves aside if players dig it out)".into()
		}
		AdminReply::SpawnUnlocked => "Spawn unlocked - its cells are editable again".into(),
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
			format!("Gave {name} {gained} {what} - now {held}{short}")
		}
		AdminReply::WereGiven { essence: e, gained } => {
			format!("You were given {gained} {}", essence(*e))
		}
		AdminReply::TimeIs { hour, minute } => format!("Time is {hour:02}:{minute:02}"),
		AdminReply::TimeSet { hour, minute } => format!("Time set to {hour:02}:{minute:02}"),
		AdminReply::ResetPlayer { name } => {
			format!("Reset {name} - skills zeroed, inventory wiped")
		}
		AdminReply::YouWereReset => "Your skills and inventory were reset".into(),
		AdminReply::NotBanningYourself => "Not banning yourself".into(),
		AdminReply::SharesYourIp { name } => format!("{name} shares your IP - not banning it"),
		AdminReply::Banned { name, ip } => format!("Banned {name} ({ip})"),
		AdminReply::NoBans => "No bans".into(),
		AdminReply::BanEntry { name, ip } => format!("{name} - {ip}"),
		AdminReply::Unbanned { arg, removed } => {
			format!("Unbanned {arg} ({removed} entry(s))")
		}
		AdminReply::NoBanMatches { arg } => format!("No ban matches {arg}"),
		AdminReply::LegacyCharacterClaimed => {
			"Your pre-account character on this server has been added to your account".into()
		}
		AdminReply::CommandsAreAdminOnly => "Commands are admin-only".into(),
		AdminReply::WithheldOnRankedServer { command } => {
			format!("{command} is off here: this server's levels reach the hiscores")
		}
	}
}

pub(super) fn unknown_player(id: PlayerId) -> String {
	format!("Player {id}")
}

pub(super) fn me() -> String {
	"me".into()
}
