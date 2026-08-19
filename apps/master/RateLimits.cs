using System.Threading.RateLimiting;
using Microsoft.AspNetCore.RateLimiting;

namespace DigBlock.Master;

/// <summary>Per-endpoint rate limits.
///
/// The master had none at all, which is half of why record eviction and
/// token flooding were open: tokens were free at any
/// rate. Accounts fixed the identity half; these cap the volume half.
///
/// Deliberately no global limiter. Every caller here sits behind something
/// that aggregates addresses — a NAT'd household, a container host running
/// several game servers, the ingress itself — so one bucket for everything
/// would throttle honest traffic long before it inconvenienced anyone. The
/// limits are per endpoint and partitioned by whatever identifies the actual
/// abuser of THAT endpoint.
///
/// No package: AddRateLimiter ships in the ASP.NET Core shared framework.</summary>
public static class RateLimits
{
    /// <summary>Starting a sign-in. Per source address; a human does this
    /// once.</summary>
    public const string Auth = "auth";

    /// <summary>Redeeming a sign-in code. Per source address. Tight because
    /// a code is one-time and burned on a wrong verifier, so retrying in
    /// volume is only ever an attack.</summary>
    public const string Exchange = "exchange";

    /// <summary>Minting join tokens. Partitioned by the SESSION, falling
    /// back to the address — the flood this stops is one account cycling
    /// joins to churn a game server's persisted records, and a session is
    /// the closest thing the limiter can see to an account without a
    /// database read.</summary>
    public const string Join = "join";

    /// <summary>A game server verifying tokens. Per server key, and
    /// generous: this is on the join path of a busy server, so it must sit
    /// far above real traffic and only catch a runaway.</summary>
    public const string Verify = "verify";

    /// <summary>Registering. Per source address. Each attempt costs the
    /// master an outbound probe, which is the resource being protected.</summary>
    public const string Register = "register";

    /// <summary>Name availability and name → id lookups. Per source
    /// address, with headroom because the client checks while the player
    /// types (debounced).</summary>
    public const string Lookup = "lookup";

    public static void AddDigBlockRateLimits(this IServiceCollection services)
    {
        services.AddRateLimiter(o =>
        {
            o.RejectionStatusCode = StatusCodes.Status429TooManyRequests;

            Fixed(o, Auth, 20, TimeSpan.FromMinutes(5), ByAddress);
            Fixed(o, Exchange, 30, TimeSpan.FromMinutes(5), ByAddress);
            Fixed(o, Join, 30, TimeSpan.FromMinutes(1), BySessionOrAddress);
            Fixed(o, Verify, 300, TimeSpan.FromMinutes(1), ByBearer);
            Fixed(o, Register, 60, TimeSpan.FromMinutes(5), ByAddress);
            Fixed(o, Lookup, 120, TimeSpan.FromMinutes(1), ByAddress);
        });
    }

    private static void Fixed(
        RateLimiterOptions o, string name, int permits, TimeSpan window, Func<HttpContext, string> partition) =>
        o.AddPolicy(name, http => RateLimitPartition.GetFixedWindowLimiter(
            $"{name}:{partition(http)}",
            _ => new FixedWindowRateLimiterOptions { PermitLimit = permits, Window = window }));

    // RemoteIpAddress is the real caller: X-Forwarded-For is only applied
    // when the peer is itself a trusted proxy network (TrustedProxies) and
    // only one hop deep, so a caller cannot pick its own partition by
    // sending the header.
    private static string ByAddress(HttpContext http) =>
        http.Connection.RemoteIpAddress?.ToString() ?? "unknown";

    /// <summary>Partition on a hash of the bearer credential. Hashed so the
    /// partition key — which ends up in logs and metrics — is never the
    /// secret itself.</summary>
    private static string ByBearer(HttpContext http)
    {
        var bearer = Bearer.From(http);
        return bearer is null
            ? ByAddress(http)
            : Convert.ToHexStringLower(Secrets.Hash(bearer))[..16];
    }

    private static string BySessionOrAddress(HttpContext http) =>
        Bearer.From(http) is null ? ByAddress(http) : ByBearer(http);
}
