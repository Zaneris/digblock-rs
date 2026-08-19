using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;

namespace DigBlock.Master;

public sealed class ServerEntry
{
    /// <summary>Stable across master restarts: it comes from the persisted
    /// <see cref="Data.ServerClaim"/> rather than being minted per
    /// registration.</summary>
    public required Guid Id { get; init; }

    /// <summary>SHA-256 of the registration key. The raw key exists only in
    /// the register response and in the game server's own data directory —
    /// the master never keeps a copy it could leak.</summary>
    public required byte[] KeyHash { get; init; }

    public required string Name { get; init; }
    public required string Host { get; init; }

    /// <summary>Where health probes dial. Equal to <see cref="Host"/> except
    /// under Master:Dev:ProbeHostOverride, which exists for the containerized
    /// dev stack (a LAN-IP hairpin breaks QUIC address validation behind
    /// Docker's NAT). This used to be a field of the registration REQUEST,
    /// which meant an anonymous caller chose the only address the master ever
    /// dialed while clients were handed a different one. It is master-side
    /// configuration now.</summary>
    public required string ProbeHost { get; init; }

    public required int Port { get; init; }

    /// <summary>SHA-256 of the server's TLS certificate (32 bytes).</summary>
    public required byte[] CertHash { get; init; }

    /// <summary>Wire and behaviour versions the server registered with.
    /// Published in the listing so a client can tell an incompatible server
    /// apart before spending a QUIC connect on it.</summary>
    public required int ProtocolVersion { get; init; }
    public required int SimVersion { get; init; }

    public int Players;
    public int MaxPlayers;

    /// <summary>Last proof of life: successful probe or heartbeat.</summary>
    public DateTimeOffset LastSeenUtc;

    /// <summary>Consecutive failed health probes.</summary>
    public int Strikes;

    // Observed-address hosts can be IPv6 literals, which URLs bracket.
    public string Url => Host.Contains(':') ? $"https://[{Host}]:{Port}" : $"https://{Host}:{Port}";
    public string CertHashHex => Convert.ToHexStringLower(CertHash);
}

/// <summary>Which registered servers are alive right now, and at what.
///
/// Still in memory, still lost on restart — a game server re-registers when
/// its heartbeat gets a 404, and rebuilding from heartbeats takes under a
/// minute. What is NOT here is the claim on an address and the key that
/// proves it; those are persisted (see
/// <see cref="Servers.ServerClaims"/>), because losing them on a restart
/// would reopen the address every deploy.</summary>
public sealed class ServerRegistry
{
    private readonly ConcurrentDictionary<Guid, ServerEntry> _servers = new();

    // Hash-keyed index for the bearer check on the join path. Keyed on the
    // SHA-256, never the key itself, so a lookup leaks nothing invertible.
    private readonly ConcurrentDictionary<string, Guid> _byKeyHash = new();

    // Serializes mutations so replacing an entry at the same host:port is
    // atomic. Reads stay lock-free on the dictionary.
    private readonly Lock _mutate = new();

    public IReadOnlyList<ServerEntry> Snapshot() => [.. _servers.Values];

    public bool TryGet(Guid id, out ServerEntry entry) => _servers.TryGetValue(id, out entry!);

    /// <summary>Is anything currently live at this address? This is the
    /// "already registered and active" half of the registration rules: a
    /// live address may only be re-registered by whoever holds its key.</summary>
    public ServerEntry? FindLive(string host, int port)
    {
        foreach (var entry in _servers.Values)
        {
            if (string.Equals(entry.Host, host, StringComparison.OrdinalIgnoreCase) && entry.Port == port)
                return entry;
        }
        return null;
    }

    /// <summary>Listed server names never exceed this many characters.</summary>
    public const int MaxNameLength = 24;

    /// <summary>Ceiling for a listed player count.</summary>
    public const int MaxPlayerCount = 1000;

    /// <summary>Player counts are display-only, so like names they are
    /// repaired rather than rejected — but they MUST be repaired, because
    /// the clients decode them as unsigned. A negative count fails the whole
    /// server-list decode on the client side, so one server registering
    /// <c>players: -1</c> hid every server on the master from everyone. The
    /// clients now clamp too; this is the other half, so the bad value never
    /// leaves here.</summary>
    public static int SanitizeCount(int n) => Math.Clamp(n, 0, MaxPlayerCount);

    /// <summary>Server names are display-only, so a bad one is repaired, never
    /// rejected: keep letters/digits/space/-/_, collapse space runs, trim, cap
    /// at <see cref="MaxNameLength"/>; an empty result falls back to the game
    /// server's own default name. Registration must not fail over cosmetics —
    /// self-hosters would see UNREACHABLE-style mystery, not a validation hint.</summary>
    public static string SanitizeName(string? raw)
    {
        var sb = new StringBuilder(MaxNameLength);
        foreach (var c in raw ?? "")
        {
            var keep = char.IsAsciiLetterOrDigit(c) || c is ' ' or '-' or '_';
            if (!keep)
                continue;
            if (c == ' ' && (sb.Length == 0 || sb[^1] == ' '))
                continue;
            sb.Append(c);
            if (sb.Length == MaxNameLength)
                break;
        }
        var name = sb.ToString().TrimEnd();
        return name.Length == 0 ? "DigBlock Server" : name;
    }

    /// <summary>Publish a server as live, replacing any existing entry at the
    /// same host:port (the restart-with-new-cert path). The id and key hash
    /// come from the caller's persisted claim, so a re-registration keeps
    /// both. The name is sanitized, not validated.</summary>
    public ServerEntry Register(
        Guid id, byte[] keyHash, string name, string host, string probeHost, int port,
        byte[] certHash, int players, int maxPlayers, int protocolVersion, int simVersion)
    {
        name = SanitizeName(name);
        lock (_mutate)
        {
            foreach (var existing in _servers.Values)
            {
                if (existing.Id == id
                    || (string.Equals(existing.Host, host, StringComparison.OrdinalIgnoreCase)
                        && existing.Port == port))
                    RemoveLocked(existing.Id);
            }
            var entry = new ServerEntry
            {
                Id = id,
                KeyHash = keyHash,
                Name = name,
                Host = host,
                ProbeHost = probeHost,
                Port = port,
                CertHash = certHash,
                ProtocolVersion = protocolVersion,
                SimVersion = simVersion,
                Players = players,
                MaxPlayers = maxPlayers,
                LastSeenUtc = DateTimeOffset.UtcNow,
            };
            _servers[entry.Id] = entry;
            _byKeyHash[HashKey(keyHash)] = entry.Id;
            return entry;
        }
    }

    public bool Remove(Guid id)
    {
        lock (_mutate)
        {
            return RemoveLocked(id);
        }
    }

    private bool RemoveLocked(Guid id)
    {
        if (!_servers.TryRemove(id, out var gone))
            return false;
        // Only drop the index entry if it still points at what we removed —
        // a re-registration under the same key must not unpublish itself.
        var key = HashKey(gone.KeyHash);
        if (_byKeyHash.TryGetValue(key, out var mapped) && mapped == id)
            _byKeyHash.TryRemove(key, out _);
        return true;
    }

    public static bool KeyMatches(ServerEntry entry, string? bearer) =>
        bearer is not null
        && CryptographicOperations.FixedTimeEquals(Secrets.Hash(bearer), entry.KeyHash);

    /// <summary>Resolve which registered server a bearer key belongs to
    /// (token verification carries no server id).</summary>
    public ServerEntry? FindByKey(string? bearer)
    {
        if (bearer is null)
            return null;
        return _byKeyHash.TryGetValue(HashKey(Secrets.Hash(bearer)), out var id)
            && _servers.TryGetValue(id, out var entry)
                ? entry
                : null;
    }

    private static string HashKey(byte[] keyHash) => Convert.ToHexStringLower(keyHash);
}
