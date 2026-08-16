using System.Collections.Concurrent;
using System.Security.Cryptography;

namespace DigBlock.Master;

/// <summary>What a consumed join token vouches for. The account id is the
/// identity the game server actually keys on; the name rides along so the
/// server does not need a second round trip to render a roster.</summary>
public sealed record TokenEntry(Guid ServerId, Guid AccountId, string DisplayName, DateTimeOffset ExpiresAt);

/// <summary>One-time join tokens: 16 random bytes as 32 lowercase hex chars
/// (sized to fit the game protocol's Hello token field). Consumption is an
/// atomic TryRemove — a token verifies successfully at most once, ever.
///
/// This is the ONE channel by which a game server learns who a player is.
/// The client never asserts its own identity: it presents 16 opaque bytes,
/// and the server asks us what they mean. That is what makes the account id
/// trustworthy on the far side.</summary>
public sealed class TokenStore
{
    private readonly ConcurrentDictionary<string, TokenEntry> _tokens = new();

    public string Mint(Guid serverId, Guid accountId, string displayName, TimeSpan ttl)
    {
        Span<byte> bytes = stackalloc byte[16];
        RandomNumberGenerator.Fill(bytes);
        var token = Convert.ToHexStringLower(bytes);
        _tokens[token] = new TokenEntry(serverId, accountId, displayName, DateTimeOffset.UtcNow + ttl);
        return token;
    }

    /// <summary>Consume a token on behalf of <paramref name="serverId"/>.
    /// The token is removed no matter what; it only *succeeds* when it was
    /// minted for that server and has not expired.</summary>
    public bool TryConsume(string token, Guid serverId, out TokenEntry consumed)
    {
        consumed = null!;
        if (!_tokens.TryRemove(token, out var entry))
            return false;
        if (entry.ServerId != serverId || entry.ExpiresAt < DateTimeOffset.UtcNow)
            return false;
        consumed = entry;
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
