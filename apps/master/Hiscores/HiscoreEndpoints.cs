using DigBlock.Master.Accounts;
using DigBlock.Master.Auth;

namespace DigBlock.Master.Hiscores;

/// <summary>The board's three faces: the trusted server's write, the JSON reads the game
/// client makes, and the page a browser reads. Reads are public — a name and a level are what
/// every player on a server already sees on a nameplate.</summary>
public static class HiscoreEndpoints
{
    public static void MapHiscoreEndpoints(this WebApplication app)
    {
        // A trusted server raising totals. The key says WHICH server; the
        // trusted list says whether that server's word counts. Refusing with
        // 403 rather than silently dropping is what lets the game server log
        // once and stop, instead of posting a batch every heartbeat forever.
        app.MapPost("/api/servers/{id:guid}/skills", async (
            Guid id, SkillReport req, HttpContext http, ServerRegistry registry,
            TrustedServers trusted, HiscoreService hiscores, CancellationToken ct) =>
        {
            if (!registry.TryGet(id, out var entry))
                return Results.NotFound();
            if (!ServerRegistry.KeyMatches(entry, Bearer.From(http)))
                return Results.Unauthorized();
            if (!trusted.IsTrusted(entry))
                return Results.Json(new ApiError("this server is not trusted to report skills"),
                    statusCode: StatusCodes.Status403Forbidden);
            var rows = req.Players ?? [];
            if (rows.Length > HiscoreService.MaxRowsPerReport)
                return Results.BadRequest(new ApiError($"at most {HiscoreService.MaxRowsPerReport} players per report"));
            foreach (var row in rows)
            {
                foreach (var (skill, xp) in row.Skills ?? [])
                {
                    if (!Skills.IsKnown(skill))
                        return Results.BadRequest(new ApiError($"unknown skill {skill}"));
                    if (xp < 0)
                        return Results.BadRequest(new ApiError("xp must not be negative"));
                }
            }
            var raised = await hiscores.ReportAsync(entry.Id, rows, ct);
            if (raised > 0)
                app.Logger.LogInformation("{Name} raised {Count} skill total(s)", entry.Name, raised);
            return Results.NoContent();
        }).RequireRateLimiting(RateLimits.Report);

        // `size` is the game client's: its page sits in a panel with no
        // scrolling, so it asks for fewer rows than the web page shows.
        // Clamped in the service; the answer says what was cut.
        app.MapGet("/api/hiscores", async (
            string? skill, int? page, int? size, HiscoreService hiscores, CancellationToken ct) =>
        {
            if (!TryBoard(skill, out var board))
                return Results.BadRequest(new ApiError(UnknownBoard));
            return Results.Ok(await hiscores.PageAsync(board, page ?? 1, size ?? HiscoreService.PageSize, ct));
        }).RequireCors(Cors.Client).RequireRateLimiting(RateLimits.Lookup);

        app.MapGet("/api/hiscores/me", async (
            HttpContext http, AccountService accounts, HiscoreService hiscores, CancellationToken ct) =>
        {
            var session = await accounts.AuthenticateAsync(Bearer.From(http), ct);
            if (session is null)
                return Results.Unauthorized();
            return Results.Ok(await hiscores.RanksAsync(session.AccountId, ct));
        }).RequireCors(Cors.Client).RequireRateLimiting(RateLimits.Lookup);

        app.MapGet("/hiscores", async (
            string? skill, int? page, HiscoreService hiscores, CancellationToken ct) =>
        {
            if (!TryBoard(skill, out var board))
                return Results.NotFound();
            var response = await hiscores.PageAsync(board, page ?? 1, HiscoreService.PageSize, ct);
            return Results.Content(HiscorePage.Render(response), "text/html; charset=utf-8");
        }).RequireRateLimiting(RateLimits.Lookup);
    }

    private const string UnknownBoard = "skill must be overall, mining, alchemy or arcana";

    /// <summary>The board a query names: absent or "overall" is every skill
    /// summed, a slug is that skill, anything else is refused.</summary>
    private static bool TryBoard(string? slug, out Skill? board)
    {
        board = null;
        if (string.IsNullOrEmpty(slug) || string.Equals(slug, Skills.Overall, StringComparison.OrdinalIgnoreCase))
            return true;
        if (!Skills.TryParse(slug, out var skill))
            return false;
        board = skill;
        return true;
    }
}
