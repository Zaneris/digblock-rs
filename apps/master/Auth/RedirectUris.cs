using System.Net;

namespace DigBlock.Master.Auth;

/// <summary>Decides where a finished sign-in is allowed to bounce back to.
///
/// The single most abusable input in the flow: whatever passes here receives a
/// one-time code in its query string. Two shapes are allowed and nothing else.
///
/// <b>Loopback</b> (RFC 8252 §7.3) — a native client binds an ephemeral port,
/// so the port cannot be registered in advance and is explicitly ignored when
/// matching. Only literal <c>127.0.0.1</c> and <c>[::1]</c>; <c>localhost</c>
/// is refused, both because the RFC prefers literals and because it resolves
/// through the host's name service, which is not ours to trust.
///
/// <b>Configured web origins</b> — exact match.
///
/// A leaked code is still useless without the PKCE verifier, so this is
/// defence in depth — but it is what keeps the master from being an open
/// redirector.</summary>
public static class RedirectUris
{
    /// <summary>The browser client's own origins, allowed without any
    /// configuration.
    ///
    /// Hardcoded rather than configured because they belong to this project.
    /// Config is the wrong tool for a constant: it can be forgotten on a
    /// deploy, and when it is, sign-in fails on the web only, with a message
    /// about an unrecognised address and nothing in the logs to connect it to a
    /// missing environment variable.
    ///
    /// Naming our OWN origins costs nothing — what this list exists to exclude
    /// is origins we do not control. <c>Master:Auth:WebRedirectUris</c> still
    /// adds to it. Desktop needs no equivalent: a native client's loopback port
    /// is ephemeral, so those are matched by shape below.</summary>
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

        // Matched on ORIGIN — scheme, host and port — not on the path.
        // A path is not a trust boundary: the page redeeming the code is
        // the same page either way, and same-origin is the boundary the
        // whole browser security model is drawn on. Matching paths too
        // would only mean sign-in working from `/` and mysteriously not
        // from `/index.html`.
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
