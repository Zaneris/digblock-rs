namespace DigBlock.Master;

/// <summary>Bound from the "Master" configuration section.</summary>
public sealed class MasterOptions
{
    public ProbeOptions Probe { get; set; } = new();
    public RegistryOptions Registry { get; set; } = new();
    public TokenOptions Token { get; set; } = new();
    public DbOptions Db { get; set; } = new();
    public AccountOptions Account { get; set; } = new();
    public AuthOptions Auth { get; set; } = new();
    public NetOptions Net { get; set; } = new();
    public DevOptions Dev { get; set; } = new();
}

public sealed class NetOptions
{
    /// <summary>EXTRA proxy networks whose <c>X-Forwarded-For</c> is
    /// believed, in CIDR form, on top of the ones built into
    /// <see cref="TrustedProxies"/>.
    ///
    /// The built-in set is every address range that cannot appear as a
    /// caller on the public internet, which is exactly the "something is in
    /// front of me" condition — an ingress, a reverse proxy, a Docker
    /// bridge. This exists for the one deployment that breaks that rule: an
    /// ingress reached over a PUBLIC address. Getting it wrong the other way
    /// is the dangerous direction, so nothing here is trusted by default.</summary>
    public string[] TrustedProxyNetworks { get; set; } = [];
}

/// <summary>Escapes that exist so `just play`, the bots and the e2e suite
/// can run without a Google account. Every one of them is honoured ONLY
/// under ASPNETCORE_ENVIRONMENT=Development, and setting one anywhere else
/// is fatal at boot rather than ignored — the same discipline as the
/// mandatory QUIC probe. A dev escape that silently does nothing in
/// production is how a dev escape ends up live in production.</summary>
public sealed class DevOptions
{
    /// <summary>Enables POST /api/dev/signin: an account per subject
    /// string, with no provider round trip.</summary>
    public bool AuthEnabled { get; set; }

    /// <summary>Dial this instead of the registering server's address, for
    /// every registration. Exists for the containerized dev stack, where a
    /// LAN-IP hairpin breaks QUIC address validation behind Docker's NAT and
    /// the master must reach the host as host.docker.internal.
    ///
    /// This replaced RegisterRequest.ProbeHost. Same effect, different
    /// authority: the master operator sets it, not an anonymous caller, so
    /// it can no longer be used to publish an address nobody verified
    /// (NET-AUDIT-2.md §1.3).</summary>
    public string? ProbeHostOverride { get; set; }
}

public sealed class AuthOptions
{
    public ProviderOptions Google { get; set; } = new();
    public ProviderOptions Facebook { get; set; } = new();

    /// <summary>EXTRA browser origins a sign-in may return to, on top of
    /// the ones built into <see cref="Auth.RedirectUris"/>.
    ///
    /// Empty is the normal case and the shipped one: this game's own
    /// origins are compiled in, because a constant cannot be forgotten on
    /// a deploy the way a setting can. This exists for somebody running
    /// their own master against their own build of the client.
    ///
    /// Full origins, e.g. <c>https://example.com/</c>. Loopback targets do
    /// NOT go here — a native client's port is ephemeral, so those are
    /// matched by shape (RFC 8252 §7.3).</summary>
    public string[] WebRedirectUris { get; set; } = [];
}

/// <summary>One OAuth provider's credentials. A provider with no client id
/// or secret configured is simply off — its button never renders and its
/// callback 404s. That IS the feature flag: Facebook ships dark until its
/// App Review clears, with nothing separate to remember to flip.</summary>
public sealed class ProviderOptions
{
    public string? ClientId { get; set; }
    public string? ClientSecret { get; set; }

    public bool Enabled => !string.IsNullOrWhiteSpace(ClientId) && !string.IsNullOrWhiteSpace(ClientSecret);
}

public sealed class ProbeOptions
{
    public int IntervalSeconds { get; set; } = 30;
    public int TimeoutSeconds { get; set; } = 5;
    public int MaxStrikes { get; set; } = 3;

    /// <summary>Let the probe dial addresses off the public internet.
    ///
    /// This is the SSRF control (see ProbeTargets), so it is off by default
    /// and a hosted master must leave it off. It is a plain option rather
    /// than a Development-only escape because a master running on the same
    /// LAN as its servers is a legitimate deployment, and there is no
    /// security gain in refusing to let an operator probe their own network
    /// — the private range that matters is the PUBLIC master's neighbours.</summary>
    public bool AllowPrivateTargets { get; set; }
}

public sealed class RegistryOptions
{
    /// <summary>Drop a server that has not heartbeated for this long, with
    /// no probe and no strikes.
    ///
    /// Separate from the 3-strike probe sweep, and stricter: strikes answer
    /// "is a server that still talks to us reachable from the internet",
    /// which needs several rounds to be fair about a blip. This answers "has
    /// this server gone away", where silence is already the answer. Four
    /// missed 30 s heartbeats.</summary>
    public int IdleTimeoutSeconds { get; set; } = 120;
}

public sealed class TokenOptions
{
    public int TtlSeconds { get; set; } = 45;
}

public sealed class AccountOptions
{
    /// <summary>How long a device stays signed in without refreshing.
    /// Refresh rotates and extends, so an active player never sees it.</summary>
    public int SessionDays { get; set; } = 90;

    /// <summary>Cooldown between display-name changes. The FIRST change is
    /// free (the clock only starts once a name has been replaced), so a
    /// player who mistypes their name at sign-up is not stuck with it.</summary>
    public int NameChangeCooldownDays { get; set; } = 30;

    /// <summary>How long a released name is held back from anyone but its
    /// former holder. Without it, renaming is an impersonation primitive:
    /// step off a name and let an accomplice take it the same second, while
    /// everyone who knows you still reads that name as you.</summary>
    public int NameQuarantineDays { get; set; } = 30;
}

public sealed class DbOptions
{
    /// <summary>SQLite file path. The default is beside the working
    /// directory for a native <c>just master</c>; the container overrides it
    /// to <c>/data/master.db</c>, which MUST be a mounted volume in
    /// production — a Container App's own filesystem is ephemeral, so
    /// without one every redeploy takes every account with it.</summary>
    public string Path { get; set; } = "master.db";

    /// <summary>Seconds SQLite waits on a locked database before erroring.
    /// Generous because the intended production filesystem is an Azure Files
    /// (SMB) mount, where a lock round trip is a network round trip.
    ///
    /// The journal mode that goes with it is NOT a setting — see
    /// <see cref="Data.JournalModeInterceptor"/>, which pins DELETE because
    /// EF Core's provider turns WAL on by itself and WAL does not work over
    /// SMB.</summary>
    public int BusyTimeoutSeconds { get; set; } = 30;
}
