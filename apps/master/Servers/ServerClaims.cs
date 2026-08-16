using System.Security.Cryptography;
using DigBlock.Master.Data;
using Microsoft.EntityFrameworkCore;

namespace DigBlock.Master.Servers;

/// <summary>Who holds which <c>host:port</c>, and the key that proves it.
///
/// Registration stays anonymous — no operator account, no signup, a
/// self-hoster still just runs the container. What changed is that the
/// FIRST registration of an address mints a key and the address remembers
/// it, so a later registration has to present it. That is the whole of
/// "anonymous but sticky", and it closes the takeover in NET-AUDIT-2.md
/// §1.2 without adding a step to anybody's day.
///
/// Persisted (unlike the liveness registry) for two reasons: the key has to
/// outlive a master restart or the sticky check would reset every deploy,
/// and a server keeps its <c>ServerId</c> across one.</summary>
public sealed class ServerClaims(MasterDb db)
{
    /// <summary>Addresses are compared lowercased. A DNS name is
    /// case-insensitive, and an IP literal is unaffected.</summary>
    public static string Normalize(string host) => host.Trim().ToLowerInvariant();

    public Task<ServerClaim?> FindAsync(string host, int port, CancellationToken ct)
    {
        var norm = Normalize(host);
        return db.ServerClaims.SingleOrDefaultAsync(c => c.Host == norm && c.Port == port, ct);
    }

    public static bool KeyMatches(ServerClaim claim, string? presented) =>
        presented is not null
        && CryptographicOperations.FixedTimeEquals(Secrets.Hash(presented), claim.KeyHash);

    /// <summary>Claim an address for the first time, or re-key one whose
    /// holder has lost its key. Returns the raw key, which is the only time
    /// it exists outside the game server.</summary>
    public async Task<(ServerClaim Claim, string Key)> ClaimAsync(
        string host, int port, string name, ServerClaim? existing, CancellationToken ct)
    {
        var key = Secrets.New();
        var now = DateTimeOffset.UtcNow;
        if (existing is null)
        {
            existing = new ServerClaim
            {
                Id = Guid.NewGuid(),
                Host = Normalize(host),
                Port = port,
                KeyHash = Secrets.Hash(key),
                Name = name,
                FirstSeenUtc = now,
                LastRegisteredUtc = now,
            };
            db.ServerClaims.Add(existing);
        }
        else
        {
            // Re-key in place: the id is what clients may have bookmarked
            // and what the operator sees in logs, so losing ./data should
            // not also change a server's identity.
            existing.KeyHash = Secrets.Hash(key);
            existing.Name = name;
            existing.LastRegisteredUtc = now;
        }
        await db.SaveChangesAsync(ct);
        return (existing, key);
    }

    /// <summary>A re-registration that presented the right key: nothing to
    /// mint, just record that it happened.</summary>
    public async Task TouchAsync(ServerClaim claim, string name, CancellationToken ct)
    {
        claim.Name = name;
        claim.LastRegisteredUtc = DateTimeOffset.UtcNow;
        await db.SaveChangesAsync(ct);
    }
}
