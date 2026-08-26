using DigBlock.Master.Data;
using Microsoft.EntityFrameworkCore;

namespace DigBlock.Master.Hiscores;

/// <summary>One account's standing on one board, before it is given a rank.</summary>
public sealed record Standing(Guid AccountId, string DisplayName, int Level, long Xp);

/// <summary>Reads and writes of the board. Every board is computed from the same in-memory
/// pass over the totals: a few thousand accounts times a handful of skills is small enough
/// that one query plus a sort beats keeping a denormalized overall row in step with three
/// per-skill ones. Revisit with a cache when a request here shows up in the master's logs.</summary>
public sealed class HiscoreService(MasterDb db)
{
    /// <summary>The longest page cut, and the web page's. A client may ask for
    /// fewer rows (<c>size=</c>), never more.</summary>
    public const int PageSize = 25;

    /// <summary>The largest batch one report may carry. A trusted server
    /// drains its outbox in slices of this many; the cap bounds one request's
    /// write, not the total.</summary>
    public const int MaxRowsPerReport = 500;

    /// <summary>Raise totals. Monotone per (account, skill): a lower figure is ignored rather
    /// than written, which is what stops a server holding an older copy of a character from
    /// rewinding the one that moved on. Unknown accounts are skipped, not created — the board
    /// names only players the master minted.</summary>
    public async Task<int> ReportAsync(Guid serverId, IReadOnlyList<SkillReportRow> rows, CancellationToken ct)
    {
        var ids = rows.Select(r => r.AccountId).Distinct().ToArray();
        var known = await db.Accounts
            .Where(a => ids.Contains(a.Id))
            .Select(a => a.Id)
            .ToHashSetAsync(ct);
        var existing = await db.SkillTotals
            .Where(t => ids.Contains(t.AccountId))
            .ToDictionaryAsync(t => (t.AccountId, t.Skill), ct);

        var now = DateTimeOffset.UtcNow;
        var raised = 0;
        foreach (var row in rows)
        {
            if (!known.Contains(row.AccountId))
                continue;
            foreach (var (skill, xp) in row.Skills)
            {
                if (existing.TryGetValue((row.AccountId, skill), out var total))
                {
                    if (xp <= total.Xp)
                        continue;
                    total.Xp = xp;
                    total.ServerId = serverId;
                    total.UpdatedUtc = now;
                }
                else
                {
                    if (xp <= 0)
                        continue;
                    total = new SkillTotal
                    {
                        AccountId = row.AccountId,
                        Skill = skill,
                        Xp = xp,
                        ServerId = serverId,
                        UpdatedUtc = now,
                    };
                    db.SkillTotals.Add(total);
                    existing[(row.AccountId, skill)] = total;
                }
                raised++;
            }
        }
        if (raised > 0)
            await db.SaveChangesAsync(ct);
        return raised;
    }

    /// <summary>Every account's standing on one board, best first. Ties break on the account
    /// id so two equal players hold stable ranks between requests. The overall board sums
    /// levels — a skill never reported counts at the first level, as the game would say — and
    /// orders by that sum, then by XP sum.</summary>
    public async Task<List<Standing>> StandingsAsync(Skill? skill, CancellationToken ct)
    {
        var rows = await db.SkillTotals
            .AsNoTracking()
            .Where(t => t.Account.DisplayName != null)
            .Select(t => new { t.AccountId, t.Account.DisplayName, t.Skill, t.Xp })
            .ToListAsync(ct);

        IEnumerable<Standing> standings;
        if (skill is { } s)
        {
            standings = rows
                .Where(r => r.Skill == (int)s)
                .Select(r => new Standing(r.AccountId, r.DisplayName!, Skills.LevelForXp(r.Xp), r.Xp));
        }
        else
        {
            standings = rows
                .GroupBy(r => r.AccountId)
                .Select(g =>
                {
                    var levels = 0;
                    long xp = 0;
                    foreach (var sk in Skills.All)
                    {
                        var row = g.FirstOrDefault(r => r.Skill == (int)sk);
                        var x = row?.Xp ?? 0;
                        levels += Skills.LevelForXp(x);
                        xp += x;
                    }
                    return new Standing(g.Key, g.First().DisplayName!, levels, xp);
                });
        }
        return standings
            .OrderByDescending(st => st.Level)
            .ThenByDescending(st => st.Xp)
            .ThenBy(st => st.AccountId)
            .ToList();
    }

    public async Task<HiscoresResponse> PageAsync(Skill? skill, int page, int pageSize, CancellationToken ct)
    {
        var standings = await StandingsAsync(skill, ct);
        page = Math.Max(page, 1);
        pageSize = Math.Clamp(pageSize, 1, PageSize);
        var rows = standings
            .Skip((page - 1) * pageSize)
            .Take(pageSize)
            .Select((st, i) => new HiscoreRow(
                (page - 1) * pageSize + i + 1, st.AccountId, st.DisplayName, st.Level, st.Xp))
            .ToArray();
        return new HiscoresResponse(
            skill?.Slug() ?? Skills.Overall, page, pageSize, standings.Count, rows);
    }

    /// <summary>One account's rank on every board it appears on.</summary>
    public async Task<MyRanksResponse> RanksAsync(Guid accountId, CancellationToken ct)
    {
        var ranks = new List<RankRow>();
        var overall = await StandingsAsync(null, ct);
        Add(overall, Skills.Overall);
        foreach (var skill in Skills.All)
            Add(await StandingsAsync(skill, ct), skill.Slug());
        return new MyRanksResponse([.. ranks]);

        void Add(List<Standing> board, string slug)
        {
            var i = board.FindIndex(st => st.AccountId == accountId);
            if (i >= 0)
                ranks.Add(new RankRow(slug, i + 1, board[i].Level, board[i].Xp));
        }
    }
}
