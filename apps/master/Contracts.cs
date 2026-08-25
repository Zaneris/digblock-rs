namespace DigBlock.Master;

// Wire shapes for the master API. Serialized camelCase (ASP.NET web default).
// This contract is mirrored by the Rust side in apps/server/src/master.rs and
// apps/desktop/src/services.rs — keep them in lockstep.

// Anonymous, but the FIRST registration of a host:port mints a key and the
// address remembers it: a later registration must present that key in an
// Authorization: Bearer header. A game server persists the key beside its
// world data. See the handler in Program.cs for the full probe order.
public sealed record RegisterRequest(
    // Display-only, sanitized rather than validated: the master keeps
    // letters/digits/space/-/_, caps at ServerRegistry.MaxNameLength, and
    // falls back to a default — registration never fails over the name.
    string? Name,
    string? Host, // optional; defaults to the observed remote IP
    int Port,
    string? CertHash, // 64 hex chars (SHA-256 of the server's TLS cert)
    int Players,
    int MaxPlayers,
    int ProtocolVersion,
    int SimVersion);
// ProbeHost is gone. It let an anonymous caller choose the only address the
// master ever dialed while clients were handed a different one, which voided
// the guarantee the probe exists to give. The dev
// NAT-hairpin case it existed for is Master:Dev:ProbeHostOverride now —
// master-side configuration rather than a request field.

/// <summary><c>ServerKey</c> is the registration key for this address: newly
/// minted on a first claim, and otherwise the same key that was presented.
/// A game server stores it and presents it on every later registration.</summary>
/// <para><c>ReportsSkills</c> says whether this address is on the master's trusted list
/// (Master:Hiscores:TrustedServers): only then are its skill reports accepted, so a server
/// that is not listed never sends one. Appended; an older game server ignores it.</para>
public sealed record RegisterOk(
    bool Reachable, Guid ServerId, string ServerKey, string ObservedHost, bool ReportsSkills);

/// <summary><c>Detail</c> is empty outside Development: it used to carry the
/// raw exception text, which described the master's own network to an
/// anonymous caller. <c>Reason</c> stays — a self-hoster needs to tell
/// certMismatch from timeout.</summary>
public sealed record RegisterFailed(bool Reachable, string Reason, string Detail);

public sealed record HeartbeatRequest(int Players, int MaxPlayers);

// ProtocolVersion/SimVersion were accepted at registration and read by
// nobody, so an incompatible server listed indistinguishably from a
// compatible one and the client only found out after a full QUIC connect
// and Hello. Surfacing them lets the list say so.
// ReportsSkills: whether play here counts toward the hiscores board — the
// trusted list's answer, published so a player can see it before joining.
public sealed record ServerSummary(
    Guid Id, string Name, string Url, string CertHash, int Players, int MaxPlayers,
    int ProtocolVersion, int SimVersion, bool ReportsSkills);

public sealed record ServerListResponse(IReadOnlyList<ServerSummary> Servers);

// Authenticated with the player's session token (Authorization: Bearer).
// There is deliberately no display name here any more: the master owns the
// name, so a joiner cannot pick one per request. That single removal is
// what closed admin impersonation — admin is keyed on
// the account id, and a name is no longer an authorization input at all.
public sealed record JoinRequest(Guid ServerId);

public sealed record JoinResponse(string Token, int ExpiresIn, string Url, string CertHash);

public sealed record VerifyRequest(string? Token);

/// <summary>What the game server learns about a joining player. The account
/// id is the persistence key on the far side (inventories, skills, admin,
/// bans); the name is display only.</summary>
public sealed record VerifyResponse(string DisplayName, Guid AccountId);

// ---- hiscores --------------------------------------------------------------
// The server half is mirrored by apps/server/src/master.rs, the client half
// by the two services fillers. SOCIAL-DESIGN.md §1.

/// <summary>A trusted server's batch of XP totals (never levels — the master derives those with
/// the pinned curve). One entry per account, only the skills that moved; a skill index the
/// master does not know refuses the whole batch, so the roster is extended on both ends or
/// neither.</summary>
public sealed record SkillReport(SkillReportRow[] Players);

public sealed record SkillReportRow(Guid AccountId, SkillXp[] Skills);

public sealed record SkillXp(int Skill, long Xp);

/// <summary>One page of one board. <c>Skill</c> is the slug asked for ("mining", …, or
/// "overall"); <c>Total</c> is how many accounts the board holds, so a client can page.</summary>
public sealed record HiscoresResponse(
    string Skill, int Page, int PageSize, int Total, HiscoreRow[] Rows);

/// <summary>On a skill board <c>Level</c>/<c>Xp</c> are that skill's; on the
/// overall board they are the level sum and the XP sum.</summary>
public sealed record HiscoreRow(int Rank, Guid AccountId, string DisplayName, int Level, long Xp);

/// <summary>Where the caller's account stands, one row per board it appears on. A board the
/// account has never been reported to is simply absent — "unranked" is the missing row, not
/// a zero.</summary>
public sealed record MyRanksResponse(RankRow[] Ranks);

public sealed record RankRow(string Skill, int Rank, int Level, long Xp);

// Music manifest entry (GET /api/music). Url is master-relative; the
// clients' audio fillers resolve it against the master base URL. Set is
// "day" or "night" — which pool the track belongs to; clients that
// predate the field ignore it and play everything.
public sealed record MusicTrack(string Name, string Url, string Set);

// ---- accounts and sessions -------------------------------------------------
// The client-facing half of sign-in. Mirrored by apps/desktop/src/services.rs
// and apps/web/src/services.rs.

/// <summary>Redeem the one-time code the sign-in redirect carried back.
/// <c>Verifier</c> is the PKCE secret the client generated before opening
/// the browser — it, not the code, is what proves this is the same client.</summary>
public sealed record ExchangeRequest(string? Code, string? Verifier);

public sealed record RefreshRequest(string? Platform);

/// <summary>Sign out. <c>AllDevices</c> revokes every live session on the
/// account, which is the honest answer to "I think someone has my session".</summary>
public sealed record SignOutRequest(bool AllDevices);

/// <summary>A minted session. <c>DisplayName</c> is null for an account
/// that has not picked one yet — that is the client's cue to open the
/// name-picker rather than the server list.</summary>
public sealed record SessionResponse(Guid AccountId, string? DisplayName, string SessionToken, int ExpiresIn);

public sealed record AccountResponse(
    Guid AccountId,
    string? DisplayName,
    string[] Providers,
    DateTimeOffset? NameChangeAvailableUtc);

public sealed record NameRequest(string? DisplayName);

public sealed record NameSetResponse(string DisplayName);

/// <summary>Live feedback while a player types a name. <c>Reason</c> is
/// shown verbatim, so the wording lives on the master with the rule.</summary>
public sealed record NameAvailableResponse(string Name, bool Available, string? Reason);

public sealed record AccountLookupResponse(Guid AccountId, string DisplayName);

/// <summary>The one error shape. Matches the anonymous <c>{ error }</c>
/// objects the registration endpoints already return.</summary>
public sealed record ApiError(string Error);

/// <summary>Development-only sign-in (see Dev/DevEndpoints.cs). The subject
/// picks the account; the same subject always lands on the same one.</summary>
public sealed record DevSignInRequest(string? Subject, string? Platform);
