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
/// Self-contained by construction: inline styles, no scripts, no fonts,
/// nothing fetched — the provider logomarks are inline SVG. A sign-in page
/// that depends on a CDN is a sign-in page that breaks when the CDN
/// does.</summary>
public static class Pages
{
    public static string Chooser(IReadOnlyList<(string Slug, string Label)> providers, string handle) =>
        Shell(
            "Sign in to DigBlock",
            new StringBuilder()
                .Append("<p>Choose how you want to sign in. DigBlock only ever learns that it was you — not your email, not your name.</p>")
                .Append(string.Concat(providers.Select(p => ProviderButton(p.Slug, p.Label, handle))))
                .Append("""<p class="fine">Then return to the game to pick your display name.</p>""")
                .ToString());

    public static string Error(string message) =>
        Shell("Sign-in failed", $"""
            <p>{Esc(message)}</p>
            <p class="fine">Close this tab and start sign-in again from the game.</p>
            """);

    // A provider's sign-in button is trade dress the provider dictates, not
    // ours to style: Google's branding guidelines are a stated requirement of
    // OAuth app verification, and Facebook's and Microsoft's brand kits make
    // the same shape of demand. So the known providers get their own markup and
    // only a provider without a kit falls back to the house button.
    //
    // The WORDING is per provider for the same reason: Microsoft's guidelines
    // approve "Sign in with Microsoft" and "Sign up with Microsoft" and nothing
    // else, so the row that reads "Continue with" everywhere else cannot read
    // that way here.
    private static string ProviderButton(string slug, string label, string handle)
    {
        var href = $"/auth/go/{Esc(slug)}?h={Esc(handle)}";
        return slug switch
        {
            "google" => $"""<a class="btn google" href="{href}">{GoogleG}<span>Continue with Google</span></a>""",
            "facebook" => $"""<a class="btn facebook" href="{href}">{FacebookF}<span>Continue with Facebook</span></a>""",
            "microsoft" => $"""<a class="btn microsoft" href="{href}">{MicrosoftSquares}<span>Sign in with Microsoft</span></a>""",
            _ => $"""<a class="btn" href="{href}">Continue with {Esc(label)}</a>""",
        };
    }

    // The multi-color G from Google's downloadable button assets, colors
    // and proportions as shipped — both are non-negotiable under the
    // branding guidelines.
    private const string GoogleG = """
        <svg viewBox="0 0 48 48" aria-hidden="true"><path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z"/><path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z"/><path fill="#FBBC05" d="M10.53 28.59c-.48-1.45-.76-2.99-.76-4.59s.27-3.14.76-4.59l-7.98-6.19C.92 16.46 0 20.12 0 24c0 3.88.92 7.54 2.56 10.78l7.97-6.19z"/><path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.15 1.45-4.92 2.3-8.16 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z"/></svg>
        """;

    // The f logomark from Facebook's brand kit: a white disc with the f cut
    // out, so the button's own blue shows through the letterform.
    private const string FacebookF = """
        <svg viewBox="0 0 24 24" aria-hidden="true"><path fill="#fff" d="M24 12.07C24 5.41 18.63 0 12 0S0 5.4 0 12.07C0 18.1 4.39 23.09 10.13 24v-8.44H7.08v-3.49h3.04V9.41c0-3.02 1.8-4.7 4.54-4.7 1.31 0 2.68.24 2.68.24v2.97h-1.5c-1.5 0-1.96.93-1.96 1.89v2.26h3.32l-.53 3.5h-2.8V24C19.62 23.1 24 18.1 24 12.07z"/></svg>
        """;

    // The four squares from Microsoft's brand kit, in the kit's own colors
    // and its 23-unit grid — the mark is 21 units with a unit of clear
    // space around it, which is why it reads a shade smaller than the G
    // beside it at the same box size. That clear space is part of the mark,
    // not padding to be reclaimed, and the squares may not be recolored,
    // outlined, or reordered.
    private const string MicrosoftSquares = """
        <svg viewBox="0 0 23 23" aria-hidden="true"><path fill="#F25022" d="M1 1h10v10H1z"/><path fill="#7FBA00" d="M12 1h10v10H12z"/><path fill="#00A4EF" d="M1 12h10v10H1z"/><path fill="#FFB900" d="M12 12h10v10H12z"/></svg>
        """;

    private static string Esc(string s) => WebUtility.HtmlEncode(s);

    // $$ so that CSS braces are plain content and interpolation is {{...}}: a
    // raw string literal has no brace-doubling escape, it raises the
    // interpolation delimiter instead.
    //
    // The .google, .facebook and .microsoft metrics and palettes are copied
    // from those providers' own brand kits. Microsoft's is the most
    // prescriptive — its own height, square corners, Segoe UI at its own size —
    // so the three buttons do NOT agree on height or radius, and that is the
    // correct outcome rather than a layout bug to tidy away.
    //
    // Two details are ours, because the kit specifies neither. The border is
    // the kit's own, which it draws on the LIGHT button: the dark one is drawn
    // for a light page and would sit here as a near-invisible rectangle. The
    // hover is a plain lift of the fill, matching the other two rows.
    //
    // Roboto and Segoe UI are the faces those specs name — first in their
    // stacks so they are used where installed, but never fetched, because
    // self-contained outranks pixel-perfect here.
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
        .btn { display: flex; align-items: center; justify-content: center; gap: 10px;
               height: 40px; margin: 0 0 .6rem; padding: 0 12px;
               background: #2a2a2a; border: 1px solid #3d3d3d; border-radius: 8px;
               color: #fff; text-decoration: none; }
        .btn:hover { background: #343434; }
        .btn svg { width: 20px; height: 20px; flex: none; }
        .btn.google { background: #131314; border-color: #8E918F; border-radius: 4px;
                      color: #E3E3E3;
                      font: 500 14px/20px Roboto, system-ui, -apple-system, Segoe UI, sans-serif;
                      letter-spacing: .25px; }
        .btn.google:hover { background: #1f1f20; }
        .btn.facebook { background: #1877F2; border-color: #1877F2; border-radius: 4px;
                        color: #fff;
                        font: 500 14px/20px Roboto, system-ui, -apple-system, Segoe UI, sans-serif;
                        letter-spacing: .25px; }
        .btn.facebook:hover { background: #166FE5; }
        .btn.microsoft { height: 41px; background: #2F2F2F; border-color: #8C8C8C; border-radius: 0;
                         color: #FFFFFF;
                         font: 400 15px/20px "Segoe UI", system-ui, -apple-system, Roboto, sans-serif;
                         gap: 12px; }
        .btn.microsoft:hover { background: #3B3B3B; }
        .btn.microsoft svg { width: 21px; height: 21px; }
        .fine { margin: 1.25rem 0 0; font-size: .875rem; color: #909090; }
        </style>
        </head>
        <body><main><h1>{{Esc(title)}}</h1>{{body}}</main></body>
        </html>
        """;
}
