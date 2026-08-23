namespace DigBlock.Master;

/// <summary>The display-name rule, mirroring <c>validate_name</c> in
/// <c>crates/digblock-net/src/protocol.rs</c> — that Rust function is the
/// definition, this the backstop on the side that mints names, and the two must
/// move together. An explicit char walk rather than a regex: a regex counts
/// UTF-16 units where the Rust side counts <c>char</c>s.</summary>
public static class Names
{
    public const int MinLength = 3;
    public const int MaxLength = 10;

    /// <summary>The alphabet. ASCII-only on purpose — see the note on
    /// <c>is_name_char</c> in protocol.rs.</summary>
    public static bool IsNameChar(char c) => char.IsAsciiLetterOrDigit(c) || c is '-' or '_';

    public static bool IsValid(string? name)
    {
        if (name is null || name.Length < MinLength || name.Length > MaxLength)
            return false;
        foreach (var c in name)
        {
            if (!IsNameChar(c))
                return false;
        }
        return true;
    }

    /// <summary>The uniqueness key. Names are unique case-insensitively, so
    /// "Zaneris" and "zaneris" are one name — the display form keeps the
    /// case the player typed.</summary>
    public static string Normalize(string name) => name.ToLowerInvariant();

    /// <summary>The one refusal wording, so every endpoint that rejects a
    /// name says the same thing the client's own gate does.</summary>
    public const string Requirement = "name must be 3-10 characters: a-z A-Z 0-9 - _";
}
