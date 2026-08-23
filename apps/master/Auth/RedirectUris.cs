using System.Net;

namespace DigBlock.Master.Auth;

/// <summary>Decides where a finished sign-in may bounce back to. Whatever passes here receives a
/// one-time code, so this is what keeps the master from being an open redirector. Two shapes and
/// nothing else: a loopback literal (RFC 8252 §7.3), whose ephemeral port cannot be registered in
/// advance and is ignored when matching — never <c>localhost</c>, which resolves through the
/// host's name service; and a configured web origin, matched exactly.</summary>
public static class RedirectUris
{
    /// <summary>The browser client's own origins, allowed without configuration — hardcoded
    /// because config is the wrong tool for a constant: forgotten on a deploy it fails sign-in on
    /// the web alone, with nothing in the logs naming the missing variable.
    /// <c>Master:Auth:WebRedirectUris</c> still adds to it; desktop needs no equivalent, its
    /// ephemeral loopback port being matched by shape below.</summary>
    private static readonly string[] Owned =
    [
        "https://digblock.net/",
        // The apex is the real address; www and the Cloudflare Pages
        // default are listed so a redirect works before, during, and after
        // a custom-domain change. All three are ours either way.
        "https://www.digblock.net/",
        "https://digblock.pages.dev/",
    ];

    public static bool IsAllowed(string? raw, IReadOnlyList<string> configured, out Uri? uri)
    {
        uri = null;
        if (string.IsNullOrWhiteSpace(raw) || raw.Length > 512)
            return false;
        if (!Uri.TryCreate(raw, UriKind.Absolute, out var parsed))
            return false;
        // We append ?code=&state= ourselves; a target that already carries a
        // query or a fragment is either a mistake or an attempt to smuggle
        // one past the match.
        if (!string.IsNullOrEmpty(parsed.Query) || !string.IsNullOrEmpty(parsed.Fragment))
            return false;

        if (IsLoopback(parsed))
        {
            uri = parsed;
            return true;
        }

        // Matched on ORIGIN — scheme, host and port — not on the path. A path is not a trust
        // boundary: the page redeeming the code is the same either way, and same-origin is the
        // boundary the whole browser security model is drawn on. Matching paths too would only
        // mean sign-in working from `/` and mysteriously not from `/index.html`.
        foreach (var allowed in configured.Concat(Owned))
        {
            if (Uri.TryCreate(allowed, UriKind.Absolute, out var known)
                && Uri.Compare(parsed, known, UriComponents.SchemeAndServer,
                    UriFormat.Unescaped, StringComparison.OrdinalIgnoreCase) == 0)
            {
                uri = parsed;
                return true;
            }
        }
        return false;
    }

    /// <summary>Plain HTTP on a loopback literal, any port. TLS is neither
    /// available nor needed for a socket that never leaves the machine.</summary>
    private static bool IsLoopback(Uri uri)
    {
        if (uri.Scheme != Uri.UriSchemeHttp)
            return false;
        if (uri.HostNameType is not (UriHostNameType.IPv4 or UriHostNameType.IPv6))
            return false;
        return IPAddress.TryParse(uri.Host, out var ip) && IPAddress.IsLoopback(ip);
    }
}
