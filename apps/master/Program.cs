using System.Net;
using System.Net.Quic;
using DigBlock.Master;
using Microsoft.Extensions.Options;

var builder = WebApplication.CreateBuilder(args);

// Local-dev default only. Never put "Urls" in appsettings.json — it would
// override ASPNETCORE_URLS, which the container relies on to bind 0.0.0.0.
if (string.IsNullOrEmpty(builder.Configuration["urls"]))
    builder.WebHost.UseUrls("http://127.0.0.1:5200");

builder.Services.Configure<MasterOptions>(builder.Configuration.GetSection("Master"));
builder.Services.AddSingleton<ServerRegistry>();
builder.Services.AddSingleton<TokenStore>();
builder.Services.AddHostedService<HealthChecker>();

// Browser clients (the wasm build) call the two client endpoints cross-
// origin; both are public and read-only-or-token-minting, so any origin
// is fine. Server endpoints are bearer-authenticated server-to-server
// calls and stay CORS-less (browsers never make them).
builder.Services.AddCors(o => o.AddPolicy("client", p =>
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
    new QuicProbe(TimeSpan.FromSeconds(
        sp.GetRequiredService<IOptions<MasterOptions>>().Value.Probe.TimeoutSeconds)));

var app = builder.Build();
app.UseCors();

// ---- game server endpoints -------------------------------------------------

app.MapPost("/api/servers/register", async (
    RegisterRequest req, HttpContext http, ServerRegistry registry,
    IReachabilityProbe probe, IOptions<MasterOptions> opts, CancellationToken ct) =>
{
    var name = req.Name?.Trim();
    if (string.IsNullOrEmpty(name) || name.Length > 64)
        return Results.BadRequest(new { error = "name must be 1-64 characters" });
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

    var host = req.Host?.Trim();
    if (string.IsNullOrEmpty(host))
    {
        var remote = http.Connection.RemoteIpAddress;
        if (remote is null)
            return Results.BadRequest(new { error = "no host given and no observable remote address" });
        if (remote.IsIPv4MappedToIPv6)
            remote = remote.MapToIPv4();
        host = remote.ToString();
    }

    var probeHost = req.ProbeHost?.Trim();
    if (string.IsNullOrEmpty(probeHost))
        probeHost = host;

    var result = await probe.ProbeAsync(probeHost, req.Port, certHash, ct);
    if (!result.Reachable)
        return Results.UnprocessableEntity(new RegisterFailed(false, result.ReasonString, result.Detail ?? ""));

    var entry = registry.Register(name, host, probeHost, req.Port, certHash, req.Players, req.MaxPlayers);
    app.Logger.LogInformation("registered {Name} at {Host}:{Port} as {Id}", name, host, req.Port, entry.Id);
    return Results.Ok(new RegisterOk(true, entry.Id, entry.Key, host));
});

app.MapPost("/api/servers/{id:guid}/heartbeat", (
    Guid id, HeartbeatRequest req, HttpContext http, ServerRegistry registry) =>
{
    // Unknown id → 404: tells a game server the master restarted (or culled
    // it) and it should re-register.
    if (!registry.TryGet(id, out var entry))
        return Results.NotFound();
    if (!ServerRegistry.KeyMatches(entry, BearerToken(http)))
        return Results.Unauthorized();
    entry.Players = req.Players;
    entry.MaxPlayers = req.MaxPlayers;
    entry.Strikes = 0;
    entry.LastSeenUtc = DateTimeOffset.UtcNow;
    return Results.NoContent();
});

app.MapDelete("/api/servers/{id:guid}", (Guid id, HttpContext http, ServerRegistry registry) =>
{
    // Graceful shutdown notify. Idempotent: an already-gone id is success.
    if (!registry.TryGet(id, out var entry))
        return Results.NoContent();
    if (!ServerRegistry.KeyMatches(entry, BearerToken(http)))
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
    var entry = registry.FindByKey(BearerToken(http));
    if (entry is null)
        return Results.Unauthorized();
    if (string.IsNullOrEmpty(req.Token) || !tokens.TryConsume(req.Token, entry.Id, out var displayName))
        return Results.NotFound();
    return Results.Ok(new VerifyResponse(displayName));
});

// ---- game client endpoints -------------------------------------------------

app.MapGet("/api/servers", (ServerRegistry registry) =>
    new ServerListResponse([.. registry.Snapshot()
        .OrderBy(e => e.Name, StringComparer.OrdinalIgnoreCase)
        .Select(e => new ServerSummary(e.Id, e.Name, e.Url, e.CertHashHex, e.Players, e.MaxPlayers))]))
    .RequireCors("client");

app.MapPost("/api/join", (JoinRequest req, ServerRegistry registry, TokenStore tokens, IOptions<MasterOptions> opts) =>
{
    var displayName = req.DisplayName?.Trim();
    if (string.IsNullOrEmpty(displayName) || displayName.Length > 32)
        return Results.BadRequest(new { error = "displayName must be 1-32 characters" });
    if (!registry.TryGet(req.ServerId, out var entry))
        return Results.NotFound();
    var ttl = TimeSpan.FromSeconds(opts.Value.Token.TtlSeconds);
    var token = tokens.Mint(entry.Id, displayName, ttl);
    return Results.Ok(new JoinResponse(token, (int)ttl.TotalSeconds, entry.Url, entry.CertHashHex));
}).RequireCors("client");

app.Run();

static string? BearerToken(HttpContext http)
{
    string? header = http.Request.Headers.Authorization;
    const string prefix = "Bearer ";
    return header?.StartsWith(prefix, StringComparison.Ordinal) == true ? header[prefix.Length..] : null;
}
