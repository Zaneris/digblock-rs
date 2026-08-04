using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;

namespace DigBlock.Master;

public sealed class ServerEntry
{
    public required Guid Id { get; init; }

    /// <summary>Bearer secret for this server instance (64 hex chars). Never listed.</summary>
    public required string Key { get; init; }

    public required string Name { get; init; }
    public required string Host { get; init; }

    /// <summary>Where health probes dial (differs from Host only in
    /// NAT-split dev topologies; see RegisterRequest.ProbeHost).</summary>
    public required string ProbeHost { get; init; }

    public required int Port { get; init; }

    /// <summary>SHA-256 of the server's TLS certificate (32 bytes).</summary>
    public required byte[] CertHash { get; init; }

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

/// <summary>In-memory list of registered game servers. Lost on restart —
/// game servers re-register when their heartbeat gets a 404.</summary>
public sealed class ServerRegistry
{
    private readonly ConcurrentDictionary<Guid, ServerEntry> _servers = new();

    // Serializes mutations so "replace any existing entry at the same
    // host:port" is atomic. Reads stay lock-free on the dictionary.
    private readonly Lock _mutate = new();

    public IReadOnlyList<ServerEntry> Snapshot() => [.. _servers.Values];

    public bool TryGet(Guid id, out ServerEntry entry) => _servers.TryGetValue(id, out entry!);

    /// <summary>Listed server names never exceed this many characters.</summary>
    public const int MaxNameLength = 24;

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

    /// <summary>Register a server, replacing any existing entry at the same
    /// host:port (the restart-with-new-cert path). The replaced entry's id and
    /// key die with it. The name is sanitized, not validated.</summary>
    public ServerEntry Register(
        string name, string host, string probeHost, int port, byte[] certHash, int players, int maxPlayers)
    {
        name = SanitizeName(name);
        lock (_mutate)
        {
            foreach (var existing in _servers.Values)
            {
                if (existing.Host == host && existing.Port == port)
                    _servers.TryRemove(existing.Id, out _);
            }
            var entry = new ServerEntry
            {
                Id = Guid.NewGuid(),
                Key = NewSecretHex(),
                Name = name,
                Host = host,
                ProbeHost = probeHost,
                Port = port,
                CertHash = certHash,
                Players = players,
                MaxPlayers = maxPlayers,
                LastSeenUtc = DateTimeOffset.UtcNow,
            };
            _servers[entry.Id] = entry;
            return entry;
        }
    }

    public bool Remove(Guid id)
    {
        lock (_mutate)
        {
            return _servers.TryRemove(id, out _);
        }
    }

    /// <summary>Constant-time key check for a known entry.</summary>
    public static bool KeyMatches(ServerEntry entry, string? bearer) =>
        bearer is not null
        && CryptographicOperations.FixedTimeEquals(
            Encoding.ASCII.GetBytes(bearer), Encoding.ASCII.GetBytes(entry.Key));

    /// <summary>Resolve which registered server a bearer key belongs to
    /// (token verification carries no server id).</summary>
    public ServerEntry? FindByKey(string? bearer)
    {
        if (bearer is null)
            return null;
        foreach (var entry in _servers.Values)
        {
            if (KeyMatches(entry, bearer))
                return entry;
        }
        return null;
    }

    private static string NewSecretHex()
    {
        Span<byte> bytes = stackalloc byte[32];
        RandomNumberGenerator.Fill(bytes);
        return Convert.ToHexStringLower(bytes);
    }
}
