using DigBlock.Master.Auth;
using DigBlock.Master.Data;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Options;

namespace DigBlock.Master.Accounts;

/// <summary>The JSON half of accounts: redeem a sign-in, keep a session
/// alive, own a display name.
///
/// Every one of these is bearer-authenticated or one-time-code
/// authenticated — never cookies. That separation is load-bearing: the
/// browser pages under <c>/auth</c> use a cookie, these do not, and so the
/// <c>AllowAnyOrigin</c> CORS policy the web client needs stays legal
/// (any-origin plus credentials is not).</summary>
public static class AccountEndpoints
{
    public static void MapAccountEndpoints(this WebApplication app)
    {
        // Redeem the one-time code the browser carried back. Unauthenticated
        // by definition — the code plus the PKCE verifier IS the credential.
        app.MapPost("/api/session/exchange", async (
            ExchangeRequest req, AuthRequestStore store, AccountService accounts,
            MasterDb db, IOptions<MasterOptions> opts, CancellationToken ct) =>
        {
            var redeemed = store.Redeem(req.Code, req.Verifier);
            if (redeemed is null)
                return Results.Unauthorized();
            var account = await db.Accounts.FindAsync([redeemed.AccountId], ct);
            if (account is null || AccountService.IsGloballyBanned(account))
                return Results.Unauthorized();
            var token = await accounts.MintSessionAsync(account, redeemed.Platform, null, ct);
            return Results.Ok(Session(account, token, opts));
        }).RequireCors(Cors.Client).RequireRateLimiting(RateLimits.Exchange);

        // Rotate. The client calls this at startup, which is also how a
        // stolen-and-replayed token gets caught: see AuthenticateAsync.
        app.MapPost("/api/session/refresh", async (
            HttpContext http, RefreshRequest? req, AccountService accounts,
            IOptions<MasterOptions> opts, CancellationToken ct) =>
        {
            var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
            if (session is null)
                return Results.Unauthorized();
            var account = session.Account;
            var token = await accounts.RefreshAsync(session, req?.Platform, ct);
            return Results.Ok(Session(account, token, opts));
        }).RequireCors(Cors.Client);

        app.MapPost("/api/session/signout", async (
            HttpContext http, SignOutRequest? req, AccountService accounts, CancellationToken ct) =>
        {
            var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
            if (session is null)
                return Results.Unauthorized();
            if (req?.AllDevices == true)
                await accounts.RevokeAllAsync(session.AccountId, ct);
            else
                await accounts.RevokeAsync(session, ct);
            return Results.NoContent();
        }).RequireCors(Cors.Client);

        app.MapGet("/api/account/me", async (
            HttpContext http, AccountService accounts, MasterDb db,
            IOptions<MasterOptions> opts, CancellationToken ct) =>
        {
            var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
            if (session is null)
                return Results.Unauthorized();
            var providers = await db.ProviderLinks
                .Where(p => p.AccountId == session.AccountId)
                .Select(p => p.Provider)
                .ToArrayAsync(ct);
            var account = session.Account;
            var cooldown = account.DisplayNameChangedUtc?
                .AddDays(opts.Value.Account.NameChangeCooldownDays);
            return Results.Ok(new AccountResponse(
                account.Id,
                account.DisplayName,
                [.. providers.Select(p => p.ToString().ToLowerInvariant())],
                cooldown > DateTimeOffset.UtcNow ? cooldown : null));
        }).RequireCors(Cors.Client);

        app.MapPost("/api/account/name", async (
            HttpContext http, NameRequest req, AccountService accounts, CancellationToken ct) =>
        {
            var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
            if (session is null)
                return Results.Unauthorized();
            var name = req.DisplayName?.Trim() ?? "";
            var (result, retry) = await accounts.SetNameAsync(session.Account, name, ct);
            return result switch
            {
                NameResult.Ok => Results.Ok(new NameSetResponse(session.Account.DisplayName!)),
                NameResult.Invalid => Results.BadRequest(new ApiError(Names.Requirement)),
                NameResult.Taken => Results.Conflict(new ApiError("that name is taken")),
                _ => Results.Json(
                    new ApiError($"you can change your name again in {Days(retry)}"),
                    statusCode: StatusCodes.Status429TooManyRequests),
            };
        }).RequireCors(Cors.Client);

        // Live feedback while the player types. Answers for the caller when
        // a session is presented, so re-picking your own name reads as
        // available rather than as a collision with yourself.
        app.MapGet("/api/accounts/name-available", async (
            HttpContext http, string? name, AccountService accounts, CancellationToken ct) =>
        {
            if (!Names.IsValid(name))
                return Results.Ok(new NameAvailableResponse(name ?? "", false, Names.Requirement));
            var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
            var free = await accounts.IsNameAvailableAsync(name!, session?.AccountId, ct);
            return Results.Ok(new NameAvailableResponse(name!, free, free ? null : "that name is taken"));
        }).RequireCors(Cors.Client).RequireRateLimiting(RateLimits.Lookup);

        // Name → account id. Public on purpose: it is how a server operator
        // turns "put me in as admin" into a DIGBLOCK_ADMIN_IDS entry, and
        // how a game server resolves --admin <name> once at boot. Names and
        // ids are both public, so this reveals nothing a join would not.
        app.MapGet("/api/accounts/by-name/{name}", async (
            string name, AccountService accounts, CancellationToken ct) =>
        {
            if (!Names.IsValid(name))
                return Results.BadRequest(new ApiError(Names.Requirement));
            var account = await accounts.FindByNameAsync(name, ct);
            return account?.DisplayName is null
                ? Results.NotFound()
                : Results.Ok(new AccountLookupResponse(account.Id, account.DisplayName));
        }).RequireCors(Cors.Client).RequireRateLimiting(RateLimits.Lookup);
    }

    private static SessionResponse Session(Account account, string token, IOptions<MasterOptions> opts) =>
        new(account.Id, account.DisplayName, token,
            (int)TimeSpan.FromDays(opts.Value.Account.SessionDays).TotalSeconds);

    private static string Days(DateTimeOffset? until)
    {
        var left = (until ?? DateTimeOffset.UtcNow) - DateTimeOffset.UtcNow;
        var days = (int)Math.Ceiling(left.TotalDays);
        return days <= 1 ? "a day" : $"{days} days";
    }
}
