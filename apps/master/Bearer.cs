namespace DigBlock.Master;

/// <summary>Pulls the bearer credential off a request. One helper, because
/// three different kinds of caller present one — a player's session, a game
/// server's registration key, and nothing at all — and they must all be
/// read the same way.</summary>
public static class Bearer
{
    private const string Prefix = "Bearer ";

    public static string? From(HttpContext http)
    {
        string? header = http.Request.Headers.Authorization;
        return header?.StartsWith(Prefix, StringComparison.Ordinal) == true ? header[Prefix.Length..] : null;
    }
}

/// <summary>CORS policy names. The browser client calls a growing set of
/// endpoints cross-origin; naming the policy once stops a new endpoint
/// quietly shipping without it and only failing in the web build.</summary>
public static class Cors
{
    public const string Client = "client";
}
