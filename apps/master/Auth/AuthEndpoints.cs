using System.Security.Claims;
using DigBlock.Master.Accounts;
using DigBlock.Master.Data;
using Microsoft.AspNetCore.Authentication;
using Microsoft.AspNetCore.Authentication.MicrosoftAccount;
using Microsoft.Extensions.Options;

namespace DigBlock.Master.Auth;

/// <summary>The browser half of sign-in. The master is the authorization
/// server as far as the game is concerned, and the client never sees a provider
/// token: it opens a browser at <c>/auth/start</c> with a redirect target, a
/// state and a PKCE challenge, and redeems the one-time code bounced back there
/// for a session. Adding a provider is a change here and nowhere else.</summary>
public static class AuthEndpoints
{
    /// <summary>The cookie scheme that carries the provider identity across
    /// the callback. Deliberately not a login session: it lives for minutes,
    /// exists only between <c>/auth/go</c> and <c>/auth/complete</c>, and is
    /// signed out the moment it has been read. The master has no logged-in
    /// browser state at all — the account screens are in the game.</summary>
    public const string ExternalScheme = "External";

    private const string HandleItem = "digblock.handle";
    private const string ProviderItem = "digblock.provider";

    public static void AddDigBlockAuth(this IServiceCollection services, MasterOptions opts)
    {
        var auth = services.AddAuthentication(ExternalScheme);
        auth.AddCookie(ExternalScheme, o =>
        {
            o.Cookie.Name = "digblock.external";
            o.Cookie.HttpOnly = true;
            // Lax, not Strict: the provider bounces the browser back with a
            // top-level GET, which Strict would strip the cookie from.
            o.Cookie.SameSite = SameSiteMode.Lax;
            // SameAsRequest, not Always: ingress terminates TLS, so the app
            // sees http. UseForwardedHeaders restores the real scheme, which
            // is what makes this resolve to Secure in production and to
            // plain http for a local `just master`.
            o.Cookie.SecurePolicy = CookieSecurePolicy.SameAsRequest;
            o.ExpireTimeSpan = AuthRequestStore.PendingTtl;
            o.SlidingExpiration = false;
        });

        // A provider with no credentials configured is simply absent — no
        // button, no callback route. See ProviderOptions.Enabled.
        if (opts.Auth.Google.Enabled)
        {
            auth.AddGoogle(o =>
            {
                o.ClientId = opts.Auth.Google.ClientId!;
                o.ClientSecret = opts.Auth.Google.ClientSecret!;
                o.SignInScheme = ExternalScheme;
                o.UsePkce = true;
                // The subject is the entire payload we want. Dropping the
                // default profile/email scopes means the consent screen asks
                // for less AND there is no PII to accidentally persist.
                o.Scope.Clear();
                o.Scope.Add("openid");
                o.SaveTokens = false;
                FailWithAPage(o.Events);
            });
        }
        if (opts.Auth.Facebook.Enabled)
        {
            auth.AddFacebook(o =>
            {
                o.AppId = opts.Auth.Facebook.ClientId!;
                o.AppSecret = opts.Auth.Facebook.ClientSecret!;
                o.SignInScheme = ExternalScheme;
                o.UsePkce = true;
                o.Scope.Clear();
                o.Scope.Add("public_profile");
                o.Fields.Clear();
                o.Fields.Add("id");
                o.SaveTokens = false;
                FailWithAPage(o.Events);
            });
        }
        if (opts.Auth.Microsoft.Enabled)
        {
            auth.AddMicrosoftAccount(o =>
            {
                o.ClientId = opts.Auth.Microsoft.ClientId!;
                o.ClientSecret = opts.Auth.Microsoft.ClientSecret!;
                o.SignInScheme = ExternalScheme;
                o.UsePkce = true;
                // The scope is left at the handler's default — the single
                // User.Read delegated permission — and this is the one
                // provider where that cannot be trimmed to openid the way
                // Google's is: the handler resolves the subject by calling
                // Graph, and Graph refuses a token without it, so dropping
                // the scope breaks sign-in rather than narrowing it.
                //
                // Narrow the RESPONSE instead. $select is Graph's spelling
                // of Facebook's Fields above, and with it the id is the only
                // field that ever crosses the wire; the handler's stock
                // name/email claim actions then find nothing to map, so no
                // PII reaches the principal, let alone the database.
                o.UserInformationEndpoint = MicrosoftAccountDefaults.UserInformationEndpoint + "?$select=id";
                o.SaveTokens = false;
                FailWithAPage(o.Events);
            });
        }
    }

    /// <summary>A remote sign-in can fail without reaching <c>/auth/complete</c>:
    /// the player cancels at the provider's consent screen, or the back button
    /// replays a callback whose correlation cookie is already spent. The
    /// handler's default is to throw — a stack page in Development, a bare 500
    /// in production — so both events answer with this flow's error page.</summary>
    private static void FailWithAPage(RemoteAuthenticationEvents events)
    {
        events.OnAccessDenied = ctx =>
        {
            ctx.HandleResponse();
            return WriteErrorPage(ctx.Response,
                "Sign-in was cancelled. Close this tab and start again from the game.");
        };
        events.OnRemoteFailure = ctx =>
        {
            ctx.HandleResponse();
            return WriteErrorPage(ctx.Response,
                "That sign-in is no longer valid. Close this tab and start again from the game.");
        };
    }

    private static Task WriteErrorPage(HttpResponse response, string message)
    {
        response.StatusCode = 400;
        response.ContentType = "text/html; charset=utf-8";
        return response.WriteAsync(Pages.Error(message));
    }

    public static void MapAuthEndpoints(this WebApplication app)
    {
        // Step 1 — the client's entry point. Validates everything that will
        // matter later, so a misconfigured client fails here, visibly, and
        // not two redirects downstream where the only clue is a blank page.
        app.MapGet("/auth/start", (
            HttpContext http, AuthRequestStore store, IOptions<MasterOptions> opts,
            string? redirect_uri, string? state, string? code_challenge,
            string? platform, string? provider) =>
        {
            if (!RedirectUris.IsAllowed(redirect_uri, opts.Value.Auth.WebRedirectUris, out var target))
                return Page(Pages.Error("That sign-in came from an address DigBlock does not recognise."), 400);
            if (string.IsNullOrEmpty(state) || state.Length > 128)
                return Page(Pages.Error("That sign-in request was malformed."), 400);
            if (!AuthRequestStore.IsWellFormedChallenge(code_challenge))
                return Page(Pages.Error("That sign-in request was malformed."), 400);

            var enabled = EnabledProviders(opts.Value);
            if (enabled.Count == 0)
                return Page(Pages.Error("Sign-in is not configured on this server."), 503);

            var handle = store.StartPending(target!.ToString(), state, code_challenge!, platform);

            // A client may name a provider to skip the chooser; anything
            // else (including a provider that is off) falls through to it.
            var direct = enabled.FirstOrDefault(p => p.Slug == provider?.ToLowerInvariant());
            if (direct.Slug is not null)
                return Results.Redirect($"/auth/go/{direct.Slug}?h={Uri.EscapeDataString(handle)}");
            return Page(Pages.Chooser([.. enabled.Select(p => (p.Slug, p.Label))], handle));
        }).RequireRateLimiting(RateLimits.Auth);

        // Step 2 — hand the browser to the provider. The pending handle
        // rides AuthenticationProperties.Items, which round-trips through
        // the correlation cookie, so the callback knows which sign-in this
        // is without putting anything guessable in a URL.
        app.MapGet("/auth/go/{provider}", (
            string provider, string? h, IOptions<MasterOptions> opts) =>
        {
            var match = EnabledProviders(opts.Value).FirstOrDefault(p => p.Slug == provider.ToLowerInvariant());
            if (match.Slug is null || string.IsNullOrEmpty(h))
                return Page(Pages.Error("That sign-in link is no longer valid."), 400);

            var props = new AuthenticationProperties { RedirectUri = "/auth/complete" };
            props.Items[HandleItem] = h;
            props.Items[ProviderItem] = match.Slug;
            return Results.Challenge(props, [match.Scheme]);
        });

        // Step 3 — the provider is done. Turn its subject into an account,
        // mint the one-time code, and bounce back to whoever started this.
        app.MapGet("/auth/complete", async (
            HttpContext http, AuthRequestStore store, AccountService accounts,
            ILoggerFactory logs, CancellationToken ct) =>
        {
            var log = logs.CreateLogger("DigBlock.Master.Auth");
            var result = await http.AuthenticateAsync(ExternalScheme);
            // Read the cookie once, then drop it. There is no reason for a
            // browser to keep carrying a provider identity around after this.
            await http.SignOutAsync(ExternalScheme);

            if (!result.Succeeded || result.Principal is null || result.Properties is null)
                return Page(Pages.Error("Sign-in did not complete."), 400);

            result.Properties.Items.TryGetValue(HandleItem, out var handle);
            var pending = store.ConsumePending(handle);
            if (pending is null)
                return Page(Pages.Error("That sign-in took too long. Start again from the game."), 400);

            result.Properties.Items.TryGetValue(ProviderItem, out var slug);
            var subject = result.Principal.FindFirstValue(ClaimTypes.NameIdentifier);
            if (slug is null || string.IsNullOrEmpty(subject) || !TryProvider(slug, out var provider))
                return Page(Pages.Error("Sign-in did not complete."), 400);

            var account = await accounts.FindOrCreateAsync(provider, subject, ct);
            if (AccountService.IsGloballyBanned(account))
            {
                log.LogInformation("refused sign-in for banned account {AccountId}", account.Id);
                return Page(Pages.Error("This account is suspended."), 403);
            }

            var code = store.IssueCode(account.Id, pending.ChallengeS256, pending.Platform);
            var back = new UriBuilder(pending.RedirectUri)
            {
                Query = $"code={Uri.EscapeDataString(code)}&state={Uri.EscapeDataString(pending.ClientState)}",
            };
            return Results.Redirect(back.Uri.ToString());
        });
    }

    private static IResult Page(string html, int status = 200) =>
        Results.Content(html, "text/html; charset=utf-8", statusCode: status);

    private static bool TryProvider(string slug, out AuthProvider provider)
    {
        switch (slug)
        {
            case "google":
                provider = AuthProvider.Google;
                return true;
            case "facebook":
                provider = AuthProvider.Facebook;
                return true;
            case "microsoft":
                provider = AuthProvider.Microsoft;
                return true;
            default:
                provider = default;
                return false;
        }
    }

    private static List<(string Slug, string Label, string Scheme)> EnabledProviders(MasterOptions opts)
    {
        var list = new List<(string, string, string)>();
        if (opts.Auth.Google.Enabled)
            list.Add(("google", "Google", "Google"));
        if (opts.Auth.Facebook.Enabled)
            list.Add(("facebook", "Facebook", "Facebook"));
        if (opts.Auth.Microsoft.Enabled)
            list.Add(("microsoft", "Microsoft", "Microsoft"));
        return list;
    }
}
