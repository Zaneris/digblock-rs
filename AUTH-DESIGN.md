# Accounts, sign-in, and the master's trust model

Date: 2026-08-14. Scope: `apps/master` in full, the identity-facing parts of `apps/server`, `crates/digblock-net`'s
account type, and the sign-in flows in both launchers.

This implements the findings of a security audit of the master's trust model: seven findings that were all one bug
wearing seven hats — *the master mints an identity for anybody who asks for one*. They were closed together, because
fixing any one of them alone just moved the hole. §5 is the standing record of what each became.

**Read this before changing anything under `apps/master`, `Hello`, or the `--admin` path.**

---

## 1. The model

| Thing               | Before                                              | Now                                                        |
| ------------------- | --------------------------------------------------- | ---------------------------------------------------------- |
| Who you are         | `SHA256(device_secret)[..16]`, chosen by the client | `AccountId`, a UUID the master owns                        |
| Display name        | free-form, per join request                         | account-owned, globally unique, rename on a cooldown       |
| Client credential   | a 16-byte bearer secret sent to every server        | an opaque session token sent only to the master            |
| The server learns   | a display name                                      | display name + `AccountId`, both from `/api/tokens/verify` |
| Character storage   | `inventories.bin` keyed by the secret's hash        | same file, keyed by `AccountId`                            |
| Admin               | `--admin-name`, a string compare                    | `--admin <id>`, an account compare                         |
| Bans                | IP only                                             | account primary, IP secondary                              |
| Server registration | anonymous, evicted the incumbent                    | anonymous + sticky key, source-probed first                |

Two sentences carry most of it:

> **The client never asserts who it is.** It presents 16 opaque bytes; the game server asks the master what they mean.

> **A player's character follows their account, not their machine.** That is the feature. Everything else here is what
> it takes to make the account worth trusting.

### 1.1 What a game server can and cannot do

A server operator sees a joining player's **display name** and **account id**. Both are public — an id names you, it
does not authenticate you — so a hostile operator learns nothing they could present anywhere else. Cert pinning still
prevents MITM, and the join token is one-time and scoped to one server.

What remains, and is accepted: a rogue operator can grief their own world, and operators who collude can correlate a
player across their servers by account id. **Per-server pseudonymous ids** (`HMAC(master_secret, account ||
server)`)
would close that, at the cost of the copy-your-id-to-an-operator flow that makes admin setup a paste rather than a
support ticket. The id is global on purpose; revisit only with a plan for how an operator names a player they cannot
see.

---

## 2. Sign-in

The master is the authorization server as far as the game is concerned. The client never touches a provider token, and
adding a provider — Apple, Discord — is a change in `apps/master/Auth/` and nowhere else: no client release, no new
redirect URI to ship, no protocol bump. Microsoft was the first one added after the flow was built, and it cost exactly
that: a package, an options block, a row in two tables and a button. Nothing in either launcher moved.

```
client                    browser                 master            provider
  |  open ------------------>  |                     |                  |
  |                            |-- /auth/start ------>|                  |
  |                            |<- provider chooser --|                  |
  |                            |-- /auth/go/google -->|-- challenge ---->|
  |                            |<-------------------- callback ---------|
  |                            |<- 302 redirect_uri?code=&state= --------|
  |<- code (loopback or page) -|                     |                  |
  |-- POST /api/session/exchange {code, verifier} --->|                  |
  |<------------------------- session token ---------|                  |
```

**Desktop** binds an ephemeral loopback port and opens the system browser (RFC 8252). **Web** navigates the page itself
and comes back with `?code=&state=`, which `lib.rs` redeems before the engine is built and then scrubs from the URL. A
popup was declined: opened from the engine's command drain it is one task removed from the click, which is exactly what
a popup blocker exists to stop, and a sign-in that silently opens nothing reads as a hang.

### 2.0 Configuring a provider

Each button on the chooser is that provider's own trade dress, not ours to style: an approved logomark, an approved
theme, and — this is the part that surprises people — an approved string. Microsoft's kit sanctions "Sign in with
Microsoft" and nothing else, so the chooser's rows deliberately do not read alike, and the three buttons do not agree on
height or corner radius either. Following each kit is the requirement; a house style that overrode one would be exactly
what the kit forbids, and for Google it is a stated condition of OAuth app verification.

A provider with no `ClientId`/`ClientSecret` is **off**: its button never renders, its callback 404s, and with none
configured at all `/auth/start` answers 503. Since joining requires an account, that also means multiplayer is closed —
which is the correct default for a master nobody has set up, and worth knowing before wondering why a fresh deployment
admits no one.

Per provider, two Container App secrets (never `appsettings.json`):

```
Master__Auth__Google__ClientId      Master__Auth__Facebook__ClientId      Master__Auth__Microsoft__ClientId
Master__Auth__Google__ClientSecret  Master__Auth__Facebook__ClientSecret  Master__Auth__Microsoft__ClientSecret
```

Those pairs are the ONLY settings a deployment needs for sign-in.

The redirect URI to register with the provider is ASP.NET's default callback path for that handler, which we do not
override:

```
https://master.digblock.net/signin-google
https://master.digblock.net/signin-facebook
https://master.digblock.net/signin-microsoft
```

Worth writing down because it is the one string that fails obscurely: a mismatch surfaces as the provider's own
`redirect_uri_mismatch`, with nothing in our logs at all. Note it is NOT the `redirect_uri` the game client sends — that
one points back at the client (a loopback port, or the web origin) and never leaves our own allowlist.

Nothing else is needed for the shipped clients. Desktop redirects are loopback and matched by shape (RFC 8252 §7.3 — the
port cannot be known in advance), and the web client's own origins are BUILT IN (`RedirectUris.Owned`), because config
is the wrong tool for a constant: an environment variable can be forgotten on a deploy, and when it is, sign-in breaks
on the web only, with a message about an unrecognised address and nothing to connect it to a missing setting.
`Master:Auth:WebRedirectUris` still adds to the built-ins, for anyone running their own master against their own build.

Google is asked for `openid` alone — the non-sensitive scope category, which is the one that avoids a verification
review. Confirm current requirements in the console when publishing; a project left in Testing status serves only a
small number of named test users.

**Microsoft is the one provider that cannot be trimmed that far**, and the reason is worth knowing before someone tries:
its handler does not read the subject out of the id token, it calls Graph's `/me` with the access token, and Graph
refuses a token that does not carry `User.Read`. Dropping the scope does not narrow the request, it breaks sign-in — the
handler throws on a non-success from the user-information endpoint, and the player gets the same dead-end page as a
cancelled consent. So the scope stays at the handler's default (that one permission, no more), and what is narrowed
instead is the **response**: `$select=id` on the endpoint is Graph's spelling of the `Fields` list already used for
Facebook, and it leaves the id as the only field that crosses the wire. The handler's stock name and email claim actions
then have nothing to map. Registering the app under `/common` — the handler's default authority — is what lets a
personal Microsoft account and a work or school account both sign in.

An OIDC-only variant was considered and declined: pointing the endpoint at `oidc/userinfo` and mapping `sub` would allow
`openid` alone, but Microsoft's `sub` is **pairwise** — it is derived from the app registration, so recreating that
registration would orphan every account filed under it, silently and irreversibly. Graph's `id` is stable against that.

**Two secrets guard the browser hop**, because a URL is the least private thing on a machine — history, logs, sometimes
a clipboard. `state` proves the callback belongs to the request we started; the PKCE verifier proves the redemption
does, so a code that merely leaked is useless. Both are pinned to RFC 7636's Appendix B vector in each launcher, because
they must agree with the master and with each other.

### 2.0 Leaving

One account holds one live connection, so hanging up matters more than it used to: the session a stale connection holds
belongs to the very player trying to get back in. `NetClient::disconnect` is the one way out — `ClientMsg::Bye`, then a
bounded flush — and every deliberate exit calls it (Leave Server, Leave World, replacing a connection, closing the
window, unloading the tab).

**The pairing is the point.** `Bye` alone is a message queued into a QUIC stream buffer; dropping the transport a line
later closes the connection and takes it with it. So the native transport finishes its send stream before the connection
goes, and `close()` blocks — up to 250 ms, the only blocking call in the transport seam — because a detached network
thread loses its race with `main` returning.

The web can only be weaker, and is: a page cannot wait, so it asks the browser to flush and moves on.

**Which winit callback fires there is not the obvious one**, and getting it wrong is silent. winit listens on `pagehide`
and splits it by `event.persisted`: the back/forward-cache case becomes `Suspended`, and an actual close or navigation
becomes `LoopExiting`. Wiring the goodbye to suspend alone therefore covers the case where the page might come back and
MISSES the case where it never will — which is also the common one. Both are handled: `exiting()` for the real
departure, and the wasm arm of `enter_suspend` for bfcache, where the connection dies with the frozen page anyway.

Native keeps suspend and exit apart for the opposite reason: a backgrounded phone is suspended and may well come back,
and hanging up on it would end a session the player still wants. There, `on_exit` comes from the window closing.

None of it is a guarantee. A pulled cable, a killed process and a closed laptop all still fall back on the server's idle
timeout — this only removes the cases where the client knew it was leaving and said nothing.

### 2.1 The session, and "device secrets go away"

They mostly do, and the residue is a different thing. A session token is still stored on the device (`session.json` at
0600; `localStorage` on web) — without it you would sign in every launch. But it is a credential *for* an account rather
than *being* one: it expires, it rotates on refresh with reuse detection (a rotated-away token presented again kills the
chain), it can be revoked, and **it is never presented to a game server**. The thing that made the old `identity` file
dangerous was that it was the account, on every server, forever.

### 2.2 Names

Globally unique, case-insensitively, over the same 3–10 char alphabet `validate_name` has always defined. Chosen once at
first sign-in, changeable on a 30-day cooldown — with the **first change free**, so a typo at sign-up is not a
month-long punishment. A released name is quarantined for 30 days against everyone but its former holder, because
without that, renaming is an impersonation primitive: step off a name and have an accomplice take it the same second,
while everyone who knows you still reads that name as you.

`Settings.name` is gone from the client. There is nothing left to remember.

---

## 3. Server registration

Registration stays **anonymous** — a self-hoster still just runs the container, with no signup and no operator account.
What changed is that an address remembers the key that claimed it.

The algorithm, in `Program.cs`'s `/api/servers/register`, and the reason for each step:

1. **Temp-banned source → refused.** See below.
2. **The SOURCE address is probed first.** Passing proves the registrant really is a live QUIC endpoint at that address
   serving that certificate. It is the strongest evidence available and the only one an attacker elsewhere on the
   internet cannot manufacture.
3. **A differing advertised address is probed too, and both must pass.** An address clients will be aimed at is never
   published unprobed. This is §1.3: `probeHost` used to be a request field, so an anonymous caller chose the only
   address the master ever dialed while clients were handed another. It is `Master:Dev:ProbeHostOverride` now —
   master-side config.
4. **If the source probe fails**, the advertised address is accepted only when it probes clean *and nothing already
   claims it*. That is the NAT-hairpin / reverse-proxy escape; confining it to unclaimed addresses is what stops it
   being a redirect primitive.
5. **A claimed AND live address may only be re-registered by whoever presents its key, or by whoever passes the SOURCE
   probe.** Anyone else gets 409 and a temp-ban on their source (15 min, doubling). This is §1.2 — the takeover was:
   read a victim's `host`, `port` and `certHash` out of the public listing, POST them back, and pass the probe *because
   the probe dials the victim*. The source probe is what that attack cannot pass: it dials the **registrant's own**
   address for the certificate the request pins, which needs the incumbent's private key.
6. **A claimed address may be re-keyed** by a registrant that passes the probes. This is the recovery path for an
   operator who lost `./data`, and it is safe precisely because passing the source probe means being the machine at that
   address.

**Steps 5 and 6 are one decision, and the order matters.** The refusal is checked *after* the probes, not on `live`
alone, because `live` is the in-memory registry and it outlives the process it describes by up to
`Registry:IdleTimeoutSeconds`. Refusing early meant an operator who lost their key and restarted was 409'd **and
temp-banned by their own stale entry** — for 15 minutes, on the address their server registers from, taking every other
server behind that address with it. The recovery path in step 6 was unreachable in exactly the case it was written for.

The cost is that a takeover attempt now spends a probe before being refused, which is the same probe every unclaimed
registration already spends and is capped by the per-address register limit.

One exception, and it is the dev stack: under `Master:Dev:ProbeHostOverride` **both** probes dial the configured host,
so the source probe stops being evidence of origin and answers the same question as the advertised one. There the
refusal falls back to firing on `live` alone. The recovery path is production-only, which is the honest reading of a dev
escape that redirects the master's own probes.

The temp-ban exists because the interesting version of the attack is a loop that re-fires every time the victim
re-registers. Refusing once is the fix; banning makes retrying pointless.

### 3.0 Where the source address comes from

Everything above rests on the master knowing who actually called it. The source address is the first probe target, the
partition every rate limit is cut by, and the subject of a temp-ban — so `X-Forwarded-For` is now a security input, and
it is believed only from a peer inside `TrustedProxies`: the address ranges that cannot appear as a caller on the public
internet.

`ForwardLimit = 1` is **not** sufficient on its own and reads as though it were. It bounds how many hops are walked,
which stops a client prepending a fake origin behind a real ingress; it says nothing about whether there was an ingress.
With the known-proxy lists cleared, a direct caller to a master with nothing in front of it — a self-host,
`just master`, the docker rig — could set the header and be believed, and so serve someone else's ban, evade their own,
or take a fresh rate-limit bucket per request.

The built-in list covers loopback, the RFC 1918 ranges, link-local, and carrier NAT — that last one because Azure
Container Apps reaches the app from `100.64.0.0/10`, which is why the framework's loopback-only default will not do.
`Master:Net:TrustedProxyNetworks` adds to it for an ingress on a public address; a malformed entry is fatal at boot
rather than skipped, since a proxy network that silently failed to parse looks like a networking fault and not a typo.

Accepted, and the same boundary `Master:Probe:AllowPrivateTargets` draws: on a LAN or a Docker network, a peer that is
already inside can still spoof the header. An operator's own network is theirs.

### 3.1 SSRF

Registration is still anonymous, so `/api/servers/register` is still an unauthenticated way to make the master dial an
address and report what happened (§1.7). The four reason codes stay — a self-hoster genuinely needs `certMismatch` apart
from `timeout` to debug a port forward — and what goes instead is the set of interesting targets: `ProbeTargets` refuses
anything off the public internet, checked **after DNS resolution**, because a hostname that resolves to `10.0.0.5` is
the whole trick. The raw exception text no longer ships either; it described the master's own network to an anonymous
caller.

`Master:Probe:AllowPrivateTargets` relaxes it, and is a plain option rather than a Development-only escape: a master
running on the same LAN as its servers is a legitimate deployment, and there is no security gain in stopping an operator
probing their own network. The public master leaves it off.

### 3.2 De-listing

Two mechanisms, answering different questions. `Registry:IdleTimeoutSeconds` (120 s, four missed heartbeats) removes a
server that has gone quiet — where silence is already the answer, so no probe is spent. The 3-strike probe sweep stays
for servers that still talk to us but have stopped being reachable from the internet, where one failure could be a blip.

---

## 4. Storage

The master's first durable state. EF Core over SQLite, with the provider as the only SQLite-shaped thing in the codebase
— no raw SQL, no SQLite-only types, so `UseSqlite` is the single line that changes to move to Postgres.

Deliberately **not** persisted: join tokens, pending auth codes, registration temp-bans, and per-server liveness. All
short-lived, all high-churn, and losing them to a restart was already the accepted behaviour.

Three things that will bite:

- **EF Core turns WAL on by itself.** WAL needs a shared-memory file and is documented as unsupported over a network
  filesystem; the intended production filesystem is an Azure Files (SMB) mount. `JournalModeInterceptor` pins DELETE
  back on every connection, unconditionally — a mode that differs between a developer's machine and production is a mode
  nobody tests. This was found by reading the boot log, not by reasoning.
- **SQLite's locks do not survive the SMB mount's defaults.** Over cifs, byte-range locks are forwarded to the SMB
  server, and SQLite's lock-upgrade sequence fails there as "database is locked" — on the very first write, with no
  other process alive, for the full busy timeout. The volume must be mounted with `nobrl`, which keeps locks on the
  node; the single replica below is what makes that honest, since one client means node-local locking sees every writer
  there is. Found the way WAL was: the first accounts revision died at `Database.Migrate()` over an empty database file,
  while the rollout silently kept serving the pre-accounts image.
- **The Container App's filesystem is ephemeral.** Without a mounted volume at `/data`, every redeploy takes every
  account with it. Single replica, too: SQLite is single-writer.

---

## 5. What the seven findings became

| Finding                                               | Closed by                                                                                                           |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| 1.1 admin is a self-asserted display name             | `--admin <account-id>`; `is_admin` compares accounts. `--admin-name` exits with guidance rather than being remapped |
| 1.2 registration evicts the incumbent                 | sticky `ServerClaim` + key; 409 + temp-ban on a keyless claim of a live address                                     |
| 1.3 client-supplied `probeHost`                       | removed from the contract; source-first probing, both addresses verified                                            |
| 1.4 the identity secret is a global bearer credential | `Hello.identity` deleted (protocol v23); identity arrives from the master                                           |
| 1.5 records evictable by anyone                       | records keyed on accounts; an account costs a real provider login, and `/api/join` is rate-limited per session      |
| 1.6 ban ordering, and IP-only bans                    | bans record the account; `DEFAULT_MAX_CONNECTIONS_PER_IP` caps the amplification the ordering allowed               |
| 1.7 unauthenticated probe with an error oracle        | `ProbeTargets` public-address filter; opaque `Detail` outside Development; per-IP rate limit + temp-ban             |

Two leftovers from the same audit: `ProtocolVersion`/`SimVersion` now ride `ServerSummary`, and the display name is no
longer an authorization input anywhere, which was the reason its validation mattered.

---

## 6. Bringing an old character across

Everyone who played before accounts has a character on every server they played on, filed under a hash of the device
secret their client used to invent. The account work orphaned all of it: the key means nothing to the master, and no
server can say which account held which device.

**The migration is per server, one time, and driven by the client.** A client that still finds an old identity file
hashes it and sends `ClientMsg::ClaimLegacy` immediately after its `Hello`, on every join, to every server. A server
holding a character under that key folds it into the joining account and deletes its copy; a server holding nothing says
nothing. Per server because each dedicated server has its own `inventories.bin` and a player migrates on each
separately; one time because deleting the source IS the one-time-ness, so there is no "claimed" flag to keep in step
with anything and a second attempt is indistinguishable from a first attempt on a server the player never visited.

The client keeps no per-server state and never deletes its old identity file. It cannot know where it has already
migrated, and trying to remember would be a list to get wrong; silence on the far side is cheaper than bookkeeping.

`player_store` grew a **quarantine** for this (format v6): pre-account records load into a second map, keyed by device
hash, belonging to nobody. Nothing reads them at join, nothing writes into them at runtime, and the only thing that
removes one is a claim — so the set only ever shrinks. That replaced v5's behaviour, which archived pre-account files
and started empty; v5 shipped nowhere, and it would have deleted every old character on the first restart after an
upgrade.

A claim **merges** rather than overwrites, because the account may already have played on that server:

- Stuff adds, saturating at every cap — a tool owned on both sides is still one tool.
- XP takes the higher total per skill, never the sum. One career, walked on two keys.
- Position and the teleport cooldown stay where the ACCOUNT left them. A claim must not teleport a player who is
  standing somewhere, nor hand them a free return by inheriting a stale tick.
- Whichever layout survives is reconciled against the merged counts, so what the other pile brought gets a slot.

### 6.1 What the claim key is worth to a hostile operator

The client sends the truncated HASH, not the secret — the server stored only the hash, so that is the lookup key and the
secret itself never leaves the machine. That is a smaller improvement than it sounds: the hash is exactly what a claim
needs, so an operator who logs one can present it to any OTHER server the same player used and take the character
waiting there.

Accepted, with the reasoning written down rather than waved at. A challenge-response would close it and cannot be built:
the server holds `H(secret)` and nothing else, so it has no way to verify a signature over a nonce. The exposure is
bounded to characters that would otherwise be **deleted outright**, it shrinks every time a legitimate claim lands, and
it ends entirely once a player has migrated everywhere they played. Weigh that against v5's alternative, which was to
throw all of it away, and against the pre-account status quo, where the same secret was sent to every server on every
join as a standing identity.

Two things follow for anyone changing this. The claim must stay a bearer transfer of records the account can already be
given by other means, never a grant of anything the account did not otherwise have — no admin, no ban clearing, no
account property. And when the migration has served its purpose, the honest end state is to delete the quarantine reader
entirely rather than leave a permanent claim path open.

---

## 7. What is NOT built

- **No Sign in with Apple.** iOS links no net crate and has no multiplayer, so it is not needed yet. When it gains one,
  App Store guideline 4.8 requires it alongside the providers already here — the provider abstraction is what keeps that
  a master-side change, as adding Microsoft demonstrated.
- **No provider linking UI.** The schema supports several providers per account (that is why `ProviderLink` is a table
  and not two columns), but nothing exposes adding one.
- **No account website.** The browser's whole job is the provider round trip. Choosing a name, reading your id, signing
  out are in-game screens, which is why the master ships exactly two HTML pages a player can DO anything on; the
  hiscores board (`/hiscores`, SOCIAL-DESIGN.md §1) is the third page and is read-only.
- **No attestation of skill reports.** The board believes a hand-kept list of server addresses
  (`Master:Hiscores:TrustedServers`) and nobody else — the registration key says WHICH server is reporting, the list
  says whether its word counts. Anything stronger waits for a player count that makes it worth having.
- **No global ban tooling.** `Account.Status` and `BanUntilUtc` exist and `/api/join` honours them; nothing sets them
  but a hand-edited row.
- **No per-server pseudonymous ids.** See §1.1.
