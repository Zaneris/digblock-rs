# The social mirror — hiscores, status, claims & trade (design document)

Status: **§1 and §2 are BUILT (2026-08-25); §3 and §4 are design.** It came out of the 2026-08-16 design review
(SKILLS-DESIGN.md §13) as the answer to its sharpest finding: DigBlock's multiplayer is mechanically real and socially
invisible. The progression mechanics are already RuneScape-shaped — the curve, threshold unlocks as events, XP from
doing the thing, cross-skill handshakes — but RuneScape was never great combat math; it was a persistent world where
your numbers were *known*. Hiscores, capes, crowded banks, "gratz on 99". Here, chat aside, another player is scenery.
Self-determination theory names the gap precisely: competence and autonomy are served, relatedness is not — and
relatedness is the cheapest of the three to serve, which is why this document's first two items ship before any big
content system. Every content hour is worth more once someone can see it.

Ordering within this document is by value-per-engineering-cost, and it is deliberate.

## 1. Hiscores

The master already owns accounts and display names; servers already persist per-account skill totals in
`inventories.bin` and already speak HTTP to the master (`apps/server/src/master.rs`). What shipped:

- **A batched XP-totals report on the heartbeat's cadence** (`POST /api/servers/{id}/skills`, one batch behind each
  heartbeat that landed): account id → per-skill totals — totals, not levels, so the master derives levels with the same
  pinned curve (`apps/master/Hiscores/Skills.cs`, held to `skills.rs` by `the_masters_curve_mirrors_ours`, which reads
  the C# source) and a stale row is self-describing. Its own endpoint rather than a heartbeat field, so a refusal has a
  status code the server can act on. The server side is `hiscores::SkillOutbox`, filled under `PlayerStore::put` — the
  one write every change of skills already goes through — and seeded with every stored record at boot, so a board that
  has never heard of a server catches up without anyone playing first. **Monotone**: a report can only raise a total,
  the game's own `set_total` rule, so a server holding an older copy of a character cannot rewind the one that moved on.
- **One EF Core table** (`SkillTotal`, keyed on account and skill) beside the accounts.
- **`GET /api/hiscores`** (overall and per-skill, paged) and **`GET /api/hiscores/me`** (the session's own ranks),
  mirrored in `Contracts.cs` and the two client fillers. The overall board sums levels, a skill never reported counting
  at the first level — exactly how the game reads an empty total.
- **A server-rendered page at `/hiscores`**, script-free in the sign-in pages' shell. Master-served rather than bundled
  beside the wasm: the master owns the data, the CORS policy and the rate limits, and the web bundle's cache model is
  built for immutable-by-name files, which a live board is not.

**Trust, by list.** Token verification proves a server is REAL, not that its sim is honest, and any registered server
could report inflated totals — so the board takes reports from `Master:Hiscores:TrustedServers`, a hand-kept list of
`host` or `host:port` entries matched against the address the server is LISTED at (which the probe verified), and from
nobody else. Registration tells a server whether it is on the list (`RegisterOk.reportsSkills`), so an unlisted server
never sends a report; one that did is refused with 403, and a refusal is final for that registration. The listing
publishes the same answer (`ServerSummary.reportsSkills`) and the browser row wears it as the sigil's icon — a picture
where the screens use pictures, never a word — so a player reads whether play here counts before spending a join.
Reading is public. A signed attestation scheme was considered and declined: not worth its complexity at this player
count, and the list is the honest statement of who is trusted.

**And the pull.** The board is not only a mirror: `/api/tokens/verify` answers with the account's totals as the board
holds them, and every server — trusted or not, since only the WRITE is gated — folds them into its own record at join
with `SkillSet::merge_from`, max and never rewinding. So a character is one character on every server, and a server that
could not write the board cannot feed an inflated ladder to the ones that can. `--local-skills`
(`DIGBLOCK_LOCAL_SKILLS`, `LOCAL_SKILLS` in the compose file) opts a server out, for a self-hosted world that wants a
ladder of its own.

In-game, the character screen grows one line per skill — your rank, when known — because the number on the board is the
number that makes the C screen's totals mean something. Opening the screen asks the master (`MenuCommand::FetchRanks`);
the answer is the engine's, not a menu page's, and a board the account is not on says nothing rather than "unranked".

## 2. Status display

**The Master's Sigil is already the pattern.** A held item that does nothing, costs everything, and shows in other
players' hands through the real geometry pipeline — that is a skillcape, shipped. Generalized:

- **The three crests** (`MinersCrest`, `AlchemistsCrest`, `ArcanistsCrest`), one per skill at `unlock::MASTERY_AT`, in
  the sigil's mold: `is_tool`, capped at one, cycling on tap-9, drawn in remotes' hands via `tool_icon`'s path, geometry
  the sigil's with the skill's emblem essence (`Skill::emblem`) set where the crystal sits. **Gated on the skill they
  honour, not the bench**: `unlock::crest` names the skill and `unlock::can_craft` takes the whole `SkillSet`, so a
  mastered miner with no Alchemy makes theirs and a level-99 alchemist cannot buy one. Priced entirely in the skill's
  own essences, led by the emblem. The Miner's and Arcanist's sit on the catalog page from the first level, locked
  behind their skill's badge — a teaser you can read the whole ladder — and the Alchemist's is the one new Alchemy
  unlock, at the content top. (The sigil keeps its place at the level cap above them — the one trophy in the tail, per
  SKILLS-DESIGN §13.3.)
- **The mastery glyphs on the nameplate**: one byte appended to `RemoteState` (`Skill::mastery_bits`, protocol v38, the
  `fx` precedent — a struct field is the one breaking append, so `MIN_PROTOCOL` rose with it), one square per mastered
  skill after the name in the skill's emblem colour, at the surface-size rules name tags already obey. The cheapest
  possible "everyone can see it" channel, and horizontal-progression literature's whole lesson: cosmetic status extends
  the ladder without inflating power.

## 3. Claims — shared building as the endgame

On a no-combat persistent server, building is the endgame, and the missing trust layer is protection between strangers.
The machinery exists: `protect.rs` has the one-predicate rule and `RejectReason::Protected` wording that prediction and
server already share; spawn-lock proves the region shape.

- A claim is **placed**, via a hand-priced functional item (§7.3 pricing — a real essence sink, and the economy wants
  more of those), anchoring a bounded region to the placing account.
- Enforcement reuses the standing predicate — every writer already asks `immutable()` first, so claimed cells refuse
  strangers' digs, places, and fluid writes with no new code paths.
- Co-builders are an account-id allow-list. Accounts, never names — the `--admin` lesson, and a rename must not drop a
  friend's access.
- Claims are refused inside dungeon regions (DELVE-DESIGN.md §7.3) and cannot overlap spawn's lock; one predicate check
  at claim time covers both.
- New state: claim records beside `player_store`, a management surface (probably the character screen's pause-menu
  sibling), and abandonment rules (a claim whose account has not connected in a long window lapses — the exact window is
  a tuning decision for the doc's next revision, not this one).

Moderate cost, and the payoff arrives the first time two strangers build near each other and neither has to trust the
other.

## 4. Trade — later, and structured

**Sequencing first:** trade is pointless while every player runs the same two ladders — with identical production
functions there is no comparative advantage, so a trade window would be a gifting UI. EVE's economy carries the lesson:
meaningful exchange emerges from *specialization*, not from a trade window. Trade ships in the era when the skill roster
widens (Delving, and whatever follows it) and a miner, a delver and a mason genuinely have different things.

When it ships, the shape is decided now:

- **Direct, server-atomic, two-party.** Everything tradeable is already a count (essences; block items), so a trade is
  one atomic swap answered by two `InventoryFull` rebases — the standing idempotent machinery, no new state. RuneScape's
  hard-won UI rule comes with it: a two-screen confirm against last-second swaps, because scam surface is a design
  input, not a moderation problem.
- **Essence-only is an acceptable first slice.** It is the smaller wire surface and most of the value.
- **Chests never carry trade.** Containers are out of v1 for good reasons, and trading through world blocks inherits
  theft, container state, and scam surface for nothing direct trade doesn't do better. If containers ever return it is
  as an account-bound vault (no world cell), and only when inventory pressure is demonstrated — the grid is deliberately
  generous.

## 5. What this document deliberately excludes

No global chat channels beyond the server's own, no guilds/clans machinery, no player shops, no auction house, no PvP
flagging — each is a later argument that should be made against a game that already has the mirror, the claims and a
wider skill roster. Bartle's frame, for the record: the game serves Achievers and Explorers, this document serves
Socializers, and competitive players get boards and contested dungeon nodes rather than ever getting PvP.
