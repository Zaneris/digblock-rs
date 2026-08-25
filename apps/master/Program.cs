using System.Net;
using System.Net.Quic;
using DigBlock.Master;
using DigBlock.Master.Accounts;
using DigBlock.Master.Auth;
using DigBlock.Master.Data;
using DigBlock.Master.Dev;
using DigBlock.Master.Hiscores;
using DigBlock.Master.Servers;
using Microsoft.AspNetCore.DataProtection;
using Microsoft.Data.Sqlite;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Options;

var builder = WebApplication.CreateBuilder(args);

// Local-dev default only. Never put "Urls" in appsettings.json — it would
// override ASPNETCORE_URLS, which the container relies on to bind 0.0.0.0.
if (string.IsNullOrEmpty(builder.Configuration["urls"]))
    builder.WebHost.UseUrls("http://127.0.0.1:5200");

var masterSection = builder.Configuration.GetSection("Master");
builder.Services.Configure<MasterOptions>(masterSection);
// Also bound eagerly: the authentication handlers have to be registered at
// service-configuration time, and which ones exist depends on which
// providers have credentials.
var masterOptions = masterSection.Get<MasterOptions>() ?? new MasterOptions();

builder.Services.AddSingleton<ServerRegistry>();
builder.Services.AddSingleton<TokenStore>();
builder.Services.AddSingleton<AuthRequestStore>();
builder.Services.AddSingleton<RegistrationBans>();
builder.Services.AddScoped<AccountService>();
builder.Services.AddScoped<ServerClaims>();
builder.Services.AddSingleton<TrustedServers>();
builder.Services.AddScoped<HiscoreService>();
builder.Services.AddHostedService<HealthChecker>();
builder.Services.AddDigBlockAuth(masterOptions);
builder.Services.AddDigBlockRateLimits();

// The OAuth correlation and External cookies are DataProtection-encrypted,
// and the default key ring lives inside the container — so a redeploy in
// the middle of somebody's sign-in would reject their callback with a
// correlation failure. Keys go next to the database, on the same mounted
// volume, so a restart is invisible. Nothing long-lived depends on this:
// session tokens are opaque and database-backed, not encrypted blobs.
//
// The "no XML encryptor configured" warning this leaves behind is expected
// and not worth chasing: there is no DPAPI on Linux, and the key ring sits
// on the same volume as the account database, so encrypting it with a key
// stored beside it would buy nothing.
builder.Services.AddDataProtection()
    .SetApplicationName("digblock-master")
    .PersistKeysToFileSystem(new DirectoryInfo(
        Path.Combine(
            Path.GetDirectoryName(Path.GetFullPath(masterOptions.Db.Path)) ?? ".", "dataprotection-keys")));

// Accounts are the master's first durable state. Everything ephemeral
// (join tokens, liveness) stays in the singletons above.
builder.Services.AddDbContext<MasterDb>((sp, o) =>
{
    var db = sp.GetRequiredService<IOptions<MasterOptions>>().Value.Db;
    o.UseSqlite(new SqliteConnectionStringBuilder
    {
        DataSource = db.Path,
        DefaultTimeout = db.BusyTimeoutSeconds,
    }.ToString());
    o.AddInterceptors(new JournalModeInterceptor());
});

// Browser clients (the wasm build) call the client endpoints cross-origin.
// AllowAnyHeader covers the Authorization header the account and join
// endpoints now require. Any-origin stays legal because none of these use
// cookies — the only cookie in the app is the External scheme, and that
// rides same-origin browser NAVIGATIONS to /auth, never a fetch. Server
// endpoints are bearer-authenticated server-to-server calls and stay
// CORS-less (browsers never make them).
builder.Services.AddCors(o => o.AddPolicy(Cors.Client, p =>
    p.AllowAnyOrigin().AllowAnyHeader().AllowAnyMethod()));

// The QUIC reachability probe is mandatory — an unverified "reachable"
// defeats the feature — so a host that can't probe fails loudly at boot.
if (!QuicConnection.IsSupported)
{
    Console.Error.WriteLine(
        """
        fatal: QUIC is not supported on this host, so server reachability cannot be probed.
          macOS: brew install libmsquic
                 export DYLD_FALLBACK_LIBRARY_PATH="$DYLD_FALLBACK_LIBRARY_PATH:$(brew --prefix)/lib"
          linux: install libmsquic (apk add libmsquic / apt install libmsquic)
          or run the container from apps/master/Dockerfile, which bundles it.
        """);
    Environment.Exit(1);
}
builder.Services.AddSingleton<IReachabilityProbe>(sp =>
{
    var probe = sp.GetRequiredService<IOptions<MasterOptions>>().Value.Probe;
    return new QuicProbe(TimeSpan.FromSeconds(probe.TimeoutSeconds), probe.AllowPrivateTargets);
});

// Same discipline as the QUIC gate above: a dev escape left on outside
// Development is fatal, never quietly ignored. Silently disabling it would
// hide the misconfiguration until the day someone changed the environment.
if (masterOptions.Dev.AuthEnabled && !builder.Environment.IsDevelopment())
{
    Console.Error.WriteLine(
        $"""
        fatal: Master:Dev:AuthEnabled is set, but ASPNETCORE_ENVIRONMENT is "{builder.Environment.EnvironmentName}".
          Dev sign-in bypasses every OAuth provider entirely and mints an account for any subject string.
          It is honoured only in Development. Unset it, or set ASPNETCORE_ENVIRONMENT=Development.
        """);
    Environment.Exit(1);
}

var app = builder.Build();

// Schema at boot. Cheap enough to be unconditional at this scale, and it
// keeps the deploy a single `az containerapp update` with no migration step
// that could be forgotten or run against the wrong revision. It DOES mean
// the app must own the database file exclusively, which is the same reason
// the Container App is pinned to one replica.
{
    var dbPath = app.Services.GetRequiredService<IOptions<MasterOptions>>().Value.Db.Path;
    var dir = Path.GetDirectoryName(Path.GetFullPath(dbPath));
    if (!string.IsNullOrEmpty(dir))
        Directory.CreateDirectory(dir);
    using var scope = app.Services.CreateScope();
    scope.ServiceProvider.GetRequiredService<MasterDb>().Database.Migrate();
    app.Logger.LogInformation("accounts database ready at {Path}", Path.GetFullPath(dbPath));
}

// The observed client address is load-bearing (probe target, rate-limit partition, ban
// subject) and behind Container Apps ingress RemoteIpAddress is the envoy proxy. TWO
// settings: ForwardLimit bounds the hops walked so a client cannot prepend a fake origin;
// KnownIPNetworks (see TrustedProxies) says there WAS an ingress, else the header is
// believed from any peer. XForwardedProto rides along, or sign-in cookies lose Secure.
var forwarded = new ForwardedHeadersOptions
{
    ForwardedHeaders = Microsoft.AspNetCore.HttpOverrides.ForwardedHeaders.XForwardedFor
        | Microsoft.AspNetCore.HttpOverrides.ForwardedHeaders.XForwardedProto,
    ForwardLimit = 1,
};
forwarded.KnownProxies.Clear();
forwarded.KnownIPNetworks.Clear();
foreach (var net in TrustedProxies.Networks(masterOptions.Net.TrustedProxyNetworks))
    forwarded.KnownIPNetworks.Add(net);
app.Logger.LogInformation(
    "trusting X-Forwarded-For from {Count} proxy networks ({Extra} configured beyond the built-ins)",
    forwarded.KnownIPNetworks.Count, masterOptions.Net.TrustedProxyNetworks.Length);
app.UseForwardedHeaders(forwarded);
app.UseCors();
app.UseAuthentication();
app.UseRateLimiter();

app.MapAuthEndpoints();
app.MapAccountEndpoints();
app.MapHiscoreEndpoints();
if (masterOptions.Dev.AuthEnabled)
{
    app.MapDevEndpoints();
    app.Logger.LogWarning("DEV SIGN-IN IS ENABLED — any subject string mints an account");
}

// Liveness for the container platform. The e2e harness historically polled
// /api/servers to decide the master was up, which conflates "listening"
// with "has registrations".
app.MapGet("/health", () => Results.Ok(new { status = "ok" }));

// ---- game server endpoints -------------------------------------------------

// Registration is anonymous but STICKY, and the probe order is the security
// property. In full, because this is the most attacked endpoint here:
//
//  1. A source address serving a temp-ban is refused outright.
//  2. The SOURCE address is probed first. Passing proves the registrant is a
//     live QUIC endpoint at that address serving that certificate — the only
//     evidence an attacker elsewhere on the internet cannot manufacture.
//  3. An advertised address that differs is probed TOO, and on the normal
//     path both must pass: an address clients will be aimed at is never
//     published unprobed.
//  4. If the source probe fails, the advertised address is accepted only when
//     it probes clean AND nothing already claims it. That is the NAT-hairpin
//     escape, and confining it to unclaimed addresses is what stops it being
//     a redirect primitive.
//  5. An address that is claimed AND live may only be re-registered by
//     whoever presents its key, or by a registrant that passes the SOURCE
//     probe; anyone else gets 409 and a temp-ban. The source probe is the
//     discriminator and is checked BEFORE the ban — see the comment at the
//     409 below. Under Master:Dev:ProbeHostOverride both probes dial the
//     configured host, so there is no discriminator and that case refuses on
//     `live` alone.
//  6. A claimed address may be re-keyed by a registrant that passes the
//     probes: the recovery path for an operator who lost ./data, safe
//     precisely because passing the source probe means being the machine at
//     that address, serving a certificate that needs the incumbent's key.
app.MapPost("/api/servers/register", async (
    RegisterRequest req, HttpContext http, ServerRegistry registry, ServerClaims claims,
    RegistrationBans bans, IReachabilityProbe probe, TrustedServers trusted,
    IOptions<MasterOptions> opts, IHostEnvironment env, CancellationToken ct) =>
{
    var source = http.Connection.RemoteIpAddress;
    if (source?.IsIPv4MappedToIPv6 == true)
        source = source.MapToIPv4();
    if (bans.IsBanned(source))
        return Results.Json(new { error = "too many failed registrations from this address" },
            statusCode: StatusCodes.Status429TooManyRequests);

    if (req.Port is < 1 or > 65535)
        return Results.BadRequest(new { error = "port out of range" });
    byte[] certHash;
    try
    {
        certHash = Convert.FromHexString(req.CertHash ?? "");
    }
    catch (FormatException)
    {
        return Results.BadRequest(new { error = "certHash must be hex" });
    }
    if (certHash.Length != 32)
        return Results.BadRequest(new { error = "certHash must be 32 bytes (64 hex chars)" });

    if (source is null)
        return Results.BadRequest(new { error = "no observable remote address" });
    var observed = source.ToString();
    var advertised = req.Host?.Trim();
    if (string.IsNullOrEmpty(advertised))
        advertised = observed;
    advertised = ServerClaims.Normalize(advertised);

    var claim = await claims.FindAsync(advertised, req.Port, ct);
    var live = registry.FindLive(advertised, req.Port);
    var presented = Bearer.From(http);
    var keyOk = claim is not null && ServerClaims.KeyMatches(claim, presented);

    // Someone is claiming an address another key holds that still looks live: a takeover,
    // or an operator who lost ./data and restarted. Only the source probe separates them,
    // so the refusal WAITS for it rather than firing on `live` alone — that in-memory
    // registry outlives its process, and firing early temp-banned a restarting operator
    // by their own stale entry. The cost is one probe, capped by RateLimits.Register.
    var contested = claim is not null && !keyOk && live is not null;

    // Master-side probe redirection for the containerized dev stack. This
    // used to be a request field; it is configuration now.
    var probeOverride = opts.Value.Dev.ProbeHostOverride;
    var probeHost = string.IsNullOrWhiteSpace(probeOverride) ? advertised : probeOverride!;
    var probeSourceHost = string.IsNullOrWhiteSpace(probeOverride) ? observed : probeOverride!;

    // Under the override the source probe is NOT evidence of origin: it dials the
    // configured host rather than the registrant, so a takeover attempt would pass it by
    // dialing the victim. The recovery path is therefore production-only, and under the
    // override the refusal goes back to firing on `live` alone — the dev stack's old
    // behaviour, and the one its e2e test pins.
    if (contested && !string.IsNullOrWhiteSpace(probeOverride))
    {
        var devBan = bans.Ban(source);
        app.Logger.LogWarning(
            "refused takeover of {Host}:{Port} from {Source} (no key, incumbent live; ProbeHostOverride means the source probe proves nothing); banned {Minutes} min",
            advertised, req.Port, observed, (int)devBan.TotalMinutes);
        return Results.Json(new { error = "that address is already registered" },
            statusCode: StatusCodes.Status409Conflict);
    }

    var sourceProbe = await probe.ProbeAsync(probeSourceHost, req.Port, certHash, ct);
    var advertisedProbe = probeHost == probeSourceHost
        ? sourceProbe
        : await probe.ProbeAsync(probeHost, req.Port, certHash, ct);

    // Contested, and NOT the machine at the address it registered from: the
    // takeover. Passing the source probe would have meant serving the
    // certificate this registration pins, from the registrant's OWN address
    // — which needs the incumbent's private key, so an attacker elsewhere
    // cannot reach past this branch. (The override case never gets here.)
    if (contested && !sourceProbe.Reachable)
    {
        var duration = bans.Ban(source);
        app.Logger.LogWarning(
            "refused takeover of {Host}:{Port} from {Source} (no key, incumbent live, source probe failed); banned {Minutes} min",
            advertised, req.Port, observed, (int)duration.TotalMinutes);
        return Results.Json(new { error = "that address is already registered" },
            statusCode: StatusCodes.Status409Conflict);
    }

    var accepted = sourceProbe.Reachable && advertisedProbe.Reachable;
    if (!accepted && advertisedProbe.Reachable && (claim is null || keyOk))
        accepted = true;
    if (!accepted)
    {
        // Report the advertised address's failure when there is one to
        // report: that is the address the operator configured and the one
        // they can act on.
        var failure = !advertisedProbe.Reachable ? advertisedProbe : sourceProbe;
        return Results.UnprocessableEntity(new RegisterFailed(false, failure.ReasonString, Detail(failure)));
    }

    var name = ServerRegistry.SanitizeName(req.Name);
    string key;
    if (keyOk)
    {
        await claims.TouchAsync(claim!, name, ct);
        key = presented!;
    }
    else
    {
        if (claim is not null)
            app.Logger.LogWarning(
                "re-keying {Host}:{Port} for {Source}: claim exists, no key presented, source probe passed (incumbent {Incumbent})",
                advertised, req.Port, observed, live is null ? "gone" : "stale-but-listed");
        (claim, key) = await claims.ClaimAsync(advertised, req.Port, name, claim, ct);
    }

    var entry = registry.Register(
        claim!.Id, claim.KeyHash, name, advertised, probeHost, req.Port, certHash,
        ServerRegistry.SanitizeCount(req.Players), ServerRegistry.SanitizeCount(req.MaxPlayers),
        req.ProtocolVersion, req.SimVersion);
    var reportsSkills = trusted.IsTrusted(entry);
    app.Logger.LogInformation(
        "registered {Name} at {Host}:{Port} as {Id}{Trusted}",
        entry.Name, advertised, req.Port, entry.Id, reportsSkills ? " (trusted: reports skills)" : "");
    return Results.Ok(new RegisterOk(true, entry.Id, key, observed, reportsSkills));

    // The four reason codes stay — a self-hoster needs certMismatch apart
    // from timeout to debug a port forward, and ProbeTargets already removed
    // the targets that made the oracle interesting. The raw exception text
    // does not: it leaks the master's own network into an anonymous
    // response (§1.7).
    string Detail(ProbeResult r) => env.IsDevelopment() ? r.Detail ?? "" : "";
}).RequireRateLimiting(RateLimits.Register);

app.MapPost("/api/servers/{id:guid}/heartbeat", (
    Guid id, HeartbeatRequest req, HttpContext http, ServerRegistry registry) =>
{
    // Unknown id → 404: tells a game server the master restarted (or culled
    // it) and it should re-register.
    if (!registry.TryGet(id, out var entry))
        return Results.NotFound();
    if (!ServerRegistry.KeyMatches(entry, Bearer.From(http)))
        return Results.Unauthorized();
    entry.Players = ServerRegistry.SanitizeCount(req.Players);
    entry.MaxPlayers = ServerRegistry.SanitizeCount(req.MaxPlayers);
    entry.Strikes = 0;
    entry.LastSeenUtc = DateTimeOffset.UtcNow;
    return Results.NoContent();
});

app.MapDelete("/api/servers/{id:guid}", (Guid id, HttpContext http, ServerRegistry registry) =>
{
    // Graceful shutdown notify. Idempotent: an already-gone id is success.
    if (!registry.TryGet(id, out var entry))
        return Results.NoContent();
    if (!ServerRegistry.KeyMatches(entry, Bearer.From(http)))
        return Results.Unauthorized();
    registry.Remove(id);
    app.Logger.LogInformation("{Name} ({Host}:{Port}) shut down", entry.Name, entry.Host, entry.Port);
    return Results.NoContent();
});

app.MapPost("/api/tokens/verify", (VerifyRequest req, HttpContext http, ServerRegistry registry, TokenStore tokens) =>
{
    // The bearer key identifies which server is asking; a token minted for a
    // different server never verifies. Unknown / expired / consumed / wrong
    // server are deliberately indistinguishable.
    var entry = registry.FindByKey(Bearer.From(http));
    if (entry is null)
        return Results.Unauthorized();
    if (string.IsNullOrEmpty(req.Token) || !tokens.TryConsume(req.Token, entry.Id, out var consumed))
        return Results.NotFound();
    return Results.Ok(new VerifyResponse(consumed.DisplayName, consumed.AccountId));
}).RequireRateLimiting(RateLimits.Verify);

// ---- game client endpoints -------------------------------------------------

app.MapGet("/api/servers", (ServerRegistry registry, TrustedServers trusted) =>
    new ServerListResponse([.. registry.Snapshot()
        .OrderBy(e => e.Name, StringComparer.OrdinalIgnoreCase)
        .Select(e => new ServerSummary(
            e.Id, e.Name, e.Url, e.CertHashHex, e.Players, e.MaxPlayers,
            e.ProtocolVersion, e.SimVersion, trusted.IsTrusted(e)))]))
    .RequireCors(Cors.Client);

// Mint a one-time join token. Authenticated now: the caller presents their
// session, and the name on the token is the one the master holds for that
// account. A joiner can no longer name themselves, which is what makes
// `client.name` trustworthy on the game server and takes the display string
// out of the authorization path entirely.
app.MapPost("/api/join", async (
    JoinRequest req, HttpContext http, ServerRegistry registry, TokenStore tokens,
    AccountService accounts, IOptions<MasterOptions> opts, CancellationToken ct) =>
{
    var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
    if (session is null)
        return Results.Unauthorized();
    var account = session.Account;
    if (AccountService.IsGloballyBanned(account))
        return Results.Json(new ApiError("this account is suspended"),
            statusCode: StatusCodes.Status403Forbidden);
    // An account with no name yet cannot join: the alternative is inventing
    // one, and a name nobody chose is a name nobody owns.
    if (account.DisplayName is null)
        return Results.Json(new ApiError("pick a display name first"),
            statusCode: StatusCodes.Status409Conflict);
    if (!registry.TryGet(req.ServerId, out var entry))
        return Results.NotFound();

    var ttl = TimeSpan.FromSeconds(opts.Value.Token.TtlSeconds);
    var token = tokens.Mint(entry.Id, account.Id, account.DisplayName, ttl);
    return Results.Ok(new JoinResponse(token, (int)ttl.TotalSeconds, entry.Url, entry.CertHashHex));
}).RequireCors(Cors.Client).RequireRateLimiting(RateLimits.Join);

// ---- music (static CC0 tracks, see music/SOURCES.md) ------------------------
// Clients (desktop AND web, single-player included) fetch tracks from the
// master at runtime so game binaries never embed multi-MB audio.

var musicDir = Path.Combine(AppContext.BaseDirectory, "music");

// A track's pool comes from a "night_" filename prefix, NOT a
// subdirectory: /music/{file} rejects anything path-shaped, so
// subdirectories would change every URL and break already-deployed
// clients' music until they are redeployed. Flat names keep the old
// contract and simply add a field older clients ignore.
app.MapGet("/api/music", () =>
{
    var files = Directory.Exists(musicDir)
        ? Directory.GetFiles(musicDir, "*.ogg").OrderBy(f => f, StringComparer.Ordinal)
        : Enumerable.Empty<string>();
    return Results.Ok(files
        .Select(f => Path.GetFileName(f))
        .Select(name => new MusicTrack(
            Path.GetFileNameWithoutExtension(name),
            $"/music/{name}",
            name.StartsWith("night_", StringComparison.Ordinal) ? "night" : "day"))
        .ToArray());
}).RequireCors(Cors.Client);

app.MapGet("/music/{file}", (string file) =>
{
    // Names come from our own manifest; reject anything path-shaped.
    if (file.Contains("..") || file.Contains('/') || file.Contains('\\') || !file.EndsWith(".ogg", StringComparison.Ordinal))
        return Results.NotFound();
    var path = Path.Combine(musicDir, file);
    return File.Exists(path) ? Results.File(path, "audio/ogg") : Results.NotFound();
}).RequireCors(Cors.Client);

app.Run();
