using System.Collections.Concurrent;
using System.Security.Cryptography;

namespace DigBlock.Master;

public sealed record TokenEntry(Guid ServerId, string DisplayName, DateTimeOffset ExpiresAt);

/// <summary>One-time join tokens: 16 random bytes as 32 lowercase hex chars
/// (sized to fit the game protocol's Hello token field). Consumption is an
/// atomic TryRemove — a token verifies successfully at most once, ever.</summary>
public sealed class TokenStore
{
    private readonly ConcurrentDictionary<string, TokenEntry> _tokens = new();

    public string Mint(Guid serverId, string displayName, TimeSpan ttl)
    {
        Span<byte> bytes = stackalloc byte[16];
        RandomNumberGenerator.Fill(bytes);
        var token = Convert.ToHexStringLower(bytes);
        _tokens[token] = new TokenEntry(serverId, displayName, DateTimeOffset.UtcNow + ttl);
        return token;
    }

    /// <summary>Consume a token on behalf of <paramref name="serverId"/>.
    /// The token is removed no matter what; it only *succeeds* when it was
    /// minted for that server and has not expired.</summary>
    public bool TryConsume(string token, Guid serverId, out string displayName)
    {
        displayName = "";
        if (!_tokens.TryRemove(token, out var entry))
            return false;
        if (entry.ServerId != serverId || entry.ExpiresAt < DateTimeOffset.UtcNow)
            return false;
        displayName = entry.DisplayName;
        return true;
    }

    public void SweepExpired()
    {
        var now = DateTimeOffset.UtcNow;
        foreach (var (token, entry) in _tokens)
        {
            if (entry.ExpiresAt < now)
                _tokens.TryRemove(token, out _);
        }
    }
}
