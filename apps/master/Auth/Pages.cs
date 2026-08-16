using System.Net;
using System.Text;

namespace DigBlock.Master.Auth;

/// <summary>The master's only HTML. Two pages: pick a provider, and say
/// what went wrong.
///
/// Everything else a player sees lives in the game client — there is no
/// account website, deliberately. Choosing a name, reading your account id
/// and signing out are all in-game screens, so the browser's whole job is
/// the provider round trip and then getting out of the way.
///
/// Self-contained by construction: inline styles, no scripts, no fonts, no
/// images. A sign-in page that depends on a CDN is a sign-in page that
/// breaks when the CDN does.</summary>
public static class Pages
{
    public static string Chooser(IReadOnlyList<(string Slug, string Label)> providers, string handle) =>
        Shell(
            "Sign in to DigBlock",
            new StringBuilder()
                .Append("<p>Choose how you want to sign in. DigBlock only ever learns that it was you — not your email, not your name.</p>")
                .Append(string.Concat(providers.Select(p =>
                    $"""<a class="btn" href="/auth/go/{Esc(p.Slug)}?h={Esc(handle)}">Continue with {Esc(p.Label)}</a>""")))
                .Append("""<p class="fine">Then return to the game to pick your display name.</p>""")
                .ToString());

    public static string Error(string message) =>
        Shell("Sign-in failed", $"""
            <p>{Esc(message)}</p>
            <p class="fine">Close this tab and start sign-in again from the game.</p>
            """);

    private static string Esc(string s) => WebUtility.HtmlEncode(s);

    // $$ so that CSS braces are plain content and interpolation is {{...}}:
    // a raw string literal has no brace-doubling escape, it raises the
    // interpolation delimiter instead.
    private static string Shell(string title, string body) => $$"""
        <!doctype html>
        <html lang="en">
        <head>
        <meta charset="utf-8">
        <meta name="viewport" content="width=device-width, initial-scale=1">
        <meta name="robots" content="noindex">
        <title>{{Esc(title)}} — DigBlock</title>
        <style>
        :root { color-scheme: dark; }
        body { margin: 0; min-height: 100vh; display: grid; place-items: center;
               background: #101010; color: #f0f0f0;
               font: 16px/1.5 system-ui, -apple-system, Segoe UI, Roboto, sans-serif; }
        main { width: min(28rem, calc(100vw - 3rem)); padding: 2rem;
               background: #1b1b1b; border: 1px solid #2e2e2e; border-radius: 12px; }
        h1 { margin: 0 0 1rem; font-size: 1.35rem; }
        p { margin: 0 0 1rem; color: #c8c8c8; }
        .btn { display: block; margin: 0 0 .6rem; padding: .8rem 1rem; text-align: center;
               background: #2a2a2a; border: 1px solid #3d3d3d; border-radius: 8px;
               color: #fff; text-decoration: none; }
        .btn:hover { background: #343434; }
        .fine { margin: 1.25rem 0 0; font-size: .875rem; color: #909090; }
        </style>
        </head>
        <body><main><h1>{{Esc(title)}}</h1>{{body}}</main></body>
        </html>
        """;
}
