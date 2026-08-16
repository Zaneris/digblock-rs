using System.Buffers.Text;
using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;

namespace DigBlock.Master.Auth;

/// <summary>A sign-in the game client started and the browser has not
/// finished. Lives only between <c>/auth/start</c> and the provider
/// callback.</summary>
public sealed record PendingAuth(
    string RedirectUri,
    string ClientState,
    string ChallengeS256,
    string? Platform,
    DateTimeOffset ExpiresAt);

/// <summary>A finished sign-in, waiting for the client that started it to
/// redeem the code.</summary>
public sealed record ExchangeCode(
    Guid AccountId,
    string ChallengeS256,
    string? Platform,
    DateTimeOffset ExpiresAt);

/// <summary>The two short-lived halves of the sign-in redirect, both in
/// memory on purpose: they live for minutes, churn per sign-in, and losing
/// them to a restart costs a retry — the same trade the join-token store
/// already makes. Nothing here is worth an fsync.</summary>
public sealed class AuthRequestStore
{
    /// <summary>How long the browser has to finish the provider round trip.
    /// Generous: it can include creating a Google account from scratch.</summary>
    public static readonly TimeSpan PendingTtl = TimeSpan.FromMinutes(10);

    /// <summary>How long the client has to redeem the code once the browser
    /// has bounced back to it. Short — the client is sitting in a loopback
    /// accept loop or a page load, not waiting on a human.</summary>
    public static readonly TimeSpan CodeTtl = TimeSpan.FromMinutes(2);

    private readonly ConcurrentDictionary<string, PendingAuth> _pending = new();
    private readonly ConcurrentDictionary<string, ExchangeCode> _codes = new();

    public string StartPending(string redirectUri, string clientState, string challengeS256, string? platform)
    {
        var handle = Secrets.NewHandle();
        _pending[handle] = new PendingAuth(
            redirectUri, clientState, challengeS256, platform, DateTimeOffset.UtcNow + PendingTtl);
        return handle;
    }

    /// <summary>Take the pending request. One-shot: a callback replayed with
    /// the same handle finds nothing.</summary>
    public PendingAuth? ConsumePending(string? handle)
    {
        if (string.IsNullOrEmpty(handle) || !_pending.TryRemove(handle, out var pending))
            return null;
        return pending.ExpiresAt < DateTimeOffset.UtcNow ? null : pending;
    }

    /// <summary>Mint the one-time code the browser carries back to the
    /// client. Returned raw and stored by hash, like every other secret
    /// here, so a heap dump is not a pile of live sign-ins.</summary>
    public string IssueCode(Guid accountId, string challengeS256, string? platform)
    {
        var code = Secrets.New();
        _codes[Key(code)] = new ExchangeCode(
            accountId, challengeS256, platform, DateTimeOffset.UtcNow + CodeTtl);
        return code;
    }

    /// <summary>Redeem a code against its PKCE verifier.
    ///
    /// The code is removed whatever happens — a wrong verifier burns it
    /// rather than letting an attacker who read the code out of a browser
    /// history or a proxy log keep guessing. The verifier is what actually
    /// authorizes: only the client that started the flow knows it, which is
    /// what makes a leaked code useless (RFC 7636).</summary>
    public ExchangeCode? Redeem(string? code, string? verifier)
    {
        if (string.IsNullOrEmpty(code) || !_codes.TryRemove(Key(code), out var entry))
            return null;
        if (entry.ExpiresAt < DateTimeOffset.UtcNow)
            return null;
        if (verifier is null || !ChallengeMatches(entry.ChallengeS256, verifier))
            return null;
        return entry;
    }

    public void SweepExpired()
    {
        var now = DateTimeOffset.UtcNow;
        foreach (var (k, v) in _pending)
        {
            if (v.ExpiresAt < now)
                _pending.TryRemove(k, out _);
        }
        foreach (var (k, v) in _codes)
        {
            if (v.ExpiresAt < now)
                _codes.TryRemove(k, out _);
        }
    }

    /// <summary>The S256 code challenge for a verifier, base64url and
    /// unpadded — the encoding RFC 7636 specifies.</summary>
    public static string Challenge(string verifier) =>
        Base64Url.EncodeToString(SHA256.HashData(Encoding.ASCII.GetBytes(verifier)));

    public static bool ChallengeMatches(string challengeS256, string verifier) =>
        CryptographicOperations.FixedTimeEquals(
            Encoding.ASCII.GetBytes(challengeS256), Encoding.ASCII.GetBytes(Challenge(verifier)));

    /// <summary>A well-formed S256 challenge: 32 bytes, base64url, unpadded.
    /// Checked at <c>/auth/start</c> so a client that forgot PKCE fails
    /// immediately and visibly rather than at redemption.</summary>
    public static bool IsWellFormedChallenge(string? challenge) =>
        challenge is { Length: 43 } && Base64Url.IsValid(challenge);

    private static string Key(string code) => Convert.ToHexStringLower(Secrets.Hash(code));
}
