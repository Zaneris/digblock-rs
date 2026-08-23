namespace DigBlock.Master.Data;

/// <summary>Which third-party identity provider vouched for an account.
/// Persisted as the integer value, so these are <b>append-only</b> — the
/// number is a storage format, exactly like the Rust side's
/// <c>Skill</c> discriminants.</summary>
public enum AuthProvider
{
    Google = 1,
    Facebook = 2,
    Microsoft = 3,
}

/// <summary>Why a session stopped working. Recorded because only ONE of
/// these is a security event: a token that was rotated away and then
/// presented again means a copy of it escaped. A signed-out or
/// cascade-revoked token turning up later is just a client that has not
/// noticed yet, and logging that as a replay would bury the real signal.</summary>
public enum SessionRevocation
{
    /// <summary>Superseded by a refresh. Presenting it again is the
    /// reuse-detection trigger.</summary>
    Rotated = 0,

    /// <summary>The player signed out.</summary>
    SignedOut = 1,

    /// <summary>Collateral: something else in the chain was replayed, or
    /// the player signed out everywhere.</summary>
    Cascade = 2,
}

public enum AccountStatus
{
    Active = 0,

    /// <summary>Globally banned by the master: no join token is minted, at
    /// any server. Distinct from a per-server ban, which the game server
    /// owns and enforces against its own <c>bans.json</c>.</summary>
    Banned = 1,
}

/// <summary>A player. The <see cref="Id"/> is the identity the whole game keys on: it reaches a
/// game server through <c>/api/tokens/verify</c> (never from the client), keys the server's
/// <c>inventories.bin</c> records, and is what <c>--admin-id</c> compares against. Deliberately
/// holds <b>no PII</b> — the providers hand back an email and a real name and neither is stored,
/// because the subject alone re-identifies a returning player.</summary>
public sealed class Account
{
    public Guid Id { get; set; }

    public DateTimeOffset CreatedUtc { get; set; }

    /// <summary>Last successful session refresh or join mint. Purely
    /// operational (pruning idle accounts, abuse triage).</summary>
    public DateTimeOffset LastSeenUtc { get; set; }

    public AccountStatus Status { get; set; }

    /// <summary>The player's chosen name, in the case they typed it. Null
    /// until they pick one, which is why <c>/api/join</c> refuses an
    /// unnamed account rather than inventing a name for it.</summary>
    public string? DisplayName { get; set; }

    /// <summary>Lowercased <see cref="DisplayName"/>, carrying the unique index. Names are unique
    /// case-insensitively so a name identifies exactly one person; impersonation in chat is the
    /// whole reason, since admin authority itself keys on <see cref="Id"/>. Nullable, and SQLite
    /// (like Postgres) treats NULLs in a unique index as distinct, so every unnamed account
    /// coexists with no filter.</summary>
    public string? DisplayNameNormalized { get; set; }

    /// <summary>When the name last changed; null if never renamed. Drives
    /// the rename cooldown.</summary>
    public DateTimeOffset? DisplayNameChangedUtc { get; set; }

    public DateTimeOffset? BanUntilUtc { get; set; }
    public string? BanReason { get; set; }

    public List<ProviderLink> Providers { get; set; } = [];
    public List<Session> Sessions { get; set; } = [];
}

/// <summary>One "sign in with X" identity attached to an account. An account
/// may carry several, which is what lets a player who signed up with Google
/// later add Facebook and keep one character.</summary>
public sealed class ProviderLink
{
    public long Id { get; set; }

    public Guid AccountId { get; set; }
    public Account Account { get; set; } = null!;

    public AuthProvider Provider { get; set; }

    /// <summary>The provider's stable subject claim (Google <c>sub</c>,
    /// Facebook <c>id</c>, Microsoft Graph <c>id</c>). Unique per provider —
    /// this pair IS the login.</summary>
    public string Subject { get; set; } = "";

    public DateTimeOffset LinkedUtc { get; set; }
}

/// <summary>A name that used to belong to someone, held back from reuse for a quarantine window
/// after it was released. Rows exist only for <b>released</b> names; a live name's uniqueness is
/// the index on <see cref="Account.DisplayNameNormalized"/>. Without the quarantine, renaming is
/// an impersonation primitive: rename away and let an accomplice take the name the same second,
/// while everyone else still expects it to be yours.</summary>
public sealed class NameReservation
{
    /// <summary>Lowercased name; the primary key.</summary>
    public string NameNormalized { get; set; } = "";

    /// <summary>Who released it. The former holder may always take their own
    /// name back immediately — the quarantine exists to stop <i>other</i>
    /// people grabbing it, not to punish a change of mind.</summary>
    public Guid AccountId { get; set; }

    public DateTimeOffset ReleasedUtc { get; set; }
}

/// <summary>A signed-in device. The client stores the raw token; the master stores only its
/// SHA-256, so a database read does not hand out sessions. It expires, it is revocable, and it is
/// presented ONLY to the master — a game server never sees it.</summary>
public sealed class Session
{
    public Guid Id { get; set; }

    public Guid AccountId { get; set; }
    public Account Account { get; set; } = null!;

    /// <summary>SHA-256 of the opaque bearer token. No password KDF: the
    /// token is 32 bytes of CSPRNG output, so there is nothing to brute
    /// force and a slow hash would only cost latency on every request.</summary>
    public byte[] TokenHash { get; set; } = [];

    /// <summary>Groups every rotation of one original sign-in. Refresh mints
    /// a new row in the same chain and revokes the old one; presenting a
    /// revoked-by-rotation token afterwards means the token leaked, so the
    /// whole chain dies (RFC 6819 refresh-token reuse detection).</summary>
    public Guid ChainId { get; set; }

    public DateTimeOffset CreatedUtc { get; set; }
    public DateTimeOffset LastSeenUtc { get; set; }
    public DateTimeOffset ExpiresUtc { get; set; }
    public DateTimeOffset? RevokedUtc { get; set; }
    public SessionRevocation? RevokedReason { get; set; }

    /// <summary>"desktop" / "web" — shown on the account screen so a player
    /// can tell which row to revoke. Client-supplied and display-only.</summary>
    public string? Platform { get; set; }
}

/// <summary>A game server's claim on a <c>host:port</c>, and the hash of the key that proves it.
/// The only persisted registry state — liveness (players, cert hash, heartbeat, strikes) lives in
/// <see cref="DigBlock.Master.ServerRegistry"/> and rebuilds from heartbeats. Persisting keeps the
/// <see cref="Id"/> stable across a redeploy and, the security half, keeps the re-register key, so
/// a restart is not a window to steal an address.</summary>
public sealed class ServerClaim
{
    public Guid Id { get; set; }

    /// <summary>The advertised address clients are handed, lowercased.</summary>
    public string Host { get; set; } = "";

    public int Port { get; set; }

    /// <summary>SHA-256 of the server's registration key. Same reasoning as
    /// <see cref="Session.TokenHash"/>.</summary>
    public byte[] KeyHash { get; set; } = [];

    /// <summary>Last sanitized name it registered under; display only.</summary>
    public string Name { get; set; } = "";

    public DateTimeOffset FirstSeenUtc { get; set; }
    public DateTimeOffset LastRegisteredUtc { get; set; }
}
