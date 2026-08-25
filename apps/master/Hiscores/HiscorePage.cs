using System.Net;
using System.Text;
using DigBlock.Master.Auth;

namespace DigBlock.Master.Hiscores;

/// <summary>The board as a page. Server-rendered and script-free like the sign-in pages: a
/// leaderboard that needs JavaScript to show a table is a leaderboard that fails on the one
/// device that only wanted to check a rank. Board tabs and paging are links.</summary>
public static class HiscorePage
{
    public static string Render(HiscoresResponse r)
    {
        var body = new StringBuilder();
        body.Append("<nav class=\"tabs\">");
        body.Append(Tab(Skills.Overall, "Overall", r.Skill));
        foreach (var s in Skills.All)
            body.Append(Tab(s.Slug(), s.Name(), r.Skill));
        body.Append("</nav>");

        var isOverall = r.Skill == Skills.Overall;
        body.Append("<table><thead><tr><th class=\"num\">Rank</th><th>Name</th>")
            .Append(isOverall ? "<th class=\"num\">Total level</th><th class=\"num\">Total XP</th>" : "<th class=\"num\">Level</th><th class=\"num\">XP</th>")
            .Append("</tr></thead><tbody>");
        if (r.Rows.Length == 0)
        {
            body.Append("<tr><td colspan=\"4\" class=\"empty\">Nobody here yet.</td></tr>");
        }
        foreach (var row in r.Rows)
        {
            body.Append("<tr><td class=\"num\">").Append(row.Rank)
                .Append("</td><td>").Append(Esc(row.DisplayName))
                .Append("</td><td class=\"num\">").Append(row.Level)
                .Append("</td><td class=\"num\">").Append(row.Xp.ToString("N0"))
                .Append("</td></tr>");
        }
        body.Append("</tbody></table>");

        var last = Math.Max(1, (r.Total + r.PageSize - 1) / r.PageSize);
        body.Append("<nav class=\"pager\">");
        body.Append(r.Page > 1
            ? $"<a href=\"{Href(r.Skill, r.Page - 1)}\">&larr; Previous</a>"
            : "<span></span>");
        body.Append($"<span class=\"fine\">Page {r.Page} of {last} &middot; {r.Total:N0} ranked</span>");
        body.Append(r.Page < last
            ? $"<a href=\"{Href(r.Skill, r.Page + 1)}\">Next &rarr;</a>"
            : "<span></span>");
        body.Append("</nav>");

        return Pages.Shell("Hiscores", body.ToString(), Style, wide: true);
    }

    private static string Tab(string slug, string label, string current) =>
        slug == current
            ? $"<span class=\"tab current\">{Esc(label)}</span>"
            : $"<a class=\"tab\" href=\"{Href(slug, 1)}\">{Esc(label)}</a>";

    private static string Href(string slug, int page) =>
        page == 1 ? $"/hiscores?skill={Esc(slug)}" : $"/hiscores?skill={Esc(slug)}&amp;page={page}";

    private static string Esc(string s) => WebUtility.HtmlEncode(s);

    private const string Style = """
        .tabs { display: flex; gap: .5rem; margin: 0 0 1rem; flex-wrap: wrap; }
        .tab { padding: .35rem .8rem; border-radius: 6px; background: #2a2a2a; color: #f0f0f0; text-decoration: none; }
        .tab:hover { background: #343434; }
        .tab.current { background: #f0f0f0; color: #101010; }
        table { width: 100%; border-collapse: collapse; }
        th, td { padding: .45rem .6rem; text-align: left; border-bottom: 1px solid #2e2e2e; }
        th { color: #909090; font-weight: 500; font-size: .875rem; }
        .num { text-align: right; font-variant-numeric: tabular-nums; }
        .empty { text-align: center; color: #909090; padding: 2rem; }
        .pager { display: flex; justify-content: space-between; align-items: center; margin-top: 1rem; }
        .pager a { color: #f0f0f0; }
        """;
}
