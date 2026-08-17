# The social mirror — hiscores, status, claims & trade (design document)

Status: **DESIGN ONLY — nothing in this document is built.** It came out of the 2026-08-16 design review
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
`inventories.bin` and already speak HTTP to the master (`apps/server/src/master.rs`). The whole feature is:

- a batched XP-totals report riding the server's existing heartbeat (account id → per-skill totals; totals, not levels,
  so the master derives levels with the same pinned curve and a stale row is self-describing);
- one EF Core table beside the accounts (nothing above `Data/` may assume SQLite, as ever);
- `GET /api/hiscores` (overall and per-skill, paged), mirrored in `Contracts.cs` and the three client fillers like every
  other master endpoint;
- and a web page beside the game. Web is the priority platform, so the hiscores page is not an afterthought — it is half
  of RuneScape's social fabric reconstituted for the price of an endpoint.

**Trust, honestly:** any registered server could report inflated totals — token verification proves a server is real,
not that its sim is honest. V1 scopes the *global* board to first-party servers, with per-server boards for everyone
else; the boundary is presentation, not enforcement, and the doc says so rather than pretending otherwise. A signed
attestation scheme is not worth its complexity at this player count.

In-game, the character screen grows one line per skill — your rank, when known — because the number on the board is the
number that makes the C screen's totals mean something.

## 2. Status display

**The Master's Sigil is already the pattern.** A held item that does nothing, costs everything, and shows in other
players' hands through the real geometry pipeline — that is a skillcape, shipped. Generalize it:

- **A mastery item per skill at its content cap**, in the sigil's mold: `is_tool`, capped at one, priced entirely in the
  skill's own essences, cycling on tap-9, drawn in remotes' hands via `tool_icon`'s path. Zero new machinery; the cost
  is art and two catalog rows. (The sigil keeps its place at the level cap above them — the one trophy in the tail, per
  SKILLS-DESIGN §13.3.)
- **A capped-skill glyph on the nameplate**: one flags byte appended to `RemoteState` (append-only wire evolution, as
  ever), rendered beside the name tag at the same surface-size rules name tags already obey. The cheapest possible
  "everyone can see it" channel, and horizontal-progression literature's whole lesson: cosmetic status extends the
  ladder without inflating power.

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
