namespace DigBlock.Master.Hiscores;

/// <summary>The skill roster and the XP curve, mirroring <c>crates/digblock-core/src/skills.rs</c>.
/// The Rust side is the definition: a board that derived levels with a different table would
/// rank a player above someone the game says outranks them. Both halves are pinned from the
/// Rust suite (<c>the_masters_curve_mirrors_ours</c> reads this file), so the mirror cannot
/// drift silently. Discriminants are a storage format and append-only, exactly like
/// <c>AuthProvider</c>.</summary>
public enum Skill
{
    Mining = 0,
    Alchemy = 1,
    Arcana = 2,
}

public static class Skills
{
    public static readonly Skill[] All = [Skill.Mining, Skill.Alchemy, Skill.Arcana];

    /// <summary>The board's own key for "every skill summed", beside the
    /// three real ones. A query string word, never a discriminant.</summary>
    public const string Overall = "overall";

    public const int MaxLevel = 99;

    /// <summary>What the game calls it, in the one stored case — a page
    /// that shouts re-cases where it draws, as the client does.</summary>
    public static string Name(this Skill s) => s switch
    {
        Skill.Mining => "Mining",
        Skill.Alchemy => "Alchemy",
        Skill.Arcana => "Arcana",
        _ => throw new ArgumentOutOfRangeException(nameof(s)),
    };

    /// <summary>The URL and query-string spelling: the name, lowercased.</summary>
    public static string Slug(this Skill s) => s.Name().ToLowerInvariant();

    public static bool TryParse(string? slug, out Skill skill)
    {
        foreach (var s in All)
        {
            if (string.Equals(slug, s.Slug(), StringComparison.OrdinalIgnoreCase))
            {
                skill = s;
                return true;
            }
        }
        skill = default;
        return false;
    }

    public static bool IsKnown(int index) => index >= 0 && index < All.Length;

    // XP to advance FROM level L (index L-1) to L+1. THIS TABLE IS THE CURVE
    // on the Rust side too, not a cache of a formula — copied verbatim.
    private static readonly long[] XpToNext =
    [
        88, 95, 102, 109, 117, 125, 134,
        144, 154, 166, 177, 190, 204, 219,
        234, 251, 269, 289, 309, 332, 355,
        381, 408, 438, 469, 503, 539, 578,
        619, 664, 711, 762, 817, 876, 939,
        1006, 1078, 1156, 1239, 1328, 1423, 1525,
        1634, 1752, 1878, 2012, 2157, 2312, 2478,
        2656, 2846, 3050, 3269, 3504, 3756, 4025,
        4314, 4624, 4956, 5312, 5693, 6101, 6539,
        7009, 7512, 8051, 8629, 9248, 9912, 10624,
        11386, 12203, 13079, 14018, 15024, 16102, 17258,
        18497, 19825, 21248, 22773, 24407, 26159, 28036,
        30049, 32205, 34517, 36994, 39650, 42496, 45546,
        48815, 52318, 56073, 60098, 64411, 69034, 73989,
    ];

    /// <summary>The level a total implies — <c>level_for_xp</c>, step for step.</summary>
    public static int LevelForXp(long xp)
    {
        var level = 1;
        long cum = 0;
        foreach (var step in XpToNext)
        {
            cum += step;
            if (xp < cum)
                break;
            level++;
        }
        return level;
    }
}
