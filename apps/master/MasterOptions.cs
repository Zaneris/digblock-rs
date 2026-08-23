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
    /// <summary>EXTRA proxy networks whose <c>X-Forwarded-For</c> is believed,
    /// in CIDR form, on top of <see cref="TrustedProxies"/>'s built-ins — every
    /// range that cannot appear as a caller on the public internet. This is for
    /// the one deployment that breaks that rule: an ingress reached over a
    /// PUBLIC address. The other way is dangerous, so nothing here is default.</summary>
    public string[] TrustedProxyNetworks { get; set; } = [];
}

/// <summary>Escapes that exist so `just play`, the bots and the e2e suite can
/// run without a Google account. Every one is honoured ONLY under
/// ASPNETCORE_ENVIRONMENT=Development, and setting one anywhere else is fatal
/// at boot rather than ignored — a dev escape that silently does nothing in
/// production is how a dev escape ends up live in production.</summary>
public sealed class DevOptions
{
    /// <summary>Enables POST /api/dev/signin: an account per subject
    /// string, with no provider round trip.</summary>
    public bool AuthEnabled { get; set; }

    /// <summary>Dial this instead of the registering server's address, for
    /// every registration. Exists for the containerized dev stack, where a
    /// LAN-IP hairpin breaks QUIC address validation behind Docker's NAT and
    /// the master must reach the host as host.docker.internal. The master
    /// operator sets it, never an anonymous caller — that is what makes it safe.</summary>
    public string? ProbeHostOverride { get; set; }
}

public sealed class AuthOptions
{
    public ProviderOptions Google { get; set; } = new();
    public ProviderOptions Facebook { get; set; } = new();
    public ProviderOptions Microsoft { get; set; } = new();

    /// <summary>EXTRA browser origins a sign-in may return to, on top of the
    /// ones built into <see cref="Auth.RedirectUris"/>. Empty is the shipped
    /// case: this game's own origins are compiled in, because a constant cannot
    /// be forgotten on a deploy the way a setting can. Full origins only —
    /// a loopback port is ephemeral, so those match by shape (RFC 8252 §7.3).</summary>
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

    /// <summary>Let the probe dial addresses off the public internet. This is
    /// the SSRF control (see ProbeTargets), so it is off by default and a
    /// hosted master must leave it off. A plain option rather than a
    /// Development-only escape because a master on the same LAN as its servers
    /// is legitimate — the range that matters is the PUBLIC master's neighbours.</summary>
    public bool AllowPrivateTargets { get; set; }
}

public sealed class RegistryOptions
{
    /// <summary>Drop a server that has not heartbeated for this long, with no
    /// probe and no strikes. Separate from the 3-strike probe sweep, and
    /// stricter: strikes answer "is a server that still talks to us reachable
    /// from the internet", which needs several rounds to be fair about a blip.
    /// This answers "has it gone away", where silence is already the answer.</summary>
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
    /// (SMB) mount, where a lock round trip is a network round trip. The
    /// journal mode is NOT a setting — <see cref="Data.JournalModeInterceptor"/>
    /// pins DELETE, because EF turns WAL on itself and WAL fails over SMB.</summary>
    public int BusyTimeoutSeconds { get; set; } = 30;
}
